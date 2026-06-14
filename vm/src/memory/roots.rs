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

/// Collect all GC root ObjectRefs from the shared VM state and the current thread.
///
/// Returns a vector of all live non-null ObjectRefs reachable from:
/// - Thread frame locals and operand stacks
/// - Static fields (all classes)
/// - Class lock objects (synthetic monitors for static synchronized methods)
/// - Thread printed values (test harness output)
pub fn collect_roots(shared: &SharedVm, thread: &JvmThread) -> Vec<ObjectRef> {
    let mut roots = Vec::new();

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
    for frame in thread.frames.iter() {
        frame.scan_local_objects(&mut roots, &shared.heap);
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

    // 4c. Native return in flight — object result after `safe_native_call`
    //     returns but before the interpreter pushes it onto the operand stack.
    if let Some(obj_ref) = thread.native_pending_return {
        roots.push(obj_ref);
    }

    // 5. Interned string pool — all interned String objects
    {
        let string_pool = shared.string_pool.read();
        for obj_ref in string_pool.values() {
            roots.push(*obj_ref);
        }
    }

    // 6. Class mirror cache — java.lang.Class objects
    {
        let class_mirrors = shared.class_mirrors.read();
        for obj_ref in class_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    // 7. System streams (System.out, System.err)
    {
        if let Some(out_ref) = *shared.system_out.read() {
            roots.push(out_ref);
        }
        if let Some(err_ref) = *shared.system_err.read() {
            roots.push(err_ref);
        }
    }

    // 8. Primitive type Class mirrors (int.class, boolean.class, etc.)
    {
        let prim_mirrors = shared.primitive_mirrors.read();
        for obj_ref in prim_mirrors.values() {
            roots.push(*obj_ref);
        }
    }

    // 9. JNI global references — prevent GC from collecting objects held by native code.
    {
        shared.jni_global_refs.lock().collect_roots(&mut roots);
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
    crate::jit::conservative_roots::scan_active_jit_frames(&shared.heap, &mut roots);

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
        thread.shadow_stack.for_each_value(|v| {
            if let Some(obj_ref) = shared.heap.is_object_address(v) {
                roots.push(obj_ref);
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
    cratonvm_native_builtins::lang_math::gc_scan_value_of_cache_roots(&mut roots);

    // 16. Round-9 perf + GC fix: process-global LambdaMetafactory CallSite
    //     cache. Cached CallSites and their bootstrap-arg ObjectRef keys
    //     must stay live across collections; the matching post-compaction
    //     remap lives in `gc.rs` (`gc_update_lambda_callsite_cache_refs`).
    cratonvm_native_builtins::lang_invoke::gc_scan_lambda_callsite_cache_roots(&mut roots);

    // 17. Overlay-backed collections (LinkedList / LinkedHashMap / TreeMap /
    //     TreeSet). These keep their backing arrays + nodes in process-global
    //     Rust side-tables, invisible to the field-tracing scan above. Without
    //     this, a moving young-gen GC reclaims/relocates a backing array
    //     reachable only through an overlay, leaving a dangling pointer that
    //     later reads as a zero-header (class_id=0) object — the
    //     `LinkedHashMap.get` crash on DaCapo's Config map. The matching
    //     post-compaction remap lives in `gc.rs`
    //     (`gc_update_collection_overlay_refs`).
    cratonvm_native_collections::gc_scan_collection_overlay_roots(&mut roots);

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

    // 18b. Process-global Locale caches (cached default Locale + synthetic
    //      Locale side-tables) in native-builtins. Same stale-pointer hazard as
    //      the class loaders: a moving young GC reclaims/relocates the cached
    //      synthetic `java/util/Locale` while `Locale.getDefault()` keeps
    //      handing back the stale ObjectRef → "Stale pointer … java/util/Locale"
    //      → SIGSEGV (TestServerInfo / TestSwallowAbortedUploads). Remap
    //      companion in `gc.rs` (`gc_update_locale_refs`).
    cratonvm_native_builtins::gc_scan_locale_roots(&mut roots);

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
        assert_eq!(roots.len(), 1); // only the one ObjectRef
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
        assert_eq!(roots.len(), 2);
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
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn roots_empty_state() {
        let shared = test_shared_vm();
        let thread = JvmThread::new(ThreadId(0), "test");
        let roots = collect_roots(&shared, &thread);
        assert!(roots.is_empty());
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
        let obj = shared.heap.alloc_object(
            crate::classloading::ClassId::new(0),
            0,
        );
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
            roots.iter().map(|r| r.as_ptr() as usize).collect::<Vec<_>>()
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
        let obj = shared.heap.alloc_object(
            crate::classloading::ClassId::new(0),
            0,
        );
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
            assert_eq!(
                cratonvm_gc::gc_quiescence::depth(),
                depth_before + 1
            );
            assert!(cratonvm_gc::gc_quiescence::is_active());
        }
        assert_eq!(cratonvm_gc::gc_quiescence::depth(), depth_before);
    }
}
