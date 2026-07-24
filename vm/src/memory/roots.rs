// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GC root scanning — collects all live ObjectRefs from the VM state.
//!
//! Root sources:
//! 1. Thread frames (locals + operand stacks)
//! 2. Static fields
//! 3. Class lock objects (for static synchronized methods)
//! 4. Thread printed values (test harness)

use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;

/// True when interpreter-frame locals should be scanned CONSERVATIVELY (every
/// pointer-shaped slot, validated by the strict `is_object_address` header
/// probe) in addition to the tag-filtered scan. Enabled exactly when the
/// non-moving + selective-promotion sweep is the collector that will run — i.e.
/// any thread is in JIT (`gc_quiescence::is_active()`), the only mode in which a
/// false-positive root is harmless (nothing is relocated). Off on the moving
/// (no-JIT) path, where a pointer-shaped `long` rooted here would be relocated
/// and corrupted. Opt out entirely with `CRATONVM_NO_CONSERVATIVE_LOCALS`.
#[inline]
pub(crate) fn conservative_locals_enabled() -> bool {
    use std::sync::OnceLock;
    // Blast-radius bound: only under the opt-in real-ForkJoinPool gate (the gate
    // the multi-thread reclamation bug lives under — see the FJP-worker test
    // case). With it OFF — the default for the entire app gauntlet and the
    // bintrees benchmarks — this returns false and the root scan is byte-
    // identical to baseline (no extra `is_object_address` probes, no over-pin
    // risk). Opt out even under the gate with `CRATONVM_NO_CONSERVATIVE_LOCALS`.
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let base = *ENABLED.get_or_init(|| {
        std::env::var_os("CRATONVM_REAL_FORKJOINPOOL").is_some()
            && std::env::var_os("CRATONVM_NO_CONSERVATIVE_LOCALS").is_none()
    });
    base && cratonvm_gc::gc_quiescence::is_active()
}

