#[allow(unused_imports)]
use super::*;

impl<'a> NativeHeapAccess for NativeContextImpl<'a> {
    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        // The same loader fidelity as `new_object_initialized` and
        // `ensure_class_initialized` (see `resolve_class_loader_faithful`):
        // by NAME alone the allocation collapsed to the Application copy of a
        // class that a user loader (Quarkus's runtime loader) also defines, and
        // the ANTLR intrinsics then put app-loader `SingletonPredictionContext`
        // objects into the runtime loader's prediction-context cache
        // (`ClassCastException ... loader 'app' ... QuarkusClassLoader`; Quarkus
        // suite wave 3, hibernate-orm).
        let class_id = self.resolve_class_loader_faithful(class_name)?;
        let (num_fields, access_flags) = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| (c.num_total_fields, c.access_flags.bits()))
            .unwrap_or((0, 0));
        // JVMS 6.5 census (nio21 close): a native `new_object` of an interface
        // or an abstract class (JNI `AllocObject` would throw
        // `InstantiationException`). The requester is this line: the trait
        // method is reached through `dyn`, which drops `#[track_caller]`.
        let _ = cratonvm_native_api::instantiable::observe_uninstantiable_receiver(
            class_name,
            access_flags,
        );
        let obj_ref = self
            .shared
            .mem
            .heap
            .try_alloc_object_full(class_id, num_fields)
            .ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: format!("Java heap space (new_object {class_name})"),
                }))
            })?;
        // Defaults only, no finalizer registration: this allocates WITHOUT
        // running a constructor, like JNI `AllocObject`, which HotSpot does not
        // register either. The `_initialized` twins below run `<init>` and do
        // register (gc-common w2-d; see `init_new_instance`).
        crate::runtime::interpreter::init_primitive_fields(self.shared, obj_ref, class_id);
        self.note_native_object_alloc(obj_ref);
        Ok(Some(Value::Object(Some(obj_ref))))
    }

    fn new_object_initialized(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        let pin_base = self.thread.native_pin_roots.len();
        let arg_pins = pin_native_object_values(self.thread, init_args);
        let result = (|| {
            // Loader fidelity (see `resolve_class_loader_faithful`): the JCA
            // provider chain instantiates SPI classes by NAME here, and a
            // provider registered from a custom-ClassLoader jar (BouncyCastle
            // under Quarkus's fast-jar RunnerClassLoader) names classes the
            // process `-cp` cannot see.
            let class_id = self.resolve_class_loader_faithful(class_name)?;
            // JVMS `new` order: initialize, then allocate (R5, lane jdkonly5).
            native_new_initializes_class_first(self.shared, self.thread, class_id)?;
            let num_fields = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            let obj_ref = self
                .shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (new_object_initialized {class_name})"
                            ),
                        },
                    ))
                })?;
            // Defaults + JLS §12.6 finalizer registration: `<init>` runs below
            // (gc-common w2-d; see `init_new_instance`).
            crate::runtime::interpreter::init_new_instance(self.shared, obj_ref, class_id);
            self.note_native_object_alloc(obj_ref);

            // Keep the new object and constructor object arguments rooted until
            // the Java `<init>` frame owns them in scanned locals.
            let obj_pin = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(obj_ref);
            super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;

            let obj_ref = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            let mut full = Vec::with_capacity(init_args.len() + 1);
            full.push(Value::Object(Some(obj_ref)));
            full.extend(reread_native_object_values(
                self.thread,
                init_args,
                &arg_pins,
            ));
            // JIT round 21 wave 10, lane finalize: `obj_ref` is current here.
            let epoch_before_init = self.shared.mem.address_epoch();
            let init_result = invoke_on_class_shared(
                self.shared,
                self.thread,
                class_id,
                "<init>",
                init_desc,
                &full,
            );

            let forwarded = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            // JIT round 21 wave 10, lane finalize
            // (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`): the registration
            // made at the allocation above goes when the constructor threw
            // before `Object.<init>` returned (the unwinder's proof).
            if init_result.is_err() {
                crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                    self.shared,
                    obj_ref,
                    epoch_before_init,
                );
            }
            init_result?;
            Ok(Some(Value::Object(Some(forwarded))))
        })();
        self.thread.native_pin_roots.truncate(pin_base);
        result
    }

    fn new_object_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        // Interpreter round i3 wave 83, lane L3 (`CRATONVM_REFLECT_CTOR_LEAN_ALLOC`,
        // default off): an initialized class's construction takes the lean
        // door (`new_object_initialized_lean`). Off: one per-VM bool load.
        if super::is_class_initialized_via_manager(self.shared, class_id)
        {
            return self.new_object_initialized_lean(class_id, init_desc, init_args);
        }
        let pin_base = self.thread.native_pin_roots.len();
        let arg_pins = pin_native_object_values(self.thread, init_args);
        let result = (|| {
            // Per-loader-identity construction (JVMS §5.3): allocate and `<init>`
            // the EXACT `class_id` the caller resolved from a Class mirror, never
            // re-resolving the name (which collapses to the first/global definer).
            // JVMS `new` order: initialize, then allocate (R5, lane jdkonly5).
            native_new_initializes_class_first(self.shared, self.thread, class_id)?;
            let num_fields = self
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.num_total_fields)
                .unwrap_or(0);
            // Fallible, exactly like the name-based twin above. This was the
            // infallible `VmHeap::alloc_object`, whose G1 implementation
            // `abort()`s the process on exhaustion where HotSpot throws a
            // catchable OutOfMemoryError -- and reflective
            // `Constructor.newInstance` comes through here (gc-common w2-d,
            // `common-f-g1-fatal-abort-from-infallible-alloc-object`). The
            // closure returns `MethodCallResult`, so `?` propagates and the
            // `native_pin_roots.truncate(pin_base)` below still runs.
            let obj_ref = self
                .shared
                .mem
                .heap
                .try_alloc_object_full(class_id, num_fields)
                .ok_or_else(|| {
                    MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::OutOfMemoryError {
                            message: format!(
                                "Java heap space (new_object_initialized_with_class_id {} fields)",
                                num_fields
                            ),
                        },
                    ))
                })?;
            // Defaults + JLS §12.6 finalizer registration: `<init>` runs below
            // (gc-common w2-d; see `init_new_instance`).
            crate::runtime::interpreter::init_new_instance(self.shared, obj_ref, class_id);
            // Charged like the name-based twin; it never was.
            self.note_native_object_alloc(obj_ref);
            // Wave 83, lane L3: the door census's `call_door` cut.
            reflect_door_gate_stamp(
                self.shared,
                cratonvm_native_builtins::ReflectDoorGatePoint::DoorAllocated,
            );

            // Pin across `<init>` exactly like the name-based path above.
            let obj_pin = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(obj_ref);

            super::ensure_class_initialized_shared(self.shared, self.thread, class_id)?;
            let obj_ref = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            let mut full = Vec::with_capacity(init_args.len() + 1);
            full.push(Value::Object(Some(obj_ref)));
            full.extend(reread_native_object_values(
                self.thread,
                init_args,
                &arg_pins,
            ));

            // Ensure the class is initialized (<clinit>) then dispatch `<init>` on
            // this precise class id — `invoke_on_class_shared` keys on the id, so
            // the weaved/loader-private constructor runs, not the global one.
            //
            // SBR wave 6 (lane dispatch): the same by-name-resolution memo the
            // virtual callbacks use (`native_callee_memo`). A reflective
            // construction of one class resolved this constructor through the
            // whole `invoke_on_class_shared_inner` prelude on every call; the
            // witness fills the memo only when the dispatch ended in that
            // function's ordinary bytecode tail, so every special arm (a native
            // `<init>`, a proxy, a stub) still answers as before.
            let init_name: &str = "<init>";
            // JIT round 21 wave 10, lane finalize: `obj_ref` is current here.
            let epoch_before_init = self.shared.mem.address_epoch();
            let init_result = match crate::runtime::native_callee_memo::lookup(
                self.shared,
                class_id,
                init_name,
                init_desc,
            ) {
                Some(declaring) => crate::runtime::interpreter::execute(
                    self.shared,
                    self.thread,
                    declaring,
                    init_name,
                    init_desc,
                    &full,
                ),
                None => {
                    let memo_save = crate::runtime::native_callee_memo::arm(
                        class_id,
                        init_name,
                        init_desc,
                        self.thread.frames.len(),
                    );
                    let result = invoke_on_class_shared(
                        self.shared,
                        self.thread,
                        class_id,
                        init_name,
                        init_desc,
                        &full,
                    );
                    if let Some(declaring) = crate::runtime::native_callee_memo::disarm(memo_save) {
                        crate::runtime::native_callee_memo::fill(
                            self.shared,
                            class_id,
                            init_name,
                            init_desc,
                            declaring,
                        );
                    }
                    result
                }
            };

            let forwarded = self
                .thread
                .native_pin_roots
                .get(obj_pin)
                .copied()
                .unwrap_or(obj_ref);
            // JIT round 21 wave 10, lane finalize
            // (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`): the registration
            // made at the allocation above goes when the constructor threw
            // before `Object.<init>` returned (the unwinder's proof).
            if init_result.is_err() {
                crate::runtime::interpreter::withdraw_unconstructed_finalizable_at_door(
                    self.shared,
                    obj_ref,
                    epoch_before_init,
                );
            }
            init_result?;
            Ok(Some(Value::Object(Some(forwarded))))
        })();
        self.thread.native_pin_roots.truncate(pin_base);
        result
    }

    /// Round 15 wave 4, lane jdkonly4 (JO3-2): `new_object_initialized` split
    /// at its class resolution. A failed `resolve_class_loader_faithful` is
    /// "no class" (`Ok(None)`); after it, a Java throwable, an
    /// `OutOfMemoryError` or a `StackOverflowError` is the construction's own
    /// and propagates, as HotSpot's `new` in a JDK throw path does. Any other
    /// failure (a synthetic class without that constructor) is still "cannot
    /// construct". `CRATONVM_THROWABLE_CTOR_ERRORS_PROPAGATE=0` restores the
    /// old answer, every failure `Ok(None)`; read live (a throw path).
    fn construct_throwable(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        // Rooted across the resolution, which can run a user loader's
        // `loadClass` (and collect), as `new_object_initialized` roots them.
        let pin_base = self.thread.native_pin_roots.len();
        let arg_pins = pin_native_object_values(self.thread, init_args);
        let resolved = self.resolve_class_loader_faithful(class_name);
        let args_now = reread_native_object_values(self.thread, init_args, &arg_pins);
        self.thread.native_pin_roots.truncate(pin_base);
        let Ok(class_id) = resolved else {
            return Ok(None);
        };
        match self.new_object_initialized_with_class_id(class_id, init_desc, &args_now) {
            Ok(Some(Value::Object(Some(exc)))) => Ok(Some(exc)),
            Ok(_) => Ok(None),
            Err(thrown @ MethodCallFailed::ExceptionThrown(_)) => Err(thrown),
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                error @ (RuntimeError::OutOfMemoryError { .. } | RuntimeError::StackOverflowError),
            ))) => Err(MethodCallFailed::InternalError(VmError::Runtime(error))),
            Err(_) => Ok(None),
        }
    }

    fn pin_native_root(&mut self, obj: ObjectRef) -> usize {
        // DIAGNOSTIC-ONLY (cceres3, CRATONVM_DBG_BLOCKGC): pin-time canary —
        // pinning an ALREADY-forwarded address preserves the staleness (the
        // GC only remaps pins through per-cycle pointer maps, which never
        // contain long-dead addresses). A hit here means the CALLER received
        // a stale value from upstream; the backtrace names it.
        if blockgc_dbg() {
            // Two sources, deliberately. `debug_forwarded_target` reads the
            // forwarding word at the old address, which G1 ZEROES when it frees
            // the from-region — the same blind spot that makes
            // `load_and_forward` a no-op on this collector. A zero from that
            // alone is not evidence of a clean pin. `was_vacated` is the exact
            // ledger (`note_allocated` removes re-issued addresses), so it sees
            // what the forwarding word cannot; it costs nothing unless
            // `CRATONVM_DBG=vacated-frames` armed it.
            let forwarded = self
                .shared
                .mem
                .heap
                .debug_forwarded_target(obj.as_ptr() as usize)
                .or_else(|| cratonvm_gc::gc_quiescence::was_vacated(obj.as_ptr() as usize));
            if let Some(new) = forwarded {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    // Identify WHAT went stale: read the class off the
                    // forwarded (live) copy.
                    let fwd_ref = unsafe { cratonvm_types::ObjectRef::from_raw(new as *mut u8) };
                    let fwd_cid = self.shared.mem.heap.class_id_of(fwd_ref);
                    let fwd_class = self
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(fwd_cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("cid={}", fwd_cid.as_u32()));
                    eprintln!(
                        "[blockgc] PIN-STALE tid={} 0x{:x}->0x{new:x} class={fwd_class} caller:\n{}",
                        self.thread.thread_id.0,
                        obj.as_ptr() as usize,
                        std::backtrace::Backtrace::force_capture(),
                    );
                }
            }
        }
        // `CRATONVM_DBG_DEADREF_STORE`: the same pin-time canary for the case
        // the two sources above are blind to.
        //
        // `debug_forwarded_target` and `was_vacated` both answer "this object
        // MOVED and here is where to". Neither can see a reference to memory
        // that holds no object at all — an address the evacuator refused, or
        // one whose semispace was emptied — because nothing was ever forwarded
        // from it. That is the shape `alloc_unmod_wrapper` and `build_module`
        // both hit on BindableTests: the caller handed down an `ObjectRef` it
        // had held across an allocation, the collection declined to relocate
        // it, and the pin faithfully preserved a dead address.
        //
        // Pinning is the right place to ask, because a pin is a promise that
        // the value is live: if it is not live HERE, no later refresh can
        // recover it, and the caller named in the backtrace is the defect.
        if cratonvm_types::flags().gc.dbg_deadref_store {
            if let Some(reason) = self
                .shared
                .mem
                .heap
                .dead_young_ref_reason(obj.as_ptr() as usize)
            {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 12 {
                    eprintln!(
                        "[deadref-pin] #{n} {reason} pin_native_root(0x{:x}) on tid={} — the \
                         value names no live object, so this pin preserves a dead address \
                         rather than protecting a live one. caller:
{:?}",
                        obj.as_ptr() as usize,
                        self.thread.thread_id.0,
                        std::backtrace::Backtrace::force_capture(),
                    );
                }
            }
        }
        // CRATONVM_DBG_BLOCKED_ACCESS: a pin pushed while this thread's
        // `in_blocked_region` flag is raised is invisible to BOTH the STW root
        // scan (which reads the deposit-time snapshot) and the blocked-thread
        // fold (seeded from that same snapshot) — the object dies or moves and
        // the pin is never remapped, defeating the self-healing pinned
        // re-read pattern. No-op when the gate is off.
        if cratonvm_gc::blocked_access_debug::enabled()
            && self
                .thread
                .gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire)
        {
            cratonvm_gc::blocked_access_debug::report_blocked_violation(
                "pin_native_root push on a census-excluded thread",
                obj.as_ptr() as usize,
            );
        }
        let idx = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(obj);
        idx
    }

    fn read_native_pin(&self, handle: usize, fallback: ObjectRef) -> ObjectRef {
        // DIAGNOSTIC-ONLY (cceres3): a handle past the pin stack means some
        // callee truncated below this caller's pins (pin-stack imbalance) —
        // the silent `unwrap_or(fallback)` then hands back the RAW address,
        // which is stale if a GC ran since the pin. Name the reader loudly
        // under the flag; the culprit truncator is inside its call subtree.
        if handle != usize::MAX && handle >= self.thread.native_pin_roots.len() {
            if blockgc_dbg() {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    let ring = if unpin_ring_enabled() {
                        UNPIN_RING.with(|r| {
                            r.borrow()
                                .iter()
                                .map(|(b, l, bt)| {
                                    format!("\n  truncate base={b} prev_len={l} at:\n{bt}")
                                })
                                .collect::<String>()
                        })
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "[blockgc] PIN-DANGLING tid={} handle={} len={} reader:\n{}\nrecent truncations:{}",
                        self.thread.thread_id.0,
                        handle,
                        self.thread.native_pin_roots.len(),
                        std::backtrace::Backtrace::force_capture(),
                        ring,
                    );
                }
            }
        }
        let entry = self
            .thread
            .native_pin_roots
            .get(handle)
            .copied()
            .unwrap_or(fallback);
        // DIAGNOSTIC-ONLY (cce0079 tree-key tail): the PIN-STALE canary
        // covers pin TIME; this covers READ time — a forwarded address
        // sitting in the pin table means a GC between pin and read failed
        // to remap THIS entry (initiator/arrive/fold writeback gap), which
        // no other canary distinguishes from caller-side misuse.
        if handle != usize::MAX && blockgc_dbg() {
            // Both sources, for the reason spelled out at the PIN-STALE canary:
            // the forwarding word is zeroed by G1 when it frees the from-region,
            // so `debug_forwarded_target` alone cannot see the case this canary
            // exists for. `was_vacated` is the exact ledger.
            let forwarded = self
                .shared
                .mem
                .heap
                .debug_forwarded_target(entry.as_ptr() as usize)
                .or_else(|| cratonvm_gc::gc_quiescence::was_vacated(entry.as_ptr() as usize));
            if let Some(new) = forwarded {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    eprintln!(
                        "[blockgc] PIN-TABLE-STALE tid={} handle={} len={} 0x{:x}->0x{new:x} \
                         (pin-table entry missed a remap) reader:\n{}",
                        self.thread.thread_id.0,
                        handle,
                        self.thread.native_pin_roots.len(),
                        entry.as_ptr() as usize,
                        std::backtrace::Backtrace::force_capture(),
                    );
                }
            }
        }
        entry
    }

    fn unpin_native_roots(&mut self, base: usize) {
        if base < self.thread.native_pin_roots.len() {
            if unpin_ring_enabled() {
                let bt = format!("{}", std::backtrace::Backtrace::force_capture());
                let short: String = bt
                    .lines()
                    .skip(8)
                    .take(12)
                    .map(|l| format!("{l}\n"))
                    .collect();
                UNPIN_RING.with(|r| {
                    let mut r = r.borrow_mut();
                    if r.len() >= 6 {
                        r.remove(0);
                    }
                    r.push((base, self.thread.native_pin_roots.len(), short));
                });
            }
            self.thread.native_pin_roots.truncate(base);
        }
    }

    // ---- handle scope support (arch/handles) ----
    //
    // Real backing for `NativeContext::handle_scope_push`/`handle_scope_pop`/
    // `handle_root`/`handle_get` (default no-ops in `native-api`). Storage is
    // `JvmThread::handle_slots` (a flat `Vec<Option<ObjectRef>>`) plus
    // `JvmThread::handle_scope_bases` (a stack of `handle_slots` lengths),
    // append/truncate exactly like `native_pin_roots`/`pin_native_root` just
    // above — the difference is WHERE the base index lives: here it's
    // recorded on the thread by `handle_scope_push` itself, so nested native
    // calls each get a plain push/pop pair instead of every caller having to
    // thread a base index through (as `pin_native_root`'s callers must via
    // `unpin_native_roots(base)`). See
    // `docs/feature-designs/native-handle-discipline.md` for the full design,
    // and the root-scan/remap-gap note at `vm/src/memory/roots.rs`'s
    // `handle_slots` splice.

    fn handle_scope_push(&mut self) {
        self.thread
            .handle_scope_bases
            .push(self.thread.handle_slots.len());
    }

    fn handle_scope_pop(&mut self) {
        if let Some(base) = self.thread.handle_scope_bases.pop() {
            // `<=` (not `==`): a scope popped out of order relative to a
            // sibling that already truncated past it is simply a no-op here,
            // matching `unpin_native_roots`' own `base < len` guard rather
            // than panicking on an already-shorter vec.
            if base <= self.thread.handle_slots.len() {
                self.thread.handle_slots.truncate(base);
            }
        }
    }

    fn handle_root(&mut self, r: ObjectRef) -> u32 {
        // Same census-visibility guard `pin_native_root` applies: a slot
        // pushed while this thread's blocked-region flag is raised is
        // invisible to both the STW root scan and the blocked-thread fold.
        if cratonvm_gc::blocked_access_debug::enabled()
            && self
                .thread
                .gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire)
        {
            cratonvm_gc::blocked_access_debug::report_blocked_violation(
                "handle_root push on a census-excluded thread",
                r.as_ptr() as usize,
            );
        }
        debug_assert!(
            self.thread.handle_slots.len() < u32::MAX as usize,
            "handle_slots overflowed u32 slot ids"
        );
        let slot = self.thread.handle_slots.len() as u32;
        self.thread.handle_slots.push(Some(r));
        slot
    }

    fn handle_get(&self, slot: u32) -> Option<ObjectRef> {
        self.thread
            .handle_slots
            .get(slot as usize)
            .copied()
            .flatten()
    }

    fn add_global_root(&mut self, obj: ObjectRef) -> usize {
        // Backed by the JNI global-ref table: a persistent, cross-thread,
        // GC-remapped root. Used by the async-socket completion path to hold a
        // CompletionHandler / attachment / ByteBuffer parked on a worker thread
        // and delivered later on the AIO dispatcher thread.
        self.shared.natives.jni_global_refs.lock().add(obj) as usize
    }

    fn resolve_global_root(&self, handle: usize) -> Option<ObjectRef> {
        self.shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(handle as crate::native::jni::JObject)
    }

    fn remove_global_root(&mut self, handle: usize) -> bool {
        self.shared
            .natives
            .jni_global_refs
            .lock()
            .remove(handle as crate::native::jni::JObject)
    }

    fn add_weak_global_root(&mut self, obj: ObjectRef) -> usize {
        // gc-common w24-b: a JNI weak global (`JniGlobalRefs::add_weak`). Not
        // a root; every stop-the-world pause remaps or clears it
        // (`jni::sweep_weak_global_refs_with`), a concurrent remark clears it
        // (`jni::clear_weak_global_refs_unmarked`), and `resolve_global_root`
        // above answers `None` once it is cleared. `remove_global_root`
        // releases it (`JniGlobalRefs::remove` checks the weak set too).
        self.shared.natives.jni_global_refs.lock().add_weak(obj) as usize
    }

    fn hashmap_string_node_cache_get_object(
        &mut self,
        map: ObjectRef,
        key: ObjectRef,
    ) -> Option<Value> {
        // Every memo read and insert goes through the pause-generation gate
        // first: the caches are emptied at the first probe after a pause, so
        // no entry is served across one and a thread that stops probing stops
        // retaining. See `memory::roots::validate_jit_memo_caches`.
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        let mut index = 0;
        while index < self.thread.jit_hashmap_string_node_cache.len() {
            // Copy out only the `Copy` fields. This used to `.clone()` the whole
            // entry, which also deep-copied its `String` key — a heap
            // allocation for EVERY entry scanned on EVERY probe, on the hottest
            // map path in the VM, and this variant does not even look at that
            // key (it compares the Java String objects directly below).
            let (entry_map, entry_node, entry_key_object, mod_count_slot, entry_mod_count) = {
                let entry = &self.thread.jit_hashmap_string_node_cache[index];
                if entry.chm_generation.is_some() {
                    index += 1;
                    continue;
                }
                (
                    entry.map,
                    entry.node,
                    entry.key_object,
                    entry.mod_count_slot,
                    entry.mod_count,
                )
            };
            if entry_map != map || entry_key_object != Some(key) {
                index += 1;
                continue;
            }
            let Value::Int(mod_count) = self.shared.mem.heap.get_field(map, mod_count_slot) else {
                self.thread.jit_hashmap_string_node_cache.swap_remove(index);
                continue;
            };
            if mod_count != entry_mod_count {
                self.thread.jit_hashmap_string_node_cache.swap_remove(index);
                continue;
            }
            return Some(self.shared.mem.heap.get_field(entry_node, 2));
        }
        None
    }

    fn hashmap_string_node_cache_get(&mut self, map: ObjectRef, key: &str) -> Option<Value> {
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        let mut index = 0;
        while index < self.thread.jit_hashmap_string_node_cache.len() {
            // Compare the cached key by BORROW and copy out only the `Copy`
            // fields — see the sibling probe above; the old `.clone()` paid a
            // `String` allocation per entry scanned just to run this compare.
            let (entry_node, mod_count_slot, entry_mod_count) = {
                let entry = &self.thread.jit_hashmap_string_node_cache[index];
                if entry.chm_generation.is_some() || entry.map != map || entry.key != key {
                    index += 1;
                    continue;
                }
                (entry.node, entry.mod_count_slot, entry.mod_count)
            };
            let Value::Int(mod_count) = self.shared.mem.heap.get_field(map, mod_count_slot) else {
                self.thread.jit_hashmap_string_node_cache.swap_remove(index);
                continue;
            };
            if mod_count != entry_mod_count {
                self.thread.jit_hashmap_string_node_cache.swap_remove(index);
                continue;
            }
            return Some(self.shared.mem.heap.get_field(entry_node, 2));
        }
        None
    }

    fn hashmap_string_node_cache_put(
        &mut self,
        map: ObjectRef,
        key_object: ObjectRef,
        key: &str,
        node: ObjectRef,
    ) {
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        let class_id = self.shared.mem.heap.class_id_of(map);
        let fields = self.shared.classes.class_manager.read();
        let Some(mod_count_slot) =
            resolve_field_index_in_hierarchy(class_id, "modCount", &fields.class_store)
        else {
            return;
        };
        drop(fields);
        let Value::Int(mod_count) = self.shared.mem.heap.get_field(map, mod_count_slot) else {
            return;
        };
        if let Some(entry) = self
            .thread
            .jit_hashmap_string_node_cache
            .iter_mut()
            .find(|entry| {
                entry.chm_generation.is_none()
                    && entry.map == map
                    && entry.key_object == Some(key_object)
            })
        {
            entry.node = node;
            entry.key_object = Some(key_object);
            entry.mod_count_slot = mod_count_slot;
            entry.mod_count = mod_count;
            entry.chm_generation = None;
            entry.chm_segment_id = None;
            return;
        }
        if self.thread.jit_hashmap_string_node_cache.len() >= 32 {
            self.thread.jit_hashmap_string_node_cache.remove(0);
        }
        self.thread.jit_hashmap_string_node_cache.push(
            crate::threading::jvm_thread::JitHashMapStringNodeCacheEntry {
                map,
                node,
                key_object: Some(key_object),
                key: key.to_owned(),
                mod_count_slot,
                mod_count,
                chm_generation: None,
                chm_segment_id: None,
            },
        );
    }

    fn chm_string_node_cache_get_object(
        &mut self,
        map: ObjectRef,
        key: ObjectRef,
    ) -> Option<(i32, u64, Value)> {
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        let mut index = 0;
        while index < self.thread.jit_hashmap_string_node_cache.len() {
            let (entry_map, entry_node, entry_key_object, entry_generation, entry_segment_id) = {
                let entry = &self.thread.jit_hashmap_string_node_cache[index];
                (
                    entry.map,
                    entry.node,
                    entry.key_object,
                    entry.chm_generation,
                    entry.chm_segment_id,
                )
            };
            let (Some(generation), Some(segment_id)) = (entry_generation, entry_segment_id) else {
                index += 1;
                continue;
            };
            if entry_map != map || entry_key_object != Some(key) {
                index += 1;
                continue;
            }
            // NOTE: do NOT "harden" this with a `node.key == key` identity
            // test. The entry was published after a CONTENT comparison
            // (`java_strings_equal`), and a CHM's stored key is almost never
            // the same object as the caller's lookup key — Tomcat's
            // `CharsetCache` puts the name it built at construction and looks
            // up a freshly lower-cased one. An identity test therefore misses
            // on every hit and silently disables the memo (measured: the
            // `LazyCsCache` arm goes 156s -> 201s). What makes the entry safe
            // is the segment generation checked by the caller, plus the fact
            // that no memoized node ever holds a `computeIfAbsent` reservation
            // marker — the only CHM writes that skip `ChmMonitorGuard`, and so
            // the only ones that do not advance that generation, are the
            // marker install/replace phases, and `native_chm_get_string_chain`
            // refuses to memoize a marker node.
            return Some((
                segment_id,
                generation,
                self.shared.mem.heap.get_field(entry_node, 2),
            ));
        }
        None
    }

    fn chm_string_node_cache_put(
        &mut self,
        map: ObjectRef,
        segment_id: i32,
        key_object: ObjectRef,
        key: &str,
        node: ObjectRef,
        generation: u64,
    ) {
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        if let Some(entry) = self
            .thread
            .jit_hashmap_string_node_cache
            .iter_mut()
            .find(|entry| {
                entry.map == map
                    && entry.key_object == Some(key_object)
                    && entry.chm_generation.is_some()
            })
        {
            entry.node = node;
            entry.key_object = Some(key_object);
            entry.chm_generation = Some(generation);
            entry.chm_segment_id = Some(segment_id);
            return;
        }
        if self.thread.jit_hashmap_string_node_cache.len() >= 32 {
            self.thread.jit_hashmap_string_node_cache.remove(0);
        }
        self.thread.jit_hashmap_string_node_cache.push(
            crate::threading::jvm_thread::JitHashMapStringNodeCacheEntry {
                map,
                node,
                key_object: Some(key_object),
                key: key.to_owned(),
                mod_count_slot: 0,
                mod_count: 0,
                chm_generation: Some(generation),
                chm_segment_id: Some(segment_id),
            },
        );
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        // DBG (`CRATONVM_DBG_DEADRECV`): is this receiver an address the
        // collector already reclaimed?
        //
        // ASKED BEFORE THE FIRST DEREFERENCE, and that is the whole point.
        // Every other consumer of the reclamation rings asks AFTER something
        // has already read the object -- a failed `checkcast` reads the class
        // id, the sweep-zero consumer reads an all-zero header -- so none of
        // them can answer when the read ITSELF faults, which is what happens
        // once `CRATONVM_GEN_UNCOMMIT` has unmapped the span. Both lookups
        // below are pure ring probes keyed on the ADDRESS and touch no heap
        // memory, so they answer whether or not the page is still mapped.
        //
        // This site because it is where the Generational `--nojit` failure
        // lands: 4 of 5 crashes on the 2026-09-06 Kafka reproducer are
        // `native_object_hash_code` -> here, faulting on the mark word.
        //
        // Returns 0 rather than walking into the fault, so one run names MANY
        // victims instead of dying at the first. That makes the flag
        // behaviour-changing -- a 0 identity hash is otherwise impossible, see
        // the C28 note below -- which is why it is opt-in and named DBG.
        if deadrecv_check(&self.shared, self.thread, obj, "identity_hash_code") {
            return 0;
        }
        // JIT round 21, lane hashlock (HASH2-21-1), re-landed t1 wave 8.
        crate::jit::helpers::note_identity_hash_receiver(&self.shared, obj);
        // C28: identityHashCode must NEVER return 0. JDK's
        // InvokerBytecodeGenerator uses identityHashCode as a HashMap key and
        // asserts non-zero ("hash must be nonzero").
        //
        // The heap itself now lazily mints and durably CASes a non-zero hash
        // into the object's header the first time it's asked (see
        // `GenerationalHeap`/`G1Collector`/`ZgcRealHeap::identity_hash_code`),
        // which is what makes the value GC-move-stable — every mover copies
        // the header's bytes verbatim once non-zero. This used to be handled
        // HERE instead, by deriving a value from the object's *current*
        // address whenever the header read back 0: that value silently went
        // stale the instant a moving GC relocated the object (which happens
        // routinely — the JIT's inline `new` fast path in `jit/src/x64.rs`
        // leaves this field TLAB-zeroed on every allocation it makes, so
        // this was not a rare fallback but the common case for JIT-hot
        // allocation sites). Any per-object identity-keyed side table built
        // on the old, address-derived value would silently stop finding its
        // own entries after the first GC move — see
        // `tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md`
        // for the bug this produced in `keystore.rs`'s `store_id_by_identity`.
        //
        // A `0` from the heap is not "no hash yet" — it is "this object's mark
        // word cannot hold one and nothing is displaced": the word is
        // THIN_LOCKED (payload = owner + recursion) or INFLATED (payload = a
        // monitor pointer) and the object had never been hashed before it got
        // there. Mapping that to `i32::MAX` and returning was wrong twice over:
        // every locked-then-hashed object answered the SAME value, and the
        // answer CHANGED to a freshly minted one as soon as the lock was
        // released and the word went back to NEUTRAL.
        //
        // A `HashMap` keyed by such an object then cannot find its own entry —
        // `put` filed it under `i32::MAX`, `get` looks under the new hash — and
        // re-putting the same key grows a second entry. That is how Spring
        // Boot's `TomcatWebServer` lost the connectors it parks in a
        // `Map<Service, Connector[]>` from inside `LifecycleBase.start()`
        // (`synchronized` on that same `StandardService`), leaving
        // `Tomcat.getConnector()` to fabricate a port-8080 connector on a
        // running service. `probes/IdentityHashWhileLockedProbe.java` is the
        // three-line repro.
        //
        // Inflate and displace the hash into the monitor instead — HotSpot's
        // answer, and a stable one, since a live object's monitor is never
        // deflated here. `java_identity_hash` also keeps the last-resort
        // "never hand back 0" guard.
        let heap = &self.shared.mem.heap;
        let heap_answer = heap.identity_hash_code(obj);
        // hl21 section 5: host census confirmed 0 non-object-start hits across
        // all app suites; decline to 0 instead of entering inflate_locked on
        // a non-object / sentinel address.
        if heap_answer == 0 && !heap.is_object_start(obj.as_ptr() as usize) {
            return 0;
        }
        self.shared
            .threads
            .monitors
            .java_identity_hash(obj, heap_answer, || heap.next_identity_hash())
    }

    fn identity_hash_code_installed(&self, obj: ObjectRef) -> Option<i32> {
        // JIT round 21, lane native (`CRATONVM_JIT_IDENTITY_HASH_LEAF`): the
        // answer `identity_hash_code` above gives whenever the heap answers
        // non-zero -- `java_identity_hash` returns a non-zero `heap_answer`
        // unchanged -- without the monitor arm. The heap's answer is a header
        // read or its lazy mint-and-CAS (no allocation, no lock, no GC point);
        // a `0` from it minted nothing, so declining then leaves no effect
        // behind. The DBG dead-receiver probe answers as there.
        if deadrecv_check(&self.shared, self.thread, obj, "identity_hash_code") {
            return Some(0);
        }
        // JIT round 21, lane hash2 (`CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE`):
        // file this receiver's class verdict for the compiled identity arm
        // (a no-op until a compile asked for the table).
        crate::jit::helpers::note_identity_hash_receiver(&self.shared, obj);
        match self.shared.mem.heap.identity_hash_code(obj) {
            0 => None,
            hash => Some(hash),
        }
    }

    fn register_var_handle_root(&mut self, vh: ObjectRef) {
        // B-J: keep VarHandles alive (and copied into the GC pointer-map) so
        // their `static final` holder slots remap correctly across a move.
        // Keyed by identity hash (stable across moves). gc-common w29-d: the
        // key is a BUCKET, not a slot -- a second live object with the same
        // hash is appended, never written over the first (which would unroot
        // it); re-registering the same object is a no-op
        // (`heap_realm::VarHandleRoots`).
        let key = self.identity_hash_code(vh);
        self.shared.mem.var_handle_roots.write().insert(key, vh);
    }

    fn read_var_handle_root(&self, identity_key: i32) -> Option<ObjectRef> {
        // The GC remaps the registry entry after a move (memory/gc.rs); raw
        // ObjectRef copies cached in native statics do NOT get rewritten, so
        // long-lived native singletons re-read the current address here.
        // gc-common w29-d: answers the FIRST object registered under the key;
        // a later same-hash registrant is still rooted but cannot be told
        // apart by its hash (use `add_global_root` for a per-object handle).
        self.shared
            .mem
            .var_handle_roots
            .read()
            .get(&identity_key)
            .copied()
    }

    // -- Heap access methods --

    fn get_field_typed(&self, obj: ObjectRef, index: usize, descriptor: u8) -> Value {
        // Same decode as `get_field`, minus `resolve_field_descriptor_byte_cached`
        // — the caller supplied the answer that lookup would have produced.
        let obj = self.shared.mem.heap.load_and_forward(obj);
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        let read = self.shared.mem.heap.get_field_as(obj, index, descriptor);
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::get_field_typed",
            Some(obj),
            Some(index),
        );
        read
    }

    fn get_fields_typed(&self, obj: ObjectRef, slots: &[(usize, u8)], out: &mut [Value]) {
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        let obj = match self.shared.mem.heap.try_get_compact_fields(obj, slots, out) {
            Some(canonical) => canonical,
            None => {
                let obj = self.shared.mem.heap.load_and_forward(obj);
                for (slot, dst) in slots.iter().zip(out.iter_mut()) {
                    *dst = self.shared.mem.heap.get_field_as(obj, slot.0, slot.1);
                }
                obj
            }
        };
        // One watch for the batch: the point of this door is that it does not
        // pay per slot, and a batch that trips the guard is worth reporting
        // even if which slot did it needs the `slot=` address to pin down.
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::get_fields_typed",
            Some(obj),
            None,
        );
    }

    fn set_fields_typed(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[cratonvm_types::Value],
    ) {
        // One forwarding read and one corrupt-cell watch for the whole batch,
        // exactly as `get_fields_typed` above does for the read side, and the
        // caller-supplied descriptor in place of
        // `resolve_field_descriptor_byte_cached` per slot.
        //
        // What is deliberately NOT skipped: `set_field_as` still coerces
        // through the descriptor, so a batch store is the same store the
        // single-slot door performs. What IS skipped is the per-slot prologue
        // -- and that prologue is the cost. See the trait method's doc comment
        // for the measurement that motivated this door.
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        for &(slot_idx, _) in slots {
            crate::runtime::frozen_pages::note_slot_write(self.shared, obj, slot_idx, false);
        }
        let obj = match self
            .shared
            .mem
            .heap
            .try_set_compact_primitive_fields(obj, slots, values)
            .or_else(|| self.try_set_compact_fields_forwarded(obj, slots, values))
        {
            Some(canonical) => canonical,
            None => {
                let (obj, _) = self.shared.mem.heap.load_and_forward_checked(obj);
                for (slot, value) in slots.iter().zip(values.iter()) {
                    let value = forward_boundary_value(&self.shared.mem.heap, *value);
                    publish_overwritten_field(
                        &self.shared.mem.heap,
                        crate::runtime::env_cache::native_store_satb,
                        obj,
                        slot.0,
                        Some(slot.1),
                    );
                    self.shared
                        .mem
                        .heap
                        .set_field_as(obj, slot.0, value, slot.1);
                }
                obj
            }
        };
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::set_fields_typed",
            Some(obj),
            None,
        );
    }

    // ------------------------------------------------------------------
    // The validate-once read region (round 9 wave 15, heapapi).
    //
    // `NativeHeapAccess::ReadCursor` carries the measurement and the
    // soundness argument; this is the only implementor that mints a TRUSTED
    // cursor. What the trust buys, per accessor call, is the
    // `is_object_address` membership probe inside `load_and_forward` — a
    // random word of a bitmap sized one bit per eight arena bytes, i.e. a
    // cache miss. MEASURED at 15.3 % of `ChmProbe get` across its two layers
    // (`ZgcRealHeap::is_object_address` 11.77 %, `VmHeap::is_object_address`
    // 3.54 %), over roughly eleven accesses per `ConcurrentHashMap.get`.
    //
    // The region cannot reach a safepoint: a cursor borrows `&self`, and
    // every allocating / Java-calling / pinning method on this trait takes
    // `&mut self`. That is a compile error, not a convention.
    //
    // `CRATONVM_NATIVE_NO_READ_CURSOR=1` makes every cursor untrusted, so the
    // whole change is an A/B inside ONE binary — the same shape as
    // `CRATONVM_GC_NO_VALIDATE_ONCE`, and for the same reason: a claim about
    // a check that was added for a reproduced crash should be falsifiable
    // without a second build.

    fn read_cursor(&self, obj: ObjectRef) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        // The entry validation, paid ONCE. `load_and_forward_checked` hands
        // back the `obj` unchanged on a miss, which is exactly why the bool
        // and not the reference is what decides here.
        let (obj, validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        if !validated {
            return None;
        }
        if !native_read_cursor_trusted() {
            return Some(cratonvm_native_api::ReadCursor::checked(obj));
        }
        Some(cratonvm_native_api::ReadCursor::trusted(
            obj,
            READ_CURSOR_TRUSTED,
        ))
    }

    fn reached(
        &self,
        parent: &cratonvm_native_api::ReadCursor<'_>,
        value: Value,
    ) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        if parent.token() != READ_CURSOR_TRUSTED {
            // An untrusted parent cannot vouch for what it read: fall back to
            // the validating constructor, which is the default impl's answer.
            let Value::Object(Some(obj)) = value else {
                return None;
            };
            return self.read_cursor(obj);
        }
        self.promote_reached(value)
    }

    fn cursor_class_id(&self, cursor: &cratonvm_native_api::ReadCursor<'_>) -> ClassId {
        if cursor.token() != READ_CURSOR_TRUSTED {
            return self.class_id_of_object(cursor.object());
        }
        self.shared.mem.heap.class_id_of_validated(cursor.object())
    }

    fn cursor_field_typed(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
        descriptor: u8,
    ) -> Value {
        if cursor.token() != READ_CURSOR_TRUSTED {
            return self.get_field_typed(cursor.object(), index, descriptor);
        }
        // `get_field_typed` minus the membership walk. The corrupt-cell watch
        // stays: it is a debug instrument keyed on a counter, not a check this
        // region has any standing to elide.
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        let read = self.shared.mem.heap.get_field_as(obj, index, descriptor);
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::cursor_field_typed",
            Some(obj),
            Some(index),
        );
        read
    }

    fn cursor_fields_typed(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        slots: &[(usize, u8)],
        out: &mut [Value],
    ) {
        if cursor.token() != READ_CURSOR_TRUSTED {
            self.get_fields_typed(cursor.object(), slots, out);
            return;
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        for (slot, dst) in slots.iter().zip(out.iter_mut()) {
            *dst = self.shared.mem.heap.get_field_as(obj, slot.0, slot.1);
        }
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::cursor_fields_typed",
            Some(obj),
            None,
        );
    }

    fn cursor_field_volatile(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
    ) -> Value {
        if cursor.token() != READ_CURSOR_TRUSTED {
            return self.get_field_volatile(cursor.object(), index);
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        // The ORDERING is not the region's to elide -- see the trait's doc.
        self.shared.mem.heap.get_field_volatile(obj, index)
    }

    // The fused read-and-promote trio. Each is ONE dynamic call where the
    // natural `reached(&c, cursor_read(&c, i))` spelling is two, and a
    // reference read inside a region is almost always read in order to walk
    // it. See the trait's own note for the measurement that made this the
    // spelling to use.

    fn cursor_field_reference(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
    ) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        if cursor.token() != READ_CURSOR_TRUSTED {
            let value = self.get_field_typed(cursor.object(), index, b'L');
            return match value {
                Value::Object(Some(obj)) => self.read_cursor(obj),
                _ => None,
            };
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        self.promote_reached(self.shared.mem.heap.get_field_as(obj, index, b'L'))
    }

    fn cursor_field_volatile_reference(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
    ) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        if cursor.token() != READ_CURSOR_TRUSTED {
            let value = self.get_field_volatile(cursor.object(), index);
            return match value {
                Value::Object(Some(obj)) => self.read_cursor(obj),
                _ => None,
            };
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        self.promote_reached(self.shared.mem.heap.get_field_volatile(obj, index))
    }

    fn cursor_array_element_reference(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
    ) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        if cursor.token() != READ_CURSOR_TRUSTED {
            let value = self.get_array_element(cursor.object(), index);
            return match value {
                Value::Object(Some(obj)) => self.read_cursor(obj),
                _ => None,
            };
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        match self.shared.mem.heap.get_array_element_unboxing(obj, index) {
            Ok(value) => self.promote_reached(value),
            Err(_) => None,
        }
    }

    fn cursor_array_shape(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
    ) -> Option<(ArrayElementType, usize)> {
        if cursor.token() != READ_CURSOR_TRUSTED {
            return self.array_shape(cursor.object());
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        if self.shared.mem.heap.kind_of_validated(obj) != ObjectKind::Array {
            return None;
        }
        Some((
            self.shared.mem.heap.element_type_of_validated(obj),
            self.shared.mem.heap.array_length(obj),
        ))
    }

    fn cursor_array_element(
        &self,
        cursor: &cratonvm_native_api::ReadCursor<'_>,
        index: usize,
    ) -> Value {
        if cursor.token() != READ_CURSOR_TRUSTED {
            return self.get_array_element(cursor.object(), index);
        }
        let obj = self
            .shared
            .mem
            .heap
            .load_and_forward_validated(cursor.object());
        match self.shared.mem.heap.get_array_element_unboxing(obj, index) {
            Ok(value) => {
                // The two cheap tests first (strings round, `str16`): the env
                // lookup cost every element read of every per-element native
                // (`--compatible`'s `Arrays.copyOfRange` shadow spent ~10% of
                // `StringBuilder.toString()` in it), for a diagnostic that
                // only ever looks at every 1024th reference element.
                if index % 1024 == 0
                    && matches!(value, Value::Object(Some(_)))
                    && g1_gray_prov_dbg(&self.shared)
                {
                    if let Value::Object(Some(child)) = value {
                        let raw = child.as_ptr() as usize;
                        if self
                            .shared
                            .mem
                            .heap
                            .is_object_address_for_root_scan(raw)
                            .is_none()
                        {
                            eprintln!("[ARRAY-READ-BAD] holder={:#x} index={index} child={raw:#x} vacated={:?}", obj.as_ptr() as usize, cratonvm_gc::gc_quiescence::was_vacated(raw));
                        }
                    }
                }
                value
            }
            // The same chosen silence as `get_array_element`: a default that
            // is faithful to the element type, never an out-of-range read.
            Err(_) => match self.shared.mem.heap.element_type_of_validated(obj) {
                ArrayElementType::Reference => Value::Object(None),
                ArrayElementType::Long => Value::Long(0),
                ArrayElementType::Float => Value::Float(0.0),
                ArrayElementType::Double => Value::Double(0.0),
                _ => Value::Int(0),
            },
        }
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        // CRATONVM_DBG_STRAYSTACK, the native READ door -- the twin of the
        // write door in `set_field` below. Every door instrumented before
        // today was a WRITE door, so an accessor that only ever READS
        // out of bounds had nothing to name it: the Tomcat
        // `CoyoteInputStream` signature is 16 hits all carrying `op="get"`,
        // and the straystack dump stayed empty on every one of them.
        if youngscan_straystack_enabled() {
            let h = self.shared.mem.heap.get_header(obj);
            if index >= h.num_slots() as usize || h.num_slots() > (1 << 24) {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let k = N.fetch_add(1, Ordering::Relaxed);
                if k < 12 {
                    let (_cb_addr, culprit) = CURRENT_NATIVE_STACK
                        .with(|s| s.borrow().last().cloned())
                        .unwrap_or((0, "<none: not inside a native>".to_string()));
                    eprintln!(
                        "[straystack-native] #{k} OOB ctx.get_field recv@0x{:x} cid={} num_slots={} idx={} CULPRIT-NATIVE={}",
                        obj.as_ptr() as usize,
                        h.class_id.as_u32(),
                        h.num_slots(),
                        index,
                        culprit,
                    );
                    eprintln!("[straystack-native] Java stack (top first):");
                    for f in self.thread.frames.iter().rev().take(28) {
                        eprintln!(
                            "[straystack-native]   {}.{}{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc,
                        );
                    }
                }
            }
        }
        // CRATONVM_DBG_CORRUPT_CELL: remember the collector's corrupt-cell
        // counter across this read, so a read that trips it can be attributed to
        // its RECEIVER and its Java frames. See `corrupt_cell_dbg`.
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        // T10.9.E вЂ” descriptor-aware read path. Resolve (and cache) the
        // declared field descriptor for the receiver's class and route
        // the slot decode through `get_field_as`, so a long-typed field
        // always surfaces as `Value::Long` regardless of whatever tag
        // the raw storage happens to hold. Missing class metadata or an
        // unknown slot falls back to the legacy raw read via
        // `coerce_field_value_by_descriptor`'s default arm.
        // `load_and_forward` hands back its argument unchanged when validation
        // failed, so the trusted twin is only safe on the validated arm. The
        // `ClassId::new(0)` fallback is exactly what `class_id_of` returns on a
        // validation miss, so this is the same value by a shorter route.
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        let read = match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => {
                let decoded = self.shared.mem.heap.get_field_as(obj, index, desc);
                // L4 gap 1 — the read half of the overlay hunter. The value
                // checked is the RAW slot contents, not `decoded`: `get_field_as`
                // has already coerced the storage tag to the declared descriptor,
                // so by the time the native sees it the disagreement is gone.
                // Reading it raw is what makes a slot that some other writer
                // overlaid visible on the way out.
                if crate::runtime::env_cache::overlay_corruption_dbg() {
                    let raw = self.shared.mem.heap.get_field(obj, index);
                    overlay_check_access(
                        self.shared,
                        self.thread,
                        class_id,
                        index,
                        raw,
                        desc,
                        true,
                    );
                }
                decoded
            }
            None => {
                if pad_access_dbg() {
                    report_pad_access(self.shared, self.thread, obj, class_id, index, "get");
                }
                self.shared.mem.heap.get_field(obj, index)
            }
        };
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::get_field",
            Some(obj),
            Some(index),
        );
        // zt t32 (2b): every native read of `Reference.referent` -- `get()`,
        // `refersTo`, the `Unsafe` getters, reflection, the handles -- is
        // resurrection-blocked until the after-release referent clear has run.
        crate::memory::ref_discovery::gate_referent_read(self.shared, obj, index, read, || {
            self.shared.mem.heap.get_field(obj, index)
        })
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, false);
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
        // DIAGNOSTIC-ONLY (cce0079 tree-key tail): decisive probe - capture
        // the minor-GC epoch at entry and compare at exit. A delta proves a
        // GC completed INSIDE a plain ref store (and names the stack);
        // zero deltas across a firing run pins all staleness on producers.
        let epoch_entry = if blockgc_dbg() {
            Some(self.shared.mem.heap.debug_minor_gc_count())
        } else {
            None
        };
        struct EpochGuard<'a> {
            heap: &'a crate::memory::VmHeap,
            entry: Option<u64>,
        }
        impl Drop for EpochGuard<'_> {
            fn drop(&mut self) {
                if let Some(e) = self.entry {
                    let now = self.heap.debug_minor_gc_count();
                    if now != e {
                        static N: std::sync::atomic::AtomicU32 =
                            std::sync::atomic::AtomicU32::new(0);
                        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                            eprintln!(
                                "[SETFIELD-GC] minor epoch changed INSIDE set_field \
                                 ({e} -> {now}) thread={}:\n{}",
                                std::thread::current().name().unwrap_or("?"),
                                std::backtrace::Backtrace::force_capture(),
                            );
                        }
                    }
                }
            }
        }
        let _epoch_guard = EpochGuard {
            heap: &self.shared.mem.heap,
            entry: epoch_entry,
        };
        // bc math-ec 0x4 (CRATONVM_DBG_STRAYSTACK): a NATIVE writing through a
        // STRAY/STALE receiver (relocated-but-unremapped or wild). Same stray
        // header signature as the interpreter putfield check. Dumps the native
        // caller's Java stack so we can trace the cached-receiver use-after-move.
        if youngscan_straystack_enabled() {
            let h = self.shared.mem.heap.get_header(obj);
            if index >= h.num_slots() as usize || h.num_slots() > (1 << 24) {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let k = N.fetch_add(1, Ordering::Relaxed);
                if k < 12 {
                    let (cb_addr, culprit) = CURRENT_NATIVE_STACK
                        .with(|s| s.borrow().last().cloned())
                        .unwrap_or((0, "<unknown>".to_string()));
                    // RVA is a Windows-debugging convenience (module-relative
                    // address is easier to correlate with a .pdb/disassembly);
                    // `GetModuleHandleW` doesn't exist on other platforms, so
                    // fall back to reporting the raw (module_base=0) address.
                    #[cfg(windows)]
                    let module_base = unsafe {
                        extern "system" {
                            fn GetModuleHandleW(name: *const u16) -> *mut core::ffi::c_void;
                        }
                        GetModuleHandleW(core::ptr::null()) as usize
                    };
                    #[cfg(not(windows))]
                    let module_base: usize = 0;
                    let rva = cb_addr.wrapping_sub(module_base);
                    eprintln!(
                        "[straystack-native] #{k} STRAY ctx.set_field recv@0x{:x} cid={} num_slots={} kind={} idx={} value={:?} CULPRIT-NATIVE={} RVA=0x{:X}",
                        obj.as_ptr() as usize, h.class_id.as_u32(), h.num_slots(), cratonvm_types::ObjectHeader::kind_tag(h.mark_word.load(std::sync::atomic::Ordering::Relaxed)), index, value, culprit, rva,
                    );
                    eprintln!("[straystack-native] Java stack (top first):");
                    for f in self.thread.frames.iter().rev().take(28) {
                        eprintln!(
                            "[straystack-native]   {}.{}{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc,
                        );
                    }
                }
            }
        }
        // T10.9.E вЂ” descriptor-aware write path. Normalizing the stored
        // `Value` variant to the declared field type prevents tag drift
        // from leaking across subsequent reads. Fallback: legacy
        // `set_field` when the descriptor is unresolvable.
        // `load_and_forward` hands back its argument unchanged when validation
        // failed, so the trusted twin is only safe on the validated arm. The
        // `ClassId::new(0)` fallback is exactly what `class_id_of` returns on a
        // validation miss, so this is the same value by a shorter route.
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        let desc = resolve_field_descriptor_byte_cached(self.shared, class_id, index);
        // Round 16 wave 9 (lane satb): the Generational SATB pre-barrier the
        // heap accessor below does not do (see `NativeStoreSatb`).
        publish_overwritten_field(
            &self.shared.mem.heap,
            crate::runtime::env_cache::native_store_satb,
            obj,
            index,
            desc,
        );
        match desc {
            Some(desc) => {
                // Overlay hunter (CRATONVM_DBG_OVERLAY): a native writing a
                // primitive value to a reference-typed slot (or a reference to
                // a primitive slot) is a synthetic overlay bound to a real JDK
                // class — the descriptor coercion below silently destroys the
                // value (Int->null / ref->numeric). Surface it.
                //
                // L4 gap 2: `overlay_check_access` also fires when the slot is
                // one where our fabricated model and the loaded image disagree,
                // *whatever* the value's tag. That is the only way an `Int`
                // written into the wrong `Int` slot is ever reported — it
                // type-checks, so the cross-type test above passes it.
                if crate::runtime::env_cache::overlay_corruption_dbg() {
                    overlay_check_access(
                        self.shared,
                        self.thread,
                        class_id,
                        index,
                        value,
                        desc,
                        false,
                    );
                }
                self.shared.mem.heap.set_field_as(obj, index, value, desc)
            }
            None => {
                if pad_access_dbg() {
                    report_pad_access(self.shared, self.thread, obj, class_id, index, "set");
                }
                self.shared.mem.heap.set_field(obj, index, value)
            }
        }
        // CRATONVM_DBG_CORRUPT_CELL, the native WRITE door -- the twin of the
        // read watch in `get_field` above. Every door instrumented before
        // 2026-08-23 was a READ door, so a producer that only ever writes had
        // nothing to name it; the first array-receiver refusal caught on a real
        // workload was a `set_field`, and only the safepoint backstop reported
        // it, two minutes after the fact.
        crate::memory::reclaim_guard::corrupt_cell_watch_close(
            self.shared,
            self.thread,
            corrupt_before,
            "NativeContext::set_field",
            Some(obj),
            Some(index),
        );
        // write_barrier fires automatically inside set_field / set_field_as
    }

    // G56-1 NOMINATION 2: the trait default for `set_field_raw` delegates to
    // `NativeContext::set_field` above, which resolves a descriptor when it
    // can and coerces through it — the opposite of "raw". Override with the
    // same uncoercing body `set_field_by_name` already uses once it has
    // resolved a slot (`self.shared.mem.heap.set_field`, not `set_field_as`),
    // minus the name resolution. `get_field_raw` needs no override: the
    // default (`get_field_typed` with `RAW_SLOT_DESCRIPTOR`) is already raw
    // and skips the same descriptor-resolution cost a direct `heap.get_field`
    // would (`native-api/src/registry.rs` §5.1).
    fn set_field_raw(&self, obj: ObjectRef, index: usize, value: Value) {
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, false);
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        if pad_access_dbg() && obj_validated {
            let class_id = self.shared.mem.heap.class_id_of_validated(obj);
            if resolve_field_descriptor_byte_cached(self.shared, class_id, index).is_none() {
                report_pad_access(self.shared, self.thread, obj, class_id, index, "set-raw");
            }
        }
        publish_overwritten_field(
            &self.shared.mem.heap,
            crate::runtime::env_cache::native_store_satb,
            obj,
            index,
            None,
        );
        self.shared.mem.heap.set_field(obj, index, value);
    }

    // One validation and one layout resolution for the whole body
    // (`VmHeap::copy_compact_object_fields`), and one corrupt-cell watch, as
    // the batch doors above take one. A fresh `dst` holds nothing a SATB
    // publish of its overwritten slots could report. The slot-by-slot raw pair
    // serves whatever the heap's copy refuses, and every copy while the pad
    // diagnostic is armed, so its per-slot report still fires.
    fn copy_fields_into_clone(&self, src: ObjectRef, dst: ObjectRef, num_fields: usize) {
        if !pad_access_dbg() {
            let corrupt_before = crate::memory::reclaim_guard::corrupt_cell_watch();
            let copied = self.shared.mem.heap.copy_compact_object_fields(src, dst);
            crate::memory::reclaim_guard::corrupt_cell_watch_close(
                self.shared,
                self.thread,
                corrupt_before,
                "NativeContext::copy_fields_into_clone",
                Some(dst),
                None,
            );
            if copied.is_some() {
                return;
            }
        }
        for i in 0..num_fields {
            let val = self.get_field_raw(src, i);
            self.set_field_raw(dst, i, val);
        }
    }

    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        // `load_and_forward` hands back its argument unchanged when validation
        // failed, so the trusted twin is only safe on the validated arm. The
        // `ClassId::new(0)` fallback is exactly what `class_id_of` returns on a
        // validation miss, so this is the same value by a shorter route.
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        if let Some(index) = resolve_field_slot_by_name_cached(self.shared, class_id, field_name) {
            self.shared.mem.heap.get_field(obj, index)
        } else {
            Value::Object(None)
        }
    }

    fn get_fields_by_name(&self, obj: ObjectRef, names: &[&'static str], out: &mut [Value]) {
        // One canonicalisation and one class lookup for the whole batch; the
        // per-field work is the cached slot resolution and the heap read, which
        // is what `get_field_by_name` does after its own prologue.
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        let n = names.len().min(out.len());
        let mut slots = [usize::MAX; FIELD_BATCH_CHUNK];
        for (names, out) in names[..n]
            .chunks(FIELD_BATCH_CHUNK)
            .zip(out[..n].chunks_mut(FIELD_BATCH_CHUNK))
        {
            resolve_static_field_slots(self.shared, class_id, names, &mut slots);
            for (i, dst) in out.iter_mut().enumerate() {
                *dst = if slots[i] == usize::MAX {
                    Value::Object(None)
                } else {
                    self.shared.mem.heap.get_field(obj, slots[i])
                };
            }
        }
    }

    fn set_fields_by_name(&self, obj: ObjectRef, fields: &[(&'static str, Value)]) {
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        let mut slots = [usize::MAX; FIELD_BATCH_CHUNK];
        let mut names = [""; FIELD_BATCH_CHUNK];
        for chunk in fields.chunks(FIELD_BATCH_CHUNK) {
            for (i, (name, _)) in chunk.iter().enumerate() {
                names[i] = name;
            }
            resolve_static_field_slots(self.shared, class_id, &names[..chunk.len()], &mut slots);
            for (i, (name, value)) in chunk.iter().enumerate() {
                let value = forward_boundary_value(&self.shared.mem.heap, *value);
                if slots[i] != usize::MAX {
                    crate::runtime::frozen_pages::note_slot_write(
                        self.shared,
                        obj,
                        slots[i],
                        false,
                    );
                    publish_overwritten_field(
                        &self.shared.mem.heap,
                        crate::runtime::env_cache::native_store_satb,
                        obj,
                        slots[i],
                        None,
                    );
                    self.shared.mem.heap.set_field(obj, slots[i], value);
                } else {
                    let class_name = {
                        let cm = self.shared.classes.class_manager.read();
                        cm.get_class(class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| "<unresolved>".to_string())
                    };
                    note_field_by_name_dropped(class_id, &class_name, name, obj, value);
                }
            }
        }
    }

    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        // `load_and_forward` hands back its argument unchanged when validation
        // failed, so the trusted twin is only safe on the validated arm. The
        // `ClassId::new(0)` fallback is exactly what `class_id_of` returns on a
        // validation miss, so this is the same value by a shorter route.
        let class_id = if obj_validated {
            self.shared.mem.heap.class_id_of_validated(obj)
        } else {
            ClassId::new(0)
        };
        if let Some(index) = resolve_field_slot_by_name_cached(self.shared, class_id, field_name) {
            crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, false);
            publish_overwritten_field(
                &self.shared.mem.heap,
                crate::runtime::env_cache::native_store_satb,
                obj,
                index,
                None,
            );
            self.shared.mem.heap.set_field(obj, index, value);
            // write_barrier fires automatically inside set_field
        } else {
            let class_name = {
                let cm = self.shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| "<unresolved>".to_string())
            };
            note_field_by_name_dropped(class_id, &class_name, field_name, obj, value);
        }
    }

    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        let class_id = cm.get_loaded_class_id(class_name)?;
        resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
    }

    fn resolve_field_index_by_class_id(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
    }

    fn throwable_field_index(&self, field_name: &str) -> Option<usize> {
        use std::sync::atomic::Ordering;

        let cache = self
            .shared
            .threads
            .throwable_layout_cache
            .field_slot(field_name)?;
        let cached = cache.load(Ordering::Relaxed);
        if cached != crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX {
            return Some(cached);
        }
        let resolved = self.resolve_field_index("java/lang/Throwable", field_name)?;
        cache.store(resolved, Ordering::Relaxed);
        Some(resolved)
    }

    /// JIT round 16 (lane throwpath, TP16-2): the fill's three field accesses
    /// as one receiver forward, one header read and the three descriptor
    /// answers `set_field` / `get_field` would resolve, then the same
    /// `set_field_as` / `get_field_as` they end in. Refuses (`None`, nothing
    /// written) unless every slot is inside the receiver and the descriptors
    /// are the real `Throwable`'s (`Object backtrace`, `int depth`,
    /// `List suppressedExceptions`), so any other layout keeps the per-field
    /// path. What it skips is diagnostics only (the stray-receiver and
    /// corrupt-cell watches, the overlay hunter), which every other store
    /// still runs.
    fn finish_throwable_fill(&mut self, throwable: ObjectRef, depth: i32) -> Option<bool> {
        if !self
            .shared
            .threads
            .throwable_layout_cache
            .fill_direct_stores_enabled()
        {
            return None;
        }
        let backtrace_slot = self.throwable_field_index("backtrace")?;
        let depth_slot = self.throwable_field_index("depth")?;
        let suppressed_slot = self.throwable_field_index("suppressedExceptions")?;
        let heap = &self.shared.mem.heap;
        let (obj, validated) = heap.load_and_forward_checked(throwable);
        if !validated {
            return None;
        }
        let slots = heap.get_header(obj).num_slots() as usize;
        if backtrace_slot >= slots || depth_slot >= slots || suppressed_slot >= slots {
            return None;
        }
        let class_id = heap.class_id_of_validated(obj);
        let descriptor = |slot| resolve_field_descriptor_byte_cached(self.shared, class_id, slot);
        if descriptor(backtrace_slot) != Some(b'L')
            || descriptor(depth_slot) != Some(b'I')
            || descriptor(suppressed_slot) != Some(b'L')
        {
            return None;
        }
        // Interpreter round i3 wave 82, lane L2: with the throwable-owned
        // holder on, the capture already stored `backtrace` (the holder, or
        // the registry marker) -- `SharedVm::retain_captured_backtrace`; only
        // a `null` it could not write is marked here.
        if !self
            .shared
            .threads
            .throwable_layout_cache
            .backtrace_in_object_enabled()
            || matches!(
                heap.get_field_as(obj, backtrace_slot, b'L'),
                Value::Object(None)
            )
        {
            // nio21 wave 5 L1 (page
            // `r21-direct-heap-api-reference-stores-outside-the-jit-skip-generational-satb-FIXED-20261012.md`):
            // a re-filled throwable's old `backtrace` is published before it is
            // overwritten, as every `NativeContextImpl` field store does (the
            // Generational accessors card-mark only; G1 and ZGC publish inside
            // theirs, so this is a no-op there).
            publish_overwritten_field(
                heap,
                crate::runtime::env_cache::native_store_satb,
                obj,
                backtrace_slot,
                Some(b'L'),
            );
            heap.set_field_as(obj, backtrace_slot, Value::Object(Some(obj)), b'L');
        }
        heap.set_field_as(obj, depth_slot, Value::Int(depth), b'I');
        Some(matches!(
            heap.get_field_as(obj, suppressed_slot, b'L'),
            Value::Object(Some(_))
        ))
    }

    /// Round 15 wave 4 (lane threadvm; TR4W3-1): the per-VM
    /// `ThreadLayoutCache` answer. The class relation and the slot are each
    /// resolved once (under the class-manager lock, as the caller's own walk
    /// would), then answered lock-free.
    fn thread_declared_field_slot(
        &self,
        thread: ObjectRef,
        field_name: &str,
    ) -> Option<cratonvm_native_api::registry::ThreadDeclaredField> {
        use cratonvm_native_api::registry::ThreadDeclaredField;
        let cache = &self.shared.threads.thread_layout_cache;
        if !cache.enabled() {
            return None;
        }
        let class_id = self.shared.mem.heap.class_id_of(thread);
        let above = match cache.thread_class_above(class_id) {
            Some(above) => above,
            None => {
                let above = {
                    let cm = self.shared.classes.class_manager.read();
                    thread_class_above(&cm.class_store, class_id)
                };
                cache.remember_thread_class_above(class_id, above);
                above
            }
        };
        let Some(thread_class) = above else {
            return Some(ThreadDeclaredField::ByName);
        };
        if !cache.declared_fields_on() {
            return Some(ThreadDeclaredField::ByName);
        }
        let slot = cache.declared_slot(thread_class, field_name, || {
            let cm = self.shared.classes.class_manager.read();
            resolve_field_index_in_hierarchy(thread_class, field_name, &cm.class_store)
        })?;
        Some(slot.map_or(ThreadDeclaredField::Absent, ThreadDeclaredField::Declared))
    }

    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("new_array");
        let _no_gc = native_alloc_no_gc_point("new_array", self.shared);
        // BN2-1 (`CRATONVM_NATIVE_ARRAY_TLAB`, default OFF): the TLAB bump
        // first. Charged by the cursor, so not by `note_native_array_alloc`.
        if let Some(array) = self.native_array_tlab_arm(ClassId::new(0), element_type, length) {
            self.native_array_gc_request_after_alloc();
            return array;
        }
        // Caller-sized allocation: when this native is running under
        // `safe_native_call`'s `catch_unwind` (no JIT frame in between), a
        // request that neither generation can satisfy raises a catchable
        // `OutOfMemoryError` instead of aborting the process. The fallible
        // twin walks the identical young -> old-gen spill path, so a request
        // that fits behaves byte-for-byte as before. See
        // `crate::runtime::native_oom`.
        let array = if crate::runtime::native_oom::unwind_ok() {
            // HotSpot refuses a length past its array limit before trying the
            // heap; the unwind's `NativeAllocOom::java_message` names that
            // refusal (gc-common w3-c).
            if crate::runtime::interpreter::array_length_exceeds_vm_limit(length) {
                crate::runtime::native_oom::raise(length, "primitive array");
            }
            match self
                .shared
                .mem
                .heap
                .try_alloc_array_full(ClassId::new(0), element_type, length)
            {
                Some(a) => a,
                None => crate::runtime::native_oom::raise(length, "primitive array"),
            }
        } else {
            self.shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), element_type, length)
        };
        self.native_array_gc_request_after_alloc();
        // Straight to the heap, never through the TLAB -- so the counters only
        // learn about it here. See `note_native_object_alloc`.
        self.note_native_array_alloc(array);
        array
    }

    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("new_ref_array");
        let _no_gc = native_alloc_no_gc_point("new_ref_array", self.shared);
        if class_id.as_u32() == 0 && crate::runtime::env_cache::dbg_toarray() {
            let frame = self
                .thread
                .frames
                .last()
                .map(|f| format!("{}.{}", f.class_name(), f.method_name()))
                .unwrap_or_default();
            eprintln!(
                "[DBG_TOARRAY] new_ref_array(Object[],len={}) from frame={}",
                length, frame
            );
        }
        // BN2-1, as in `new_array`: the zeroed chunk reads back as nulls.
        if let Some(array) =
            self.native_array_tlab_arm(class_id, ArrayElementType::Reference, length)
        {
            self.native_array_gc_request_after_alloc();
            return array;
        }
        // Same caller-sized-allocation contract as `new_array` above.
        let array = if crate::runtime::native_oom::unwind_ok() {
            if crate::runtime::interpreter::array_length_exceeds_vm_limit(length) {
                crate::runtime::native_oom::raise(length, "reference array");
            }
            match self.shared.mem.heap.try_alloc_array_full(
                class_id,
                ArrayElementType::Reference,
                length,
            ) {
                Some(a) => a,
                None => crate::runtime::native_oom::raise(length, "reference array"),
            }
        } else {
            self.shared
                .mem
                .heap
                .alloc_array(class_id, ArrayElementType::Reference, length)
        };
        self.native_array_gc_request_after_alloc();
        self.note_native_array_alloc(array);
        array
    }

    fn try_new_ref_array(&mut self, class_id: ClassId, length: usize) -> Option<ObjectRef> {
        if crate::runtime::native_oom::forced_try_refusal("try_new_ref_array") {
            return None;
        }
        let _no_gc = native_alloc_no_gc_point("try_new_ref_array", self.shared);
        // Fallible sibling of `new_ref_array`: the heap's no-GC
        // young→old-gen spill path reports `None` on true exhaustion instead of
        // aborting, so native callers (e.g. `ArrayList(int)`) can raise a
        // catchable OutOfMemoryError matching HotSpot.
        //
        // BN2-1 (`CRATONVM_NATIVE_ARRAY_TLAB`, default OFF): the TLAB bump
        // first, charged by the cursor rather than `note_native_array_alloc`.
        if let Some(a) = self.native_array_tlab_arm(class_id, ArrayElementType::Reference, length) {
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
            return Some(a);
        }
        let array = self.shared.mem.heap.try_alloc_array_full(
            class_id,
            ArrayElementType::Reference,
            length,
        );
        if let Some(a) = array {
            self.note_native_array_alloc(a);
            // gcf f3/o: a graced retry that allocated is counted.
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
        }
        array
    }

    fn try_new_array(
        &mut self,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if crate::runtime::native_oom::forced_try_refusal("try_new_array") {
            return None;
        }
        let _no_gc = native_alloc_no_gc_point("try_new_array", self.shared);
        // Fallible sibling of `new_array` (primitive arrays) — see
        // `try_new_ref_array`.
        //
        // BN2-1 (`CRATONVM_NATIVE_ARRAY_TLAB`, default OFF): the TLAB bump
        // first; every `java.math` result's `mag` arrives here
        // (`bi_alloc_mag_array`). Charged by the cursor.
        if let Some(a) = self.native_array_tlab_arm(ClassId::new(0), element_type, length) {
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
            return Some(a);
        }
        let array =
            self.shared
                .mem
                .heap
                .try_alloc_array_full(ClassId::new(0), element_type, length);
        if let Some(a) = array {
            self.note_native_array_alloc(a);
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
        }
        array
    }

    fn reclaim_before_alloc_retry(&mut self) -> bool {
        // gci i13/oa item 4: the unsized door is the sized one with no size
        // (site `vm-exec`, as before).
        self.reclaim_before_alloc_retry_for(0)
    }

    fn reclaim_before_alloc_retry_for(&mut self, bytes: usize) -> bool {
        // The same ladder `gc_alloc_array` (interpreter) and `jit_newarray`
        // (JIT) run between their failed attempts. See the trait method for the
        // precondition the CALLER is responsible for — this function cannot
        // check it, because the locals at risk are the caller's.
        //
        // gcd d10/o (applied by lane f): the shared ladder in its single-retry
        // shape -- the interpreter's rungs (the soft-reference rung before any
        // OOME, Generational majors and the second major, G1's marking cycle
        // under a latched streak), where this body had its own pre-gen-r4w4
        // copy. `CRATONVM_GC_OVERHEAD_PROGRESS=0` restores the old body
        // (`native_reclaim_before_alloc_retry_historical`). `false` still means
        // "a retry is not worth it": the overhead limit's honest OOM.
        //
        // gcf f3/o: a graced retry is recorded so the factory's next
        // successful `try_*` allocation counts it (`ladder_latched_grace_ok=`).
        //
        // gci i13/oa item 4: `bytes` picks the young collection's site (a G1
        // humongous request collects under `G1 Humongous Allocation`).
        let verdict = crate::runtime::interpreter::gc_and_alloc::native_reclaim_before_alloc_retry_graced_for(
            self.shared,
            self.thread,
            bytes,
        );
        if verdict.graced {
            crate::runtime::interpreter::gc_and_alloc::note_graced_native_retry(
                self.shared,
                self.thread,
            );
        }
        // gcf f4/o: the `[GC] oome_ladder_doors:` census (`factory_*`).
        use crate::runtime::interpreter::gc_and_alloc::LadderDoor;
        crate::runtime::interpreter::gc_and_alloc::note_ladder_door_entered(
            self.shared,
            LadderDoor::NativeFactory,
        );
        if verdict.retry {
            crate::runtime::interpreter::gc_and_alloc::note_ladder_door_allocated(
                self.shared,
                LadderDoor::NativeFactory,
            );
        }
        verdict.retry
    }

    fn reclaim_young_before_alloc_retry(&mut self) -> bool {
        self.reclaim_young_before_alloc_retry_for(0)
    }

    fn reclaim_young_before_alloc_retry_for(&mut self, bytes: usize) -> bool {
        use crate::runtime::interpreter::gc_and_alloc as ladder;
        // gci i13/oa item 4: the young collection's site follows `bytes`.
        let ran = ladder::native_reclaim_young_stage_for(self.shared, self.thread, bytes);
        if ran {
            ladder::note_ladder_door_entered(self.shared, ladder::LadderDoor::NativeFactory);
            ladder::note_ladder_door_allocated(self.shared, ladder::LadderDoor::NativeFactory);
        }
        ran
    }

    fn reclaim_before_alloc_retry_after_young(&mut self) -> bool {
        use crate::runtime::interpreter::gc_and_alloc as ladder;
        let verdict = ladder::native_reclaim_after_young_stage(self.shared, self.thread);
        if verdict.graced {
            ladder::note_graced_native_retry(self.shared, self.thread);
        }
        if verdict.retry {
            ladder::note_ladder_door_allocated(self.shared, ladder::LadderDoor::NativeFactory);
        }
        verdict.retry
    }

    fn reclaim_after_failed_alloc_retry(&mut self) -> bool {
        // zr z8/nr: pays a ZGC room skip's clear-soft rung on this thread, if
        // one is pending; nothing otherwise (Generational, G1: never pending).
        crate::runtime::interpreter::gc_and_alloc::zgc_native_repay_skipped_soft_rung(
            self.shared,
            self.thread,
        )
    }

    fn reclaim_after_fragmented_humongous_retry(&mut self) -> bool {
        // The preceding allocation retry has just failed.  For G1 alone that
        // failure may now be the first evidence of fragmentation: its first
        // full cycle could have reclaimed dead humongous/Old data in separate
        // holes.  Do not turn this into a general third try; the G1 field is
        // set only when free capacity covers the exact request but no run does.
        if !self.shared.mem.heap.is_g1()
            || !self.shared.mem.heap.g1_has_fragmented_humongous_request()
        {
            return false;
        }
        crate::runtime::interpreter::last_ditch_reclaim(self.shared, self.thread);
        true
    }

    fn array_component_class_id(&self, class_id: ClassId) -> Option<ClassId> {
        // `array_info` is `Some` only for array classes; its `component_class_id`
        // is the immediate element type (e.g. `String[]` for `String[][]`).
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .and_then(|c| c.array_info.as_ref().map(|ai| ai.component_class_id))
    }

    #[track_caller]
    fn array_length(&self, obj: ObjectRef) -> usize {
        // KINDOF-SENTINEL: `obj` reaches this native accessor from callers
        // (e.g. `native-collections`' `map_state`, which validates a bucket
        // array via `heap_kind_of` and then calls this several lines later)
        // that treat an earlier validation as still good at the point of
        // use. Observed once with `obj` as the all-ones sentinel
        // `0xFFFFFFFFFFFFFFFF` — not a stale-but-plausible address, and not
        // reachable through any conservative-root-scan path (those all
        // filter through `is_object_address`, which this call chain does
        // not). `load_and_forward`'s very first read (`header.is_forwarded()`)
        // dereferences the raw pointer unconditionally, so an invalid `obj`
        // here is a hard crash rather than a wrong answer. Validate before
        // touching memory at all, and fall back to the same "not an array"
        // diagnostic path already used below for a live-but-wrong-kind object.
        if self
            .shared
            .mem
            .heap
            .is_object_address(obj.as_ptr() as usize)
            .is_none()
        {
            if crate::runtime::env_cache::dbg_arrlen() {
                let loc = std::panic::Location::caller();
                eprintln!(
                    "[ARRAY-LEN-GUARD] obj is not a valid heap address rust-caller={}:{} obj={:?}",
                    loc.file(),
                    loc.line(),
                    obj
                );
            }
            return 0;
        }
        // The KINDOF-SENTINEL guard above has just validated `obj`, so the
        // barrier and the kind read below both take their trusted twins:
        // three membership walks per call become one, with no object left
        // unvalidated.
        let obj = self.shared.mem.heap.load_and_forward_validated(obj);
        let kind = self.shared.mem.heap.kind_of_validated(obj);
        if kind != ObjectKind::Array {
            // Only emit the (noisy) diagnostic when explicitly requested — the
            // `#[track_caller]` location pinpoints the offending native/opcode
            // far more reliably than the previously-broken symbolized backtrace.
            if crate::runtime::env_cache::dbg_arrlen() {
                let class_id = self.shared.mem.heap.class_id_of(obj);
                let class_name = self
                    .shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                let top = self
                    .thread
                    .frames
                    .last()
                    .map(|f| {
                        format!(
                            "{}.{}{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        )
                    })
                    .unwrap_or_else(|| "<no-frame>".to_string());
                let loc = std::panic::Location::caller();
                eprintln!(
                    "[ARRAY-LEN-GUARD] non-array object class={} kind={:?} caller={} rust-caller={}:{} obj={:?}",
                    class_name, kind, top, loc.file(), loc.line(), obj
                );
            }
            return 0;
        }
        self.shared.mem.heap.array_length(obj)
    }

    /// # The out-of-range answer is CHOSEN, not inherited
    ///
    /// `VmHeap::get_array_element_unboxing` DOES range-check (`gen_heap.rs`
    /// returns `Err(index)` for `index >= array_length`, and
    /// `ObjectHeader::array_length` answers `0` for a non-array, so a
    /// non-array receiver lands on the same `Err` rather than a wild read).
    /// This impl deliberately turns that `Err` into a default value instead of
    /// an exception, for two reasons, in this order:
    ///
    ///  1. **The trait forbids anything else.** `NativeContext::
    ///     get_array_element` is declared `-> Value` (`native-api/src/
    ///     registry.rs`) and its contract reads: *"the caller is responsible
    ///     for range-checking against `array_length`, and the implementation
    ///     MUST bounds-check and MUST NOT read out of range (fail safe — e.g.
    ///     default value or VM error — never an out-of-bounds heap read)"*.
    ///     There is no error channel to propagate into, and ~3 300 call sites
    ///     across `native-builtins` / `native-collections` / `native-io` were
    ///     written against that contract.
    ///  2. **A range check here would be in the wrong place anyway.** The Java
    ///     exception a bad index must raise depends on the *caller*:
    ///     `ArrayIndexOutOfBoundsException` for a `VarHandle`/`Array.get`
    ///     accessor, `IndexOutOfBoundsException` for a `List` view,
    ///     `BufferUnderflowException` for a NIO buffer, and nothing at all for
    ///     the many probe-shaped callers that read speculatively. Each of
    ///     those is the caller's to raise, from the caller's own length.
    ///     `native-builtins`'s `vh_array_index` is the worked example: it
    ///     range-checks the coordinate and raises `RuntimeError::aioobe`,
    ///     whose text is `Preconditions.checkIndex`'s character for character.
    ///
    /// What this DOES fix is the shape of the default. It used to be
    /// `Value::Int(0)` for every element type, so an out-of-range read of a
    /// reference array handed the caller an `Int` where it expected an oop,
    /// and a `long[]`/`double[]` read handed it an `Int` where it expected a
    /// `Long`/`Double` — a tag mismatch a caller cannot tell from a real
    /// element. The default is now the JLS zero value **of the array's own
    /// element type**, and `Int(0)` only where the element type is unknowable
    /// (non-array receiver). The type lookup is on the cold `Err` arm only.
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        // DBG (`CRATONVM_DBG_DEADRECV`) — the second measured face of the
        // Generational `--nojit` reclaim: 1 crash in 3 faults in
        // `get_array_element_unboxing` rather than in `identity_hash_code`.
        // Asked before `load_and_forward`, which dereferences.
        if deadrecv_check(&self.shared, self.thread, obj, "get_array_element") {
            return Value::Object(None);
        }
        let obj = self.shared.mem.heap.load_and_forward(obj);
        match self.shared.mem.heap.get_array_element_unboxing(obj, index) {
            Ok(value) => {
                // The two cheap tests first (strings round, `str16`): the env
                // lookup cost every element read of every per-element native
                // (`--compatible`'s `Arrays.copyOfRange` shadow spent ~10% of
                // `StringBuilder.toString()` in it), for a diagnostic that
                // only ever looks at every 1024th reference element.
                if index % 1024 == 0
                    && matches!(value, Value::Object(Some(_)))
                    && g1_gray_prov_dbg(&self.shared)
                {
                    if let Value::Object(Some(child)) = value {
                        let raw = child.as_ptr() as usize;
                        if self
                            .shared
                            .mem
                            .heap
                            .is_object_address_for_root_scan(raw)
                            .is_none()
                        {
                            eprintln!("[ARRAY-READ-BAD] holder={:#x} index={index} child={raw:#x} vacated={:?}", obj.as_ptr() as usize, cratonvm_gc::gc_quiescence::was_vacated(raw));
                        }
                    }
                }
                value
            }
            Err(_) => match array_element_type_of(self.shared, obj) {
                Some(ArrayElementType::Reference) => Value::Object(None),
                Some(ArrayElementType::Long) => Value::Long(0),
                Some(ArrayElementType::Float) => Value::Float(0.0),
                Some(ArrayElementType::Double) => Value::Double(0.0),
                // Boolean/Char/Byte/Short/Int all live in an `Int` slot, and
                // so does the "not an array at all" case, where there is no
                // element type to be faithful to.
                _ => Value::Int(0),
            },
        }
    }

    fn object_is_array(&self, obj: ObjectRef) -> bool {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        self.shared.mem.heap.kind_of(obj) == ObjectKind::Array
    }

    /// The dropped `Err` is the same CHOSEN silence as
    /// `get_array_element`'s default above — see that doc comment for why it
    /// cannot be anything else here, and for where the range check belongs
    /// instead. `VmHeap::set_array_element` returns `Err(index)` and writes
    /// nothing when the index is past the end (or the receiver is not an
    /// array), so dropping it discards a *report*, never a write: the store is
    /// already suppressed by the time we see the result.
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        // Round 16 wave 9 (lane satb): Generational's `set_array_element`
        // does the card but not the SATB pre-barrier, so the overwritten
        // element is published here, before the store -- the chokepoint for
        // `System.arraycopy`'s per-element loops and every `ArrayList` native
        // that shifts or copies a backing array (see `NativeStoreSatb`).
        publish_overwritten_array_element(
            &self.shared.mem.heap,
            crate::runtime::env_cache::native_store_satb,
            obj,
            index,
        );
        // Deliberate discard: `NativeContext::set_array_element` is `-> ()`
        // and its contract is "no-op or VM error, never an out-of-bounds heap
        // write". The caller range-checks; `native-builtins`'s
        // `vh_array_index` is the worked example.
        //
        // The ONE code that is not an index gets a report. Since 2026-09-03 a
        // store that needs an auto-box wrapper on a full heap returns
        // `ARRAY_STORE_OUT_OF_MEMORY` instead of `std::process::abort()`-ing
        // (see that constant); the interpreter's `*astore` arms raise
        // `OutOfMemoryError` from it, but this accessor has no error channel to
        // raise through, so the store is dropped. Dropping it is still the
        // right behaviour — the alternative was killing the VM — but it must
        // not be SILENT, because a dropped element is exactly the shape of
        // defect that takes a week to trace back to a heap that was full for
        // one millisecond. Reported once per process: a full heap produces
        // these in floods, and the first one is the one that matters.
        if let Err(cratonvm_gc::heap::ARRAY_STORE_OUT_OF_MEMORY) =
            self.shared.mem.heap.set_array_element(obj, index, value)
        {
            static REPORTED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "OutOfMemoryError: Java heap space while auto-boxing a primitive into a \
                     reference array from native code (index {index}); the element was left \
                     unchanged. This accessor has no exception channel — see \
                     ARRAY_STORE_OUT_OF_MEMORY."
                );
            }
        }
        // write_barrier fires automatically inside set_array_element for ref arrays
        //
        // Phase 10 #2: the host just wrote this array, so any device
        // buffer mirroring it is stale.
        //
        // AUDIT 2026-09-02: native code writes arrays too, and none of it
        // went through an interpreter `*astore` arm. `System.arraycopy`
        // lands here for its per-element shapes, and every other native
        // array writer that reaches `NativeHeapAccess` does as well. This
        // is the chokepoint for all of them, which is why the fix is here
        // rather than in `native-builtins` — that crate cannot see
        // `input_cache`, and its `gpu-offload` feature is empty.
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::invalidate(obj);
    }

    // -- Bulk primitive-array intrinsics (perf override) --------------------
    //
    // The trait defaults loop via `set_array_element` / `get_array_element`,
    // each iteration doing a virtual dispatch + Value boxing + element-type
    // match. For multi-KB byte/char copies (ZipFile entry reads, file I/O,
    // String construction from char[]) that's the round-2 review's largest
    // unfixed hotspot. Here we replace the loop with a single
    // `ptr::copy_nonoverlapping` against the compact array payload exposed
    // by `VmHeap::array_data_ptr`. Bounds + element-type are verified up
    // front; on mismatch we fall through to `false` / `0` to match the
    // documented contract — callers fall back to the per-element path.

    fn write_byte_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u8]) -> bool {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        // invariant: returns true iff every src byte landed inside the
        // destination array (kind/element-type/bounds all OK). The default
        // impl in `native-api/src/registry.rs` now performs the same
        // bounds check up front; callers may rely on `true` meaning the
        // full slice was written and `false` meaning nothing was written.
        // Verify destination is a byte/boolean array (both store 1 byte per element).
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return false;
        }
        let et = self.shared.mem.heap.element_type_of_validated(arr);
        if !matches!(et, ArrayElementType::Byte | ArrayElementType::Boolean) {
            return false;
        }
        let len = self.shared.mem.heap.array_length(arr);
        let end = match dst_off.checked_add(src.len()) {
            Some(e) => e,
            None => return false,
        };
        if end > len {
            return false;
        }
        if src.is_empty() {
            return true;
        }
        // SAFETY: bounds checked above (`dst_off + src.len() <= array_length`).
        // `array_data_ptr` returns a pointer to the compact 1-byte-per-element
        // payload immediately after the object header. `src` is a borrowed
        // slice that cannot overlap with the heap arena (Rust borrow rules
        // — we hold `&mut self`, and the heap arena's data is not aliased
        // by a Rust-side `&mut [u8]`). The destination range is
        // `[dst_off, dst_off + src.len())` which lies wholly within the
        // array's payload of `len` bytes.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                // gcstress face-1 hunt (no-op unless CRATONVM_DBG_WATCH_CELL set).
                cratonvm_gc::heap::cell_watch_check(
                    base.add(dst_off) as usize,
                    src.len(),
                    "write_byte_array_from",
                    &arr.as_ptr(),
                );
                std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(dst_off), src.len());
            },
            // G1 humongous byte[]: payload is split across non-contiguous
            // regions, so there is no flat base pointer. Store element-by-
            // element through the region-aware accessor (bounds already
            // verified, so every index lands inside the array).
            None => {
                for (i, &b) in src.iter().enumerate() {
                    if self
                        .shared
                        .mem
                        .heap
                        .set_array_element(arr, dst_off + i, Value::Int(b as i8 as i32))
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn read_byte_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u8]) -> usize {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return 0;
        }
        let et = self.shared.mem.heap.element_type_of_validated(arr);
        if !matches!(et, ArrayElementType::Byte | ArrayElementType::Boolean) {
            return 0;
        }
        let len = self.shared.mem.heap.array_length(arr);
        if src_off > len {
            return 0;
        }
        let available = len - src_off;
        let n = available.min(dst.len());
        if n == 0 {
            return 0;
        }
        // SAFETY: bounds checked above (`src_off + n <= array_length`,
        // `n <= dst.len()`). Source pointer comes from `array_data_ptr`
        // (compact 1-byte-per-element payload). Destination is a caller-
        // owned `&mut [u8]` which cannot alias the heap arena.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(base.add(src_off), dst.as_mut_ptr(), n);
            },
            // G1 humongous byte[]: region-safe per-element read. Byte slots
            // decode to a sign-extended `Value::Int`; mask back to the raw
            // 8-bit value.
            None => {
                for (i, slot) in dst.iter_mut().take(n).enumerate() {
                    match self.shared.mem.heap.get_array_element(arr, src_off + i) {
                        Ok(Value::Int(x)) => *slot = x as u8,
                        _ => return i,
                    }
                }
            }
        }
        n
    }

    fn read_int_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [i32]) -> usize {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return 0;
        }
        if self.shared.mem.heap.element_type_of_validated(arr) != ArrayElementType::Int {
            return 0;
        }
        let len = self.shared.mem.heap.array_length(arr);
        if src_off > len {
            return 0;
        }
        let available = len - src_off;
        let n = available.min(dst.len());
        if n == 0 {
            return 0;
        }
        // SAFETY: bounds checked above. Int arrays are a flat 4-bytes-per-element
        // payload (`element_byte_size`), so `n * 4` bytes from `base + src_off*4`
        // is exactly elements `src_off..src_off+n`, in host order — the same
        // convention the char twin above relies on. `dst` is caller-owned and
        // cannot alias the heap arena.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(
                    base.add(src_off * 4),
                    dst.as_mut_ptr() as *mut u8,
                    n * 4,
                );
            },
            // G1 humongous int[]: region-safe per-element read, same fallback
            // shape as the byte/char twins.
            None => {
                for (i, slot) in dst.iter_mut().take(n).enumerate() {
                    match self.shared.mem.heap.get_array_element(arr, src_off + i) {
                        Ok(Value::Int(x)) => *slot = x,
                        _ => return i,
                    }
                }
            }
        }
        n
    }

    fn write_primitive_array_bytes(&mut self, arr: ObjectRef, byte_off: usize, src: &[u8]) -> bool {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        let Some(capacity) = primitive_array_byte_capacity_of(&self.shared.mem.heap, arr) else {
            return false;
        };
        if byte_off
            .checked_add(src.len())
            .map_or(true, |end| end > capacity)
        {
            return false;
        }
        if src.is_empty() {
            return true;
        }
        // SAFETY: the receiver is a primitive array (checked), and the
        // write stays inside its payload (checked). `array_data_ptr`
        // yields the base of that payload, whose elements are contiguous
        // and `element_byte_size` wide — the same layout argument
        // `write_int_array_from` rests on.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(byte_off), src.len());
                true
            },
            None => false,
        }
    }

    fn read_primitive_array_bytes(&self, arr: ObjectRef, byte_off: usize, dst: &mut [u8]) -> usize {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        let Some(capacity) = primitive_array_byte_capacity_of(&self.shared.mem.heap, arr) else {
            return 0;
        };
        if byte_off >= capacity {
            return 0;
        }
        let n = dst.len().min(capacity - byte_off);
        if n == 0 {
            return 0;
        }
        // SAFETY: as above, with the length clamped to what remains.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(base.add(byte_off), dst.as_mut_ptr(), n);
                n
            },
            None => 0,
        }
    }

    unsafe fn misaligned_raw_atomic_update(
        &self,
        addr: *mut u8,
        width: usize,
        f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        // JIT round 21 proposal ATOMICS21-4: the arena arm of the array
        // arm's misaligned `LOCK CMPXCHG` below.
        // SAFETY: the caller's contract (live, writable `width` bytes).
        unsafe { misaligned_locked_update(addr, width, f) }
    }

    fn primitive_array_atomic_update(
        &mut self,
        arr: ObjectRef,
        byte_off: usize,
        width: usize,
        f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering::SeqCst};
        let arr = self.shared.mem.heap.load_and_forward(arr);
        // nio21 wave 6 L3: width 2 serves the `SubwordLane` straddle (a short
        // over two `byte[]` elements); 4 and 8 the raw wide accesses.
        if width != 2 && width != 4 && width != 8 {
            return None;
        }
        // The width-2 arm is new in wave 6 and shares the misaligned arm's
        // kill switch, so `=0` puts the straddle back on its lock entirely.
        if width == 2 && !crate::runtime::env_cache::unsafe_misaligned_hw_cas_enabled() {
            return None;
        }
        let capacity = primitive_array_byte_capacity_of(&self.shared.mem.heap, arr)?;
        if byte_off
            .checked_add(width)
            .map_or(true, |end| end > capacity)
        {
            return None;
        }
        let base = self.shared.mem.heap.array_data_ptr(arr)?;
        // SAFETY: in bounds of a primitive array's flat payload (checked
        // above, as `read_primitive_array_bytes`); the native holds no
        // safepoint between `array_data_ptr` and the access, so the array
        // cannot move under it -- the discipline every payload helper here
        // relies on.
        let addr = unsafe { base.add(byte_off) };
        if (addr as usize) % width != 0 {
            // nio21 wave 6 L3
            // (`docs/feature-designs/real-jdk-mode.md`):
            // HotSpot's x86-64 `Unsafe` CAS at a misaligned address is a
            // `LOCK CMPXCHG` there -- atomic against everything, never a
            // throw. Do the same when the access stays inside the payload
            // (checked above). `None` keeps the caller's process lock.
            // SAFETY: `addr .. addr + width` is live payload of this array
            // (bounds checked above), stable for the call (see above).
            return unsafe { misaligned_locked_update(addr, width, f) };
        }
        // SAFETY: `addr` is naturally aligned for `width` and points at
        // `width` live payload bytes; Java code reaches the same bytes only
        // through atomic or plain machine accesses, as with HotSpot's
        // `Unsafe` on an array.
        unsafe {
            if width == 2 {
                let a = AtomicU16::from_ptr(addr.cast::<u16>());
                let mut cur = a.load(SeqCst);
                loop {
                    let Some(new) = f(u64::from(cur)) else {
                        return Some((u64::from(cur), false));
                    };
                    match a.compare_exchange(cur, new as u16, SeqCst, SeqCst) {
                        Ok(_) => return Some((u64::from(cur), true)),
                        Err(seen) => cur = seen,
                    }
                }
            }
            if width == 4 {
                let a = AtomicU32::from_ptr(addr.cast::<u32>());
                let mut cur = a.load(SeqCst);
                loop {
                    let Some(new) = f(u64::from(cur)) else {
                        return Some((u64::from(cur), false));
                    };
                    match a.compare_exchange(cur, new as u32, SeqCst, SeqCst) {
                        Ok(_) => return Some((u64::from(cur), true)),
                        Err(seen) => cur = seen,
                    }
                }
            }
            let a = AtomicU64::from_ptr(addr.cast::<u64>());
            let mut cur = a.load(SeqCst);
            loop {
                let Some(new) = f(cur) else {
                    return Some((cur, false));
                };
                match a.compare_exchange(cur, new, SeqCst, SeqCst) {
                    Ok(_) => return Some((cur, true)),
                    Err(seen) => cur = seen,
                }
            }
        }
    }

    fn write_int_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[i32]) -> bool {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return false;
        }
        if self.shared.mem.heap.element_type_of_validated(arr) != ArrayElementType::Int {
            return false;
        }
        let len = self.shared.mem.heap.array_length(arr);
        if dst_off.checked_add(src.len()).map_or(true, |end| end > len) {
            return false;
        }
        if src.is_empty() {
            return true;
        }
        // SAFETY: bounds checked above; see `read_int_array_into` for the
        // 4-bytes-per-element layout argument.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr() as *const u8,
                    base.add(dst_off * 4),
                    src.len() * 4,
                );
            },
            None => {
                for (i, v) in src.iter().enumerate() {
                    if self
                        .shared
                        .mem
                        .heap
                        .set_array_element(arr, dst_off + i, Value::Int(*v))
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn read_char_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u16]) -> usize {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return 0;
        }
        if self.shared.mem.heap.element_type_of_validated(arr) != ArrayElementType::Char {
            return 0;
        }
        let len = self.shared.mem.heap.array_length(arr);
        if src_off > len {
            return 0;
        }
        let available = len - src_off;
        let n = available.min(dst.len());
        if n == 0 {
            return 0;
        }
        // SAFETY: bounds checked above. Char arrays are stored as 2 bytes
        // per element (see `read_prim_element` Char branch / `element_byte_size`).
        // We copy `n * 2` bytes from `base + src_off*2` into `dst.as_mut_ptr()`.
        // Same host-endian convention as `VmHeap::read_char_array_bulk`,
        // which already uses `copy_nonoverlapping` between this payload and
        // a host `[u16]`.
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(
                    base.add(src_off * 2),
                    dst.as_mut_ptr() as *mut u8,
                    n * 2,
                );
            },
            // G1 humongous char[]: region-safe per-element read. Char slots
            // decode to `Value::Int(u16 as i32)`; mask back to the code unit.
            None => {
                for (i, slot) in dst.iter_mut().take(n).enumerate() {
                    match self.shared.mem.heap.get_array_element(arr, src_off + i) {
                        Ok(Value::Int(x)) => *slot = x as u16,
                        _ => return i,
                    }
                }
            }
        }
        n
    }

    fn write_char_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u16]) -> bool {
        let arr = self.shared.mem.heap.load_and_forward(arr);
        // AUDIT 2026-05-17: symmetric to `read_char_array_into` above.
        // Used by `stream_decoder::refill` to populate the read-ahead
        // char buffer in one `memcpy` instead of N virtual dispatches.
        if self.shared.mem.heap.kind_of(arr) != ObjectKind::Array {
            return false;
        }
        if self.shared.mem.heap.element_type_of_validated(arr) != ArrayElementType::Char {
            return false;
        }
        let len = self.shared.mem.heap.array_length(arr);
        let end = match dst_off.checked_add(src.len()) {
            Some(e) => e,
            None => return false,
        };
        if end > len {
            return false;
        }
        if src.is_empty() {
            return true;
        }
        // SAFETY: bounds checked above. Char arrays store 2 bytes per
        // element matching host-endian `u16` (see `read_char_array_into`).
        match self.shared.mem.heap.array_data_ptr(arr) {
            Some(base) => unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr() as *const u8,
                    base.add(dst_off * 2),
                    src.len() * 2,
                );
            },
            // G1 humongous char[]: region-safe per-element store. Char slots
            // store the low 16 bits of `Value::Int`.
            None => {
                for (i, &c) in src.iter().enumerate() {
                    if self
                        .shared
                        .mem
                        .heap
                        .set_array_element(arr, dst_off + i, Value::Int(c as i32))
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn bulk_array_copy(
        &mut self,
        src: ObjectRef,
        src_off: usize,
        dst: ObjectRef,
        dst_off: usize,
        len: usize,
    ) -> bool {
        if len == 0 {
            return true;
        }
        let src = self.shared.mem.heap.load_and_forward(src);
        let dst = self.shared.mem.heap.load_and_forward(dst);
        if self.shared.mem.heap.kind_of(src) != ObjectKind::Array
            || self.shared.mem.heap.kind_of(dst) != ObjectKind::Array
        {
            return false;
        }
        let src_et = self.shared.mem.heap.element_type_of(src);
        let dst_et = self.shared.mem.heap.element_type_of(dst);
        // Primitive arrays only — reference arrays need per-element store
        // checks (ArrayStoreException) and the GC write barrier, which we
        // intentionally do not bypass here.
        if src_et != dst_et || src_et == ArrayElementType::Reference {
            return false;
        }
        let stride = match src_et {
            ArrayElementType::Byte | ArrayElementType::Boolean => 1usize,
            ArrayElementType::Char | ArrayElementType::Short => 2,
            ArrayElementType::Int | ArrayElementType::Float => 4,
            ArrayElementType::Long | ArrayElementType::Double => 8,
            ArrayElementType::Reference => return false, // unreachable, guarded above
        };
        let src_len = self.shared.mem.heap.array_length(src);
        let dst_len = self.shared.mem.heap.array_length(dst);
        let src_end = match src_off.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        let dst_end = match dst_off.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        if src_end > src_len || dst_end > dst_len {
            return false;
        }
        // SAFETY: bounds checked above for both source and destination.
        // For same-array overlap we use `copy` (memmove semantics); for
        // distinct arrays the ranges cannot overlap (different heap
        // allocations) so `copy_nonoverlapping` is also valid, but `copy`
        // is fine in either case and lets us share one branch.
        //
        // Either array may be a G1 humongous array, whose payload is split
        // across non-contiguous regions and therefore has no flat base
        // pointer. Only take the bulk-memcpy fast path when BOTH arrays
        // expose a contiguous pointer; otherwise fall back to a region-safe
        // per-element copy through `get_array_element` / `set_array_element`
        // (both route humongous objects through `humongous_copy`).
        match (
            self.shared.mem.heap.array_data_ptr(src),
            self.shared.mem.heap.array_data_ptr(dst),
        ) {
            (Some(src_base), Some(dst_base)) => unsafe {
                let src_ptr = src_base.add(src_off * stride);
                let dst_ptr = dst_base.add(dst_off * stride);
                // JIT round 20 wave 15 (lane fuzz14c; correctness,
                // unconditional): element-atomic, as HotSpot's arraycopy
                // stubs are. A byte `memmove` may move one 2/4/8-byte
                // element in pieces, so a racing writer's element was
                // observed TORN (`R20FuzzGTearCopy1` `tear.copyof 11` under
                // `--compatible`: `Arrays.copyOf(int[])`'s native, `clone()`
                // and `System.arraycopy`'s native all copy here).
                bulk_copy_element_atomic(src_ptr, dst_ptr, len * stride, stride);
            },
            _ => {
                // memmove semantics: when copying within the same array and the
                // ranges overlap with `dst_off > src_off`, iterate backwards so
                // a source element is read before it is overwritten.
                let same_array = src.as_ptr() == dst.as_ptr();
                if same_array && dst_off > src_off {
                    for i in (0..len).rev() {
                        match self.shared.mem.heap.get_array_element(src, src_off + i) {
                            Ok(v) => {
                                if self
                                    .shared
                                    .mem
                                    .heap
                                    .set_array_element(dst, dst_off + i, v)
                                    .is_err()
                                {
                                    return false;
                                }
                            }
                            Err(_) => return false,
                        }
                    }
                } else {
                    for i in 0..len {
                        match self.shared.mem.heap.get_array_element(src, src_off + i) {
                            Ok(v) => {
                                if self
                                    .shared
                                    .mem
                                    .heap
                                    .set_array_element(dst, dst_off + i, v)
                                    .is_err()
                                {
                                    return false;
                                }
                            }
                            Err(_) => return false,
                        }
                    }
                }
            }
        }
        // Phase 10 #2: `dst` has just been overwritten in bulk, so any
        // device buffer mirroring it is stale.
        //
        // AUDIT 2026-09-02: this is the FAST path — `System.arraycopy`
        // for same-type primitive arrays takes it and never touches
        // `set_array_element`, so invalidating there alone left this hole
        // open. `GpuRuntimeStress`'s `bulk_writes` scenario is the one
        // that found it: a `System.arraycopy` into a kernel's input array
        // between submits, after which the next submit computed from the
        // device copy the host had replaced.
        //
        // `src` is not invalidated: a copy READS it and leaves it byte
        // for byte as the device already has it.
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::invalidate(dst);
        true
    }

    /// Round 16 lane `arraylist`: the reference twin of `bulk_array_copy`,
    /// for `System.arraycopy`'s identical-component-class case (see the trait
    /// doc). Opt-in through `CRATONVM_JIT_BULK_REF_ARRAYCOPY` at its callers
    /// (`native_system_arraycopy`, the `native-collections` `ArrayList`
    /// natives).
    ///
    /// The per-element trait path this replaces paid, for EVERY element, a
    /// `deadrecv_check` + `load_and_forward` (an object-start walk) of the
    /// source in `get_array_element`, and another `load_and_forward` of the
    /// destination plus one of the stored value in `set_array_element` --
    /// 42% of a reference `System.arraycopy`'s samples were
    /// `load_and_forward_inner` alone. Both receivers are now validated and
    /// forwarded once per call: a native holds raw `ObjectRef`s across its
    /// own Rust code (the STW census waits for `NativeRunning`), so neither
    /// array, nor any slot of a live array, can move during the copy.
    ///
    /// Two element paths, each carrying the barriers `set_array_element`
    /// carries:
    ///
    /// * ZGC with neither its concurrent-mark (SATB) barrier nor its read
    ///   barrier armed -- the steady state: the slots are copied word by
    ///   word (relaxed atomics, so a racing reader never sees a torn
    ///   reference; memmove direction for an overlapping same-array copy),
    ///   then ONE `note_ref_store(dst)`, which is exactly what each
    ///   per-element store would have recorded (an object-keyed card).
    ///   Words are copied verbatim: an `AUTOBOX_CLASS_ID` wrapper stays the
    ///   wrapper instead of being un-boxed and re-boxed into a fresh one.
    /// * Every other collector state: the heap's own per-element accessors,
    ///   which keep their SATB / card / auto-box handling, minus the
    ///   per-element receiver walks.
    fn copy_reference_array_elements(
        &mut self,
        src: ObjectRef,
        src_off: usize,
        dst: ObjectRef,
        dst_off: usize,
        len: usize,
    ) -> bool {
        let shared = self.shared;
        if deadrecv_check(shared, &*self.thread, src, "copy_reference_array_elements")
            || deadrecv_check(shared, &*self.thread, dst, "copy_reference_array_elements")
        {
            return false;
        }
        let heap = &shared.mem.heap;
        let src = heap.load_and_forward(src);
        let dst = heap.load_and_forward(dst);
        // `kind_of` first: it validates the base, and `element_type_of`
        // answers `Reference` for an address that is not an object at all.
        if heap.kind_of(src) != ObjectKind::Array
            || heap.kind_of(dst) != ObjectKind::Array
            || heap.element_type_of(src) != ArrayElementType::Reference
            || heap.element_type_of(dst) != ArrayElementType::Reference
        {
            return false;
        }
        let (Some(src_end), Some(dst_end)) = (src_off.checked_add(len), dst_off.checked_add(len))
        else {
            return false;
        };
        if src_end > heap.array_length(src) || dst_end > heap.array_length(dst) {
            return false;
        }
        if len == 0 {
            return true;
        }
        // Memmove direction: only a same-array copy to a higher index can
        // overwrite a source slot before it is read.
        let backward = src.as_ptr() == dst.as_ptr() && dst_off > src_off;
        // From here on the copy always completes: the trait contract forbids
        // a `false` after a write.
        if !copy_reference_slots_raw(heap, src, src_off, dst, dst_off, len, backward) {
            // Generational's `set_array_element` does the card but NOT the
            // SATB pre-barrier (its own doc: a native store must publish the
            // overwritten reference itself), and the per-element loop this
            // replaces never did -- so a reference `System.arraycopy` during
            // an old-generation concurrent mark could hide the value it
            // overwrote. G1 and ZGC publish inside the accessor.
            //
            // Round 16 wave 9 (lane satb): the gate is now the queue's state
            // (`NativeStoreSatb::native_store_must_publish`), asked BEFORE the
            // old element is read -- this loop used to read every overwritten
            // element on Generational even while nothing was marking, only for
            // `satb_barrier` to discard it. Not under the accessor's kill
            // switch: this arm always published.
            for step in 0..len {
                let i = if backward { len - 1 - step } else { step };
                publish_overwritten_array_element(heap, || true, dst, dst_off + i);
                // Same default as `get_array_element`'s `Err` arm for a
                // reference array. Unreachable: both ranges were validated.
                let value = heap
                    .get_array_element_unboxing(src, src_off + i)
                    .unwrap_or(Value::Object(None));
                match value {
                    Value::Object(_) => {
                        // A reference store cannot fail: the index is in
                        // range and nothing is boxed.
                        let _ = heap.set_array_element(dst, dst_off + i, value);
                    }
                    // An un-boxed `AUTOBOX_CLASS_ID` payload: re-boxing
                    // allocates and can run out of memory, so it takes the
                    // trait accessor that reports that (rare: such wrappers
                    // never live in a Java-level `Object[]`).
                    other => NativeHeapAccess::set_array_element(self, dst, dst_off + i, other),
                }
            }
        }
        // See `bulk_array_copy`: `dst` was overwritten, `src` only read.
        #[cfg(feature = "gpu-offload")]
        crate::runtime::offload::input_cache::invalidate(dst);
        true
    }

    /// JIT round 23 wave 2e (`jit-r23-w2-proxy-proposals.md` item 2; trait
    /// doc): the checked reference copy, raw, on ZGC with both reference
    /// barriers quiet -- the state [`copy_reference_slots_raw`] serves. The
    /// two arrays are validated and forwarded once (the per-element trait
    /// loop it replaces paid `deadrecv_check` + `load_and_forward` per element
    /// for the source, the destination and the value); each element is one
    /// relaxed word load, a header class-id read and one relaxed word store.
    /// The proof per element is: null, or a plain (non-array, non-autobox)
    /// object whose class id is `dst_component` or a subclass of it
    /// (`is_subclass`, the published secondary-supers closure; the last
    /// proven id is memoised for the call). Anything else stops the copy at
    /// that element for the caller's full predicate. Then one
    /// `note_ref_store(dst)`, as `copy_reference_slots_raw`.
    fn copy_reference_array_elements_checked(
        &mut self,
        src: ObjectRef,
        src_off: usize,
        dst: ObjectRef,
        dst_off: usize,
        len: usize,
        dst_component: ClassId,
    ) -> Option<usize> {
        #[cfg(feature = "zgc")]
        {
            let shared = self.shared;
            if deadrecv_check(shared, &*self.thread, src, "copy_reference_array_elements_checked")
                || deadrecv_check(
                    shared,
                    &*self.thread,
                    dst,
                    "copy_reference_array_elements_checked",
                )
            {
                return None;
            }
            let heap = &shared.mem.heap;
            if !heap.is_zgc() {
                return None;
            }
            if heap.mark_active() || heap.load_barrier_armed() {
                return None;
            }
            let src = heap.load_and_forward(src);
            let dst = heap.load_and_forward(dst);
            if heap.kind_of(src) != ObjectKind::Array
                || heap.kind_of(dst) != ObjectKind::Array
                || heap.element_type_of(src) != ArrayElementType::Reference
                || heap.element_type_of(dst) != ArrayElementType::Reference
            {
                return None;
            }
            let (Some(src_end), Some(dst_end)) =
                (src_off.checked_add(len), dst_off.checked_add(len))
            else {
                return None;
            };
            if src_end > heap.array_length(src) || dst_end > heap.array_length(dst) {
                return None;
            }
            // Only the forward direction: a same-array copy to a higher index
            // is the caller's pre-check-then-backward path.
            if src.as_ptr() == dst.as_ptr() && dst_off > src_off {
                return None;
            }
            if cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8 {
                return None;
            }
            let (Some(src_base), Some(dst_base)) =
                (heap.array_data_ptr(src), heap.array_data_ptr(dst))
            else {
                return None;
            };
            let mut proven: Option<ClassId> = None;
            let mut n = 0usize;
            while n < len {
                // SAFETY: both ranges were validated against the arrays'
                // lengths; slots are 8-aligned words of live reference arrays
                // that cannot move while this native runs; unarmed ZGC slots
                // hold plain pointers.
                let word = unsafe {
                    (*(src_base.add((src_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                if word != 0 {
                    // SAFETY: a non-zero word of a live reference slot of a
                    // quiet ZGC heap is the address of a live object.
                    let elem = unsafe { ObjectRef::from_raw(word as usize as *mut u8) };
                    let header = heap.get_header(elem);
                    if header.kind() != ObjectKind::Object {
                        break;
                    }
                    let cid = header.class_id;
                    if cid == cratonvm_types::AUTOBOX_CLASS_ID {
                        break;
                    }
                    if proven != Some(cid) {
                        if cid != dst_component
                            && !cratonvm_native_api::NativeClassAccess::is_subclass(
                                &*self,
                                cid,
                                dst_component,
                            )
                        {
                            break;
                        }
                        proven = Some(cid);
                    }
                }
                if heap.zgc_mark_active() {
                    // SAFETY: `dst_base + (dst_off + n) * 8` is a valid, uncolored slot.
                    let word = unsafe {
                        (*(dst_base.add((dst_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                            .load(std::sync::atomic::Ordering::Relaxed)
                    };
                    if word != 0 {
                        heap.zgc_satb_pre_barrier(word as usize);
                    }
                }
                // SAFETY: as for the load.
                unsafe {
                    (*(dst_base.add((dst_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                        .store(word, std::sync::atomic::Ordering::Relaxed);
                }
                n += 1;
            }
            if n > 0 {
                heap.zgc_note_ref_store(dst.as_ptr() as usize);
                #[cfg(feature = "gpu-offload")]
                crate::runtime::offload::input_cache::invalidate(dst);
            }
            Some(n)
        }
        #[cfg(not(feature = "zgc"))]
        {
            let _ = (src, src_off, dst, dst_off, len, dst_component);
            None
        }
    }

    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        self.shared.mem.heap.kind_of(obj)
    }

    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        self.shared.mem.heap.element_type_of(obj)
    }

    /// One membership walk and one forwarding barrier for all three answers.
    ///
    /// `object_is_array` + `heap_element_type_of` + `array_length` repeat both
    /// per call, and `array_length`'s walk alone was measurable on the
    /// `String/Regex` row: `is_object_address` is 5% of its builder phase, and
    /// `sb_view` asks all three of these questions about the same payload
    /// array on every `append`. The KINDOF-SENTINEL guard comes first for the
    /// reason `array_length` documents — an invalid `obj` must not reach
    /// `load_and_forward`, whose first read dereferences it unconditionally.
    fn array_shape(&self, obj: ObjectRef) -> Option<(ArrayElementType, usize)> {
        self.shared
            .mem
            .heap
            .is_object_address(obj.as_ptr() as usize)?;
        let obj = self.shared.mem.heap.load_and_forward_validated(obj);
        if self.shared.mem.heap.kind_of_validated(obj) != ObjectKind::Array {
            return None;
        }
        Some((
            self.shared.mem.heap.element_type_of_validated(obj),
            self.shared.mem.heap.array_length(obj),
        ))
    }

    fn create_string(&mut self, text: &str) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("create_string");
        let _no_gc = native_alloc_no_gc_point("create_string", self.shared);
        // Round 14 wave 1 (lane compat), design A of
        // `r13w8-bigdec-dynamic-strings-interned-into-a-never-pruned-root`: a
        // pool HIT answers the pooled object (text equal to a resolved literal
        // stays `==` to it); a MISS is fresh and NOT inserted, so computed text
        // no longer grows the never-pruned root. Callers whose Java semantics
        // need identity call `intern_string`.
        // `CRATONVM_NATIVE_CREATE_STRING_HIT_OR_FRESH=0` restores insert-on-miss.
        if crate::runtime::env_cache::native_create_string_hit_or_fresh() {
            // gci i21/sw: the hit through the weak G1 table's keep-alive.
            if let Some(&obj) = self.shared.mem.string_pool.read().get(text) {
                return crate::memory::string_table::keep_alive_on_hit(self.shared, obj);
            }
            return self.create_string_uninterned(text);
        }
        self.intern_string(text)
    }

    fn intern_string(&mut self, text: &str) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("intern_string");
        // Pooled: an intern HIT returns an existing object and allocates
        // nothing, and only a miss is charged. The constructor reports which
        // happened, so the hit path does not pay a second pool lock.
        //
        // gen r4w3/hunter (2026-09-23): on a thread that may unwind (inside
        // `safe_native_call`), heap exhaustion raises a catchable
        // `OutOfMemoryError` exactly as `heap_alloc_object` / `new_array` do.
        // The infallible constructor below `abort()`s the process with
        // "FATAL: heap exhausted allocating java/lang/String" instead.
        let (s, allocated) = if crate::runtime::native_oom::unwind_ok() {
            match super::try_create_java_string_reporting(self.shared, text) {
                Some(r) => r,
                None => crate::runtime::native_oom::raise(text.len(), "String"),
            }
        } else {
            super::create_java_string_reporting(self.shared, text)
        };
        if allocated {
            self.note_native_string_alloc(s);
        }
        s
    }

    fn intern_string_receiver(&mut self, receiver: ObjectRef, text: &str) -> Option<ObjectRef> {
        // gcg g2/w: `CRATONVM_STRING_INTERN_RETURNS_RECEIVER` (per VM; default on
        // since gcg g4/rs, `=0` off).
        // Allocates nothing on the heap, so there is no OOM arm.
        if !self.shared.mem.string_table.intern_returns_receiver() {
            return None;
        }
        Some(crate::memory::string_table::intern_receiver(
            self.shared,
            receiver,
            text,
        ))
    }

    fn string_pool_is_weak(&self) -> bool {
        // gci i4/wr (`gcg-g2w-string-table-review-findings` item 2): the
        // surrogate pool's handles follow the main pool's disposition.
        crate::memory::string_table::weak_table_engaged(self.shared)
    }

    fn create_string_uninterned(&mut self, text: &str) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("create_string_uninterned");
        // The threaded constructor never collects: a TLAB refill falls back
        // to the same no-GC heap allocator as this door used before. Thus a
        // caller may still hold unpinned references across this allocation.
        let accounted_before = self.thread.tlab.thread_allocated_bytes();
        let (s, all_in_tlab) = if crate::runtime::native_oom::unwind_ok() {
            match super::try_create_java_string_uninterned_gc_safe_threaded_with_tlab_status(
                self.shared,
                self.thread,
                text,
            ) {
                Some(res) => res,
                None => crate::runtime::native_oom::raise(text.len(), "String"),
            }
        } else {
            (super::create_java_string_uninterned(self.shared, text), false)
        };
        if all_in_tlab {
            self.charge_external_allocation(0);
            return s;
        }
        // TLAB bumps already contribute to ThreadMXBean's allocation total;
        // only charge the portion that fell back to a shared heap allocation.
        let already_accounted = self
            .thread
            .tlab
            .thread_allocated_bytes()
            .saturating_sub(accounted_before);
        self.note_native_string_alloc_minus_tlab(s, already_accounted);
        s
    }

    fn create_string_uninterned_gc_safe(&mut self, text: &str) -> ObjectRef {
        // Native callers opt into this only after pinning every live Java
        // reference.  Native string construction uses two no-GC allocations
        // (the String plus its backing byte[]); if young is fragmented, the
        // generic fallible allocator would otherwise spill both into old gen
        // without crossing `needs_gc()`.  Request a normal young collection
        // before that spill, leaving old-gen collection policy unchanged.
        const STRING_ALLOCATION_HEADROOM: usize = 256;
        if !self
            .shared
            .mem
            .heap
            .young_bump_headroom(STRING_ALLOCATION_HEADROOM)
            && !self
                .shared
                .mem
                .heap
                .young_has_free_block(STRING_ALLOCATION_HEADROOM)
        {
            self.shared
                .mem
                .gc_requested
                .store(true, std::sync::atomic::Ordering::Relaxed);
            crate::runtime::interpreter::maybe_gc(self.shared, self.thread);
        }
        let s = match super::try_create_java_string_uninterned_gc_safe_threaded(
            self.shared,
            self.thread,
            text,
        ) {
            Some(s) => s,
            // gc-common w5-c (`common-w3c-native-callback-allocations-cannot-collect`):
            // this is the one native allocator whose CALLER has promised to
            // hold no unpinned reference (the trait contract above), so on
            // exhaustion it may do what `new` does -- the shared
            // collect-and-retry ladder -- before reporting the catchable
            // `OutOfMemoryError`. It used to fall back to the infallible
            // constructor, which aborts the process on a full heap.
            None if crate::runtime::native_oom::unwind_ok() => {
                // gci i19/oo (i13/oa item 4 residual (a)): a G1 humongous
                // `value` collects as `G1 Humongous Allocation`, as HotSpot's.
                // Computed on the refusal path only.
                let footprint = crate::runtime::interpreter::native_string_value_footprint(
                    self.shared.classes.latin1_coder_allowed(),
                    text.encode_utf16().count(),
                    text.chars().all(|c| (c as u32) <= 0xFF),
                );
                match crate::runtime::interpreter::native_alloc_collecting_sized(
                    self.shared,
                    self.thread,
                    "native-gc-safe-string",
                    footprint,
                    |shared, thread| {
                        super::try_create_java_string_uninterned_gc_safe_threaded(
                            shared, thread, text,
                        )
                        .unwrap_or_else(|| crate::runtime::native_oom::raise(text.len(), "String"))
                    },
                ) {
                    Ok(s) => s,
                    Err(_) => crate::runtime::native_oom::raise(text.len(), "String"),
                }
            }
            // Outside a native callback (a context no `catch_unwind` guards):
            // the historical infallible constructor.
            None => super::create_java_string_uninterned(self.shared, text),
        };
        self.note_native_string_alloc(s);
        s
    }

    fn get_ascii_case_string_cached(
        &mut self,
        source: ObjectRef,
        locale: Option<ObjectRef>,
        upper: bool,
    ) -> Option<ObjectRef> {
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        let entry = self.thread.string_case_cache.iter_mut().find(|entry| {
            entry.source == source && entry.locale == locale && entry.upper == upper
        })?;
        let result = if entry.next {
            entry.second
        } else {
            entry.first
        };
        entry.next = !entry.next;
        Some(result)
    }

    fn create_ascii_case_string_cached(
        &mut self,
        source: ObjectRef,
        locale: Option<ObjectRef>,
        text: &str,
        upper: bool,
    ) -> ObjectRef {
        if let Some(result) = self.get_ascii_case_string_cached(source, locale, upper) {
            return result;
        }

        // Keep source and the first result rooted across the second allocation:
        // the allocation slow path is allowed to request a moving young GC.
        let base = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(source);
        // gen r4w3/rooting: `locale` is stored into `string_case_cache` (a GC
        // root) below, after both GC-safe allocations — pin and re-read it too.
        let locale_pin = locale.map(|l| {
            let idx = self.thread.native_pin_roots.len();
            self.thread.native_pin_roots.push(l);
            idx
        });
        let first = self.create_string_uninterned_gc_safe(text);
        let first_pin = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(first);
        let second = self.create_string_uninterned_gc_safe(text);
        let source = self.thread.native_pin_roots[base];
        let locale = locale_pin.map(|idx| self.thread.native_pin_roots[idx]);
        let first = self.thread.native_pin_roots[first_pin];
        self.thread.native_pin_roots.truncate(base);

        // The two allocations above may each have run a pause: re-gate before
        // inserting, so the new entry (whose objects were just re-read from
        // the pins) is stamped at the generation it is valid for rather than
        // cleared at the next probe.
        crate::memory::roots::validate_jit_memo_caches(self.shared, self.thread);
        if self.thread.string_case_cache.len() >= 32 {
            self.thread.string_case_cache.remove(0);
        }
        self.thread
            .string_case_cache
            .push(crate::threading::jvm_thread::StringCaseCacheEntry {
                source,
                locale,
                upper,
                first,
                second,
                next: true,
            });
        first
    }

    fn init_string_from_units(&mut self, this: ObjectRef, units: &[u16]) -> bool {
        let ok = super::populate_java_string_fields(self.shared, this, units);
        if ok {
            // gcf f4/o: the String factories' graced retry is counted too.
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
        }
        ok
    }

    fn java_string_hash_code(&self, obj: ObjectRef) -> Option<i32> {
        compact_java_string_hash(self.shared, obj)
    }

    fn java_strings_equal(&self, a: ObjectRef, b: ObjectRef) -> Option<bool> {
        if !is_real_java_string(self.shared, a) || !is_real_java_string(self.shared, b) {
            return None;
        }
        compact_java_strings_equal(self.shared, a, b)
    }

    fn read_string_units(&self, obj: ObjectRef) -> Option<Vec<u16>> {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        // Guarded exactly as `read_string` is, and for the same reason its own
        // comment gives: the structural reader duck-types a String from field
        // 0, and a CratonVM synthetic `StringBuilder` is also char[]-backed
        // with an OVER-allocated buffer, so a shape test alone would decode the
        // buffer's capacity as text. `read_java_string_units` shares
        // `read_string`'s guards below the surface -- they were factored out of
        // the `str` reader precisely so the two cannot drift -- but the class
        // identity test is this layer's, so it is applied here too.
        if is_real_java_string(&self.shared, obj) {
            if let Some(units) =
                super::vm_object::read_java_string_units(&self.shared.mem.heap, obj)
            {
                return Some(units);
            }
        }
        // Not a confirmed String, or unreadable as one: fall back to the same
        // answer the trait default would have given, which is what every
        // non-String caller of this already expects.
        self.read_string(obj).map(|s| s.encode_utf16().collect())
    }

    fn read_string_units_into(&self, obj: ObjectRef, out: &mut [u16]) -> Option<usize> {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        if is_real_java_string(&self.shared, obj) {
            if let Some(len) = super::vm_object::read_java_string_units_into(
                &self.shared.mem.heap,
                obj,
                out,
            ) {
                return Some(len);
            }
        }
        let units = self.read_string_units(obj)?;
        if units.len() > out.len() {
            return None;
        }
        out[..units.len()].copy_from_slice(&units);
        Some(units.len())
    }

    /// JIT r23 wave 5, lane L5: the caller guarantees a `java.lang.String`,
    /// so no class-identity lookup (`is_real_java_string` takes the class
    /// manager's lock); `append_java_string_units` keeps its own shape guards
    /// (value array kind, coder in {0, 1}) and reads the payload in place.
    /// On `false` it may have left nothing in `out` -- it appends only on
    /// success.
    fn append_known_string_units(&self, obj: ObjectRef, out: &mut Vec<u16>) -> bool {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        super::vm_object::append_java_string_units(&self.shared.mem.heap, obj, out)
    }

    fn create_string_from_units(&mut self, units: &[u16]) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("create_string_from_units");
        // Same unwinding contract as `create_string` (gc-common w5-c): this was
        // the one native String constructor without it, so a native building a
        // String from UTF-16 units on a full heap aborted the process
        // ("FATAL: heap exhausted allocating java/lang/String") where its
        // `&str` twins raise a catchable `OutOfMemoryError`.
        let s = if crate::runtime::native_oom::unwind_ok() {
            match super::try_create_java_string_from_units(self.shared, units) {
                Some(s) => s,
                None => crate::runtime::native_oom::raise(units.len(), "String"),
            }
        } else {
            super::create_java_string_from_units(self.shared, units)
        };
        self.note_native_string_alloc(s);
        s
    }

    fn create_string_from_units_gc_safe(&mut self, units: &[u16]) -> ObjectRef {
        let s = if crate::runtime::native_oom::unwind_ok() {
            // gci i19/oo (i13/oa item 4 residual (a)): the `value` footprint
            // names a G1 humongous request's collection `G1 Humongous
            // Allocation`; the first attempt of the ladder is unchanged.
            let footprint = crate::runtime::interpreter::native_string_value_footprint(
                self.shared.classes.latin1_coder_allowed(),
                units.len(),
                units.iter().all(|&u| u <= 0xFF),
            );
            match crate::runtime::interpreter::native_alloc_collecting_sized(
                self.shared,
                self.thread,
                "native-gc-safe-string-units",
                footprint,
                |shared, _thread| {
                    super::try_create_java_string_from_units(shared, units)
                        .unwrap_or_else(|| crate::runtime::native_oom::raise(units.len(), "String"))
                },
            ) {
                Ok(s) => s,
                Err(_) => crate::runtime::native_oom::raise(units.len(), "String"),
            }
        } else {
            super::create_java_string_from_units(self.shared, units)
        };
        self.note_native_string_alloc(s);
        s
    }

    fn read_ascii_string_into(&self, obj: ObjectRef, dst: &mut [u8]) -> Option<usize> {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        let string_class =
            super::vm_object::cached_java_string_class_id(self.shared).or_else(|| {
                // A leaf must decline the cold lookup: the class-manager lock
                // can be held by code that reaches a safepoint.
                if LEAF_CALL.with(|state| state.get()) != LEAF_CALL_NONE {
                    None
                } else {
                    Some(super::vm_object::java_string_allocation_layout(self.shared).0)
                }
            })?;
        let heap = &self.shared.mem.heap;
        if heap.kind_of(obj) != ObjectKind::Object || heap.class_id_of(obj) != string_class {
            return None;
        }
        let mut fields = [Value::Int(0); 2];
        if heap
            .try_get_compact_fields(obj, &[(0, b'L'), (1, b'B')], &mut fields)
            .is_none()
        {
            fields = [heap.get_field(obj, 0), heap.get_field(obj, 1)];
        }
        let Value::Object(Some(value)) = fields[0] else {
            return None;
        };
        let value = heap.load_and_forward(value);
        // A fabricated char[] String and a UTF16 byte[] String use the full
        // reader. No by-name field lookup is permitted on this leaf door.
        if heap.array_element_type(value) != Some(ArrayElementType::Byte)
            || fields[1] != Value::Int(0)
        {
            return None;
        }
        let len = heap.array_length(value);
        if len > dst.len() || self.read_byte_array_into(value, 0, &mut dst[..len]) != len {
            return None;
        }
        dst[..len].is_ascii().then_some(len)
    }

    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        let class_id = self.shared.mem.heap.class_id_of(obj);
        // Reference arrays carry their component class ID; a String[] is not
        // a String.
        if self.shared.mem.heap.kind_of(obj) != ObjectKind::Object {
            return None;
        }
        // Guard by class identity BEFORE the structural reader. `read_java_string`
        // below duck-types a String from the char[]/byte[] in field 0, but a
        // CratonVM synthetic `StringBuilder`/`StringBuffer` is *also* char[]-backed
        // (field 0 = char[] buffer, field 1 = int count) with an OVER-allocated
        // buffer. The structural reader's char[] path has no length field, so it
        // decodes the buffer's full *capacity* instead of its `count`, appending
        // the unused trailing slots as NUL/space padding — the `"x" + sb`
        // string-concatenation padding bug. A real String is always allocated with
        // the java/lang/String class id (see `create_java_string`), so reject any
        // object whose class is known and is not java/lang/String. Only when the
        // class is genuinely unresolvable (early bootstrap, before String itself
        // is loaded) do we fall through to the best-effort structural reader.
        {
            let cm = self.shared.classes.class_manager.read();
            if let Some(cls) = cm.get_class(class_id) {
                if &*cls.name != "java/lang/String" {
                    return None;
                }
            }
        }
        if let Some(s) = super::read_java_string(&self.shared.mem.heap, obj) {
            return Some(s);
        }
        let cm = self.shared.classes.class_manager.read();
        let vidx = resolve_field_index_in_hierarchy(class_id, "value", &cm.class_store)?;
        let cidx = resolve_field_index_in_hierarchy(class_id, "coder", &cm.class_store);
        drop(cm);
        let value_array = match self.shared.mem.heap.get_field(obj, vidx) {
            Value::Object(Some(a)) => a,
            _ => return None,
        };
        let coder = cidx
            .map(|idx| match self.shared.mem.heap.get_field(obj, idx) {
                Value::Int(c) => c,
                _ => 0,
            })
            .unwrap_or(0);
        super::vm_object::decode_java_string_value_array(&self.shared.mem.heap, value_array, coder)
    }

    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("get_class_mirror");
        super::get_or_create_class_mirror(self.shared, class_id)
    }

    /// zr z4/ab: the fallible mirror on ZGC (`CRATONVM_ZGC_INTERNAL_ALLOC_OOME`,
    /// default ON); ZGC with that switch off keeps the infallible door.
    ///
    /// gci i22/oo (`gcg-g4oo-oome-ladder-review-findings` item 4): Generational
    /// and G1 decline too, under `CRATONVM_GC_NATIVE_MIRROR_RECLAIM` (default
    /// ON, `=0` restores the infallible door). The one caller
    /// (`Object.getClass`, `native-builtins` `native_object_get_class`) then
    /// collects through `reclaim_before_alloc_retry` and asks again, as on ZGC.
    /// Before, a lazily minted mirror on a heap full of garbage raised the
    /// catchable `OutOfMemoryError` on Generational with nothing collected
    /// (G1 spent its reserve, or raised with no Free region left), where
    /// HotSpot's `getClass()` never allocates (the mirror exists from class
    /// load) and answers. The cached mirror is the same read-lock hit as
    /// before; the switch is read only on a refusal.
    fn try_get_class_mirror(&mut self, class_id: ClassId) -> Option<ObjectRef> {
        if crate::runtime::native_oom::forced_try_refusal("try_get_class_mirror") {
            return None;
        }
        if super::vm_object::zgc_internal_alloc_converted(self.shared) {
            return super::try_get_or_create_class_mirror(self.shared, class_id);
        }
        if super::vm_object::heap_is_zgc(self.shared) {
            return Some(super::get_or_create_class_mirror(self.shared, class_id));
        }
        let fallible = super::try_get_or_create_class_mirror(self.shared, class_id);
        match native_try_mirror_answer(fallible, native_mirror_reclaim_enabled) {
            NativeTryMirror::Mirror(mirror) => Some(mirror),
            NativeTryMirror::Decline => None,
            NativeTryMirror::Infallible => {
                Some(super::get_or_create_class_mirror(self.shared, class_id))
            }
        }
    }

    /// zs s7/nd6: the reserved mirror on ZGC (`zgc_internal_alloc_converted`);
    /// `Unsupported` elsewhere, so Generational / G1 define as before.
    fn try_reserve_class_mirror(
        &mut self,
        pooled_name: Option<&str>,
    ) -> cratonvm_native_api::MirrorReservation {
        use cratonvm_native_api::MirrorReservation;
        if !super::vm_object::zgc_internal_alloc_converted(self.shared) {
            return MirrorReservation::Unsupported;
        }
        use super::vm_object::{note_define_mirror, DefineMirrorCensus};
        if crate::runtime::native_oom::forced_try_refusal("try_reserve_class_mirror") {
            note_define_mirror(self.shared, DefineMirrorCensus::Refused);
            return MirrorReservation::Refused;
        }
        // zt t1/nd: the census of `[GC] zgc-define-mirror:`.
        match super::vm_object::try_reserve_class_mirror(self.shared, pooled_name) {
            Some(mirror) => {
                note_define_mirror(self.shared, DefineMirrorCensus::Reserved);
                MirrorReservation::Reserved(mirror)
            }
            None => {
                note_define_mirror(self.shared, DefineMirrorCensus::Refused);
                MirrorReservation::Refused
            }
        }
    }

    fn adopt_reserved_class_mirror(
        &mut self,
        class_id: ClassId,
        reserved: ObjectRef,
    ) -> Option<ObjectRef> {
        if !super::vm_object::zgc_internal_alloc_converted(self.shared) {
            return None;
        }
        let adopted =
            super::vm_object::adopt_reserved_class_mirror(self.shared, class_id, reserved);
        if adopted.is_some_and(|m| m.as_ptr() == reserved.as_ptr()) {
            super::vm_object::note_define_mirror(
                self.shared,
                super::vm_object::DefineMirrorCensus::Adopted,
            );
        }
        adopted
    }

    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        crate::runtime::native_oom::forced_door_refusal("alloc_object");
        // JIT round 18 wave 10 (lane ffmeintr): debug-build check of "an
        // allocation is not a GC point" (`native_alloc_no_gc_point`).
        let _no_gc = native_alloc_no_gc_point("alloc_object", self.shared);
        // The census, the `ClassId::new(0)` substitution and the slot-count
        // clamp live in `native_alloc_layout`, shared with `try_alloc_object`
        // (gc-common w8-b) so the two doors cannot drift. It must run before
        // anything is allocated: `native-api/tests/layout_alias_coverage.rs`.
        let layout = self.native_alloc_layout(class_id, num_fields);
        if layout.anon_cache_hit {
            let obj = self.heap_alloc_object(layout.class_id, layout.slots);
            self.note_native_object_alloc(obj);
            return obj;
        }
        if let Some(obj) = self.native_alloc_no_gc_arms(layout.class_id, layout.slots) {
            return obj;
        }
        // The TLAB arm is NOT charged here -- its bytes are the TLAB cursor's
        // advance, which both counters already read. Every other arm of this
        // method bypassed the TLAB and has to say so.
        let obj = self.heap_alloc_object(layout.class_id, layout.slots);
        self.note_native_object_alloc(obj);
        obj
    }

    /// gc-common w8-b (`handoff-w7b-native-fallible-object-alloc-and-may-collect`):
    /// `alloc_object` with the terminal made fallible. Same layout decision
    /// and census (`native_alloc_layout`), same no-GC arms, then
    /// `try_alloc_object_full` in place of `heap_alloc_object`. So it never
    /// collects and never unwinds: `None` is the heap's refusal, handed to a
    /// native that may reclaim and retry once it has pinned everything it
    /// holds.
    fn try_alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let _no_gc = native_alloc_no_gc_point("try_alloc_object", self.shared);
        let layout = self.native_alloc_layout(class_id, num_fields);
        // The forced-refusal test knob (zs s3/nd3) refuses AFTER the shared
        // layout helper, so this door has no exit ahead of the layout-alias
        // census (`native-api/tests/layout_alias_coverage.rs`
        // `every_vm_object_door_runs_the_shared_layout_first`; JIT round 22
        // wave 6, lane review). The helper allocates no heap object.
        if crate::runtime::native_oom::forced_try_refusal("try_alloc_object") {
            return None;
        }
        if let Some(obj) = self.native_alloc_no_gc_arms(layout.class_id, layout.slots) {
            crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
                self.shared,
                self.thread,
            );
            return Some(obj);
        }
        let obj = self
            .shared
            .mem
            .heap
            .try_alloc_object_full(layout.class_id, layout.slots)?;
        self.note_native_object_alloc(obj);
        // gcf f3/o: a graced retry that allocated is counted.
        crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
            self.shared,
            self.thread,
        );
        Some(obj)
    }

    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        self.shared.mem.heap.get_header(obj).num_slots() as usize
    }

    /// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`):
    /// the allocator's own footprint. Objects go through
    /// `native_object_footprint` (compact body when `GC_FLAG_COMPACT`, legacy
    /// otherwise, clamped to the legacy size); arrays through
    /// `cratonvm_gc::gc::object_total_size`, which answers 0 for an
    /// implausible header, reported here as `None`.
    fn heap_object_footprint(&self, obj: ObjectRef) -> Option<usize> {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        match self.shared.mem.heap.kind_of(obj) {
            ObjectKind::Object => Some(self.native_object_footprint(obj)),
            ObjectKind::Array => {
                let total =
                    cratonvm_gc::gc::object_total_size(self.shared.mem.heap.get_header(obj));
                (total >= cratonvm_gc::heap::HEADER_SIZE).then_some(total)
            }
            ObjectKind::HumongousFiller => None,
        }
    }

    fn heap_allocated_bytes(&self) -> usize {
        // `live_bytes_estimate`, NOT `allocated_bytes`. The two differ by the
        // young free list, which is reusable space the arena hands straight
        // back out — see the trait doc for what reporting the cursor instead
        // cost. The other collectors' `live_bytes_estimate` falls through to
        // `allocated_bytes`, so this is a no-op for them.
        self.shared.mem.heap.live_bytes_estimate()
    }

    fn current_thread_allocated_bytes(&self) -> Option<u64> {
        Some(self.thread.tlab.thread_allocated_bytes())
    }

    fn total_allocated_bytes(&self) -> Option<u64> {
        // Every thread's RETIRED total, plus this thread's live TLAB span. A
        // peer's in-flight cursor may not be read while its owner runs, so the
        // under-count is bounded by one TLAB per running thread — and the value
        // stays monotonic, which the occupancy gauge this replaced was not.
        //
        // The LIVE SPAN, not `thread_allocated_bytes()`, which is that span
        // PLUS this thread's whole running total. That total is already inside
        // the published total: every retire and every
        // `note_external_allocation` feeds both. Adding it again made this
        // counter report exactly 2x of the truth on Generational and G1 (and
        // 3x under ZGC, whose heap-staging TLAB layer was crediting the total
        // a second time — see `TlabAccounting`). Hibernate's
        // `HqlParserMemoryUsageTest` reads this counter and budgets 256 MiB
        // against it, so the doubling alone turned a passing parse into a FAIL.
        //
        // gc-common w6-c: the published total is THIS VM's
        // (`HeapRealm::thread_allocated_total`), not the former process static
        // `tlab::PROCESS_ALLOCATED_BYTES`, so in an embedding VM A's answer
        // no longer grows while A is idle and VM B allocates. The calling
        // thread's term is `vm_thread_unpublished_bytes()`: the live span, as
        // before, plus the settled carry the VM total has not received yet
        // (non-TLAB bytes are published in 64 KiB batches, not per object,
        // and a buffer that was never attached publishes at its first
        // attach). So the calling thread's own allocation is fully counted;
        // another thread's in-flight bytes were never visible, and the batch
        // adds at most 64 KiB to that per-thread bound.
        Some(
            self.shared
                .mem
                .thread_allocated_total
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_add(self.thread.tlab.vm_thread_unpublished_bytes()),
        )
    }

    fn committed_heap_bytes(&self) -> usize {
        self.shared.mem.heap.committed_bytes()
    }

    // -- WP0.2 ObjectStreamClass cache --

    fn osc_cache_get(&self, class_id: ClassId) -> Option<ObjectRef> {
        self.shared.classes.osc_cache.get(class_id)
    }

    fn osc_cache_put(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        self.shared
            .classes
            .osc_cache
            .insert_if_absent(class_id, desc)
    }

    // -- JIT round 16 (lane mh, MH3-2): bound `MethodType` table --

    fn bound_method_type_get(&self, desc: &str) -> Option<ObjectRef> {
        self.shared.classes.bound_method_types.get(desc)
    }

    fn bound_method_type_put(&self, desc: &str, mt: ObjectRef) -> ObjectRef {
        self.shared
            .classes
            .bound_method_types
            .insert_if_absent(desc, mt)
    }

    fn method_handle_linker_get(&self, key: &str) -> Option<ObjectRef> {
        self.shared.classes.bound_method_types.get(key)
    }

    fn method_handle_linker_put(&self, key: &str, pair: ObjectRef) -> ObjectRef {
        self.shared
            .classes
            .bound_method_types
            .insert_if_absent(key, pair)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        let obj = self.shared.mem.heap.load_and_forward(obj);
        // T10.9.E вЂ” descriptor-aware volatile read.
        let class_id = self.shared.mem.heap.class_id_of(obj);
        match resolve_field_descriptor_byte_cached(self.shared, class_id, index) {
            Some(desc) => self.shared.mem.heap.get_field_volatile_as(obj, index, desc),
            None => self.shared.mem.heap.get_field_volatile(obj, index),
        }
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, true);
        let obj = self.shared.mem.heap.load_and_forward(obj);
        let value = forward_boundary_value(&self.shared.mem.heap, value);
        if crate::runtime::env_cache::dbg_loader_trace() {
            if let Value::Object(Some(o)) = value {
                let new_cid = self.shared.mem.heap.class_id_of(o);
                let cn = self
                    .shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(new_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                if cn.contains("RootReference") {
                    eprintln!(
                        "[LOADER-TRACE] set_field_volatile holder_obj={:p} slot={} new_obj={:p} new_class={} new_cid={}",
                        obj.as_ptr(), index, o.as_ptr(), cn, new_cid.as_u32()
                    );
                }
            }
        }
        // T10.9.E вЂ” descriptor-aware volatile write.
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let desc = resolve_field_descriptor_byte_cached(self.shared, class_id, index);
        publish_overwritten_field(
            &self.shared.mem.heap,
            crate::runtime::env_cache::native_store_satb,
            obj,
            index,
            desc,
        );
        match desc {
            Some(desc) => self
                .shared
                .mem
                .heap
                .set_field_volatile_as(obj, index, value, desc),
            None => self.shared.mem.heap.set_field_volatile(obj, index, value),
        }
        // write_barrier fires automatically inside set_field_volatile в†’ set_field
    }

    /// Hardware `fetch_add` on an `int` field — the override the trait's
    /// default impl has always asked for ("the VM override should map this to
    /// a single `LOCK XADD`").
    ///
    /// This is not only a speed-up over the CAS retry loop: it is what puts the
    /// native path in the SAME atomicity domain as the JIT's ATOMIC_INT
    /// intrinsic. While this was the default lock-based loop, a compiled
    /// `LOCK XADD` and an interpreted increment on one counter lost updates
    /// (24,908 of 600,000 in `MixAtom`), which corrupted
    /// `LinkedBlockingQueue`'s element count and hung
    /// `DefaultPromiseTest.testListenerNotifyOrder`.
    fn atomic_fetch_add_int(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i32,
    ) -> Result<i32, MethodCallFailed> {
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, true);
        let obj = self.shared.mem.heap.load_and_forward(obj);
        if let Some(addr) = hw_atomic_addr(self.shared, obj, index, HwAtomicKind::Int) {
            // SAFETY: `addr` is a live, forwarded, aligned `int` payload in a
            // correctly tagged cell (see `hw_atomic_addr`); this single atomic
            // cannot reach a safepoint, so the object cannot move under it.
            let cell = unsafe { hw_atomic_i32(addr) };
            return Ok(cell.fetch_add(delta, std::sync::atomic::Ordering::SeqCst));
        }
        // Fall back to the trait's CAS retry loop for anything the hardware
        // path refused (non-`int` slot, unresolvable layout, misalignment).
        loop {
            let current = self.get_field_volatile(obj, index);
            let old = match current {
                Value::Int(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_int: field {} on object is not Int: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Int(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    /// Hardware `fetch_add` on a `long` field — the `AtomicLong` analogue of
    /// [`Self::atomic_fetch_add_int`], and unified for the same reason: a
    /// future 64-bit atomic intrinsic must be in the same domain as this.
    fn atomic_fetch_add_long(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i64,
    ) -> Result<i64, MethodCallFailed> {
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, true);
        let obj = self.shared.mem.heap.load_and_forward(obj);
        if let Some(addr) = hw_atomic_addr(self.shared, obj, index, HwAtomicKind::Long) {
            // SAFETY: see `atomic_fetch_add_int`.
            let cell = unsafe { hw_atomic_i64(addr) };
            return Ok(cell.fetch_add(delta, std::sync::atomic::Ordering::SeqCst));
        }
        loop {
            let current = self.get_field_volatile(obj, index);
            let old = match current {
                Value::Long(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_long: field {} on object is not Long: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Long(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool {
        // Body lives in `compare_and_swap_field_shared` so the JIT funnel's
        // CAS fast path reuses it verbatim. See that function.
        crate::runtime::frozen_pages::note_slot_write(self.shared, obj, index, true);
        compare_and_swap_field_shared(self.shared, obj, index, expected, new_val)
    }

    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        crate::runtime::native_oom::forced_door_refusal("allocate_instance");
        let class_id = self
            .shared
            .classes
            .class_manager_write_settling()
            .load_class(class_name)
            .ok()?;
        let num_fields = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        // Fallible inside a native callback: exhaustion unwinds to a catchable
        // OutOfMemoryError exactly as `NativeContext::alloc_object` does
        // (`heap_alloc_object`, `crate::runtime::native_oom`). It was the
        // infallible `VmHeap::alloc_object`, which on G1 `abort()`s the
        // process (gc-common w2-d,
        // `common-f-g1-fatal-abort-from-infallible-alloc-object`). Exhaustion
        // must NOT be answered with `None`: the callers read `None` as "this
        // class cannot be instantiated" and would throw the wrong exception.
        let obj = self.heap_alloc_object(class_id, num_fields);
        // Defaults only. No finalizer registration: `Unsafe.allocateInstance`
        // runs no constructor, and HotSpot registers a finalizable instance
        // when its `Object.<init>` returns, so it does not register this one
        // either (see `init_new_instance`; a constructor `MethodHandle`
        // registers at its `linkToSpecial` of `<init>`). JIT round 21 wave 8,
        // lane reflect: a `java.lang.ref` subclass IS noted for discovery
        // (`init_no_constructor_instance`, `CRATONVM_NATIVE_ALLOC_NOTES_REFERENCES`).
        crate::runtime::interpreter::init_no_constructor_instance(self.shared, obj, class_id);
        // Charged like every other native object allocation; it never was.
        self.note_native_object_alloc(obj);
        Some(obj)
    }

    /// Round 15 wave 4, lane jdkonly6 (JO5-1): see the trait. By the exact
    /// `class_id` (never re-resolved by name, which collapses to the first
    /// same-named definer), initialized first
    /// (`native_alloc_initializes_class_first`).
    fn instantiate(
        &mut self,
        class_id: ClassId,
        construct: cratonvm_native_api::registry::NativeConstruct,
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        crate::runtime::native_oom::forced_door_refusal("instantiate");
        use cratonvm_native_api::registry::NativeConstruct;
        let (num_fields, ancestor) = {
            let cm = self.shared.classes.class_manager.read();
            let Some(num_fields) = cm.get_class(class_id).map(|c| c.num_total_fields) else {
                return Ok(None);
            };
            let ancestor = match construct {
                NativeConstruct::NoConstructor => None,
                NativeConstruct::AncestorInit(ancestor) => match cm.get_class(ancestor) {
                    Some(c) => Some((ancestor, c.name.to_string())),
                    None => return Ok(None),
                },
            };
            (num_fields, ancestor)
        };
        native_alloc_initializes_class_first(self.shared, self.thread, class_id)?;
        let obj = self.heap_alloc_object(class_id, num_fields);
        let Some((ancestor, ancestor_name)) = ancestor else {
            // No constructor runs: defaults only, not registered (see
            // `allocate_instance`).
            crate::runtime::interpreter::init_no_constructor_instance(self.shared, obj, class_id);
            self.note_native_object_alloc(obj);
            return Ok(Some(obj));
        };
        // `Object.<init>` runs below: defaults + JLS §12.6 registration, at
        // the point the constructor-running natives register
        // (`init_new_instance`).
        crate::runtime::interpreter::init_new_instance(self.shared, obj, class_id);
        self.note_native_object_alloc(obj);
        let pin_base = self.thread.native_pin_roots.len();
        self.thread.native_pin_roots.push(obj);
        let init = self.invoke_special_by_class_id(
            ancestor,
            &ancestor_name,
            "<init>",
            "()V",
            &[Value::Object(Some(obj))],
        );
        let obj = self
            .thread
            .native_pin_roots
            .get(pin_base)
            .copied()
            .unwrap_or(obj);
        self.thread.native_pin_roots.truncate(pin_base);
        init?;
        Ok(Some(obj))
    }

    fn native_doors_collect_on_refusal(&self) -> bool {
        // zr z8/nr, gci i48/zl2: every collector -- Generational, G1 and ZGC
        // -- despite the switch's name (kept). HotSpot collects before a heap
        // `OutOfMemoryError` from `Unsafe.allocateInstance`,
        // `Thread.getThreads` and `dumpThreads` on every collector. The ladder
        // these natives then take (`retry_after_reclaim`: the staged young
        // door or the whole door, ZGC's owed soft rung, G1's fragmented
        // humongous step) is the one `Arrays.copyOf` and the other reclaiming
        // natives already run on Generational and G1, from the same state: a
        // Java thread inside a builtin native, outside the JNI native band,
        // that can stop for the pause. Each caller holds no unpinned reference
        // across it. `=0` keeps the one-attempt doors on all three.
        // zt t3/nd (zs3rv2 §3): memoised in a `MemoSlot` (one relaxed load;
        // a test's thread override resets it).
        static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
        match SLOT.load() {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                SLOT.publish(if on { 2 } else { 1 });
                on
            }
        }
    }

    fn heap_is_zgc(&self) -> bool {
        // zt t1/nd: the FFM memo `ADDRESS` return's ZGC kill switch.
        super::vm_object::heap_is_zgc(self.shared)
    }

    fn zgc_concurrent_mark_active(&self) -> bool {
        // zt t27/zc10: the inherited-`ThreadLocal` take's shadow rows
        // (`CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`). `false` off ZGC.
        self.shared.mem.heap.zgc_concurrent_mark_active()
    }

    fn zgc_soft_max_heap_flag(&self) -> Option<(u64, u64, u8)> {
        // zt t6/tc: the manageable `SoftMaxHeapSize` JMX row (ZGC only).
        // zt t7/tc2: value and bound in `-Xmx` terms, as HotSpot's flag
        // (`MaxHeapSize` by default: 1073741824 under `-Xmx1g`, row SM1);
        // the arena's capacity is not required to equal `-Xmx` byte for byte
        // (the `gc,init` line clamps the same way).
        let (soft, cap, origin) = self.shared.mem.heap.zgc_soft_max_flag()?;
        let xmx = u64::try_from(self.shared.config.max_heap_size).unwrap_or(u64::MAX);
        let max = if xmx == 0 { cap } else { xmx };
        // The ergonomic default IS `MaxHeapSize` on HotSpot -- or, zt t28/sm
        // (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`), the 90 % the launcher decided
        // when the command line sized neither (row `sm28_zgc_ergo`).
        let value = match (origin, self.shared.config.zgc_ergonomic_soft_max_heap_size) {
            (0, Some(ergonomic)) => u64::try_from(ergonomic).unwrap_or(u64::MAX).min(max),
            (0, None) => max,
            _ => soft.min(max),
        };
        Some((value, max, origin))
    }

    fn zgc_set_soft_max_heap_flag(&mut self, bytes: u64) -> bool {
        // zt t6/tc: `setVMOption("SoftMaxHeapSize", ..)` (ZGC only).
        self.shared.mem.heap.zgc_set_soft_max_by_management(bytes)
    }

    fn zgc_stop_director_tick_before_exit(&mut self) {
        // zt t12/tg5 (`zgc-zt11tg4-*` item 4): the exit natives' arm of the
        // launcher's `stop_zgc_director_tick_and_wait`. The stop is
        // idempotent (the pre-exit hook stops again before the `gc,exit`
        // summary). The wait runs in a VM-wait region on THIS thread, so the
        // in-flight tick pause, which waits for every counted mutator, does
        // not wait for the exiting thread; bounded by
        // `wait_for_zgc_tick_pause`. A no-op off ZGC and without a tick (no
        // pause is ever in flight there).
        use crate::runtime::interpreter as interp;
        interp::stop_zgc_director_tick(self.shared);
        // zt t31/x10: G1's periodic service stops with it.
        interp::stop_g1_periodic_gc_service(self.shared);
        if interp::zgc_tick_stop_waits_enabled() && interp::zgc_tick_pause_in_flight(self.shared) {
            self.begin_vm_wait_region();
            let _ = interp::wait_for_zgc_tick_pause(self.shared);
            self.end_blocking_region();
        }
    }

    fn try_instantiate_no_constructor(
        &mut self,
        class_id: ClassId,
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        // zr z8/nr: `instantiate(.., NoConstructor)` with the fallible
        // allocation (no `native_oom::raise`, no infallible arm): the caller
        // reclaims and retries on `Ok(None)`.
        let Some(num_fields) = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
        else {
            return Ok(None);
        };
        native_alloc_initializes_class_first(self.shared, self.thread, class_id)?;
        let Some(obj) = self
            .shared
            .mem
            .heap
            .try_alloc_object_full(class_id, num_fields)
        else {
            return Ok(None);
        };
        crate::runtime::interpreter::init_no_constructor_instance(self.shared, obj, class_id);
        self.note_native_object_alloc(obj);
        crate::runtime::interpreter::gc_and_alloc::note_native_alloc_succeeded(
            self.shared,
            self.thread,
        );
        Ok(Some(obj))
    }

    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    ) {
        use cratonvm_gc::ReferenceType;
        let reference_obj = self.shared.mem.heap.load_and_forward(reference_obj);
        let referent = self.shared.mem.heap.load_and_forward(referent);
        let queue = queue.map(|q| self.shared.mem.heap.load_and_forward(q));
        let ref_addr = reference_obj.as_ptr() as usize;
        let referent_addr = referent.as_ptr() as usize;
        let queue_addr = queue.map(|q| q.as_ptr() as usize);
        // `CRATONVM_DBG_REFDISC=1` — name the CLASS of every reference the
        // processor is told about, not just its numeric type tag. The tag
        // cannot distinguish an ordinary `PhantomReference` from a
        // `jdk.internal.ref.Cleaner`, because a `Cleaner` IS a phantom: its
        // `super(referent, dummyQueue)` runs the `PhantomReference.<init>`
        // native and arrives here as `2`. This is the instrument that answered
        // `direct-bytebuffers-are-never-reclaimed-20260805.md`'s open question
        // ("find where a real-JDK `jdk.internal.ref.Cleaner` is actually
        // discovered"), and it is placed BEFORE the type-4 branch below so it
        // reports whichever wire value a given build's discovery site chose.
        if crate::runtime::interpreter::dbg_refdisc_enabled() {
            let cn = self
                .class_name_of_id(self.shared.mem.heap.class_id_of(reference_obj))
                .unwrap_or_else(|| "<unknown>".to_string());
            eprintln!(
                "[refdisc] wire={ref_type} class={cn} ref=0x{ref_addr:x} referent=0x{referent_addr:x} queue={queue_addr:x?}"
            );
        }
        // 4 = `jdk.internal.ref.Cleaner`: a phantom that RUNS instead of being
        // enqueued. Deliberately not `ReferenceType::Cleaner` — that variant is
        // the synthetic `Cleaner$Cleanable` shape, whose slot 0 is its action
        // rather than a referent, so it must never reach the pre-GC
        // referent-nulling pass. See `ReferenceEntry::runs_cleaner`.
        // THE IDENTITY STAMP (see `ReferenceProcessor::identity_stamps`).
        // Minted here because it needs the heap: `identity_hash_code` installs
        // a value from a monotonic counter into the object's own mark word when
        // it has none and returns the existing one otherwise. It travels with
        // the object across a relocation, so it identifies the OBJECT rather
        // than its address -- which is what every guard downstream of this
        // registry has been approximating with a class-shape test.
        let stamp = self.shared.mem.heap.identity_hash_code(reference_obj);
        if ref_type == 4 {
            let mut rp = self.shared.mem.ref_processor.lock();
            rp.discover_phantom_cleaner(ref_addr, referent_addr, queue_addr);
            rp.stamp_reference(ref_addr, stamp);
            return;
        }
        let rt = match ref_type {
            0 => ReferenceType::Weak,
            1 => ReferenceType::Soft,
            2 => ReferenceType::Phantom,
            3 => ReferenceType::Cleaner,
            _ => return,
        };
        let mut rp = self.shared.mem.ref_processor.lock();
        rp.discover_reference(rt, ref_addr, referent_addr, queue_addr);
        rp.stamp_reference(ref_addr, stamp);
    }

    /// Build the genuine Reference Handler's discovered chain without a
    /// GC-capable call between taking the rooted batch and returning its head.
    fn take_reference_pending_list(&mut self) -> Option<ObjectRef> {
        let slot = {
            let cm = self.shared.classes.class_manager.read();
            let reference = cm.find_bootstrap_class_by_name("java/lang/ref/Reference")?;
            resolve_field_index_in_hierarchy(reference, "discovered", cm.class_store())?
        };
        let pairs = self.shared.mem.cleaner_thread.drain_reference_enqueues();
        let pin_base = self.thread.native_pin_roots.len();
        for &(address, _) in &pairs {
            // SAFETY: pending batch roots, no intervening GC-capable call.
            self.thread.native_pin_roots.push(unsafe {
                ObjectRef::from_raw(address as *mut u8)
            });
        }
        let mut head = None;
        for index in pin_base..self.thread.native_pin_roots.len() {
            let reference = self.thread.native_pin_roots[index];
            self.shared.mem.heap.set_field(reference, slot, Value::Object(head));
            head = Some(reference);
        }
        self.thread.native_pin_roots.truncate(pin_base);
        head
    }

    fn has_reference_pending_list(&self) -> bool {
        self.shared.mem.cleaner_thread.has_reference_enqueues()
    }

    /// PGJDBC-PHANTOM-GHOST (2026-08-07): see the trait doc and
    /// `ReferenceProcessor::mark_manually_enqueued`. Retires the registry
    /// entry for a `Reference` the application just enqueued itself via
    /// `Reference.enqueue()`, so the GC's own weak/phantom processing never
    /// rediscovers and re-delivers it once its (possibly still-shared)
    /// referent later dies for real.
    ///
    /// gc-common w2-f (2026-09-23): ALSO reached from `Reference.clear()` /
    /// `clear0()` (`native-builtins/src/reference.rs::retire_cleared_reference`)
    /// for a reference that still names a live queue — an application clear
    /// settles the row exactly as an application enqueue does. The registry
    /// side is `ReferenceProcessor::retire_by_application`, O(1).
    fn mark_reference_manually_enqueued(&mut self, reference_obj: ObjectRef) {
        let reference_obj = self.shared.mem.heap.load_and_forward(reference_obj);
        let ref_addr = reference_obj.as_ptr() as usize;
        // t1 wave 6, lane G6 (`CRATONVM_REF_NOTES_PER_THREAD`, default ON):
        // a reference cleared while its allocation note still waits in this
        // thread's stripe has the note removed, without the processor's
        // mutex. Same verdict as the locked retire, which finds no row for an
        // unabsorbed note and leaves the bind to drop it (null referent):
        // `AllocationNoteStripes`, "The application retire".
        if self.cancel_reference_allocation_note(reference_obj) {
            return;
        }
        self.shared
            .mem
            .ref_processor
            .lock()
            .mark_manually_enqueued(ref_addr);
    }

    /// t1 wave 7, lane G6: the queue-less retire (`NULL_QUEUE`, `ENQUEUED`,
    /// no queue) and the first step of [`Self::mark_reference_manually_enqueued`]:
    /// drop the reference's allocation note from this thread's stripe when it
    /// still waits there. Never the processor's lock; `false` without stripes.
    fn cancel_reference_allocation_note(&mut self, reference_obj: ObjectRef) -> bool {
        let reference_obj = self.shared.mem.heap.load_and_forward(reference_obj);
        match self.shared.mem.ref_discovery.note_stripes() {
            Some(stripes) => stripes.cancel_recent(reference_obj.as_ptr() as usize),
            None => false,
        }
    }

    /// Round-5 fix (HIGH): wire native `Reference.get()` into the
    /// reference processor's SoftReference LRU so cached referents stay
    /// alive across major GCs proportional to how recently the
    /// application touched them. The cost is one `SystemTime::now()`
    /// plus a short linear scan / `BTreeMap` re-key — the same overhead
    /// HotSpot pays on every soft-ref `get()` call.
    ///
    /// gc-common w6-d (2026-09-24): a `get()` whose touch would change nothing
    /// no longer takes the processor's lock. Every locked touch stamps the
    /// object's own `SoftReference.timestamp` field
    /// (`cratonvm_gc::reference::soft_touch_stamp`: the millisecond, plus the
    /// processor's soft index epoch); a later `get()` of the same object in
    /// the same millisecond, with no collection having moved or pruned
    /// anything since, finds its stamp there and returns. That is exactly the
    /// case in which the locked touch is a no-op, so the registry, its LRU
    /// index and its clock end up identical (the equivalence argument is on
    /// `soft_touch_stamp`, the model test is
    /// `w6d_stamped_soft_touch_is_equivalent_to_the_locked_touch`). A hot soft
    /// cache read from N threads now takes the mutex at most once per
    /// reference per millisecond, instead of once per `get()`.
    /// `docs/feature-designs/real-jdk-mode.md`.
    ///
    /// Only a receiver of a real `java.lang.ref.SoftReference` layout (a
    /// `long timestamp` declared by `SoftReference` itself) is stamped; any
    /// other shape keeps the locked touch on every call, as before. The field
    /// is private to `SoftReference` and is read by nothing else here: the
    /// `get()` bytecode that would compare it with `SoftReference.clock` is
    /// replaced by this `Bridge`, so program output is unchanged.
    fn touch_soft_reference(&mut self, reference_obj: ObjectRef) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let ref_addr = reference_obj.as_ptr() as usize;
        let shared: &SharedVm = self.shared;
        let heap = &shared.mem.heap;
        // The object the field ops address (`get_field` forwards the same way).
        let (obj, obj_valid) = heap.load_and_forward_checked(reference_obj);
        let vm = shared.vm_identity;
        // The memo's stamp. Under `CRATONVM_INTERP_CLASS_MEMO_NARROW_EPOCH`
        // (interpreter round i4 wave 95, lane L7) a memo that found the layout
        // is stamped with [`class_id_memo_epoch`]: its answers then read only
        // stored classes by id (`SoftReference`'s own fields, a receiver
        // class's superclass chain). One that found none keeps
        // `class_origin_epoch`: that `None` may be a name -> class answer
        // (`SoftReference` not defined yet) which a definition changes.
        // Switch off, the two are the same load.
        let narrow_epoch = class_id_memo_epoch(shared);
        let wide_epoch = cratonvm_classloading::class_origin_epoch();
        let stamp_for = |layout: &Option<(ClassId, usize)>| {
            if layout.is_some() {
                narrow_epoch
            } else {
                wide_epoch
            }
        };
        // Fast path: `Some((layout, redundant))` on a memo hit, `None` on a miss.
        let memo_hit: Option<(Option<(ClassId, usize)>, bool)> = SOFT_TOUCH_MEMO.with(|cell| {
            let memo = cell.borrow();
            let m = memo.as_ref()?;
            if m.vm_identity != vm || m.class_epoch != stamp_for(&m.layout) {
                return None;
            }
            // The bounds test is memory safety only: a stamp can match solely
            // in an object the locked path below proved a `SoftReference`.
            let redundant = match (m.layout, obj_valid) {
                (Some((_, slot)), true) if slot < heap.num_fields(obj) => {
                    // gcj j4/rf: the epoch first, then the clock (a clock read
                    // newer than the epoch only fails the match).
                    let epoch = m.epoch.load(std::sync::atomic::Ordering::Acquire);
                    let key = cratonvm_gc::reference::soft_touch_key_ms(
                        now_ms,
                        m.clock.load(std::sync::atomic::Ordering::Acquire),
                    );
                    cratonvm_gc::reference::soft_touch_stamp(key, epoch)
                        .is_some_and(|stamp| soft_timestamp_field(shared, obj, slot) == Some(stamp))
                }
                _ => false,
            };
            if redundant && m.observed.load(std::sync::atomic::Ordering::Relaxed) < now_ms {
                // gcj j4/rf: keyed on the clock, the skipped touch's only
                // effect -- the observed mutator clock -- still lands.
                m.observed
                    .fetch_max(now_ms, std::sync::atomic::Ordering::AcqRel);
            }
            Some((m.layout, redundant))
        });
        if let Some((_, true)) = memo_hit {
            return;
        }
        let memo_missed = memo_hit.is_none();
        let layout = match memo_hit {
            Some((layout, _)) => layout,
            None => soft_reference_timestamp_layout(shared),
        };
        // Stamp only a receiver that is provably a `SoftReference` with room
        // for the slot. The class question takes the ClassManager (L10), so it
        // is asked BEFORE the processor lock (L7), per the lock order.
        // The last class this thread proved a `SoftReference` is remembered,
        // so a cache of one reference class asks the ClassManager once.
        let mut verified_cid: Option<ClassId> = None;
        let stamp_slot = match layout {
            Some((soft_cid, slot))
                if obj_valid
                    && heap.kind_of(obj) != ObjectKind::Array
                    && slot < heap.num_fields(obj) =>
            {
                let cid = heap.class_id_of(obj);
                let known = SOFT_TOUCH_MEMO.with(|cell| {
                    cell.borrow().as_ref().is_some_and(|m| {
                        m.vm_identity == vm
                            && m.class_epoch == stamp_for(&m.layout)
                            && m.verified_cid == Some(cid)
                    })
                });
                let is_soft = known
                    || shared
                        .classes
                        .class_manager
                        .read()
                        .is_subclass_of(cid, soft_cid);
                if is_soft {
                    verified_cid = Some(cid);
                    Some(slot)
                } else {
                    None
                }
            }
            _ => None,
        };
        let mut rp = shared.mem.ref_processor.lock();
        rp.touch_soft_reference(ref_addr, now_ms);
        if let Some(slot) = stamp_slot {
            // Under the lock: the stamp must name the epoch this touch's row
            // resolution belongs to, and no other touch of this object may
            // interleave between the row update and the field write.
            // No stamp (a `0` clock): this touch still moved the row, so the
            // field must stop vouching for the previous one -- `0` is a fresh
            // object's value, which no stamp equals. Found by the model test
            // (w6 orchestrator): touch at 1, touch at 0, touch at 1 again was
            // skipped and left the row at 0.
            let stamp = rp.soft_touch_stamp_now(now_ms).unwrap_or(0);
            heap.set_field_as(obj, slot, Value::Long(stamp), b'J');
        }
        if memo_missed {
            let epoch = rp.soft_touch_epoch();
            let clock = rp.soft_touch_clock();
            let observed = rp.soft_touch_observed_ms();
            SOFT_TOUCH_MEMO.with(|cell| {
                *cell.borrow_mut() = Some(SoftTouchMemo {
                    vm_identity: vm,
                    class_epoch: stamp_for(&layout),
                    layout,
                    verified_cid,
                    epoch,
                    clock,
                    observed,
                });
            });
        } else if verified_cid.is_some() {
            SOFT_TOUCH_MEMO.with(|cell| {
                if let Some(m) = cell.borrow_mut().as_mut() {
                    m.verified_cid = verified_cid;
                }
            });
        }
        drop(rp);
    }

    /// INT-8: `Reference.get()` keep-alive — route the just-read referent
    /// through the heap's standalone SATB barrier so an active G1 mark cycle logs
    /// it as a root (the marker cannot see it through the hidden referent
    /// slot). No-op when no cycle is active; on Generational/ZGC the
    /// SATB barrier's marking-active gate keeps it equally cheap.
    fn gc_reference_keep_alive(&mut self, referent: ObjectRef) {
        // This is a keep-alive, not a reference store. `write_barrier_pre`
        // must pair with a post-store barrier; using the standalone SATB
        // enqueue avoids leaving that debug triad armed.
        //
        // zt t23/zb7: `referent_keep_alive` -- under the ZGC color flip the
        // load barrier's mark step, otherwise this same SATB enqueue.
        self.shared
            .mem
            .heap
            .referent_keep_alive(Value::Object(Some(referent)));
    }

    /// zt t23/zb8 (`CRATONVM_ZGC_REFERS_TO_NO_KEEPALIVE`, default ON, `=0`
    /// kill): the `refersTo` read. While ZGC's read barrier is armed
    /// (`CRATONVM_ZGC_LOAD_BARRIER=colored`) the strong read
    /// ([`Self::get_field`]) of a bad colored word MARKS its target during a
    /// mark (`load_barrier_slot` with `ZBarrierKind::Load`; under
    /// `CRATONVM_ZGC_COLOR_FLIP=1` every `Remapped` word is bad), so a
    /// `refersTo` on a slot nobody else loaded kept the referent alive for
    /// that cycle; HotSpot's `refersTo0` loads it `AS_NO_KEEPALIVE`. Here the
    /// slot goes through `VmHeap::get_field_weak` (ZGC's `Weak` load: forwards,
    /// never `mark_live`). Unarmed -- every default run, every other
    /// collector -- the weak read IS the strong read, so the arm costs one
    /// relaxed load there and the flag is never asked.
    fn get_field_no_keepalive(&self, obj: ObjectRef, index: usize) -> Value {
        if !self.shared.mem.heap.zgc_load_barrier_armed()
        {
            return self.get_field(obj, index);
        }
        let (obj, obj_validated) = self.shared.mem.heap.load_and_forward_checked(obj);
        if !obj_validated {
            return self.get_field(obj, index);
        }
        // zt t32 (2b): `refersTo` compares against the blocked answer too.
        let read = self.shared.mem.heap.get_field_weak(obj, index);
        crate::memory::ref_discovery::gate_referent_read(self.shared, obj, index, read, || {
            self.shared.mem.heap.get_field_weak(obj, index)
        })
    }

    fn gc_referent_read_keep_alive(&mut self, receiver: ObjectRef, slot: usize, value: Value) {
        if let Value::Object(Some(referent)) = value {
            if self.shared.mem.heap.is_generational() {
                gen_referent_read_keep_alive(self.shared, receiver, slot, referent);
            } else {
                crate::memory::ref_discovery::keep_alive_referent_read(
                    self.shared,
                    receiver,
                    slot,
                    value,
                );
            }
        }
    }

    fn gc_referent_slot_read_keep_alive(
        &mut self,
        receiver: ObjectRef,
        slot: usize,
        referent: ObjectRef,
    ) {
        // gcj j1/rf: the Generational half (G1 / ZGC: nothing).
        gen_referent_read_keep_alive(self.shared, receiver, slot, referent);
    }
}