/// Collect all GC root ObjectRefs from the shared VM state and the current thread.
///
/// Returns a vector of all live non-null ObjectRefs reachable from:
/// - Thread frame locals and operand stacks
/// - Static fields (all classes)
/// - Class lock objects (synthetic monitors for static synchronized methods)
/// - Thread printed values (test harness output)
pub fn collect_roots(shared: &SharedVm, thread: &JvmThread) -> Vec<ObjectRef> {
    let mut roots = Vec::new();

    // Stage B (precise oop maps, B-K fix): reset the movable precise-JIT-root
    // set so it reflects only THIS collection's stack. `scan_active_jit_frames`
    // below republishes the covered, rewritable JIT-frame oops; the young
    // collector then excludes them from the pin set. No-op unless precise
    // relocation is engaged (the set stays empty on the default path).
    cratonvm_gc::gc_quiescence::clear_movable_jit_roots();
    // G1 pin-in-place: reset the conservative-JIT-root pin set too, so it
    // reflects only THIS collection's stack (republished by the JIT-frame scan
    // below, under G1). See that scan site and `G1Collector::young_collection`.
    cratonvm_gc::gc_quiescence::clear_pinned_jit_roots();
    // Reset the per-cycle incomplete-JIT-coverage fallback. The JIT root scan
    // below sets it again if moving-young must use the conservative/non-moving
    // path for this collection.
    cratonvm_gc::gc_quiescence::clear_force_non_moving_jit_roots();
    // A5 fix: reset the unregistered-JIT-frame flag; `scan_active_jit_frames`
    // below re-sets it iff it finds a guard-less JIT frame on the native stack,
    // and the generational collector consults it to pick the non-moving sweep.
    cratonvm_gc::gc_quiescence::clear_unregistered_jit_frame_on_stack();

    // 1. Thread frames — scan locals and operand stacks (SoA layout).
    //
    // Spring Boot SEGV fix (2026-05-16): `ValueStack::scan_object_refs`
    // still reports `CompactTag::Long` operand-stack slots whose bits
    // happen to look like an aligned pointer as roots without consulting
    // the heap (the bug already removed from `Frame::scan_local_objects`
    // in frame.rs). Filter operand-stack-sourced roots against
    // `heap.is_object_address` so a primitive `long` (file size, hash,
    // jboss-modules token) can no longer poison the root set and cause a
    // `0xC0000005` SEGV when the GC later dereferences the bogus pointer.
    // Multi-thread non-moving-sweep root hardening (Fork6 FJP reclamation):
    // when the non-moving + selective-promotion sweep is the collector that will
    // run (any thread in JIT → `gc_quiescence::is_active()`), conservatively
    // probe every local for a lost-tag object reference. A JIT callee's object
    // return value can reach an interpreter local under a non-object tag (e.g.
    // `main`'s `f = POOL.submit(t)`); the tag-filtered `scan_local_objects` then
    // omits it, so selective promotion neither pins nor remaps it and the young
    // slot is evacuated+zeroed → stale all-zero receiver. The conservative probe
    // roots (and thereby PINS, since the pin set is keyed by root value) such
    // slots. Sound here ONLY because this collector never relocates — a
    // false-positive can only over-retain. Opt out with
    // `CRATONVM_NO_CONSERVATIVE_LOCALS`.
    let conservative_locals = conservative_locals_enabled();
    for frame in thread.frames.iter() {
        if conservative_locals {
            // In the non-moving stress collector, local-liveness precision can
            // drop an active FJP receiver while it is also parked in a callee
            // frame. Over-retaining a dead reference is harmless here; missing
            // the receiver lets selective promotion zero it under `join()`.
            frame.scan_local_objects_all_live(&mut roots, &shared.heap);
        } else {
            frame.scan_local_objects(&mut roots, &shared.heap);
        }
        if conservative_locals {
            frame.scan_locals_conservative(&mut roots, &shared.heap);
        }
        let before = roots.len();
        frame.stack.scan_object_refs(&mut roots, &shared.heap);
        if roots.len() > before {
            let added = roots.split_off(before);
            for o in added {
                let addr = o.as_ptr() as usize;
                if shared.heap.is_object_address(addr).is_some() {
                    roots.push(o);
                }
            }
        }
        if conservative_locals {
            frame
                .stack
                .scan_object_refs_conservative(&mut roots, &shared.heap);
        }
    }

    // 2. Static fields — all classes
    {
        let statics = shared.statics.read();
        for fields in statics.values() {
            for val in fields {
                if let Value::Object(Some(obj_ref)) = val {
                    roots.push(*obj_ref);
                }
            }
        }
    }

    // 3. Class lock objects — synthetic objects for static synchronized methods
    {
        let class_locks = shared.class_locks.read();
        for obj_ref in class_locks.values() {
            roots.push(*obj_ref);
        }
    }

    // 4. Thread printed values — test harness output buffer
    for val in &thread.printed {
        if let Value::Object(Some(obj_ref)) = val {
            roots.push(*obj_ref);
        }
    }

    // 4b. Native invoke pins — object args popped off the operand stack for
    //     `safe_native_call` (see `JvmThread::native_pin_roots`).
    for obj_ref in &thread.native_pin_roots {
        roots.push(*obj_ref);
    }

    // ---- handle scope support (arch/handles) ----
    // 4b'. Rooted-handle slots — `NativeContext::handle_root`'s per-thread
    //      backing store (`JvmThread::handle_slots`, see that field's doc
    //      comment). Same shape as the `native_pin_roots` splice just above:
    //      a live (`Some`) slot is a GC root exactly like a pin. A `None`
    //      hole is an already-released slot and contributes nothing.
    //
    //      NOTE (integration gap, out of scope for this change): a moving
    //      collection must ALSO rewrite these slots in place after
    //      relocating an object, mirroring the `native_pin_roots` remap
    //      loop in `vm::memory::gc::update_all_roots`
    //      (vm/src/memory/gc.rs, right after that function's own
    //      `native_pin_roots` block) — that companion remap has NOT been
    //      added yet. Until it lands, a handle survives a NON-moving
    //      collection correctly (this scan keeps the slot's object alive)
    //      but is NOT yet immune to going stale across a MOVING collection,
    //      same residual risk `native_pin_roots` would have without its own
    //      remap loop. See docs/feature-designs/native-handle-discipline.md.
    for slot in &thread.handle_slots {
        if let Some(obj_ref) = slot {
            roots.push(*obj_ref);
        }
    }

    // 4c. Native object in flight — object return before the interpreter pushes
    //     it onto the operand stack, or native-thrown exception before it is
    //     routed into a Java handler / uncaught dispatch.
    if let Some(obj_ref) = thread.native_pending_return {
        roots.push(obj_ref);
    }

    // 4d. Direct JIT HashMap node cache. Both refs remain valid across a
    // moving collection because this scan and gc.rs remap them with the thread.
    for entry in &thread.jit_hashmap_string_node_cache {
        roots.push(entry.map);
        roots.push(entry.node);
    }
    for entry in &thread.string_case_cache {
        roots.extend([entry.source, entry.first, entry.second]);
    }

    // 5. Interned string pool — all interned String objects
    {
        let string_pool = shared.string_pool.read();
        for obj_ref in string_pool.values() {
            roots.push(*obj_ref);
        }
    }

    // 6. Class mirror cache — java.lang.Class objects
    //
    // A mirror's `classLoader` field is a real heap edge to its defining
    // ClassLoader, so unconditionally rooting every mirror ever created here
    // keeps that loader alive forever too — completely defeating
    // `CRATONVM_LOADER_UNLOAD` (default ON, see `gc_scan_loader_singleton_roots`)
    // for any class that ever had a mirror created (`getClass()`, reflection,
    // annotation scanning — i.e. virtually every class). Symptom: a
    // `WeakReference<Class<?>>` (e.g. Tomcat's `ManagedConcurrentWeakHashMap`
    // used by `DefaultInstanceManager`'s annotation cache) never clears for an
    // unloaded webapp class even after its ClassLoader is otherwise
    // unreachable — `TestDefaultInstanceManager.testClassUnloading` count
    // off-by-one.
    //
    // Built-in loaders (bootstrap/extension/application) are permanent for the
    // process lifetime, so their classes' mirrors stay unconditionally rooted.
    // Classes loaded by a user-defined `ClassLoader` are NOT unconditionally
    // rooted here when the gate is on — otherwise this loop would defeat
    // `defining_loader_store`'s own unloading the same way it defeated it
    // before this fix. Such a mirror still stays alive whenever its DEFINING
    // LOADER is independently reachable (built-in-loader liveness, another
    // live reference to the loader, or a live instance of one of its OTHER
    // classes via `loader_pin`) via the `cratonvm_types::mirror_pin`
    // propagation the GC marker consults for exactly this — mirroring
    // `loader_pin`'s instance→loader edge in the opposite direction, since
    // CratonVM's synthetic `ClassLoader` model has no heap-traceable
    // `ClassLoader.classes` bookkeeping to make plain reachability of the
    // mirror alone sufficient. A mirror whose loader turns out unreachable
    // this cycle is pruned post-GC by `memory::gc::reconcile_class_mirrors`.
    //
    // The mirror_pin propagation is only wired into the Generational
    // collector's NON-MOVING marker (`gen_heap.rs`'s `mark_young` worklist
    // loop + old-gen BFS, both keyed off a STABLE object address — see the
    // mirror_pin call sites there) — the SAME collector
    // `conservative_locals_enabled` above already keys off
    // `gc_quiescence::is_active()` for an analogous reason. It is NOT wired
    // into G1's or ZGC's own marker (a pre-existing gap shared with
    // `loader_pin` itself, which also only instruments `gen_heap.rs`), nor
    // into the Generational collector's MOVING young-gen Cheney-copy path,
    // which relocates objects while scanning and would need the mirror_pin
    // lookup keyed by each object's PRE-copy address (not implemented this
    // pass — no test exercises it and it is a materially different,
    // higher-risk change to the copying loop). So: only take mirrors out of
    // the unconditional root set when BOTH the configured algorithm is
    // Generational AND its non-moving marker is what will actually run this
    // cycle. Under G1/ZGC or the moving path this falls back to the original
    // (safe, if still-leaky) unconditional rooting.
    //
    // Classification note: whether to skip unconditional rooting MUST use a
    // PERMANENT signal — `class_manager`'s `ClassLoaderId::UserDefined(_)`,
    // set once when the class is registered and never cleared — NOT
    // `defining_loader_for` (a mutable liveness side-table pruned by
    // `gc_reconcile_defining_loaders` the moment a loader is confirmed dead).
    // Using the mutable signal here is a trap that reintroduces this exact
    // bug: the instant a user-defined class's own loader legitimately dies
    // and its `defining_loader_store` entry is pruned, `defining_loader_for`
    // starts returning `None` for it — indistinguishable from "always was a
    // built-in class" — which would flip this loop to root it
    // UNCONDITIONALLY forever from that point on (verified: this exact
    // mistake measured `expected:8 actual:9`, i.e. no improvement at all,
    // because the JSP-eviction test's whole point is a loader dying mid-run).
    // `defining_loader_for` remains the right call for `mirror_pin`'s OWN
    // bookkeeping below (rebuild_mirror_pins / gen_heap.rs) — there it
    // legitimately means "does this class currently have a live pairing,"
    // which is exactly what that machinery wants.
    {
        let class_mirrors = shared.class_mirrors.read();
        if cratonvm_native_builtins::classloader::loader_unload_enabled()
            && shared.config.gc_algorithm == crate::config::GcAlgorithm::Generational
            && (cratonvm_gc::gc_quiescence::is_active()
                || cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack()
                || cratonvm_gc::gc_quiescence::major_gc_requested())
        {
            let cm = shared.class_manager.read();
            for (&class_id, obj_ref) in class_mirrors.iter() {
                let is_user_defined = cm.get_class(class_id).is_some_and(|c| {
                    matches!(
                        c.loader_id,
                        crate::classloading::ClassLoaderId::UserDefined(_)
                    )
                });
                if is_user_defined {
                    continue;
                }
                roots.push(*obj_ref);
            }
        } else {
            for obj_ref in class_mirrors.values() {
                roots.push(*obj_ref);
            }
        }
    }

    // 7. System streams (System.out, System.err, System.in)
    //
    // try_read, NOT read: the singleton initializers (e.g.
    // `ensure_system_stdin_object`) hold the WRITE guard across allocating
    // calls, and an allocation-triggered GC on that same thread would
    // self-deadlock on the non-reentrant RwLock (observed live: instant
    // 0-output wedge at the first stress GC inside the stdin window). A
    // locked guard means the initializer is mid-population — the in-flight
    // object is covered by its native_pin_roots pin, and the cache slot is
    // not yet (or already) consistent, so skipping the scan is sound.
    {
        if let Some(g) = shared.system_out.try_read() {
            if let Some(out_ref) = *g {
                roots.push(out_ref);
            }
        }
        if let Some(g) = shared.system_err.try_read() {
            if let Some(err_ref) = *g {
                roots.push(err_ref);
            }
        }
        // gcstress residual face fix — `system_in` was missing from both this
        // root scan and the update_all_roots remap (out/err had both): the
        // cached System.in FileInputStream went stale on the first moving
        // young GC after `ensure_system_stdin_object` populated it, and every
        // later use of the cache served a dangling ObjectRef.
        if let Some(g) = shared.system_in.try_read() {
            if let Some(in_ref) = *g {
                roots.push(in_ref);
            }
        }
    }

    // 8. Primitive type Class mirrors (int.class, boolean.class, etc.)
    {
        let prim_mirrors = shared.primitive_mirrors.read();
        for obj_ref in prim_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    // 8a. Canonical java.lang.Module mirrors (one per module name). These are
    //     long-lived singletons handed back by `Class.getModule()`; without
    //     rooting them a moving GC would reclaim/relocate them and the cache
    //     in `shared.module_mirrors` would hand out a stale ref.
    {
        let module_mirrors = shared.module_mirrors.read();
        for obj_ref in module_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    // 8b. VarHandle permanent roots (B-J). VarHandles live in `static final`
    //     fields and are used for lock-free CAS; without rooting them here a
    //     moving GC reclaimed them and left their static holder slots stale.
    {
        let vhs = shared.var_handle_roots.read();
        for obj_ref in vhs.values() {
            roots.push(*obj_ref);
        }
    }

    // 8c. Pre-allocated singleton OutOfMemoryError — thrown on a 100%-full heap
    //     when a fresh exception cannot be materialized. Must survive every GC
    //     permanently (it is held only by `SharedVm`, not any Java field), so a
    //     moving collector cannot reclaim it and leave the OOM-fallback dangling.
    {
        if let Some(oom_ref) = *shared.singleton_oom.read() {
            roots.push(oom_ref);
        }
    }

    // 8d. Cached "main" java.lang.ThreadGroup singleton
    //     (`NativeContextImpl::get_or_create_main_thread_group`,
    //     vm/src/vm/vm_exec.rs). This mirrors the `singleton_oom`/
    //     `system_out`/`system_err`/`system_in` entries just above: the
    //     cache is a bare `SharedVm` field, not itself reachable through any
    //     other root chain at the moment it is first published (a fresh
    //     `Thread$FieldHolder.<init>` that is about to store it into
    //     `holder.group` hasn't run yet), so without scanning it here a
    //     moving GC that fires between the cache's publish and that store
    //     can reclaim/relocate the object out from under the cache. Found
    //     live via a `cratonvm-aio-dispatch-N` SIGSEGV
    //     (`is_forwarded`/`get_header:1558` inside the `FieldHolder.<init>`
    //     field-setter that consumes this very cache's value) that survived
    //     the TOCTOU claim/wait/notify fix for the same function — the
    //     claim/wait/notify fix closes the *concurrent-double-build* race,
    //     but does nothing for a *single*, correctly-built group going
    //     stale on a *later* GC once every builder/waiter has already
    //     returned. try_read (not read): `get_or_create_main_thread_group`
    //     briefly holds the write lock only for the final store (never
    //     across an allocation), so contention here is transient, but
    //     mirror the try_read convention used by the adjacent
    //     system-streams scan rather than assume that can never coincide
    //     with a GC-safepoint poll.
    {
        if let Some(g) = shared.main_thread_group.try_read() {
            if let Some(tg_ref) = *g {
                roots.push(tg_ref);
            }
        }
    }

    // 9. JNI global references — prevent GC from collecting objects held by native code.
    {
        shared.jni_global_refs.lock().collect_roots(&mut roots);
    }

    // 9a. Native upcall table — each live slot holds a `target: ObjectRef` for the
    //     Java callback the legacy `pe_upcall_invoke` dispatch path invokes.
    //     Un-rooted, a moving GC could reclaim/relocate the target out from under
    //     a still-registered upcall (the Panama closure registry remaps its own
    //     copy, but this table's copy was previously neither scanned nor remapped
    //     — see gc.rs section 9a counterpart).
    {
        shared.upcall_table.lock().collect_roots(&mut roots);
    }

    // 9b. JNI LOCAL references (vm-jni-roots #1).
    //
    //     Previously only global refs (section 9) were rooted. The per-thread
    //     `JNI_LOCAL_FRAMES` stack — pushed by PushLocalFrame and every JNI
    //     accessor that hands a fresh local jobject back to native code — held
    //     raw heap pointers that were NEVER scanned. A heap object reachable
    //     only through a JNI local ref could therefore be collected mid-native-
    //     call, or left dangling at a from-space address under the moving GC.
    //     Fold this thread's active local refs into the root set here; the
    //     matching remap after a moving collection is
    //     `crate::native::jni::update_local_refs_after_gc` (gc.rs).
    crate::native::jni::collect_local_ref_roots(&mut roots);

    // 9c. JNI keep-alive pin set (INT-10): arrays checked out via
    //     GetPrimitiveArrayCritical / Get<Type>ArrayElements. Native code
    //     holds a detached COPY of the body (so relocation is safe), but the
    //     copy-back at Release targets the OBJECT — which must therefore
    //     stay alive even when its only other reference was dropped while
    //     the native held the copy. Previously this set was spliced only
    //     into the semispace `Heap` backend; generational/G1/ZGC relied on
    //     the (initiator-only) JNI-local scan above. The matching re-key
    //     after a move is `cratonvm_gc::pinned::update_after_gc` (gc.rs).
    if cratonvm_gc::pinned::any_pinned() {
        for addr in cratonvm_gc::pinned::pinned_addrs() {
            if let Some(obj) = shared.heap.is_object_address(addr) {
                roots.push(obj);
            }
        }
    }

    // 10. Thread-local ObjectRefs — java_thread_obj, pending_async_exception
    if let Some(ref obj_ref) = thread.java_thread_obj {
        roots.push(*obj_ref);
    }
    if let Some(ref obj_ref) = thread.pending_async_exception {
        roots.push(*obj_ref);
    }

    // 10b. Registry-held java.lang.Thread mirrors of every ALIVE thread.
    //      HotSpot semantics: a thread's mirror is a strong root while the
    //      thread lives. Natives serve these raw copies back into bytecode
    //      (`enumerate_threads`, `Thread.getAllStackTraces`) and the
    //      `unpark(Thread)` reverse index is keyed by their addresses — a
    //      mirror reachable ONLY through the registry must not be collected
    //      (a collected one resurfaces as the all-zero-header invokevirtual
    //      receiver). The matching remap is
    //      `ThreadRegistry::update_thread_objs_after_gc` (gc.rs step 21).
    for obj_ref in shared.thread_registry.alive_thread_objects(usize::MAX) {
        roots.push(obj_ref);
    }

    // 11. Root snapshot (for cross-thread GC scanning)
    {
        let snapshot = thread.root_snapshot.lock();
        for obj_ref in snapshot.iter() {
            roots.push(*obj_ref);
        }
    }

    // 12. Scoped value bindings (JEP 446)
    //
    // Round-9 GC fix: also push the ScopedValue KEY ObjectRef when present.
    // The previous version only pushed VALUEs, so the key object itself
    // (which JDK-internal code reaches via Carrier.get(ScopedValue)) could
    // be reclaimed while the binding was still live — a use-after-free on
    // the next reflective `Carrier.get` traversal.
    for (_key_id, key_ref, val) in &thread.scoped_values {
        if let Some(obj_ref) = key_ref {
            roots.push(*obj_ref);
        }
        if let Value::Object(Some(obj_ref)) = val {
            roots.push(*obj_ref);
        }
    }

    // 13. Resolution cache — CONSTANT_Dynamic values may hold ObjectRefs
    {
        let cache = shared.resolution_cache.read();
        cache.scan_condy_roots(&mut roots);
    }

    // 14. NEW-1.5 — conservative scan of every active JIT spill region on the
    //     calling thread. Each qword in a JIT frame's stack region whose value
    //     is a valid object header is reported as a root. The semispace
    //     collector consults `any_thread_in_jit()` to skip compaction while
    //     this is in flight, so a value coincidentally equal to an object
    //     address never causes a wrong relocation.
    //
    //     This is the AUTHORITATIVE root set for the current thread's collection,
    //     so it must NOT be served from the per-thread JIT-scan cache: that cache
    //     can drop a live root that appeared on the conservatively-scanned native
    //     stack since the last boundary bump (see
    //     `invalidate_scan_cache_for_gc`). Discard the cached snapshot first so
    //     this scan is a full, current walk.
    crate::jit::conservative_roots::invalidate_scan_cache_for_gc();
    let jit_scan_start = roots.len();
    // Moving young gen (`CRATONVM_MOVING_YOUNG`): the shadow stack now publishes a
    // COMPLETE rewritable precise root map for every live JIT frame, so the
    // conservative frame scan is normally SUPPRESSED. Running it anyway would
    // fold un-rewritable slot values into `roots` that the moving Cheney copy
    // would relocate but could not patch — and a false-positive non-oop word
    // would pin (or mis-relocate) a random object. "Once a frame is precise, it
    // must be fully precise" (default-moving-young-gen.md, Risks): rely solely
    // on the shadow stack (folded in at 14b below) plus the
    // interpreter/statics/JNI roots. If any active JIT frame cannot prove that
    // coverage, we deliberately re-enable the conservative scan and tell the
    // collector to use the non-moving sweep for this cycle. On any non-moving
    // path this scan remains the authoritative JIT root set.
    let moving_young = crate::jit::conservative_roots::moving_young_enabled();
    let moving_young_osr_fallback =
        moving_young && crate::jit::conservative_roots::moving_young_osr_shadow_fallback_needed();
    if moving_young_osr_fallback {
        cratonvm_gc::gc_quiescence::set_force_non_moving_jit_roots();
        cratonvm_gc::gc_quiescence::mark_moving_young_coverage_incomplete();
    }
    let moving_young_precise_only = moving_young
        && !moving_young_osr_fallback
        && crate::jit::conservative_roots::refresh_moving_young_coverage_for_current_thread()
        && !cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete();
    if !moving_young_precise_only {
        crate::jit::conservative_roots::scan_active_jit_frames(&shared.heap, &mut roots);
    }
    // G1 pin-in-place for conservative JIT roots: the generational collector
    // protects a conservatively-scanned JIT root (a register/spill slot the
    // collector cannot rewrite) by running its NON-MOVING young sweep while any
    // thread is in JIT, so nothing moves. G1 always evacuates, so it must
    // instead PIN the regions holding these roots (exclude them from the
    // collection set) — otherwise it relocates the object and the un-rewritable
    // JIT-frame slot is left dangling (the SteadyChurn `-XX:+UseG1GC` + JIT
    // wrong-result: the `live` list head, held only in a callee-saved register
    // and its canonical frame slot, went stale after the young GC moved it).
    // Publish each conservative JIT-frame root so the G1 collector can pin its
    // region. Gated on G1 (the generational path doesn't read this set) and on
    // there actually being JIT roots this cycle.
    if shared.heap.is_g1() && roots.len() > jit_scan_start {
        for r in &roots[jit_scan_start..] {
            cratonvm_gc::gc_quiescence::add_pinned_jit_root(r.as_ptr() as usize);
        }
    }

    // 14b. Shadow-stack precise roots (CRATONVM_SHADOW_STACK). JIT code pushes
    //      every live oop (locals AND operand-stack entries) onto this thread's
    //      shadow stack immediately before a GC-capable call. Unlike the
    //      conservative scan above, every slot here is — by construction —
    //      exactly one object reference, so it is BOTH a precise mark root and
    //      a *rewritable* root (the matching post-move rewrite is
    //      `thread.shadow_stack.remap` in gc.rs). We still validate each value
    //      via `is_object_address`: a defensive guard against a stale slot that
    //      an abnormal unwind left above `top` is impossible (scan is bounded by
    //      `top`), but a slot holding null / a not-yet-stored value reads as a
    //      non-object and is harmlessly skipped.
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        // spring-bug-10 experiment: `CRATONVM_SHADOW_PIN` publishes the
        // shadow-stack oops as PINNED marking roots instead of MOVABLE ones.
        // Rationale: the shadow stack is a LIFO scanned only between a
        // push-before-call and its reload-after-call, so an oop is pinned only
        // *transiently* (during one GC-capable call) — once popped it promotes
        // normally on a later GC. Pinned avoids the movable path's
        // evacuate→remap→reload (the null-reload SIGSEGV) entirely. The doc's
        // "pinning OOMs bt18" was reasoned for pinning the WHOLE operand stack,
        // never measured for this transient minimal set; this gate lets us
        // measure it directly.
        let pin = crate::jit::conservative_roots::shadow_pin_roots() || moving_young_osr_fallback;
        // spring-bug-10 diagnostic: log this thread's shadow-stack depth at each
        // GC. A monotonically growing depth across collections means the JIT
        // `top` is DRIFTING (a push without a paired reload) — which makes the
        // pop-only reload read above the real data and corrupt a home register.
        if std::env::var_os("CRATONVM_DBG_SHADOW_DEPTH").is_some() {
            let d = thread.shadow_stack.depth();
            if d > 0 {
                eprintln!(
                    "[SHADOW_DEPTH] tid={:?} depth={} pin={}",
                    thread.thread_id, d, pin
                );
            }
        }
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.heap.is_object_address(v) {
                roots.push(obj_ref);
                if !pin {
                    // B-K kafka fix: shadow-stack oops are precise AND
                    // *rewritable* (`thread.shadow_stack.remap` rewrites them
                    // after a move, then the JIT's post-safepoint reload
                    // refreshes the register). So they may be EVACUATED rather
                    // than pinned — publish movable so the non-moving sweep's
                    // selective promotion drains them.
                    cratonvm_gc::gc_quiescence::add_movable_jit_root(obj_ref.as_ptr() as usize);
                }
            }
        });
    }

    // 15. Round-9 CRIT GC-correctness fix: process-global Integer.valueOf
    //     (-128..=127) and Boolean.TRUE/FALSE caches. These live in
    //     `native-builtins/src/lang_math.rs` and previously used
    //     `thread_local!`, which (a) violated the JLS-mandated
    //     cross-thread `==` identity for boxed primitives and (b) was
    //     invisible to the GC root scanner — under a moving collector the
    //     cached ObjectRefs would point at relocated or reclaimed memory
    //     after the first compaction.
    cratonvm_native_builtins::lang_math::gc_scan_value_of_cache_roots(
        shared.vm_identity,
        &mut roots,
    );

    // 15a. Unsafe / Class$Atomic synthetic-offset side stores. These hold live
    //      `ObjectRef`s that exist in NO heap slot (the synthetic-offset scheme
    //      services load/CAS/store from a Rust-side map when the field's real
    //      heap slot is unknown to our layout), so they are unreachable through
    //      the heap graph. Chief offender: `Class$Atomic.casReflectionData`
    //      stows the `SoftReference<ReflectionData>` here — without rooting it a
    //      young GC reclaims the still-live SoftReference (all-zero header) and
    //      the reflection subgraph hanging off it decays (the Tomcat DoHead
    //      start/stop corruption flood, first victim always a
    //      `java/lang/ref/SoftReference`). Remap companion in `gc.rs`
    //      (`gc_update_unsafe_side_store_refs`).
    cratonvm_native_builtins::gc_scan_unsafe_side_store_roots(&mut roots);

    // 16. Round-9 perf + GC fix: process-global LambdaMetafactory CallSite
    //     cache. Cached CallSites and their bootstrap-arg ObjectRef keys
    //     must stay live across collections; the matching post-compaction
    //     remap lives in `gc.rs` (`gc_update_lambda_callsite_cache_refs`).
    cratonvm_native_builtins::lang_invoke::gc_scan_lambda_callsite_cache_roots(&mut roots);

    // 16a. Zero-capture lambda proxy singleton cache (companion to the
    //      LambdaMetafactory CallSite cache in step 16 above, but for the
    //      cached proxy INSTANCE of a non-capturing lambda rather than the
    //      CallSite metadata). Lives in `vm/src/runtime/invokedynamic.rs`.
    crate::runtime::invokedynamic::gc_scan_lambda_singleton_roots(shared.vm_identity, &mut roots);

    // 17. Overlay-backed collections (LinkedList / LinkedHashMap / TreeMap /
    //     TreeSet). These keep backing arrays + nodes in Rust side-tables,
    //     invisible to ordinary field tracing. The moving/G1/ZGC paths retain
    //     the conservative global-root behavior because their marker has no
    //     stable-address overlay propagation. The Generational non-moving
    //     marker can propagate an overlay only after its OWNER has been marked
    //     (gen_heap.rs). An explicit System.gc() also selects that non-moving
    //     full-GC path so it can reclaim an otherwise-dead overlay owner rather
    //     than globally rooting its transient compiler graph.
    let conditional_overlay_marking = shared.config.gc_algorithm
        == crate::config::GcAlgorithm::Generational
        && (cratonvm_gc::gc_quiescence::is_active()
            || cratonvm_gc::gc_quiescence::unregistered_jit_frame_on_stack()
            || cratonvm_gc::gc_quiescence::major_gc_requested());
    if !conditional_overlay_marking {
        cratonvm_native_collections::gc_scan_collection_overlay_roots(&mut roots);
    }

    // 18. Singleton built-in class loaders (app / platform). These synthetic
    //     `ClassLoader` objects live ONLY in process-global mutexes in
    //     `native-builtins/src/classloader.rs`, invisible to every scan above.
    //     Without rooting them, a moving young GC reclaims/relocates the cached
    //     loader and `getClassLoader()` returns a stale `ObjectRef` whose slot
    //     was reused — BouncyCastle `ClassUtil.loadClass`'s receiver then reads
    //     as a String OID ("Not able to load any cryptoProvider", intermittent
    //     / heap-size dependent). Remap companion in `gc.rs`
    //     (`gc_update_loader_singleton_refs`).
    cratonvm_native_builtins::classloader::gc_scan_loader_singleton_roots(&mut roots);
    cratonvm_native_builtins::jmx::gc_scan_platform_mbean_server_root(&mut roots);

    // 18a. Process-global `System.getenv()` / `System.getProperties()`
    //      singletons cached in `native-builtins/src/lang_system.rs`. Like the
    //      class loaders above, these synthetic objects live ONLY in process-
    //      global mutexes, invisible to every scan above; without rooting them a
    //      moving young GC reclaims/relocates the cached Map/Properties and the
    //      next `getenv()`/`getProperties()` returns a stale `ObjectRef`. Remap
    //      companion in `gc.rs` (`gc_update_system_singleton_refs`).
    cratonvm_native_builtins::lang_system::gc_scan_system_singleton_roots(&mut roots);

    // 18b. Process-global Locale caches (cached default Locale + synthetic
    //      Locale side-tables) in native-builtins. Same stale-pointer hazard as
    //      the class loaders: a moving young GC reclaims/relocates the cached
    //      synthetic `java/util/Locale` while `Locale.getDefault()` keeps
    //      handing back the stale ObjectRef → "Stale pointer … java/util/Locale"
    //      → SIGSEGV (TestServerInfo / TestSwallowAbortedUploads). Remap
    //      companion in `gc.rs` (`gc_update_locale_refs`).
    cratonvm_native_builtins::gc_scan_locale_roots(&mut roots);

    // 18c. `java.lang.ClassValue` memoization cache (BUG-W) — cached
    //      `computeValue(Class)` results live only in a process-global
    //      side-table in `native-builtins/src/phases_late.rs`, invisible to
    //      every scan above. Without rooting them a moving GC can
    //      reclaim/relocate a cached value while a later `ClassValue.get()`
    //      keeps handing back the stale `ObjectRef`. Remap companion in
    //      `gc.rs` (`gc_update_classvalue_cache_refs`).
    cratonvm_native_builtins::phases_late::gc_scan_classvalue_cache_roots(&mut roots);

    // 19. JBoss MSC container-held service objects. The `ServiceContainer` Rust
    //     state machine references Java objects (the `Service` instance whose
    //     `start()`/`stop()` we invoke, the synthetic `ServiceController`
    //     mirror, the child `ServiceTarget`, the in-flight `StartContext`) only
    //     through a process-global side-table in
    //     `native-builtins/src/jboss_msc.rs`, invisible to every scan above.
    //     Without rooting them a moving GC reclaims/relocates a held service
    //     and the next `invoke_virtual(service, "start", ...)` is a
    //     use-after-free. Remap companion in `gc.rs`
    //     (`gc_update_msc_service_refs`).
    cratonvm_native_builtins::jboss_msc::gc_scan_msc_service_roots(&mut roots);

    // 19b. Round-4 B4: java.util.logging / JBoss LogManager mirrors — the
    //     LogManager / Logger / LogContext singletons and the attachments
    //     table are cached as raw addresses in process-global side-tables in
    //     `native-builtins/src/logmanager.rs` with no GC visibility. Root them
    //     so a moving GC cannot reclaim/relocate a cached logger out from under
    //     a later native lookup (use-after-free). Remap companion in `gc.rs`
    //     (`gc_update_logmanager_refs`).
    cratonvm_native_builtins::logmanager::gc_scan_logmanager_roots(&mut roots);

    // 20. Class-level annotation-proxy identity cache. The per-class
    //     `getAnnotation(X)` / `getDeclaredAnnotations()` proxies are cached in
    //     a process-global side-table in `native-builtins/src/lang_class.rs`
    //     (so repeated reads return the same instance, matching HotSpot's
    //     `Class.annotationData`), invisible to the field scan above. Root them
    //     so a moving young GC cannot reclaim/relocate a cached proxy out from
    //     under a later `getAnnotation` read (use-after-free). Remap companion
    //     in `gc.rs` (`gc_update_annotation_proxy_refs`).
    cratonvm_native_builtins::lang_class::gc_scan_annotation_proxy_roots(&mut roots);

    //     Synthetic `com.sun.net.httpserver` server registry: each registered
    //     `HttpHandler` ObjectRef lives only in a native map (no Java-heap edge
    //     once the test drops the returned HttpContext), so a moving young GC
    //     would otherwise reclaim/relocate it and the per-request dispatcher
    //     would invoke a stale receiver (NoSuchMethodError java/lang/Object.handle).
    //     Remap companion in `gc.rs` (`gc_update_re10_handler_refs`).
    cratonvm_native_builtins::net_phase_e::gc_scan_re10_handler_roots(&mut roots);

    //     Process-global InetAddress side table (`net_phase_e.rs`): each
    //     synthetic InetAddress mirror's (hostName, ipAddress) pair lives only
    //     in this ObjectRef-keyed map. Without rooting it, a moving young GC
    //     that relocates a live mirror leaves it keyed on a vacated from-space
    //     slot, and getHostAddress()/getAddress()/toString() silently fall
    //     back to reporting "0.0.0.0" (ES
    //     InetAddressRandomBinaryDocValuesRangeQueryTests CONTAINS-query false
    //     negative). Remap companion in `gc.rs` (`gc_update_inet_addr_refs`).
    cratonvm_native_builtins::net_phase_e::gc_scan_inet_addr_roots(&mut roots);

    //     NIO SelectionKey table: channel/selector/attachment/key_obj ObjectRefs
    //     live only in `sk_table`; remap was already wired (gc.rs
    //     `sk_table_update_after_gc`) but the root SCAN was missing, so a key
    //     reachable only through sk_table could be swept before the remap ran.
    cratonvm_native_io::nio_selector::gc_scan_selector_roots(&mut roots);
    cratonvm_native_io::socket_channel::gc_scan_channel_roots(&mut roots);
    cratonvm_native_io::socket_channel::gc_scan_ss_back_ref_roots(&mut roots);
    cratonvm_native_api::server_socket_ports::gc_scan_roots(&mut roots);
    //     ScheduledThreadPoolExecutor pending runnables (stored as relocatable
    //     addresses; remap companion `scheduled_pump::gc_update_scheduled_refs`).
    cratonvm_native_builtins::scheduled_pump::gc_scan_scheduled_roots(&mut roots);
    //     XNIO IoFuture notifier/attachment/result refs held across allocations
    //     until the future settles (remap companion
    //     `xnio_async::gc_update_xnio_future_refs`).
    cratonvm_native_builtins::xnio_async::gc_scan_xnio_future_roots(&mut roots);
    //     FFM/Panama upcall targets: the Java MethodHandle/lambda a libffi
    //     trampoline dispatches to, reachable only through the leaked upcall
    //     userdata (Step 5 GAP C; remap companion in `gc.rs`).
    cratonvm_native_builtins::panama::gc_scan_upcall_target_roots(&mut roots);
    //     TLS SSLContext TrustManager[] objects (t27_tls::ctx_trust_managers_table),
    //     held so the post-handshake trust check (OCSP/CRL revocation checkers,
    //     custom X509TrustManagers) can still call them long after
    //     SSLContext.init returned (remap companion
    //     `t27_tls::gc_update_tls_ctx_trust_manager_refs` in `gc.rs`).
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_trust_manager_roots(&mut roots);
    //     TLS SSLContext KeyManager[] objects (t27_tls::ctx_key_managers_table),
    //     held so `JavaKeyManagerResolver::resolve` can synchronously consult
    //     the real `KeyManager.chooseClientAlias` mid-handshake, long after
    //     SSLContext.init returned (remap companion
    //     `t27_tls::gc_update_tls_ctx_key_manager_refs` in `gc.rs`).
    cratonvm_native_builtins::t27_tls::gc_scan_tls_ctx_key_manager_roots(&mut roots);
    //     The process-wide default SSLContext (t27_tls::default_ssl_context_slot),
    //     installed by SSLContext.setDefault(ctx) and returned by later
    //     SSLContext.getDefault() calls -- held long after setDefault
    //     returned (remap companion `t27_tls::gc_update_default_ssl_context_ref`
    //     in `gc.rs`).
    cratonvm_native_builtins::t27_tls::gc_scan_default_ssl_context_root(&mut roots);

    //     ForkJoinTask done/result side-table. Real-JDK ForkJoin overrides cache
    //     task results in Rust state keyed by task identity; cached Object
    //     results live in no heap slot, so a GC between `submit` and `get` must
    //     root them here. Remap companion in `gc.rs`.
    cratonvm_native_builtins::phases_early::gc_scan_forkjoin_roots(&mut roots);

    // 21. Uniform native-root registry. Any native subsystem holding ObjectRefs
    //     in a process-global side-table can register a scan callback here
    //     instead of hand-wiring a new `gc_scan_*` call into this function (see
    //     `crate::memory::native_roots`). Fans out to every registered source;
    //     a no-op (byte-identical to baseline) until a subsystem registers, so
    //     it is safe to land ahead of any adopter. The matching post-move remap
    //     is `native_roots::remap_all_native_roots` in `gc.rs`.
    crate::memory::native_roots::scan_all_native_roots(&mut roots);

    if let Some(w) = crate::memory::gc::watch_addr() {
        let rooted = roots.iter().any(|o| o.as_ptr() as usize == w);
        eprintln!(
            "[watch] roots GC#{} addr=0x{w:x} rooted={rooted} frames={} pins={}",
            shared.heap.collection_count(),
            thread.frames.len(),
            thread.native_pin_roots.len()
        );
        if !rooted {
            for (i, f) in thread.frames.iter().enumerate().rev().take(8) {
                eprintln!(
                    "[watch]   frame#{i} {}.{} pc={} stack_len={} locals={}",
                    f.class_name(),
                    f.method_name(),
                    f.pc,
                    f.stack.len(),
                    f.locals_len()
                );
            }
        }
    }
    roots
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::config::VmConfig;
    // FIX: `Heap` import removed — the locals/stack root tests now allocate
    // from `shared.heap` (the heap actually scanned) instead of an orphan
    // `Heap::new()`, so the standalone `Heap` type is no longer referenced.
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    fn test_shared_vm() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(VmConfig::default()))
    }

    #[test]
    fn roots_from_frame_locals() {
        let shared = test_shared_vm();
        // FIX: allocate from `shared.heap` (the heap the scanner validates
        // against via `is_object_address`), not an orphan `Heap::new()`.
        // The frame scanner drops any slot whose address is not resident in
        // the heap being scanned — a foreign-heap object can never be a root.
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            5,
            &[Value::Object(Some(obj)), Value::Int(42)],
        );
        thread.frames.push(frame);

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    #[test]
    fn roots_from_frame_stack() {
        let shared = test_shared_vm();
        // FIX: allocate from `shared.heap` so the operand-stack scanner's
        // `is_heap_addr` validation recognizes the objects as live heap
        // residents. Objects from a disconnected `Heap::new()` are correctly
        // rejected by the scanner and would never appear as roots.
        let obj1 = shared.heap.alloc_object(ClassId::new(0), 0);
        let obj2 = shared.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        let mut frame = Frame::new(
            ClassId::new(0),
            "TestClass".to_string(),
            "test".to_string(),
            "()V".to_string(),
            None,
            vec![],
            vec![],
            10,
            2,
            &[],
        );
        frame.stack.push(Value::Object(Some(obj1))).unwrap();
        frame.stack.push(Value::Int(5)).unwrap();
        frame.stack.push(Value::Object(Some(obj2))).unwrap();
        thread.frames.push(frame);

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj1));
        assert!(roots.contains(&obj2));
    }

    #[test]
    fn roots_from_statics() {
        let shared = test_shared_vm();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);

        {
            let mut statics = shared.statics.write();
            statics.insert(
                ClassId::new(1),
                vec![Value::Object(Some(obj)), Value::Int(0)],
            );
        }

        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    #[test]
    fn roots_from_printed() {
        let shared = test_shared_vm();
        let obj = shared.heap.alloc_object(ClassId::new(0), 0);

        let mut thread = JvmThread::new(ThreadId(0), "test");
        thread.printed.push(Value::Object(Some(obj)));
        thread.printed.push(Value::Int(100));

        let roots = collect_roots(&shared, &thread);
        assert!(roots.contains(&obj));
    }

    #[test]
    fn roots_empty_state() {
        let shared = test_shared_vm();
        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(
            roots.iter().all(|root| shared
                .heap
                .is_object_address(root.as_ptr() as usize)
                .is_none()),
            "empty SharedVm/thread state must not contribute roots from this heap: {roots:?}",
        );
    }

    /// NEW-1.5 end-to-end: an object whose only live reference lives on the
    /// native stack (in a fake "JIT spill slot") is discovered by the
    /// conservative scanner and reported as a root, provided a JIT entry
    /// guard is active. Without the guard, the object is *not* discovered
    /// (the chain is empty) — confirming that we don't accidentally scan
    /// the entire native stack on every root collection.
    #[test]
    fn conservative_jit_root_scan_finds_spilled_object() {
        let shared = test_shared_vm();
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let thread = JvmThread::new(ThreadId(0), "test");

        // Place the object's address into a stack-allocated slot. The
        // conservative scanner walks `[scanner_sp .. entry_sp)` for each
        // active JIT entry, treating each qword as a possible heap pointer.
        // We push a JIT entry guard FIRST (so entry_sp is captured *above*
        // this point in the stack), then allocate the spill slot below it,
        // then run the scan from inside this same scope so the scanner's
        // own SP is even further down the stack.
        let _g = crate::jit::conservative_roots::JitEntryGuard::enter();

        // Keep the spill slot live via std::hint::black_box so the
        // optimizer can't elide it and the address remains observable.
        let mut spill_slot: usize = obj.as_ptr() as usize;
        std::hint::black_box(&mut spill_slot);

        let roots = collect_roots(&shared, &thread);
        // The object should appear in the root set via the conservative
        // JIT scan path. We can't assert exact length because the scan
        // may also pick up unrelated stack values that coincidentally
        // look like object headers — but those are filtered by
        // is_object_address so the count is bounded.
        assert!(
            roots.contains(&obj),
            "conservative JIT root scan must report the spilled object as a root \
             (heap addr = {:#x}, roots = {:?})",
            obj.as_ptr() as usize,
            roots
                .iter()
                .map(|r| r.as_ptr() as usize)
                .collect::<Vec<_>>()
        );
        // Hint to the compiler that spill_slot is still live at this point
        // — otherwise it might be reused by an earlier register before the
        // scan runs and we'd get a false negative.
        std::hint::black_box(&spill_slot);
    }

    /// Negative companion to the above: with NO active JIT entry guard,
    /// the conservative scanner sees an empty chain and must not report
    /// the object as a root (it has no other reference path).
    #[test]
    fn conservative_jit_root_scan_skips_inactive_threads() {
        let shared = test_shared_vm();
        let obj = shared
            .heap
            .alloc_object(crate::classloading::ClassId::new(0), 0);
        let thread = JvmThread::new(ThreadId(0), "test");

        let mut spill_slot: usize = obj.as_ptr() as usize;
        std::hint::black_box(&mut spill_slot);

        let roots = collect_roots(&shared, &thread);
        assert!(
            !roots.contains(&obj),
            "with no JIT entry guard, the conservative scanner must not \
             scan the calling thread's native stack"
        );
    }

    /// NEW-1.5 GC quiescence: while a JIT entry guard is held, the
    /// process-wide quiescence flag is set, telling the GC to defer
    /// compaction.
    #[test]
    fn jit_entry_sets_gc_quiescence_flag() {
        let depth_before = cratonvm_gc::gc_quiescence::depth();
        {
            let _g = crate::jit::conservative_roots::JitEntryGuard::enter();
            assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before + 1);
            assert!(cratonvm_gc::gc_quiescence::is_active());
        }
        assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before);
    }
}
