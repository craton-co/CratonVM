#[allow(unused_imports)]
use super::*;

/// [`jit_integer_value_of_direct_body`] on a thread the caller already HOLDS
/// (`given`), JIT round 23 wave 3b, lane L2: the compiled lambda arm boxes an
/// adapted site's `int` return here (`lambda_site_adapt_return`,
/// `CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX`) instead of through the native funnel,
/// and it already holds the JIT thread -- a second `jit_thread_mut` borrow from
/// inside it would be a sibling borrow. `None` takes the thread as before. The
/// semantics are the helper's: the box cache (`-128..=IntegerCache.high`)
/// answers its own objects, a value past it is a fresh TLAB object, and a cold
/// slot or an unresolved bound takes the canonical native.
pub unsafe fn jit_integer_value_of_direct_body_on(
    vm_ptr: i64,
    value: i64,
    mut given: Option<&mut JvmThread>,
) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as `jit_invoke_dispatch`: the
    // per-thread conservative-scan cache must be invalidated, and a callee
    // that allocates may enter the GC barrier, so the SATB queue is flushed.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_box_helper_entry_flush(vm_ptr, given.is_some());
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let value = value as i32;
    // Outside the box cache, where `valueOf` must return a FRESH object. The
    // cache's upper end is `-Djava.lang.Integer.IntegerCache.high`, not 127:
    // this helper used to allocate for everything past 127, so with `high=1000`
    // `Integer.valueOf(500) == Integer.valueOf(500)` was true interpreted and
    // false once the caller compiled. Until the native has resolved this VM's
    // bound, every value above 127 takes the native path, which resolves it.
    let beyond_cache = value < -128
        || cratonvm_native_builtins::lang_math::integer_cache_high_for(vm.vm_identity)
            .is_some_and(|high| value > high);
    if beyond_cache {
        let vm_key = vm.vm_identity;
        let cached_class = INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
            cache
                .get()
                .filter(|(cached_vm, _)| *cached_vm == vm_key)
                .map(|(_, raw)| ClassId::new(raw))
                .filter(|class_id| !class_was_redefined(vm, *class_id))
        });
        if let Some(class_id) = cached_class {
            // Wave 3b: the caller's thread when it holds one (see `_on`).
            let mut _jit_borrow = None;
            let held: Option<&mut JvmThread> = match given.as_deref_mut() {
                Some(t) => Some(t),
                None => jit_thread_mut().map(|(t, g)| {
                    _jit_borrow = Some(g);
                    t
                }),
            };
            if let Some(thread) = held {
                // Direct TLAB bump — the SAME allocation function
                // `NativeContextImpl::alloc_object`'s TLAB arm uses (full
                // header init, fresh identity hash), minus that method's
                // per-call clamp-cache scan, alloc-pool probes and context
                // plumbing. The slot count is the class's real declared
                // field count, resolved once per (vm, class) below — so the
                // undersized-layout clamp is honored, not skipped. TLAB
                // exhaustion (or an oversized layout) falls back to the full
                // allocator with its old-gen spill/batch behavior.
                thread_local! {
                    // (vm_key, class_id, slots). The exact-class redefine gate
                    // above prevents use after that class's layout changes.
                    pub static INTEGER_ALLOC_SLOTS: std::cell::Cell<Option<(usize, u32, u32)>> =
                        const { std::cell::Cell::new(None) };
                }
                let slots = INTEGER_ALLOC_SLOTS.with(|cache| {
                    if let Some((vk, cid, slots)) = cache.get() {
                        if vk == vm_key && cid == class_id.as_u32() {
                            return slots as usize;
                        }
                    }
                    let resolved = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1);
                    // Cast: field counts are far below u32::MAX.
                    cache.set(Some((vm_key, class_id.as_u32(), resolved as u32)));
                    resolved
                });
                // The shape planner, not a bare legacy size: every TLAB object
                // site has to agree with the header its allocation will be
                // stamped with. See `plan_tlab_object_shape`.
                //
                // r9w12: memoized per `(vm_key, class_id, slots)`, because for
                // a fixed triple the planner is a pure function and this arm
                // called it on EVERY escaping box. Its
                // `compact_tlab_body_size` is a `with_current_class_layout`
                // lookup — 3.65 % of `BoxProbe box` on the 2026-09-20 dev
                // build, on top of the 14.88 % this whole helper cost.
                //
                // The key carries `class_id`, which the `class_was_redefined`
                // screen above has already rejected if this class's layout
                // moved, so a stale plan cannot be served across a
                // redefinition. It is keyed on `slots` as well because that is
                // the planner's second argument, not because it can drift
                // under a fixed class.
                //
                // `tlab_shape_planner_is_instrumented` is the one thing that
                // makes the planner impure: the shape counters and
                // `CRATONVM_DBG_COMPACT_TLAB`. While either is armed this
                // re-plans per call, so a memo never blinds an instrument. See
                // that predicate.
                thread_local! {
                    // (vm_key, class_id, slots, requested_size)
                    pub static INTEGER_ALLOC_PLAN: std::cell::Cell<
                        Option<(usize, u32, u32, u32)>,
                    > = const { std::cell::Cell::new(None) };
                }
                let planner_instrumented =
                    crate::runtime::interpreter::tlab_shape_planner_is_instrumented();
                let memo_key = (vm_key, class_id.as_u32(), slots as u32);
                let cached_plan = (!planner_instrumented)
                    .then(|| INTEGER_ALLOC_PLAN.with(|c| c.get()))
                    .flatten()
                    .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                    .map(|(_, _, _, size)| size as usize);
                let requested_size = match cached_plan {
                    Some(size) => size,
                    None => {
                        let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
                            vm,
                            class_id,
                            slots,
                            crate::runtime::interpreter::tlab_site::JIT_HELPER,
                        );
                        if !planner_instrumented {
                            if let Ok(size_u32) = u32::try_from(size) {
                                INTEGER_ALLOC_PLAN.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, size_u32)))
                                });
                            }
                        }
                        size
                    }
                };
                let tlab_object = if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
                    crate::runtime::interpreter::tlab_alloc_object(
                        thread,
                        vm,
                        class_id,
                        slots,
                        requested_size,
                    )
                } else {
                    None
                };
                if let Some(object) = tlab_object {
                    // Raw primitive-cell write: this arm JUST allocated
                    // `object` through the legacy TLAB path
                    // (`init_object_header`, zeroed 16-byte Value cells), so
                    // field 0 is the Value cell at HEADER_SIZE — the exact
                    // bytes `set_field_as(.., b'I')` would store, minus that
                    // path's per-call header read + layout dispatch. A
                    // primitive store takes no write barrier.
                    // ...but ONLY when it really is legacy. That premise was
                    // an assumption about which allocator this site calls, not
                    // a property of the object, and `CRATONVM_COMPACT_TLAB_ALLOC`
                    // falsified it: with compact planning on, `java.lang.Integer`
                    // gets a packed 4-byte `value` and this 16-byte `Value` cell
                    // overwrote it and the bytes after it. `FjpProbe` summed
                    // boxed integers and returned 215812748544 instead of
                    // 499999500000 -- no collection involved, exactly as the
                    // note on `tlab_alloc_object_inner` described. The cold arm
                    // immediately below has always used the layout-aware store
                    // and says why; this asks the object rather than assuming.
                    // SAFETY: `object` was just allocated here, so its first
                    // HEADER_SIZE bytes are a live, fully written `ObjectHeader`.
                    let header =
                        unsafe { &*(object.as_ptr() as *const cratonvm_types::ObjectHeader) };
                    let compact = cratonvm_types::is_compact_object(header);
                    if compact {
                        // r9w12: the packed `value` cell, memoized like the
                        // shape plan above and for the same reason.
                        //
                        // `set_field_as(.., 0, Int(v), b'I')` is three lookups
                        // for one 4-byte store: `coerce_field_value_for_slot`
                        // (which reads `class_id_of(obj)` through the checked
                        // heap API), then `set_field`'s SATB arm, then
                        // `set_field_no_card`'s `zgc_compact_field_storage` --
                        // a THIRD `with_current_class_layout` walk, after the
                        // planner's and the header's. On the 2026-09-20 dev
                        // build `BoxProbe box` spent 3.04 % in
                        // `coerce_field_value_for_slot`, 1.30 % in
                        // `set_field_as`, 1.54 % in `set_field_no_card` and
                        // 1.27 % in `CurrentLayoutCache::find`.
                        //
                        // A `CompactLayout` belongs to a `(class_id,
                        // field_count)` pair, not to an instance, so the
                        // `(offset, storage)` of field 0 is the same for every
                        // object this arm allocates until that class is
                        // redefined -- which the `class_was_redefined` screen
                        // at the top of this arm already refuses to serve past.
                        //
                        // Restricted to a 4-byte `Int` cell, which is what
                        // `java.lang.Integer.value` is. Anything else (a
                        // reference-typed slot 0, a narrowed width, no
                        // registered layout) declines to `set_field_as`, so the
                        // coercion, the boxing arm and the
                        // HIB-DCAST-LATEPHASE.1 guard all stay reachable for
                        // every shape that is not exactly this one.
                        thread_local! {
                            // (vm_key, class_id, slots, offset)
                            pub static INTEGER_VALUE_CELL: std::cell::Cell<
                                Option<(usize, u32, u32, u32)>,
                            > = const { std::cell::Cell::new(None) };
                        }
                        let offset = INTEGER_VALUE_CELL
                            .with(|c| c.get())
                            .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                            .map(|(_, _, _, offset)| offset as usize)
                            .or_else(|| {
                                let (offset, storage) =
                                    cratonvm_types::compact_object_field_disp(header, 0)?;
                                if storage != cratonvm_types::FieldStorageKind::Int {
                                    return None;
                                }
                                let offset_u32 = u32::try_from(offset).ok()?;
                                INTEGER_VALUE_CELL.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, offset_u32)))
                                });
                                Some(offset)
                            });
                        match offset {
                            // SAFETY: `object` is this arm's own just-allocated
                            // compact body, and `offset` is field 0's byte
                            // offset in the layout THIS object's header names,
                            // whose storage was checked to be a naturally
                            // aligned 4-byte `Int` cell. A primitive store into
                            // a fresh object takes no write barrier -- the same
                            // argument the legacy arm below makes.
                            Some(offset) => unsafe {
                                cratonvm_types::write_compact_field(
                                    object.as_ptr().add(offset),
                                    cratonvm_types::FieldStorageKind::Int,
                                    Value::Int(value),
                                    std::sync::atomic::Ordering::Relaxed,
                                )
                            },
                            None => vm.mem.heap.set_field_as(object, 0, Value::Int(value), b'I'),
                        }
                    } else {
                        // SAFETY: `object` is a live legacy-layout allocation
                        // with >= 1 slot (`slots.max(1)` above) -- now CHECKED
                        // immediately above, not assumed; the cell is
                        // exclusively ours until published below.
                        unsafe {
                            std::ptr::write(
                                object.as_ptr().add(cratonvm_gc::heap::HEADER_SIZE) as *mut Value,
                                Value::Int(value),
                            );
                        }
                    }
                    // Object-return handoff root (see `call_integer_native_raw`).
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                // zr z3/lt: fallible on ZGC, collect-and-retry, and on a
                // refusal FALL THROUGH to the native funnel below, which
                // throws the OutOfMemoryError (it used to abort the process).
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    // Descriptor-typed write (`Integer.value`, field 0, `I`) —
                    // this cold arm's allocator may pick a non-legacy layout,
                    // so keep the layout-aware store.
                    vm.mem.heap.set_field_as(object, 0, Value::Int(value), b'I');
                    // Object-return handoff root (see `call_integer_native_raw`).
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
            }
        }
    }
    // In-range fast path (round 9 wave 10, collect10; landed by wave 10b,
    // wire10b): a value this VM's box cache already holds is answered from it,
    // without the `safe_native_call` funnel -- the same object
    // `native_integer_value_of` would return (it reads the same table under
    // the same lock). A cold slot, an unresolved bound or a value past it
    // answers `None` and falls through to the native, which fills the slot and
    // discovers the wrapper class for the arm above. The cached object is
    // rooted by the cache (`gc_scan_value_of_cache_roots`), which is why the
    // native path below returns it without a pending-return handoff too.
    // MEASURED (w9b, `BoxProbe put`): `safe_native_call_impl` 18.8 % +
    // `native_integer_value_of` 17.7 % self is this path.
    if !beyond_cache {
        if let Some(object) =
            cratonvm_native_builtins::lang_math::integer_cache_peek(vm.vm_identity, value)
        {
            return object.as_ptr() as i64;
        }
    }
    // Cold / in-range / redefine-window path: canonical native callback via
    // the full safe-native-call wrapper (identity cache; also discovers the
    // real wrapper ClassId for the fast path above).
    let mut _jit_borrow = None;
    let held: Option<&mut JvmThread> = match given {
        Some(t) => Some(t),
        None => jit_thread_mut().map(|(t, g)| {
            _jit_borrow = Some(g);
            t
        }),
    };
    let Some(thread) = held else {
        // No JIT thread context — cannot safely run the native. Signal the
        // deopt sentinel; the caller's post-invoke check bails to the
        // interpreter, which re-dispatches through the normal path.
        //
        // r10 `help`: that last clause was FALSE until the flag below was
        // added. `jit_bridge`'s post-JIT path takes its `result == i64::MIN`
        // arm only `&& deopt_signaled`; with every signal clear it pushes the
        // sentinel as a real value, and this helper returns a REFERENCE, so
        // `i64::MIN` would land in an oop slot.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let arg = Value::Int(value);
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::intrinsics::integer::intrinsic_integer_value_of,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return handle_jit_dispatch_error(vm, thread, error, &INTEGER_VALUE_OF_INFO),
    };
    match result {
        Some(Value::Object(Some(object))) => {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
            object.as_ptr() as i64
        }
        _ => 0,
    }
}

/// Synthetic call-site info for [`jit_integer_int_value_direct`]'s
/// generic-dispatch fallback (invalid non-null receiver — a shape the
/// verifier rules out for a `final`-class receiver, kept for defensive
/// parity with `call_integer_native_raw`'s bail-to-dispatch behavior).
pub static INTEGER_INT_VALUE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Integer",
    method_name: "intValue",
    descriptor: "()I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokevirtual Integer.intValue()` sites
/// whose constant-pool class is exactly `java/lang/Integer` (a `final`
/// class, so the site is statically monomorphic — no receiver guard
/// needed; only `null` remains, which throws NPE per JVMS).
///
/// Mirrors the `IntegerNativeKind::IntValue` arm of
/// [`call_integer_native_raw`]: a heap-validated field-0 read that cannot
/// allocate or safepoint. Null receiver → pending-NPE + deopt sentinel
/// (the canonical implicit-NPE signal). A non-null receiver that fails
/// heap validation falls back to the full generic dispatcher, exactly like
/// the existing arm's `None` return.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_integer_int_value_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_integer_int_value_direct_body(vm_ptr, receiver);
    contain(
        "jit_integer_int_value_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_integer_int_value_direct`], run under its panic guard.
pub unsafe fn jit_integer_int_value_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let census = crate::runtime::interp_census::direct_binds_enabled();
    let raw = receiver as u64;
    if raw == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        // The arena-membership probe stays. Nothing on this path has read the
        // receiver's header yet, so this probe is the ONLY thing standing
        // between a stale/fabricated argument and the `get_field` dereference
        // below; the alignment and canonical-address tests above reject wild
        // bit patterns but not a plausible-looking non-object address. It also
        // costs ~0 here (see the hashmap-half-gap closeout doc: restoring all
        // three probes measured inside run-to-run noise).
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            if census {
                crate::runtime::interp_census::note_int_value_direct(false);
            }
            return match vm.mem.heap.get_field(object, 0) {
                Value::Int(value) => value as i64,
                _ => 0,
            };
        }
    }
    // Defensive fallback: hand the call to the generic dispatcher (same
    // machinery the non-direct site would have used).
    if census {
        crate::runtime::interp_census::note_int_value_direct(true);
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &INTEGER_INT_VALUE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_long_value_of_direct`]'s error path —
/// same role as [`INTEGER_VALUE_OF_INFO`], and read only for diagnostics and
/// exception context.
pub static LONG_VALUE_OF_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Long",
    method_name: "valueOf",
    descriptor: "(J)Ljava/lang/Long;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Synthetic call-site info for [`jit_long_long_value_direct`]'s
/// generic-dispatch fallback (a non-null receiver that fails heap validation —
/// defensive parity with [`INTEGER_INT_VALUE_INFO`]).
pub static LONG_LONG_VALUE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Long",
    method_name: "longValue",
    descriptor: "()J",
    num_jit_args: 1,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

thread_local! {
    /// Real `java/lang/Long` class discovered from the first ordinary
    /// `valueOf` result in each VM, as `(vm_identity, class id)` — the
    /// `Long` twin of [`INTEGER_WRAPPER_CLASS_CACHE`], and separate from it
    /// because the two wrappers are different classes with different layouts.
    pub static LONG_WRAPPER_CLASS_CACHE: std::cell::Cell<Option<(usize, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// Thin direct-call target for JIT `invokestatic Long.valueOf(J)` sites
/// (registered into `cratonvm_jit::LONG_VALUE_OF_DIRECT_FN` by
/// `build_helpers`; the recognition lives in `jit::try_compile`).
///
/// Line-for-line the same contract as [`jit_integer_value_of_direct`], against
/// the same registered native's semantics
/// (`lang_math.rs::native_long_value_of`, reached here through
/// `intrinsics::long::intrinsic_long_value_of`, which is a verbatim delegation
/// to it):
///
///  * out-of-cache values allocate a fresh wrapper through the mutator's normal
///    TLAB path and publish it via `native_pending_return` (the established
///    JIT→native object-return handoff root);
///  * `-128..=127`, the cold pre-discovery case, and any class-redefine window
///    route through `safe_native_call` to the canonical native callback, so the
///    JLS `Long`-cache identity contract is that native's, not a copy of it;
///  * errors (OOM) route through `handle_jit_dispatch_error` exactly like the
///    dispatch helper.
///
/// The argument arrives as a full 64-bit word — unlike the `Integer` twin
/// there is no `as i32` narrowing step, and there must not be one: the JIT
/// passes a category-2 value in one operand slot.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_long_value_of_direct(vm_ptr: i64, value: i64) -> i64 {
    let f = || jit_long_value_of_direct_body(vm_ptr, value);
    let bits = contain(
        "jit_long_value_of_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_long_value_of_direct`], run under its panic guard.
pub unsafe fn jit_long_value_of_direct_body(vm_ptr: i64, value: i64) -> i64 {
    jit_long_value_of_direct_body_on(vm_ptr, value, None)
}

/// [`jit_long_value_of_direct_body`] on a thread the caller already holds
/// (JIT round 23 wave 3b, lane L2; see [`jit_integer_value_of_direct_body_on`]).
pub unsafe fn jit_long_value_of_direct_body_on(
    vm_ptr: i64,
    value: i64,
    mut given: Option<&mut JvmThread>,
) -> i64 {
    // Same Rust<->JIT boundary bookkeeping as `jit_invoke_dispatch`.
    crate::jit::conservative_roots::note_jit_boundary();
    jit_box_helper_entry_flush(vm_ptr, given.is_some());
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if !(-128..=127).contains(&value) {
        let vm_key = vm.vm_identity;
        let cached_class = LONG_WRAPPER_CLASS_CACHE.with(|cache| {
            cache
                .get()
                .filter(|(cached_vm, _)| *cached_vm == vm_key)
                .map(|(_, raw)| ClassId::new(raw))
                .filter(|class_id| !class_was_redefined(vm, *class_id))
        });
        if let Some(class_id) = cached_class {
            // Wave 3b: the caller's thread when it holds one (see `_on`).
            let mut _jit_borrow = None;
            let held: Option<&mut JvmThread> = match given.as_deref_mut() {
                Some(t) => Some(t),
                None => jit_thread_mut().map(|(t, g)| {
                    _jit_borrow = Some(g);
                    t
                }),
            };
            if let Some(thread) = held {
                thread_local! {
                    // (vm_key, class_id, slots). The exact-class redefine gate
                    // above prevents use after that class's layout changes.
                    pub static LONG_ALLOC_SLOTS: std::cell::Cell<Option<(usize, u32, u32)>> =
                        const { std::cell::Cell::new(None) };
                }
                let slots = LONG_ALLOC_SLOTS.with(|cache| {
                    if let Some((vk, cid, slots)) = cache.get() {
                        if vk == vm_key && cid == class_id.as_u32() {
                            return slots as usize;
                        }
                    }
                    let resolved = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1);
                    // Cast: field counts are far below u32::MAX.
                    cache.set(Some((vm_key, class_id.as_u32(), resolved as u32)));
                    resolved
                });
                // The shape planner, not a bare legacy size: every TLAB object
                // site has to agree with the header its allocation will be
                // stamped with. See `plan_tlab_object_shape`.
                //
                // Memoized per `(vm_key, class_id, slots)` exactly as the
                // `Integer` twin's `INTEGER_ALLOC_PLAN` is, and under the same
                // two conditions: the `class_was_redefined` screen above, and
                // no memo while `tlab_shape_planner_is_instrumented` (round 11:
                // this twin re-planned on every escaping box).
                thread_local! {
                    // (vm_key, class_id, slots, requested_size)
                    pub static LONG_ALLOC_PLAN: std::cell::Cell<
                        Option<(usize, u32, u32, u32)>,
                    > = const { std::cell::Cell::new(None) };
                }
                let planner_instrumented =
                    crate::runtime::interpreter::tlab_shape_planner_is_instrumented();
                let memo_key = (vm_key, class_id.as_u32(), slots as u32);
                let cached_plan = (!planner_instrumented)
                    .then(|| LONG_ALLOC_PLAN.with(|c| c.get()))
                    .flatten()
                    .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                    .map(|(_, _, _, size)| size as usize);
                let requested_size = match cached_plan {
                    Some(size) => size,
                    None => {
                        let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
                            vm,
                            class_id,
                            slots,
                            crate::runtime::interpreter::tlab_site::JIT_HELPER,
                        );
                        if !planner_instrumented {
                            if let Ok(size_u32) = u32::try_from(size) {
                                LONG_ALLOC_PLAN.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, size_u32)))
                                });
                            }
                        }
                        size
                    }
                };
                let tlab_object = if requested_size <= cratonvm_gc::tlab::tlab_max_alloc() {
                    crate::runtime::interpreter::tlab_alloc_object(
                        thread,
                        vm,
                        class_id,
                        slots,
                        requested_size,
                    )
                } else {
                    None
                };
                if let Some(object) = tlab_object {
                    // Raw primitive-cell write, exactly as the `Integer` twin
                    // does and for the same reason: this arm JUST allocated
                    // `object` through the legacy TLAB path (`init_object_header`,
                    // zeroed 16-byte `Value` cells), so field 0 is the `Value`
                    // cell at `HEADER_SIZE`. A primitive store takes no write
                    // barrier.
                    // ...but ONLY when it really is legacy -- see the
                    // `Integer` twin for the miscompile that premise produced.
                    // SAFETY: `object` was just allocated here, so its first
                    // HEADER_SIZE bytes are a live, fully written `ObjectHeader`.
                    let header =
                        unsafe { &*(object.as_ptr() as *const cratonvm_types::ObjectHeader) };
                    let compact = cratonvm_types::is_compact_object(header);
                    if compact {
                        // The packed `value` cell, memoized like the `Integer`
                        // twin's `INTEGER_VALUE_CELL` and for the same reason
                        // (`set_field_as` is three layout lookups for one
                        // store). Restricted to an 8-byte `Long` cell at an
                        // 8-aligned address; any other shape declines to
                        // `set_field_as`.
                        thread_local! {
                            // (vm_key, class_id, slots, offset)
                            pub static LONG_VALUE_CELL: std::cell::Cell<
                                Option<(usize, u32, u32, u32)>,
                            > = const { std::cell::Cell::new(None) };
                        }
                        let offset = LONG_VALUE_CELL
                            .with(|c| c.get())
                            .filter(|(vk, cid, sl, _)| (*vk, *cid, *sl) == memo_key)
                            .map(|(_, _, _, offset)| offset as usize)
                            .or_else(|| {
                                // An ABSOLUTE displacement (header included)
                                // since the 8-byte compact header.
                                let (offset, storage) =
                                    cratonvm_types::compact_object_field_disp(header, 0)?;
                                if storage != cratonvm_types::FieldStorageKind::Long
                                    || offset % 8 != 0
                                {
                                    return None;
                                }
                                let offset_u32 = u32::try_from(offset).ok()?;
                                LONG_VALUE_CELL.with(|c| {
                                    c.set(Some((memo_key.0, memo_key.1, memo_key.2, offset_u32)))
                                });
                                Some(offset)
                            });
                        match offset {
                            // SAFETY: `object` is this arm's own just-allocated
                            // compact body (8-aligned), and `offset` is field
                            // 0's byte offset in the layout this object's
                            // header names, checked to be an 8-aligned `Long`
                            // cell. A primitive store into a fresh object takes
                            // no write barrier.
                            Some(offset) => unsafe {
                                cratonvm_types::write_compact_field(
                                    object.as_ptr().add(offset),
                                    cratonvm_types::FieldStorageKind::Long,
                                    Value::Long(value),
                                    std::sync::atomic::Ordering::Relaxed,
                                )
                            },
                            None => vm
                                .mem
                                .heap
                                .set_field_as(object, 0, Value::Long(value), b'J'),
                        }
                    } else {
                        // SAFETY: `object` is a live legacy-layout allocation
                        // with >= 1 slot (`slots.max(1)` above) -- now CHECKED
                        // immediately above, not assumed; the cell is
                        // exclusively ours until published below.
                        unsafe {
                            std::ptr::write(
                                object.as_ptr().add(cratonvm_gc::heap::HEADER_SIZE) as *mut Value,
                                Value::Long(value),
                            );
                        }
                    }
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                // zr z3/lt: see the `Integer.valueOf` twin -- a refusal falls
                // through to the native funnel below, which throws.
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    // Descriptor-typed write (`Long.value`, field 0, `J`) —
                    // this cold arm's allocator may pick a non-legacy layout,
                    // so keep the layout-aware store.
                    vm.mem
                        .heap
                        .set_field_as(object, 0, Value::Long(value), b'J');
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
            }
        }
    }
    // In-cache-range fast path (round 9 wave 10b, wire10b; the `Integer`
    // twin's reasoning): a filled `LongCache` slot is answered directly, the
    // same object `native_long_value_of` would return. A cold slot falls
    // through to the native, which fills it.
    if (-128..=127).contains(&value) {
        if let Some(object) =
            cratonvm_native_builtins::lang_math::long_cache_peek(vm.vm_identity, value)
        {
            return object.as_ptr() as i64;
        }
    }
    // Cold / in-cache-range / redefine-window path: canonical native callback
    // via the full safe-native-call wrapper (identity cache; also discovers the
    // real wrapper ClassId for the fast path above).
    let mut _jit_borrow = None;
    let held: Option<&mut JvmThread> = match given {
        Some(t) => Some(t),
        None => jit_thread_mut().map(|(t, g)| {
            _jit_borrow = Some(g);
            t
        }),
    };
    let Some(thread) = held else {
        // No JIT thread context — signal the deopt sentinel; the caller's
        // post-invoke check bails to the interpreter, which re-dispatches.
        // The flag is what makes that true — see the `Integer.valueOf` twin.
        set_jit_deopt_pending();
        return i64::MIN;
    };
    let arg = Value::Long(value);
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        cratonvm_native_builtins::intrinsics::long::intrinsic_long_value_of,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return handle_jit_dispatch_error(vm, thread, error, &LONG_VALUE_OF_INFO),
    };
    match result {
        Some(Value::Object(Some(object))) => {
            LONG_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
            object.as_ptr() as i64
        }
        _ => 0,
    }
}

/// Thin direct-call target for JIT `invokevirtual Long.longValue()` sites whose
/// constant-pool class is exactly `java/lang/Long` (a `final` class, so the site
/// is statically monomorphic — no receiver guard needed; only `null` remains,
/// which throws NPE per JVMS).
///
/// Mirrors [`jit_integer_int_value_direct`] and the registered
/// `lang_math.rs::native_wrapper_long_value` it stands in for, INCLUDING that
/// native's `Value::Int` arm: reflection can legally leave an int-tagged raw
/// slot in a `Long` wrapper, and a `()J` caller must see the widened value
/// rather than the compact tag bits. Dropping that arm here would make the fast
/// path disagree with the funnel on exactly the receiver the funnel was written
/// for.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_long_long_value_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_long_long_value_direct_body(vm_ptr, receiver);
    contain(
        "jit_long_long_value_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_long_long_value_direct`], run under its panic guard.
pub unsafe fn jit_long_long_value_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    let census = crate::runtime::interp_census::direct_binds_enabled();
    let raw = receiver as u64;
    if raw == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        // The arena-membership probe stays, for the reason the `Integer` twin
        // states: nothing on this path has read the receiver's header yet, so
        // it is the only thing between a fabricated argument and the
        // `get_field` dereference.
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            match vm.mem.heap.get_field(object, 0) {
                Value::Long(value) => {
                    if census {
                        crate::runtime::interp_census::note_long_value_direct(false);
                    }
                    return value;
                }
                Value::Int(value) => {
                    if census {
                        crate::runtime::interp_census::note_long_value_direct(false);
                    }
                    return i64::from(value);
                }
                // Anything else is a shape the registered native answers 0 for;
                // hand it to the generic dispatcher rather than guessing, so the
                // two paths cannot disagree.
                _ => {}
            }
        }
    }
    // Defensive fallback: hand the call to the generic dispatcher (same
    // machinery the non-direct site would have used).
    if census {
        crate::runtime::interp_census::note_long_value_direct(true);
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &LONG_LONG_VALUE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// A one-entry, per-thread memo of "the raw address `recv` holds a live object
/// of class `class_id`", valid only inside one GC epoch.
///
/// # Why
///
/// `ZgcRealHeap::is_object_address` plus `ZObjectStarts::contains` measured
/// **16 %** of `JdkZlibIntegrationTest#testHugeDecompress` once the funnel was
/// off its three hot calls -- and the receiver it re-validates is the SAME
/// object 268 million times running: one `MessageDigest`, one pooled
/// `ByteBuffer` per megabyte chunk.
///
/// # Why the epoch is the whole safety argument
///
/// The hazard a memo has to answer is an address whose object died and whose
/// storage was handed to something else. That cannot happen inside one epoch:
/// an address occupied by a live object at memo time is not on any free list
/// until a collection reclaims it, and every collection bumps
/// `VmHeap::collection_count`. A memo from a previous epoch therefore never
/// matches, and a moving collection is covered twice over -- the compiled
/// frame's receiver is rewritten to the new address, which misses on `recv`
/// as well.
///
/// Per-thread, so no lock and no cross-thread visibility question. A miss costs
/// exactly what every call used to pay.
#[derive(Clone, Copy)]
pub struct DirectReceiverMemo {
    /// The VM whose heap `recv` was proven in (`SharedVm::vm_identity`).
    /// gc-common w36-a: the memo is per THREAD, and one thread can run two
    /// VMs in turn (the `vm` test binary under `--test-threads=1` runs every
    /// test on its main thread; an embedder may too). A disposed VM's heap
    /// range can be mapped again by the next VM, whose collection count
    /// starts from the same value, so `(recv, epoch)` alone could serve the
    /// old VM's class id and identity hash for the new VM's object -- a
    /// wrong-class VarHandle plan admission or a digest fed another's byte.
    /// Identities are never recycled, so `(vm, recv, epoch)` cannot collide.
    pub vm: usize,
    pub recv: usize,
    pub epoch: u64,
    pub class_id: u32,
    /// The `NativeContext`-compatible identity hash, so a helper keyed by it
    /// (`MessageDigest.update`) does not re-derive one per call. Never `0` for
    /// a filled entry: `java_identity_hash` refuses to hand back zero.
    pub identity: i32,
}

/// FOUR entries, and the number is measured rather than chosen.
///
/// A ONE-entry memo removed nothing on the workload it was written for:
/// `testHugeDecompress`'s inner loop alternates `in.writeByte(b)` with
/// `digest.update(b)`, so consecutive calls arrive with two different receivers
/// and a single slot thrashes on every one. `is_object_address` +
/// `ZObjectStarts::contains` stayed at 15 % of the profile across the change.
/// EIGHT, for the same reason four beat one. `testHugeDecompress`'s inner loop
/// touches four distinct receivers per byte once the `VarHandle` read is on the
/// memo too — the digest, the pooled `ByteBuffer`, the `RefCnt` the accessibility
/// check reads, and the `VarHandle` itself — and the loop's other calls bring
/// their own. Eight leaves headroom for that without the scan mattering: it is
/// eight predictable compares against a probe that walks a sorted page index.
pub const DIRECT_RECEIVER_MEMO_WAYS: usize = 8;

thread_local! {
    pub static DIRECT_RECEIVER_MEMO: [std::cell::Cell<DirectReceiverMemo>; DIRECT_RECEIVER_MEMO_WAYS] =
        const {
            [const { std::cell::Cell::new(DirectReceiverMemo {
                vm: 0, recv: 0, epoch: 0, class_id: 0, identity: 0,
            }) }; DIRECT_RECEIVER_MEMO_WAYS]
        };
    /// Round-robin victim for the memo above.
    pub static DIRECT_RECEIVER_MEMO_NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `is_object_address` + `class_id_of` (+ the identity hash) for a thin direct
/// helper's receiver, memoised per GC epoch. `None` means the address is not a
/// live object base and the caller must decline.
///
/// # Safety
/// `raw` is a receiver argument from JIT-compiled code; `vm` is live.
#[inline]
pub unsafe fn direct_receiver_facts(vm: &SharedVm, raw: usize) -> Option<(u32, i32)> {
    let epoch = vm.mem.address_epoch();
    let vm_id = vm.vm_identity;
    // Read individual entries in place: copying the whole eight-way table on
    // every byte access made the memo itself a measured hot path.
    if let Some(facts) = DIRECT_RECEIVER_MEMO.with(|ways| {
        ways.iter().find_map(|cell| {
            let way = cell.get();
            (way.recv == raw && way.epoch == epoch && way.vm == vm_id && way.class_id != 0)
                .then_some((way.class_id, way.identity))
        })
    }) {
        return Some(facts);
    }
    let object = vm.mem.heap.is_object_address(raw)?;
    // Round 11 wave 13 (lane `icwide`): 0 for an array, whose header carries
    // its COMPONENT's id (`jit_dispatch_class`). Every caller reads this id
    // as the receiver's CLASS and declines on 0: a `VarHandle`'s receiver
    // coordinate is `Object`-typed at the call site, so an `H[]` reached
    // `varhandle_plan_admits_receiver` as an `H` and had a field of `H`
    // read from, written into or CASed in the array's element storage.
    let class_id = jit_dispatch_class_validated(vm, object)
        .class_id()
        .map_or(0, |cid| cid.as_u32());
    // The SAME key `NativeContextImpl::identity_hash_code` computes: the heap
    // answer, then the monitor's displaced-hash arbitration. A locked object's
    // mark word cannot hold a hash, so the heap answer alone is the wrong key
    // for any side table the natives maintain.
    let heap = &vm.mem.heap;
    let identity =
        vm.threads
            .monitors
            .java_identity_hash(object, heap.identity_hash_code(object), || {
                heap.next_identity_hash()
            });
    if class_id != 0 {
        let slot = DIRECT_RECEIVER_MEMO_NEXT.with(|c| {
            let i = c.get();
            c.set((i + 1) % DIRECT_RECEIVER_MEMO_WAYS);
            i
        });
        DIRECT_RECEIVER_MEMO.with(|ways| {
            ways[slot].set(DirectReceiverMemo {
                vm: vm_id,
                recv: raw,
                epoch,
                class_id,
                identity,
            })
        });
    }
    Some((class_id, identity))
}

/// Synthetic call-site info for the `ByteBuffer` single-byte element helpers'
/// DECLINE edge. `num_jit_args` counts the receiver, the way an instance
/// site's does.
pub static NIO_BYTEBUFFER_PUT_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/ByteBuffer",
    method_name: "put",
    descriptor: "(IB)Ljava/nio/ByteBuffer;",
    num_jit_args: 3,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static NIO_BYTEBUFFER_GET_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/ByteBuffer",
    method_name: "get",
    descriptor: "(I)B",
    num_jit_args: 2,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The shared prologue of both `ByteBuffer` single-byte helpers: prove the
/// receiver is a direct buffer of a class the FUNNEL has already served with
/// the modelled layout, and return the element's absolute address.
///
/// It reads the same three fields `native-io`'s `dbb_elem_addr` reads, through
/// the same descriptor-aware accessor, in the same order, and refuses on the
/// same conditions — a negative or out-of-limit index, a read-only carrier for
/// a write, a non-positive address. That is deliberate duplication of a
/// SEQUENCE, not of a LAYOUT: the three field indices come from
/// `elem_fastpath::slots()`, which is what the native itself resolved, so
/// there is no second copy of the layout to drift.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub unsafe fn dbb_direct_elem_addr(
    vm: &SharedVm,
    receiver: i64,
    index: i64,
    for_write: bool,
) -> Option<i64> {
    let raw = receiver as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let (address_slot, limit_slot, read_only_slot) =
        cratonvm_native_io::direct_buffer::elem_fastpath::slots_for_vm(vm.vm_identity)?;
    // The arena-membership probe, for the reason the `Long.longValue` twin
    // states: nothing on this path has read the receiver's header yet, so it is
    // the only thing between a fabricated argument and the field reads below.
    // Memoised per GC epoch -- see `direct_receiver_facts`.
    let (class_id, _identity) = direct_receiver_facts(vm, raw as usize)?;
    if !cratonvm_native_io::direct_buffer::elem_fastpath::class_is_served_for_vm(vm.vm_identity, class_id, for_write) {
        return None;
    }
    // The memo proved this address is a live object base in this epoch, so the
    // reference is reconstructible without a second probe -- the same step
    // `jit_hashmap_get_direct` takes after its own exact-receiver screen.
    let object = ObjectRef::from_raw(raw as usize as *mut u8);
    // W7-89 follow-up: `class_is_served` is keyed by CLASS, and
    // `java/nio/DirectByteBuffer` is the one class both a plain
    // `allocateDirect()` buffer and an FFM `MemorySegment.asByteBuffer()` view
    // share -- so a class the funnel marked served from the former must still
    // be refused PER RECEIVER when a later call at this same compiled site
    // sees the latter. This helper has no way to check the segment's session
    // liveness/confinement itself (this crate is below `native-builtins`,
    // where that check lives), so it declines outright and falls through to
    // the generic dispatcher, which runs the real `DirectByteBuffer` bytecode
    // -- `session()` -> `ScopedMemoryAccess.getByte` -> `checkValidState()` --
    // exactly as the interpreter's own bail (`dbb_has_ffm_segment`) does.
    if let Some(segment_slot) = cratonvm_native_io::direct_buffer::elem_fastpath::segment_slot_for_vm(vm.vm_identity) {
        if matches!(
            vm.mem.heap.get_field_as(object, segment_slot, b'L'),
            cratonvm_types::Value::Object(Some(_))
        ) {
            return None;
        }
    }
    // H12-1, as on the `HashMap` helpers. The gate is `for_write`'s own
    // choice, because that flag is exactly the choice between `ByteBuffer.put`
    // and `ByteBuffer.get` — two registered rows, two policy questions, and
    // the decline edges below dispatch through two different `JitInvokeInfo`s.
    // Placed after the receiver screens so the counters count calls the fast
    // path would otherwise have SERVED.
    let gate: &'static DirectHelperGate = if for_write {
        &NIO_BYTEBUFFER_PUT_BYTE_GATE
    } else {
        &NIO_BYTEBUFFER_GET_BYTE_GATE
    };
    if jit_direct_helper_refused(vm, gate) {
        return None;
    }
    let cratonvm_types::Value::Int(limit) = vm.mem.heap.get_field_as(object, limit_slot, b'I')
    else {
        return None;
    };
    // Cast: the JIT operand stack carries one sign-extended i64 per int.
    let idx = index as i32;
    if idx < 0 || idx >= limit {
        return None;
    }
    if for_write
        && !matches!(
            vm.mem.heap.get_field_as(object, read_only_slot, b'Z'),
            cratonvm_types::Value::Int(0)
        )
    {
        return None;
    }
    let cratonvm_types::Value::Long(address) = vm.mem.heap.get_field_as(object, address_slot, b'J')
    else {
        return None;
    };
    if address <= 0 {
        return None;
    }
    address.checked_add(i64::from(idx))
}

/// Thin direct-call target for JIT `ByteBuffer.put(int,byte)` sites.
///
/// See `cratonvm_jit::NIO_BYTEBUFFER_PUT_BYTE_DIRECT_FN` for the census. The
/// fast path writes one byte and returns the receiver, which is what
/// `put(int,byte)` returns; **every** case it is not certain of — a null or
/// fabricated receiver, a class the funnel has not served, an out-of-limit
/// index, a read-only carrier, an address the off-heap store refuses — falls
/// through to the generic dispatcher, so the exception a caller sees is still
/// the registered native's.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_dbb_put_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    index: i64,
    value: i64,
) -> i64 {
    let f = || jit_dbb_put_byte_direct_body(vm_ptr, receiver, index, value);
    contain(
        "jit_dbb_put_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_dbb_put_byte_direct`], run under its panic guard.
pub unsafe fn jit_dbb_put_byte_direct_body(vm_ptr: i64, receiver: i64, index: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(at) = dbb_direct_elem_addr(vm, receiver, index, true) {
        // Cast: only the low 8 bits are the Java `byte`; the operand stack
        // widened it to int.
        let byte = value as u8;
        // Classified by the TAG BIT and not by an arena liveness probe, for the
        // reason `NativeContextImpl::copy_to_native_memory` states: a tagged
        // handle whose arena was freed must be DECLINED, never copied as an OS
        // pointer.
        let stored = if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(at) {
            cratonvm_native_builtins::unsafe_arena_copy_in(at, &[byte])
        } else if at > 0 {
            // SAFETY: `at` is `address + index` for an index the carrier's own
            // `limit` admits, and `address` is the block this buffer owns.
            std::ptr::write(at as *mut u8, byte);
            true
        } else {
            false
        };
        if stored {
            cratonvm_jit::NIO_BYTE_ELEMENT_SERVED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return receiver;
        }
    }
    cratonvm_jit::NIO_BYTE_ELEMENT_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, index, value];
    jit_invoke_dispatch(
        vm_ptr,
        &NIO_BYTEBUFFER_PUT_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// Thin direct-call target for JIT `ByteBuffer.get(int)` sites — the read twin
/// of [`jit_dbb_put_byte_direct`], with the same decline contract.
///
/// The value is returned SIGN-EXTENDED through `i8`, which is what
/// `dbb_get_abs` produces (`Value::Int(i32::from(byte as i8))`) and what every
/// `b < 0` caller expects.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_dbb_get_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    index: i64,
) -> i64 {
    let f = || jit_dbb_get_byte_direct_body(vm_ptr, receiver, index);
    contain(
        "jit_dbb_get_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_dbb_get_byte_direct`], run under its panic guard.
pub unsafe fn jit_dbb_get_byte_direct_body(vm_ptr: i64, receiver: i64, index: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(at) = dbb_direct_elem_addr(vm, receiver, index, false) {
        let mut byte = [0u8; 1];
        let loaded = if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(at) {
            cratonvm_native_builtins::unsafe_arena_copy_out(at, &mut byte)
        } else if at > 0 {
            // SAFETY: as the store twin — inside the carrier's own block.
            byte[0] = std::ptr::read(at as *const u8);
            true
        } else {
            false
        };
        if loaded {
            cratonvm_jit::NIO_BYTE_ELEMENT_SERVED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return i64::from(byte[0] as i8);
        }
    }
    cratonvm_jit::NIO_BYTE_ELEMENT_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, index];
    jit_invoke_dispatch(
        vm_ptr,
        &NIO_BYTEBUFFER_GET_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Synthetic call-site info for [`jit_md_update_byte_direct`]'s decline edge.
pub static MD_UPDATE_BYTE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/security/MessageDigest",
    method_name: "update",
    descriptor: "(B)V",
    num_jit_args: 2,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `MessageDigest.update(byte)` sites.
///
/// See `cratonvm_jit::MD_UPDATE_BYTE_DIRECT_FN` for the census. The fast path
/// appends one byte to the accumulator the registered native keeps for this
/// receiver, and it may do so only when BOTH of the native's own conditions
/// hold: the receiver's class is the exact one `md_receiver_is_ours` accepted,
/// and the side table already has an entry for it.
///
/// The key is computed the way `NativeContextImpl::identity_hash_code`
/// computes it — heap answer, then `Monitors::java_identity_hash` — and NOT
/// from the heap alone. A locked object's mark word cannot hold a hash, so the
/// heap answer for one is `0` and the monitor holds the displaced value; using
/// the heap answer directly would file a byte under a key the native never
/// uses. Getting it wrong would MISS rather than corrupt (the entry is looked
/// up, never created), but a miss on every call is a fast path that does
/// nothing while looking installed.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_md_update_byte_direct(
    vm_ptr: i64,
    receiver: i64,
    value: i64,
) -> i64 {
    let f = || jit_md_update_byte_direct_body(vm_ptr, receiver, value);
    contain(
        "jit_md_update_byte_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_md_update_byte_direct`], run under its panic guard.
pub unsafe fn jit_md_update_byte_direct_body(vm_ptr: i64, receiver: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // A null receiver at an `invokevirtual` this intrinsic replaced, so
        // the JEP-358 action is the one the generic dispatch helper records:
        // `INVOKE_RECEIVER` names the opcode family that trapped and lets
        // `runtime::interpreter::jit_npe_message` corroborate the bci in this
        // frame's safepoint-id slot. A direct-bound intrinsic must not answer
        // `getMessage()` differently from the dispatch it stands in for.
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    'fast: {
        let raw = receiver as u64;
        if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            break 'fast;
        }
        // The arena-membership probe: the only thing between a fabricated
        // argument and the class-header read below. Memoised per GC epoch --
        // see `direct_receiver_facts`. This receiver is the SAME digest for
        // every one of the 268 million bytes a side, so the memo hits on all
        // but the first (and the entry carries the identity key, which is what
        // this helper would otherwise re-derive per byte).
        let Some((class_id, key)) = direct_receiver_facts(vm, raw as usize) else {
            break 'fast;
        };
        if !md_update_class_is_ours(vm, class_id) {
            break 'fast;
        }
        if jit_direct_helper_refused(vm, &MD_UPDATE_BYTE_GATE) {
            break 'fast;
        }
        // Cast: only the low 8 bits are the Java `byte`; the operand stack
        // widened it to int. The VM names the row too (gc-common w36-a): a
        // lone row of another VM's digest is never this receiver's.
        if !cratonvm_native_builtins::jca::message_digest::update_fastpath::append_byte(
            vm.vm_identity,
            key,
            value as u8,
        ) {
            break 'fast;
        }
        cratonvm_jit::MD_UPDATE_BYTE_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return 0;
    }
    cratonvm_jit::MD_UPDATE_BYTE_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, value];
    jit_invoke_dispatch(
        vm_ptr,
        &MD_UPDATE_BYTE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

thread_local! {
    /// The last `(vm_identity, class id, verdict)` [`md_update_class_is_ours`]
    /// resolved on this thread. One entry: a hashing loop feeds one digest.
    pub static MD_OURS_CLASS_MEMO: std::cell::Cell<(usize, u32, bool)> =
        const { std::cell::Cell::new((0, 0, false)) };
}

/// Is `class_id`, in `vm`, the exact class the `MessageDigest` accumulator
/// natives own (`update_fastpath::class_name_is_ours`)?
///
/// gc-common w36-a (`common-w22b-md-update-fastpath-class-id-is-process-wide`):
/// this used to read one process-wide id latched by the first VM whose native
/// accepted a digest. `ClassId`s are minted per VM, so in a second VM that id
/// could name a provider's digest subclass, whose bytes were then appended to
/// another digest's accumulator. Now the class is named by THIS VM's own class
/// table, memoised per thread on `(vm_identity, class id)`. Identities are
/// never recycled, so a line never answers for another VM; a `true` line names
/// the bootstrap `java/security/MessageDigest`, which is never unloaded; and a
/// `false` line that went stale (were an id ever reused) could only decline
/// to the native. Steady state: one thread-local compare; a miss takes the
/// class manager's read lock once.
pub fn md_update_class_is_ours(vm: &SharedVm, class_id: u32) -> bool {
    md_class_verdict_memoised(vm.vm_identity, class_id, || {
        vm.classes
            .class_manager
            .read()
            .get_class(ClassId::new(class_id))
            .map(|c| {
                cratonvm_native_builtins::jca::message_digest::update_fastpath::class_name_is_ours(
                    &c.name,
                )
            })
    })
}

/// The memo behind [`md_update_class_is_ours`]: `resolve` names the verdict
/// for `(vm_id, class_id)` on a miss (`None`: the class is unknown, which is
/// not "ours" and is not memoised).
pub fn md_class_verdict_memoised(
    vm_id: usize,
    class_id: u32,
    resolve: impl FnOnce() -> Option<bool>,
) -> bool {
    if class_id == 0 {
        return false;
    }
    let (memo_vm, memo_class, memo_ours) = MD_OURS_CLASS_MEMO.with(std::cell::Cell::get);
    if memo_vm == vm_id && memo_class == class_id {
        return memo_ours;
    }
    match resolve() {
        Some(ours) => {
            MD_OURS_CLASS_MEMO.with(|c| c.set((vm_id, class_id, ours)));
            ours
        }
        None => false,
    }
}

/// Synthetic call-site info for [`jit_preconditions_check_index_direct`]'s
/// cold arm — the out-of-range case, which must throw exactly what the
/// registered native throws.
pub static PRECONDITIONS_CHECK_INDEX_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/Preconditions",
    method_name: "checkIndex",
    descriptor: "(IILjava/util/function/BiFunction;)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokestatic
/// jdk/internal/util/Preconditions.checkIndex(int,int,BiFunction)` sites
/// (registered into `cratonvm_jit::PRECONDITIONS_CHECK_INDEX_DIRECT_FN` by
/// `build_helpers`, recognised in `jit::try_compile`'s ladder and the IR path).
///
/// **Why this one.** `--dump-native-registry`'s invocation census on
/// `probes/NioAccessorRate.java` put it at the TOP of the list — 4 000 000
/// calls for 800 000 `ByteBuffer` accessor operations, ~2.5 per accessor,
/// ahead of the store itself. `Objects.checkIndex` sits under every
/// `java.nio.Buffer` absolute accessor, every `String` index check and every
/// `List` bounds check, and it is a compare and a branch: paying the ~160 ns
/// generic native funnel for it is the single largest rung under
/// `HttpContentDecompressorTest.testZipBomb`
/// (httpcontentdecompressortest-snappy-varhandle-bind-RETIRED-20260820.md).
///
/// Fast path: `0 <= index < length` returns `index`, with no funnel, no
/// argument buffer, no `safe_native_call` wrapper. **Everything else — an
/// out-of-range index, a negative length, anything that must throw — falls
/// through to the generic dispatcher**, so the exception's class, message and
/// the `BiFunction` formatter selection stay byte-for-byte what the registered
/// native produces. That matters here more than usual: `throw_out_of_bounds`
/// reads `Preconditions`' three static formatters to tell
/// `StringIndexOutOfBoundsException` from `IndexOutOfBoundsException`, and
/// reimplementing that choice in the fast path is exactly how the wrong-class
/// bug the module comment warns about comes back.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_preconditions_check_index_direct(
    vm_ptr: i64,
    index: i64,
    length: i64,
    formatter: i64,
) -> i64 {
    let f = || jit_preconditions_check_index_direct_body(vm_ptr, index, length, formatter);
    contain(
        "jit_preconditions_check_index_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_preconditions_check_index_direct`], run under its panic guard.
pub unsafe fn jit_preconditions_check_index_direct_body(
    vm_ptr: i64,
    index: i64,
    length: i64,
    formatter: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // H12-1. `jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I`
    // is `bridge`, and the OSR door binds this helper with no policy question.
    // Declining takes the same generic dispatcher the throwing case already
    // uses, so the refusal costs a route, not a contract.
    //
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    let i = index as i32;
    let n = length as i32;
    if i >= 0 && n >= 0 && i < n && !jit_direct_helper_refused(vm, &PRECONDITIONS_CHECK_INDEX_GATE)
    {
        return i as i64;
    }
    // Throwing case (and any shape this fast path declines to judge): the
    // generic dispatcher runs the registered native, formatter and all.
    let args = [index, length, formatter];
    jit_invoke_dispatch(
        vm_ptr,
        &PRECONDITIONS_CHECK_INDEX_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// JIT round 19 proposal wave 7, lane tail7 (OSRFRAME18-2,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS`, opt-in): the LEAF TWIN of
/// [`jit_preconditions_check_index_direct`]. Its fast arm, verbatim, and `-1`
/// (an answer the helper never gives: it answers only an index in
/// `[0, length)`) where the helper would dispatch.
///
/// The contract the single-pass tail site relies on
/// (`x64/op_invoke.rs` `try_emit_sp_tail_leaf_twin`): this function cannot
/// reach the VM. It allocates nothing, raises nothing (a panic is RECORDED
/// and declines, never stashed as an `InternalError`), runs no Java and
/// dispatches nothing, so no stack walk can happen while it runs and the
/// caller owes it no safepoint publication. The gate read is the helper's
/// own per-call policy question (a memoised registry lookup and a per-thread
/// counter). A decline sends the caller to the helper itself, with its frame
/// published.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_preconditions_check_index_leaf(
    vm_ptr: i64,
    index: i64,
    length: i64,
    _formatter: i64,
) -> i64 {
    let f = || {
        // SAFETY: vm_ptr originates from JIT code compiled against this live
        // VM, exactly as for the helper this twins.
        let vm = &*(vm_ptr as *const SharedVm);
        let i = index as i32;
        let n = length as i32;
        if i >= 0
            && n >= 0
            && i < n
            && !jit_direct_helper_refused(vm, &PRECONDITIONS_CHECK_INDEX_GATE)
        {
            i64::from(i)
        } else {
            -1
        }
    };
    contain("jit_preconditions_check_index_leaf", OnPanic::RecordFor { vm_ptr }, -1, f)
}

/// Synthetic call-site infos for the cold arm of the two
/// [`jit_decimal_digits_latin1_int_direct`] / `_long_` helpers: the real
/// `DecimalDigits` bytecode, through the generic dispatcher.
pub static DECIMAL_DIGITS_LATIN1_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/DecimalDigits",
    method_name: "uncheckedGetCharsLatin1",
    descriptor: "(II[B)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub static DECIMAL_DIGITS_LATIN1_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/DecimalDigits",
    method_name: "uncheckedGetCharsLatin1",
    descriptor: "(JI[B)I",
    num_jit_args: 3,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// The digits `jdk.internal.util.DecimalDigits.uncheckedGetCharsLatin1`
/// writes for `value`, written the way it writes them: backwards from
/// `index` (exclusive), a `'-'` in front of a negative value, LATIN1 bytes.
/// Answers the new `charPos` (`index - digits`), or `None` -- writing
/// nothing -- when that range is not wholly inside `buf`, which the JDK
/// body would write past (it asserts the bounds only under `-esa`).
///
/// `i64` covers both overloads: an `int` argument arrives sign-extended,
/// and `unsigned_abs` makes `Integer.MIN_VALUE` / `Long.MIN_VALUE` exact.
pub fn decimal_digits_latin1_into(value: i64, index: i64, buf: &mut [u8]) -> Option<i64> {
    let mut scratch = [0u8; 20];
    let mut at = scratch.len();
    let mut magnitude = value.unsigned_abs();
    loop {
        at -= 1;
        scratch[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        at -= 1;
        scratch[at] = b'-';
    }
    let digits = &scratch[at..];
    let end = usize::try_from(index).ok()?;
    let start = end.checked_sub(digits.len())?;
    buf.get_mut(start..end)?.copy_from_slice(digits);
    // Cast: `start <= index`, which came in as an i64.
    Some(start as i64)
}

/// Thin direct-call targets for JIT `invokestatic
/// jdk/internal/util/DecimalDigits.uncheckedGetCharsLatin1(II[B)I` and
/// `(JI[B)I` sites (JIT round 16, lane strings, STR16-6;
/// `CRATONVM_JIT_DECIMAL_DIGITS_DIRECT`, opt-in).
///
/// **Why.** Every JDK 25 `int`/`long` -> decimal conversion --
/// `AbstractStringBuilder.append(int|long)`, `Integer.toString`,
/// `Long.toString`, `String.valueOf` -- ends here, and the JDK body writes
/// each digit pair through `uncheckedPutPairLatin1` ->
/// `uncheckedPutCharLatin1` -> `Unsafe.putByte`: two compiled-to-compiled
/// calls and one `Unsafe` helper call per DIGIT (the splicer refuses the
/// chain, `uncheckedPutCharLatin1` carries an `AssertionError` arm), so
/// `append(-i)` measured 3 124-3 480 ns against 235 for the call-site
/// intrinsic's path. This writes the same bytes in one call.
///
/// Fast path: `buf` a live `byte[]` and `[index - digits, index)` inside it.
/// Everything else -- a null or non-`byte[]` `buf`, a range the JDK body
/// would write out of bounds -- falls through to the generic dispatcher, so
/// the real bytecode decides what happens, exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_decimal_digits_latin1_int_direct(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
) -> i64 {
    let f = || {
        jit_decimal_digits_latin1_direct_body(
            vm_ptr,
            i64::from(value as i32),
            index,
            buf,
            &DECIMAL_DIGITS_LATIN1_INT_INFO,
        )
    };
    contain(
        "jit_decimal_digits_latin1_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// The `(JI[B)I` twin of [`jit_decimal_digits_latin1_int_direct`].
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_decimal_digits_latin1_long_direct(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
) -> i64 {
    let f = || {
        jit_decimal_digits_latin1_direct_body(
            vm_ptr,
            value,
            index,
            buf,
            &DECIMAL_DIGITS_LATIN1_LONG_INFO,
        )
    };
    contain(
        "jit_decimal_digits_latin1_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of the two `jit_decimal_digits_latin1_*_direct` helpers, run under
/// their panic guards.
pub unsafe fn jit_decimal_digits_latin1_direct_body(
    vm_ptr: i64,
    value: i64,
    index: i64,
    buf: i64,
    info: &'static JitInvokeInfo,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // `index` is a Java `int`; the callers pass it sign-extended.
    let index = i64::from(index as i32);
    // j27 wave 3 (lane BACKLOG, STR16-P7): no per-call census here. The
    // served / declined counters were written on every call (one shared
    // cache line for every thread converting a number) and read by no one.
    if let Some(char_pos) = decimal_digits_latin1_serve(vm, value, index, buf) {
        return char_pos;
    }
    let args = [value, index, buf];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// The fast path of the `jit_decimal_digits_latin1_*_direct` helpers: the
/// same `byte[]` validation `raw_accessor_write_bytes` makes (aligned,
/// heap-resident, an array, a BYTE array -- `boolean[]` is not a `byte[]`
/// here) before [`decimal_digits_latin1_into`] writes into its payload.
pub unsafe fn decimal_digits_latin1_serve(
    vm: &SharedVm,
    value: i64,
    index: i64,
    buf: i64,
) -> Option<i64> {
    let raw = buf as u64;
    if buf == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.element_type_of_validated(object) != ArrayElementType::Byte
    {
        return None;
    }
    let length = vm.mem.heap.array_length(object);
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `base` is the payload of a live `byte[]` of `length` elements,
    // and nothing between here and the write can move or free it (no
    // allocation, no safepoint).
    let payload = std::slice::from_raw_parts_mut(base, length);
    decimal_digits_latin1_into(value, index, payload)
}

/// Synthetic call-site info for the cold arms of
/// [`jit_byte_array_clone_direct`] and [`jit_object_clone_virtual_direct`]: a
/// VIRTUAL `clone()` through the generic dispatcher, which selects on the
/// receiver's runtime class (an override, or `Object.clone` for an array) and
/// raises the null receiver's NPE. j27 wave 3 (lane CLONE): one descriptor for
/// both (it was `[B`-only), named against `Object`, where `clone()` is
/// declared, so it reads true for every receiver it now stands for.
pub static CLONE_VIRTUAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "clone",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for a compiled `byte[].clone()` (strings round 2,
/// `str17`; `CRATONVM_JIT_BYTE_ARRAY_CLONE_DIRECT`). An array receiver never
/// sits in a monomorphic inline cache, so every call took
/// `jit_invoke_virtual_mic` and its slow path; JDK 25's
/// `Arrays.copyOfRange(v, 0, v.length)` is `v.clone()`, so every
/// `StringBuilder.toString()` of a full builder paid it.
///
/// Fast path: a live `byte[]` copied into a bump of the thread's current TLAB
/// -- the arm natives allocate small arrays with
/// (`native_array_tlab_mode`, correct under ZGC, G1 and Generational) -- and
/// its payload copied. Everything else (a null receiver, the TLAB arm off or
/// missing) takes the generic dispatcher, as the site did.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_byte_array_clone_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_byte_array_clone_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_byte_array_clone_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_byte_array_clone_direct`], run under its panic guard.
pub unsafe fn jit_byte_array_clone_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = array_clone_serve(vm, receiver) {
        return copy;
    }
    clone_virtual_dispatch(vm_ptr, receiver)
}

/// The cold arm of the virtual `clone()` helpers: the site's call, made by the
/// generic dispatcher ([`CLONE_VIRTUAL_INFO`]).
pub unsafe fn clone_virtual_dispatch(vm_ptr: i64, receiver: i64) -> i64 {
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &CLONE_VIRTUAL_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// The array fast path of the `clone()` helpers ([`jit_byte_array_clone_direct`],
/// [`jit_object_clone_virtual_direct`]): a live array of any element type,
/// copied into a bump of the thread's current TLAB -- the arm natives allocate
/// small arrays with (`native_array_tlab_mode`, correct under ZGC, G1 and
/// Generational) -- with the same class word, as `Object.clone`'s native
/// allocates it (`new_array` / `new_ref_array` of the receiver's class id). A
/// pure TLAB bump cannot collect, so the receiver cannot move between the read
/// and the copy. `None` = not served: nothing anything can see (at worst a
/// zeroed, unreferenced array in the TLAB, which the next collection drops).
///
/// j27 wave 3 (lane CLONE): every element type; it served `byte[]` only.
pub unsafe fn array_clone_serve(vm: &SharedVm, receiver: i64) -> Option<i64> {
    if crate::runtime::env_cache::native_array_tlab_mode()
        == crate::runtime::env_cache::NATIVE_ARRAY_TLAB_OFF
        || clone_vm_object_alloc_armed(vm)
    {
        return None;
    }
    // The receiver is a compiled `clone()` site's own operand, typed by the
    // verifier, so a plausibility screen and the header checks below stand in
    // for a registry lookup (`is_object_address`, a ZGC object-starts probe per
    // call).
    if !cratonvm_types::plausible_heap_pointer(receiver as u64) {
        return None;
    }
    let object = ObjectRef::from_raw(receiver as usize as *mut u8);
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let element_type = vm.mem.heap.element_type_of_validated(object);
    let length = vm.mem.heap.array_length(object);
    let class_id = vm.mem.heap.class_id_of(object);
    let (thread, _guard) = jit_thread_mut()?;
    // w2a: REFILL, not a hit-only bump. A hit-only miss (the buffer's tail is
    // shorter than the array) sends the clone down the arena-lock native road,
    // and nothing in a clone-only loop ever refills the buffer, so every clone
    // after the first buffer boundary paid ~900 ns (`WeakHashMapBench`
    // `array.clone`). The refill never collects (`refill_needs_young_room =
    // false`), so `object` still cannot move before the copy.
    let copy = crate::runtime::interpreter::tlab_alloc_array_native(
        thread,
        vm,
        class_id,
        element_type,
        length,
        true,
    )?;
    array_clone_copy(vm, object, copy, element_type, length)?;
    Some(copy.as_ptr() as i64)
}

/// j27 wave 3 (lane CLONE; parity, no switch): does a JVMTI agent listen for
/// `VMObjectAlloc`? `Object.clone`'s native posts the event for its copy
/// (`vm_object_alloc_event`), and the clone helpers' copies did not, so an
/// agent saw a clone from interpreted code and missed the same clone once
/// compiled. Armed, every clone helper declines to the native. One relaxed
/// load (`NativeContextImpl::jvmti_vm_object_alloc_armed`'s question).
///
/// j27 wave 4 (lane CLONE2; parity, no switch): `SampledObjectAlloc` too. An
/// armed sampler sends every method to the interpreter
/// (`publish_debugger_gates`), but a compiled frame already running (an OSR
/// loop) keeps calling the helper, whose TLAB bump no sampler sees; the
/// native the decline reaches allocates as the interpreter's clone does.
pub fn clone_vm_object_alloc_armed(vm: &SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        let gates = &vm.debug.debugger_gates;
        gates.native_event_armed(crate::debug::NativeEvent::VmObjectAlloc)
            || gates.native_event_armed(crate::debug::NativeEvent::SampledAlloc)
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = vm;
        false
    }
}

/// [`array_clone_serve`]'s copy of `length` elements of `src` into the fresh
/// `copy`. A primitive payload is copied raw; a reference payload goes through
/// the unchecked reference arraycopy (`try_reference_arraycopy_pair`: same
/// component class, so no store check; the collector's raw-word arm or its
/// per-element accessors, each with the barriers a store carries). `None` =
/// not copied (an auto-box payload the reference copy refuses), `copy`
/// unwritten.
pub unsafe fn array_clone_copy(
    vm: &SharedVm,
    src: ObjectRef,
    copy: ObjectRef,
    element_type: ArrayElementType,
    length: usize,
) -> Option<()> {
    if element_type == ArrayElementType::Reference {
        return try_reference_arraycopy_pair(vm, src, 0, length, copy, 0, length, length);
    }
    let bytes = length.checked_mul(cratonvm_types::element_byte_size(element_type))?;
    let from = vm.mem.heap.array_data_ptr(src)?;
    let to = vm.mem.heap.array_data_ptr(copy)?;
    // SAFETY: both are the payloads of live primitive arrays of one element
    // type and `length` elements, so `bytes` long; the copy is fresh, so the
    // two cannot overlap.
    std::ptr::copy_nonoverlapping(from as *const u8, to, bytes);
    Some(())
}

/// Synthetic call-site info for the cold arm of [`jit_object_clone_direct`]:
/// the `invokespecial java/lang/Object.clone` it stands for, through the
/// generic dispatcher (which raises the null receiver's NPE and
/// `CloneNotSupportedException`, and serves every receiver the fast path
/// declines).
pub static OBJECT_CLONE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "clone",
    descriptor: "()Ljava/lang/Object;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for a compiled `super.clone()` that reaches
/// `Object.clone` (`invokespecial java/lang/Object.clone`;
/// `CRATONVM_JIT_OBJECT_CLONE_DIRECT`). The site went through the inline-cache
/// door into the registered native (`safe_native_call`, a `NativeContext`, the
/// class questions, a slot-by-slot copy): ~1.3 us a clone against HotSpot's
/// ~0.1 us, and `TimeZone.getDefault()` clones its default zone per call --
/// once per entry time `jdk.nio.zipfs` converts.
///
/// Fast path ([`object_clone_serve`]): a plain instance whose class is
/// `Cloneable` and needs nothing but its fields copied, into a bump of the
/// thread's TLAB (the arm natives allocate with; it cannot collect), through
/// the heap's one-validation copy. Everything else takes the generic
/// dispatcher, as the site did.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_direct(vm_ptr: i64, receiver: i64) -> i64 {
    let f = || jit_object_clone_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_clone_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_object_clone_direct`], run under its panic guard.
pub unsafe fn jit_object_clone_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = object_clone_serve(vm, receiver) {
        return copy;
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &OBJECT_CLONE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// The fast path of [`jit_object_clone_direct`]; `None` = not served, nothing
/// allocated or written that anything can see.
pub unsafe fn object_clone_serve(vm: &SharedVm, receiver: i64) -> Option<i64> {
    object_clone_serve_where(vm, receiver, object_clone_is_plain)
}

/// [`object_clone_serve`] for an instance whose class `admits` answers
/// (`object_clone_is_plain` at a `super.clone()` site,
/// `object_clone_virtual_serves` at a virtual one).
pub unsafe fn object_clone_serve_where(
    vm: &SharedVm,
    receiver: i64,
    admits: fn(&SharedVm, ClassId) -> bool,
) -> Option<i64> {
    // The receiver is a compiled `clone()` site's own operand, typed by the
    // verifier, so a plausibility screen and the header checks below stand in
    // for a registry lookup (as for `byte[].clone()`); the copy validates both
    // objects once more.
    if !cratonvm_types::plausible_heap_pointer(receiver as u64) || clone_vm_object_alloc_armed(vm)
    {
        return None;
    }
    let object = ObjectRef::from_raw(receiver as usize as *mut u8);
    let object = vm.mem.heap.load_and_forward(object);
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Object {
        return None;
    }
    let class_id = vm.mem.heap.class_id_of(object);
    // j27 wave 4 (lane CLONE2): every refusal of the copy that is not about
    // this object is asked here, before the bump -- the class's in `admits`
    // (`OBJECT_CLONE_COMPACT_COPY_BIT`), the heap's per-call state below --
    // so a class the copy refuses declines with nothing allocated.
    if class_id == ClassId::new(0)
        || !admits(vm, class_id)
        || !vm.mem.heap.compact_object_copy_ready()
    {
        return None;
    }
    let slots = vm.mem.heap.get_header(object).num_slots() as usize;
    let (size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
        vm,
        class_id,
        slots,
        crate::runtime::interpreter::tlab_site::NATIVE,
    );
    if size > cratonvm_gc::tlab::tlab_max_alloc() {
        return None;
    }
    let (thread, _guard) = jit_thread_mut()?;
    // A pure bump (no refill collects), so `object` cannot move before the copy.
    let copy =
        crate::runtime::interpreter::tlab_alloc_object(thread, vm, class_id, slots, size)?;
    // `None` (now only a per-object refusal: a non-compact receiver body)
    // leaves a zeroed, unreferenced object in the TLAB: garbage the next
    // collection drops, and the dispatcher below makes the real clone.
    vm.mem.heap.copy_compact_object_fields(object, copy)?;
    Some(copy.as_ptr() as i64)
}

thread_local! {
    /// [`object_clone_class_verdict`]'s answers: `(vm identity, address
    /// epoch, class id) -> verdict bits`, direct-mapped. A `ClassId` is reused
    /// only after an unload, which happens in a collection and so ends the
    /// epoch (the native's own memo, `object_clone_class_facts`, keys the same
    /// way). A class's methods and superclass chain are fixed once it is
    /// loaded (a redefinition cannot add or remove a method), so the
    /// inheritance bit holds for the same key too.
    pub static OBJECT_CLONE_PLAIN: std::cell::Cell<[(usize, u64, u32, u8); 16]> =
        const { std::cell::Cell::new([(0, 0, 0, 0); 16]) };
}

/// [`object_clone_class_verdict`]: `Object.clone` of an instance of the class
/// copies its fields and nothing else ([`object_clone_is_plain`]).
pub const OBJECT_CLONE_PLAIN_BIT: u8 = 1;
/// [`object_clone_class_verdict`]: no class on the chain below `Object`
/// declares `clone()Ljava/lang/Object;` (a covariant override's bridge
/// included), so a virtual `clone()` of an instance selects `Object.clone`.
pub const OBJECT_CLONE_INHERITED_BIT: u8 = 2;
/// [`object_clone_class_verdict`] (j27 wave 4, lane CLONE2): the heap's
/// compact field copy covers the class (`VmHeap::compact_object_copy_admits_class`).
/// Without it `copy_compact_object_fields` refuses EVERY instance, after the
/// helper had already bumped the copy: a zeroed dead object per clone.
pub const OBJECT_CLONE_COMPACT_COPY_BIT: u8 = 4;

/// Whether a `super.clone()` of an instance of `class_id` is
/// [`object_clone_serve`]'s copy: `Object.clone` of it is its fields copied
/// and nothing else -- the class is `Cloneable` (else the native throws
/// `CloneNotSupportedException`), not a `Throwable` (the clone keeps its own
/// copy of the trace, `clone_throwable_backtrace`), without a finalizer (the
/// clone is registered), and not a `LinkedHashMap` or `Properties` (their
/// side-table overlays are copied) -- and the compact copy covers the class.
/// A class this cannot answer is not served.
pub fn object_clone_is_plain(vm: &SharedVm, class_id: ClassId) -> bool {
    const BOTH: u8 = OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_COMPACT_COPY_BIT;
    object_clone_class_verdict(vm, class_id) & BOTH == BOTH
}

/// j27 wave 3 (lane CLONE): whether a VIRTUAL `clone()` of an instance of
/// `class_id` is [`object_clone_serve`]'s copy -- [`object_clone_is_plain`],
/// and the class inherits `Object.clone` (no override on its chain). The
/// one-class answer both [`jit_object_clone_virtual_direct`] (per call) and
/// the compiler's bind (`jit_object_clone_served_class`) read, so the two
/// cannot disagree.
pub fn object_clone_virtual_serves(vm: &SharedVm, class_id: ClassId) -> bool {
    const ALL: u8 =
        OBJECT_CLONE_PLAIN_BIT | OBJECT_CLONE_INHERITED_BIT | OBJECT_CLONE_COMPACT_COPY_BIT;
    object_clone_class_verdict(vm, class_id) & ALL == ALL
}

/// The memoised verdict bits of `class_id` (see [`OBJECT_CLONE_PLAIN`]).
pub fn object_clone_class_verdict(vm: &SharedVm, class_id: ClassId) -> u8 {
    object_clone_class_verdict_by(vm, class_id, || object_clone_compute_verdict(vm, class_id))
}

/// [`object_clone_class_verdict`] with its computation `compute` injected:
/// a memo hit answers without asking; a `None` (the class manager was busy)
/// answers `0` -- not plain, not inherited: the helper declines and the
/// compiler does not bind -- and is NOT remembered, so the next ask computes.
pub fn object_clone_class_verdict_by(
    vm: &SharedVm,
    class_id: ClassId,
    compute: impl FnOnce() -> Option<u8>,
) -> u8 {
    let key = (vm.vm_identity, vm.mem.address_epoch(), class_id.as_u32());
    let line = (class_id.as_u32() as usize) & 15; // Widening: always safe
    let memo = OBJECT_CLONE_PLAIN.with(|m| m.get()[line]);
    if (memo.0, memo.1, memo.2) == key {
        return memo.3;
    }
    let Some(verdict) = compute() else {
        return 0;
    };
    OBJECT_CLONE_PLAIN.with(|m| {
        let mut lines = m.get();
        lines[line] = (key.0, key.1, key.2, verdict);
        m.set(lines);
    });
    verdict
}

/// [`object_clone_class_verdict`]'s class questions, asked of the class
/// manager, or `None` when it is busy.
///
/// j27 wave 4 (lane CLONE2): never waits. The compiler thread asks this
/// through `jit_object_clone_served_class` at a bind, where a WAITING read is
/// one queued writer away from deadlocking a compile against a class load
/// (the `jit_well_known_class_id` discipline); a Java thread asks it inside a
/// compiled helper with its frame's references unpublished. It was a
/// blocking `read()` on both. `try_read_untracked` adds no wait-for edge, so
/// it stays outside the lock-order graph.
pub fn object_clone_compute_verdict(vm: &SharedVm, class_id: ClassId) -> Option<u8> {
    let cm = vm.classes.class_manager.try_read_untracked()?;
    let sub = |name: &str| {
        cm.get_loaded_class_id(name)
            .map(|id| cm.is_subclass_of(class_id, id))
    };
    let plain = cm.class_store.get(class_id).is_some_and(|c| !c.has_finalizer)
        && sub("java/lang/Cloneable") == Some(true)
        && sub("java/lang/Throwable") != Some(true)
        && sub("java/util/LinkedHashMap") != Some(true)
        && sub("java/util/Properties") != Some(true);
    let mut verdict = if plain { OBJECT_CLONE_PLAIN_BIT } else { 0 };
    if !class_chain_overrides_clone(&cm, class_id) {
        verdict |= OBJECT_CLONE_INHERITED_BIT;
    }
    if vm.mem.heap.compact_object_copy_admits_class(class_id) {
        verdict |= OBJECT_CLONE_COMPACT_COPY_BIT;
    }
    Some(verdict)
}

/// Does a class on `class_id`'s superclass chain below `java/lang/Object`
/// declare `clone()Ljava/lang/Object;`? A bridge counts: javac emits one for a
/// covariant `Foo clone()`, and a virtual call selects it. A chain this cannot
/// walk to `Object` (an unknown class, a cut chain, more than 64 levels)
/// answers `true`, the refusing answer.
pub fn class_chain_overrides_clone(cm: &crate::classloading::ClassManager, class_id: ClassId) -> bool {
    let mut c = class_id;
    for _ in 0..64 {
        let Some(class) = cm.class_store.get(c) else {
            return true;
        };
        let Some(parent) = class.superclass else {
            // Only `java/lang/Object` has no superclass: its own `clone`.
            return false;
        };
        if class.find_method("clone", "()Ljava/lang/Object;").is_some() {
            return true;
        }
        c = parent;
    }
    true
}

/// Thin direct helper for a compiled VIRTUAL `clone()` (j27 wave 3, lane
/// CLONE; `CRATONVM_JIT_CLONE_VIRTUAL_THIN`): an array of any element type
/// ([`array_clone_serve`]; `clone()` of an array is `Object.clone`, which
/// HotSpot's compilers expand inline), or an instance whose class is plain and
/// inherits `Object.clone` ([`object_clone_virtual_serves`], then
/// [`object_clone_serve`]'s copy). Such a site took `jit_invoke_virtual_mic`'s
/// slow path into the registered native on every call (an array receiver
/// never sits in a monomorphic inline cache). Everything else -- a null
/// receiver, an override, `CloneNotSupportedException`, a finalizable class,
/// a TLAB that cannot take the copy -- is the site's own virtual call through
/// the generic dispatcher ([`CLONE_VIRTUAL_INFO`]).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_virtual_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_clone_virtual_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_clone_virtual_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_object_clone_virtual_direct`], run under its panic guard.
pub unsafe fn jit_object_clone_virtual_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    if let Some(copy) = array_clone_serve(vm, receiver) {
        return copy;
    }
    if let Some(copy) = object_clone_serve_where(vm, receiver, object_clone_virtual_serves) {
        return copy;
    }
    clone_virtual_dispatch(vm_ptr, receiver)
}

/// The compiler's one-class question for the virtual bind
/// (`DirectHelperTable::object_clone_served_class`): `1` when
/// [`jit_object_clone_virtual_direct`] serves an instance of class `class_id`
/// ([`object_clone_virtual_serves`]), else `0`. Compile time only.
///
/// SAFETY: `ctx` is the live `SharedVm` the table was published for.
pub(in crate::jit) unsafe extern "C" fn jit_object_clone_served_class(
    ctx: i64,
    class_id: i64,
) -> i64 {
    let Ok(raw) = u32::try_from(class_id) else {
        return 0;
    };
    if ctx == 0 || raw == 0 {
        return 0;
    }
    // SAFETY: as the function contract.
    let vm = &*(ctx as *const SharedVm);
    i64::from(object_clone_virtual_serves(vm, ClassId::new(raw)))
}


/// The `StringBuilder` append intrinsics' growth (strings round 2, `str17`;
/// `CRATONVM_JIT_SB_GROW`), called by the stub `emit_sb_grow_stub` emits on a
/// full LATIN1 payload. The decline edge it replaces was the generic
/// `jit_invoke_dispatch` into the JDK's `append` and from there its compiled
/// growth chain (`ensureCapacitySameCoder`, `newCapacity`, `Arrays.copyOf`):
/// ~2 400 cycles, on 29% of `R16Bench strings`' iterations.
///
/// Installs in `value` -- the reference slot `value_offset` bytes into the
/// builder, the address the inline body itself reads, passed per layout by
/// the stub -- a copy grown exactly as JDK 25's `newCapacity` grows a LATIN1
/// payload: `ArraysSupport.newLength(old, min - old, old + 2)`, i.e.
/// `max(min, 2 * old + 2)` below `SOFT_MAX_ARRAY_LENGTH`. Returns the builder;
/// the stub then runs the inline body again, which finds room and writes the
/// characters and `count`. `capacity()` observes the rule, so it is the JDK's
/// and nothing else. `maybeLatin1` is written only on UTF16 and shrinking
/// paths, so a LATIN1 growth leaves it alone, as here.
///
/// `0` = not served, nothing written: the stub takes the decline edge as
/// before. That covers a `hugeLength` size, a payload already large enough
/// (so a disagreement can never loop the stub), a slot that is not a `byte[]`,
/// an armed ZGC load barrier (colored slots), and a TLAB that cannot take the
/// copy -- a pure bump, the arm natives
/// allocate small arrays with, so nothing here can collect and the stub needs
/// no safepoint.
///
/// The builder is the inline body's own receiver, already null-checked and
/// guarded on its exact class, so it is not looked up again; the slot is
/// written as compiled code writes a reference field: the collector's SATB
/// pre-barrier on the old payload, the store, its post barrier on the new one
/// (`jit_satb_pre_write_barrier` / `jit_write_barrier`'s own calls).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_sb_grow_direct(
    vm_ptr: i64,
    builder: i64,
    value_offset: i64,
    min_capacity: i64,
) -> i64 {
    let f = || sb_grow_serve(vm_ptr, builder, value_offset, min_capacity).unwrap_or(0);
    let bits = contain("jit_sb_grow_direct", OnPanic::RecordFor { vm_ptr }, 0, f);
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// The body of [`jit_sb_grow_direct`]; `None` = not served.
pub unsafe fn sb_grow_serve(
    vm_ptr: i64,
    builder: i64,
    value_offset: i64,
    min_capacity: i64,
) -> Option<i64> {
    // `ArraysSupport.SOFT_MAX_ARRAY_LENGTH`: past it `newLength` takes its
    // `hugeLength` arm, which this does not reproduce.
    const SOFT_MAX: usize = i32::MAX as usize - 8;
    crate::jit::conservative_roots::note_jit_boundary();
    if crate::runtime::env_cache::native_array_tlab_mode()
        == crate::runtime::env_cache::NATIVE_ARRAY_TLAB_OFF
        || cratonvm_types::narrow_oop::narrow_oops_enabled()
    {
        return None;
    }
    let min = usize::try_from(min_capacity)
        .ok()
        .filter(|&m| m > 0 && m <= SOFT_MAX)?;
    // An 8-byte reference slot inside the object, naturally aligned, as the
    // layout the stub was compiled against places it.
    let offset = usize::try_from(value_offset)
        .ok()
        .filter(|&o| o >= 8 && o < 4096 && o % 8 == 0)?;
    if !cratonvm_types::plausible_heap_pointer(builder as u64) {
        return None;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let heap = &vm.mem.heap;
    // zt t33: while a ZGC load barrier is armed the slot holds a colored word
    // the raw read below would not heal, and the raw store would leave a
    // plain word no barrier ever forwards (the word R6's plan used to walk
    // the live set for). The decline edge stores through `putfield`.
    if heap.zgc_load_barrier_armed() {
        return None;
    }
    let slot = (builder as usize as *mut u8).add(offset);
    // SAFETY: `slot` is the builder's `value` reference slot (see above).
    let old_raw = read_ref_slot(slot);
    if !cratonvm_types::plausible_heap_pointer(old_raw) {
        return None;
    }
    let old = ObjectRef::from_raw(old_raw as usize as *mut u8);
    if heap.kind_of(old) != cratonvm_types::ObjectKind::Array
        || heap.element_type_of_validated(old) != ArrayElementType::Byte
    {
        return None;
    }
    let old_len = heap.array_length(old);
    if min <= old_len {
        return None;
    }
    let new_len = std::cmp::max(min, old_len.checked_mul(2)?.checked_add(2)?);
    if new_len > SOFT_MAX {
        return None;
    }
    let class_id = heap.class_id_of(old);
    let (thread, _guard) = jit_thread_mut()?;
    let grown = crate::runtime::interpreter::tlab_alloc_array_native(
        thread,
        vm,
        class_id,
        ArrayElementType::Byte,
        new_len,
        // w2a: refills without collecting (see `array_clone_serve`).
        true,
    )?;
    let src = heap.array_data_ptr(old)?;
    let dst = heap.array_data_ptr(grown)?;
    // SAFETY: `src` is the payload of a live `byte[]` of `old_len` elements
    // and `dst` that of a fresh one of `new_len > old_len`; they cannot
    // overlap. The tail is zeroed as `Arrays.copyOf` leaves it.
    std::ptr::copy_nonoverlapping(src as *const u8, dst, old_len);
    std::ptr::write_bytes(dst.add(old_len), 0, new_len - old_len);
    // zt t32 (2f): the slot-aware form.
    heap.satb_barrier_slot(slot as *const u8, Value::Object(Some(old)));
    // SAFETY: the same slot, read above.
    write_ref_slot(slot, grown.as_ptr() as u64);
    heap.write_barrier(
        ObjectRef::from_raw(builder as usize as *mut u8),
        Value::Object(Some(grown)),
    );
    Some(builder)
}

/// Synthetic call-site info for the cold arm of
/// [`jit_vectorized_hash_code_direct`]: the registered native, through the
/// generic dispatcher.
pub static VECTORIZED_HASH_CODE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/ArraysSupport",
    method_name: "vectorizedHashCode",
    descriptor: "(Ljava/lang/Object;IIII)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// HotSpot `BasicType` codes `ArraysSupport.vectorizedHashCode` switches on.
pub const VH_T_BOOLEAN: i64 = 4;
pub const VH_T_CHAR: i64 = 5;
pub const VH_T_BYTE: i64 = 8;
pub const VH_T_SHORT: i64 = 9;
pub const VH_T_INT: i64 = 10;

/// Thin direct helper for `jdk/internal/util/ArraysSupport.vectorizedHashCode
/// (Object,IIII)I` (strings round 2, `str17`;
/// `CRATONVM_JIT_VECTORIZED_HASH_DIRECT`). The callee is a registered
/// `Intrinsic` native, so compiled code reached it through
/// `jit_invoke_dispatch` -- argument decode, the native funnel, a `Value` per
/// argument -- on every `String.hashCode()` of a string not hashed before:
/// ~1 500 cycles for a 17-byte payload, most of `R16Bench strings`.
///
/// Fast path: `array` a live primitive array whose element type is the one
/// `basicType` names (or a `byte[]` of UTF-16 pairs for `T_CHAR`, as
/// `StringUTF16.hashCode` passes it) and `[fromIndex, fromIndex + length)`
/// inside it; the fold is the JDK body's (`31 * h + element`, the element
/// widened as that body widens it). Everything else falls through to the
/// generic dispatcher, so the native decides it exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_hash_code_direct(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    let f = || {
        jit_vectorized_hash_code_direct_body(
            vm_ptr,
            array,
            from_index,
            length,
            initial_value,
            basic_type,
        )
    };
    contain(
        "jit_vectorized_hash_code_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_vectorized_hash_code_direct`], run under its panic guard.
pub unsafe fn jit_vectorized_hash_code_direct_body(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The three `int` arguments arrive sign-extended; read them as `int`s.
    let (from_index, length) = (i64::from(from_index as i32), i64::from(length as i32));
    let basic_type = i64::from(basic_type as i32);
    if let Some(hash) = vectorized_hash_code_serve(
        vm,
        array,
        from_index,
        length,
        initial_value as i32,
        basic_type,
    ) {
        return i64::from(hash);
    }
    let args = [array, from_index, length, initial_value, basic_type];
    jit_invoke_dispatch(
        vm_ptr,
        &VECTORIZED_HASH_CODE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

/// JIT round 19 proposal wave 8, lane tail8 (TAIL7-19-3,
/// `CRATONVM_JIT_SP_TAIL_LEAF_HELPERS_WIDE`, opt-in): the LEAF TWIN of
/// [`jit_vectorized_hash_code_direct`]: its fast arm verbatim
/// (`vectorized_hash_code_serve`: a validated primitive array's payload folded
/// in place, "no allocation, no safepoint"), and
/// `cratonvm_jit::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT` -- outside the
/// sign-extended `int` range every hash lies in -- where the helper would
/// dispatch the registered native. The contract is
/// [`jit_preconditions_check_index_leaf`]'s.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_hash_code_leaf(
    vm_ptr: i64,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i64,
    basic_type: i64,
) -> i64 {
    const DECLINE: i64 = cratonvm_jit::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT;
    let f = || {
        // SAFETY: as in `jit_vectorized_hash_code_direct_body`.
        let vm = &*(vm_ptr as *const SharedVm);
        // The three `int` arguments arrive sign-extended; read them as `int`s.
        let (from_index, length) = (i64::from(from_index as i32), i64::from(length as i32));
        let basic_type = i64::from(basic_type as i32);
        match vectorized_hash_code_serve(
            vm,
            array,
            from_index,
            length,
            initial_value as i32,
            basic_type,
        ) {
            Some(hash) => i64::from(hash),
            None => DECLINE,
        }
    };
    contain("jit_vectorized_hash_code_leaf", OnPanic::RecordFor { vm_ptr }, DECLINE, f)
}

/// The fast path of [`jit_vectorized_hash_code_direct`]: the same validation
/// `decimal_digits_latin1_serve` makes (aligned, heap-resident, an array of
/// the expected element type), then [`vectorized_hash_fold`] over the range.
pub unsafe fn vectorized_hash_code_serve(
    vm: &SharedVm,
    array: i64,
    from_index: i64,
    length: i64,
    initial_value: i32,
    basic_type: i64,
) -> Option<i32> {
    if from_index < 0 || length < 0 {
        return None;
    }
    let raw = array as u64;
    if array == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    // `(payload bytes per array slot, payload bytes per hashed element)`.
    let (width, step) = match (vm.mem.heap.element_type_of_validated(object), basic_type) {
        (ArrayElementType::Byte, VH_T_BOOLEAN | VH_T_BYTE) => (1usize, 1usize),
        (ArrayElementType::Byte, VH_T_CHAR) => (1, 2),
        (ArrayElementType::Char, VH_T_CHAR) | (ArrayElementType::Short, VH_T_SHORT) => (2, 2),
        (ArrayElementType::Int, VH_T_INT) => (4, 4),
        _ => return None,
    };
    let payload_len = vm.mem.heap.array_length(object).checked_mul(width)?;
    let start = (from_index as usize).checked_mul(step)?;
    let end = start.checked_add((length as usize).checked_mul(step)?)?;
    if end > payload_len {
        return None;
    }
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `base` is the payload of a live primitive array of
    // `payload_len` bytes, and nothing between here and the fold can move or
    // free it (no allocation, no safepoint).
    let payload = std::slice::from_raw_parts(base as *const u8, payload_len);
    if vectorized_hash_unroll_enabled(vm) {
        return vectorized_hash_fold_unrolled(&payload[start..end], step, basic_type, initial_value);
    }
    vectorized_hash_fold(&payload[start..end], step, basic_type, initial_value)
}

/// `CRATONVM_JIT_VECTORIZED_HASH_UNROLL` (JIT round 21 wave 3, lane
/// intrinsics, INTRINSICS21-6; perf, **opt-in**): the direct
/// `ArraysSupport.vectorizedHashCode` helper folds four elements per step
/// ([`vectorized_hash_fold_unrolled`]) instead of one. Read once per VM per
/// thread (the helper serves every compiled `String.hashCode` /
/// `Arrays.hashCode` miss).
pub fn vectorized_hash_unroll_enabled(vm: &SharedVm) -> bool {
    thread_local! {
        pub static ON: std::cell::Cell<Option<(usize, bool)>> = const { std::cell::Cell::new(None) };
    }
    let id = vm.vm_identity;
    ON.with(|c| match c.get() {
        Some((owner, on)) if owner == id => on,
        _ => {
            let on =
                true;
            c.set(Some((id, on)));
            on
        }
    })
}

/// `h = 31 * h + e(i)` over `n` elements, four per step:
/// `h' = 31^4 h + 31^3 e0 + 31^2 e1 + 31 e2 + e3`. The same value as the
/// one-at-a-time fold for every input -- wrapping `i32` arithmetic is a ring,
/// so the expansion is exact mod 2^32 -- with ONE multiply-add on the loop-
/// carried chain per four elements instead of four (HotSpot's AVX2
/// `vectorizedHashCode` stub uses the same power table, eight lanes wide).
pub fn hash_fold_unrolled4(n: usize, initial_value: i32, elem: impl Fn(usize) -> i32) -> i32 {
    const P2: i32 = 31 * 31;
    const P3: i32 = 31 * 31 * 31;
    const P4: i32 = 31 * 31 * 31 * 31;
    let mut h = initial_value;
    let mut i = 0usize;
    while i + 4 <= n {
        h = h
            .wrapping_mul(P4)
            .wrapping_add(elem(i).wrapping_mul(P3))
            .wrapping_add(elem(i + 1).wrapping_mul(P2))
            .wrapping_add(elem(i + 2).wrapping_mul(31))
            .wrapping_add(elem(i + 3));
        i += 4;
    }
    while i < n {
        h = h.wrapping_mul(31).wrapping_add(elem(i));
        i += 1;
    }
    h
}

pub fn hash_fold_unrolled8(n: usize, initial_value: i32, elem: impl Fn(usize) -> i32) -> i32 {
    const P2: i32 = 31i32.wrapping_pow(2);
    const P3: i32 = 31i32.wrapping_pow(3);
    const P4: i32 = 31i32.wrapping_pow(4);
    const P5: i32 = 31i32.wrapping_pow(5);
    const P6: i32 = 31i32.wrapping_pow(6);
    const P7: i32 = 31i32.wrapping_pow(7);
    const P8: i32 = 31i32.wrapping_pow(8);
    let mut h = initial_value;
    let mut i = 0usize;
    while i + 8 <= n {
        let lo = elem(i + 4)
            .wrapping_mul(P3)
            .wrapping_add(elem(i + 5).wrapping_mul(P2))
            .wrapping_add(elem(i + 6).wrapping_mul(31))
            .wrapping_add(elem(i + 7));
        let hi = elem(i)
            .wrapping_mul(P7)
            .wrapping_add(elem(i + 1).wrapping_mul(P6))
            .wrapping_add(elem(i + 2).wrapping_mul(P5))
            .wrapping_add(elem(i + 3).wrapping_mul(P4));
        h = h.wrapping_mul(P8).wrapping_add(hi).wrapping_add(lo);
        i += 8;
    }
    while i < n {
        h = h.wrapping_mul(31).wrapping_add(elem(i));
        i += 1;
    }
    h
}

/// [`vectorized_hash_fold`]'s answer through [`hash_fold_unrolled8`]: the
/// same element widening per `(step, basic_type)`, the same `None` for a pair
/// the JDK never passes, and the same whole-element count (a trailing partial
/// element is ignored, as `chunks_exact` ignores it).
pub fn vectorized_hash_fold_unrolled(
    bytes: &[u8],
    step: usize,
    basic_type: i64,
    initial_value: i32,
) -> Option<i32> {
    let n = bytes.len() / step.max(1);
    Some(match (step, basic_type) {
        (1, VH_T_BOOLEAN) => hash_fold_unrolled8(n, initial_value, |i| i32::from(bytes[i])),
        (1, VH_T_BYTE) => hash_fold_unrolled8(n, initial_value, |i| i32::from(bytes[i] as i8)),
        (2, VH_T_CHAR) => hash_fold_unrolled8(n, initial_value, |i| {
            i32::from(u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        }),
        (2, VH_T_SHORT) => hash_fold_unrolled8(n, initial_value, |i| {
            i32::from(i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        }),
        (4, VH_T_INT) => hash_fold_unrolled8(n, initial_value, |i| {
            i32::from_le_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]])
        }),
        _ => return None,
    })
}

/// `ArraysSupport.vectorizedHashCode`'s fold over `bytes` (whole elements of
/// `step` bytes, little-endian): `h = 31 * h + element`, the element widened
/// as the JDK body widens it -- `T_BOOLEAN` an unsigned byte
/// (`unsignedHashCode`), `T_BYTE` a signed byte, `T_CHAR` an unsigned
/// 16-bit unit, `T_SHORT` a signed one, `T_INT` the word. `None` for a
/// `(step, basic_type)` pair the JDK never passes.
pub fn vectorized_hash_fold(
    bytes: &[u8],
    step: usize,
    basic_type: i64,
    initial_value: i32,
) -> Option<i32> {
    let mut h = initial_value;
    match (step, basic_type) {
        (1, VH_T_BOOLEAN) => {
            for &b in bytes {
                h = h.wrapping_mul(31).wrapping_add(i32::from(b));
            }
        }
        (1, VH_T_BYTE) => {
            for &b in bytes {
                h = h.wrapping_mul(31).wrapping_add(i32::from(b as i8));
            }
        }
        (2, VH_T_CHAR) => {
            for c in bytes.chunks_exact(2) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from(u16::from_le_bytes([c[0], c[1]])));
            }
        }
        (2, VH_T_SHORT) => {
            for c in bytes.chunks_exact(2) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from(i16::from_le_bytes([c[0], c[1]])));
            }
        }
        (4, VH_T_INT) => {
            for c in bytes.chunks_exact(4) {
                h = h
                    .wrapping_mul(31)
                    .wrapping_add(i32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        _ => return None,
    }
    Some(h)
}

/// Synthetic call-site info for the cold arm of
/// [`jit_vectorized_mismatch_direct`]: the registered native, through the
/// generic dispatcher. A `const`, not a `static` (the per-VM statics ratchet):
/// the one `&` below is promoted to a single immutable allocation.
pub const VECTORIZED_MISMATCH_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/util/ArraysSupport",
    method_name: "vectorizedMismatch",
    descriptor: "(Ljava/lang/Object;JLjava/lang/Object;JII)I",
    num_jit_args: 6,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// `Unsafe.arrayBaseOffset` of every primitive array in this VM
/// (`native_unsafe_array_base_offset`): the JDK passes `vectorizedMismatch`
/// offsets measured from the object as if its header were HotSpot's 16 bytes.
pub const VECTORIZED_MISMATCH_UNSAFE_ABASE: i64 = 16;

/// Thin direct helper for `jdk/internal/util/ArraysSupport.vectorizedMismatch
/// (Object,long,Object,long,int,int)int` (JIT round 22 wave 1, lane intrinsics,
/// INTRINSICS21-1; `CRATONVM_JIT_VECTORIZED_MISMATCH_DIRECT`, opt-in). The
/// callee is a registered `Intrinsic` native, so compiled code reached it
/// through `jit_invoke_dispatch` on every `Arrays.equals` / `mismatch` /
/// `compare` range and every buffer compare.
///
/// Fast path ([`vectorized_mismatch_serve`]): two live on-heap primitive arrays
/// whose element width is `1 << log2ArrayIndexScale`, element-aligned Unsafe
/// offsets at or past the base, `length > 0` elements inside both. The answer
/// is the index of the first element whose BYTES differ, or `-1` -- HotSpot's
/// memory compare, and the native's strict answer (a float / double pair
/// differs exactly when its bits do). Everything else -- a null base (off-heap
/// memory), a typed view over a `byte[]`, a range the helper cannot prove,
/// `length <= 0` -- falls through to the generic dispatcher, so the native
/// decides it exactly as it does today.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit) unsafe extern "C" fn jit_vectorized_mismatch_direct(
    vm_ptr: i64,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> i64 {
    let f = || {
        jit_vectorized_mismatch_direct_body(vm_ptr, a, a_offset, b, b_offset, length, log2_scale)
    };
    contain(
        "jit_vectorized_mismatch_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_vectorized_mismatch_direct`], run under its panic guard.
#[allow(clippy::too_many_arguments)]
pub unsafe fn jit_vectorized_mismatch_direct_body(
    vm_ptr: i64,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The two `int` arguments arrive sign-extended; read them as `int`s.
    let (length32, log2_32) = (i64::from(length as i32), i64::from(log2_scale as i32));
    if let Some(answer) =
        vectorized_mismatch_serve(vm, a, a_offset, b, b_offset, length32, log2_32)
    {
        return i64::from(answer);
    }
    let args = [a, a_offset, b, b_offset, length, log2_scale];
    jit_invoke_dispatch(
        vm_ptr,
        &VECTORIZED_MISMATCH_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        6,
    )
}

/// The payload of `array` when it is a live on-heap primitive array whose
/// element width is `scale`: `(first payload byte, payload bytes)`. `None`
/// for anything else (null, misaligned or non-heap word, a reference array, a
/// width mismatch -- the typed-view shape the native's raw-bytes path owns).
pub unsafe fn vectorized_mismatch_payload(
    vm: &SharedVm,
    array: i64,
    scale: usize,
) -> Option<(*const u8, usize)> {
    let raw = array as u64;
    if array == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let width = match vm.mem.heap.element_type_of_validated(object) {
        ArrayElementType::Boolean | ArrayElementType::Byte => 1usize,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        ArrayElementType::Reference => return None,
    };
    if width != scale {
        return None;
    }
    let bytes = vm.mem.heap.array_length(object).checked_mul(width)?;
    let base = vm.mem.heap.array_data_ptr(object)?;
    Some((base as *const u8, bytes))
}

/// The fast path of [`jit_vectorized_mismatch_direct`]: `Some(index | -1)` for
/// the shape it serves (see that function), `None` to hand the call to the
/// native.
pub unsafe fn vectorized_mismatch_serve(
    vm: &SharedVm,
    a: i64,
    a_offset: i64,
    b: i64,
    b_offset: i64,
    length: i64,
    log2_scale: i64,
) -> Option<i32> {
    if length <= 0 || !(0..=3).contains(&log2_scale) {
        return None;
    }
    let scale = 1usize << log2_scale;
    // An Unsafe offset at or past the base and a whole number of elements past
    // it; anything else is a shape no JDK caller passes, and the native decides.
    let payload_start = |off: i64| -> Option<usize> {
        let rel = usize::try_from(off.checked_sub(VECTORIZED_MISMATCH_UNSAFE_ABASE)?).ok()?;
        (rel % scale == 0).then_some(rel)
    };
    let a_start = payload_start(a_offset)?;
    let b_start = payload_start(b_offset)?;
    let (a_base, a_bytes) = vectorized_mismatch_payload(vm, a, scale)?;
    let (b_base, b_bytes) = vectorized_mismatch_payload(vm, b, scale)?;
    let n_bytes = usize::try_from(length).ok()?.checked_mul(scale)?;
    if a_start.checked_add(n_bytes)? > a_bytes || b_start.checked_add(n_bytes)? > b_bytes {
        return None;
    }
    // SAFETY: both ranges lie inside the payload of a live primitive array
    // (checked just above), and nothing between the validation and the
    // compare can move or free either array (no allocation, no safepoint).
    let x = std::slice::from_raw_parts(a_base.add(a_start), n_bytes);
    let y = std::slice::from_raw_parts(b_base.add(b_start), n_bytes);
    Some(match first_differing_byte(x, y) {
        // `n_bytes / scale == length`, an `int`: the index fits.
        Some(at) => i32::try_from(at / scale).ok()?,
        None => -1,
    })
}

/// The index of the first byte at which `x` and `y` differ (over the shorter
/// length), or `None`: eight bytes per step (the lowest set bit of the XOR of
/// two little-endian words is the first differing byte), then one at a time.
pub fn first_differing_byte(x: &[u8], y: &[u8]) -> Option<usize> {
    let n = x.len().min(y.len());
    let mut i = 0usize;
    while i + 8 <= n {
        let mut wx = [0u8; 8];
        let mut wy = [0u8; 8];
        wx.copy_from_slice(&x[i..i + 8]);
        wy.copy_from_slice(&y[i..i + 8]);
        let d = u64::from_le_bytes(wx) ^ u64::from_le_bytes(wy);
        if d != 0 {
            return Some(i + (d.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < n {
        if x[i] != y[i] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Synthetic call-site infos for the cold arms of the three zip-checksum thin
/// helpers below: each names its own site's method, so a decline runs exactly
/// what the site would have run (the JDK's range-checking `CRC32.updateBytes`
/// included). `const`s, not `static`s (the per-VM statics ratchet).
pub const CRC32_UPDATE_BYTES0_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/CRC32",
    method_name: "updateBytes0",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub const CRC32_UPDATE_BYTES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/CRC32",
    method_name: "updateBytes",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub const ADLER32_UPDATE_BYTES_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/zip/Adler32",
    method_name: "updateBytes",
    descriptor: "(I[BII)I",
    num_jit_args: 4,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct helper for `java/util/zip/CRC32.updateBytes0(I[BII)I` (JIT
/// round 22 wave 2, lane intrinsics, INTRINSICS22-2;
/// `CRATONVM_JIT_ZIP_CHECKSUM_DIRECT`, opt-in). Fast path
/// ([`zip_checksum_payload`]): `b` a live `byte[]` and `[off, off + len)`
/// inside it; the answer is zlib's CRC-32 over that range in the JDK's public
/// representation (`cratonvm_native_builtins::zip_real::zlib_crc32`, the
/// native's own function), with no copy and no native funnel. Everything else
/// goes to the registered native through the generic dispatcher.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_crc32_update_bytes0_direct(
    vm_ptr: i64,
    crc: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, crc, b, off, len, &CRC32_UPDATE_BYTES0_INFO, false)
    };
    contain("jit_crc32_update_bytes0_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// [`jit_crc32_update_bytes0_direct`] for the `CRC32.updateBytes` wrapper
/// (registered over the JDK's range-checking body in `--compatible`): the
/// same answer for an in-range call; any other call is the site's own method.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_crc32_update_bytes_direct(
    vm_ptr: i64,
    crc: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, crc, b, off, len, &CRC32_UPDATE_BYTES_INFO, false)
    };
    contain("jit_crc32_update_bytes_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// [`jit_crc32_update_bytes0_direct`]'s Adler-32 twin for
/// `java/util/zip/Adler32.updateBytes(I[BII)I`
/// (`cratonvm_native_builtins::zip_real::zlib_adler32`).
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_adler32_update_bytes_direct(
    vm_ptr: i64,
    adler: i64,
    b: i64,
    off: i64,
    len: i64,
) -> i64 {
    let f = || {
        jit_zip_checksum_direct_body(vm_ptr, adler, b, off, len, &ADLER32_UPDATE_BYTES_INFO, true)
    };
    contain("jit_adler32_update_bytes_direct", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of the three zip-checksum helpers, run under their panic guards.
pub unsafe fn jit_zip_checksum_direct_body(
    vm_ptr: i64,
    checksum: i64,
    b: i64,
    off: i64,
    len: i64,
    info: &'static JitInvokeInfo,
    adler: bool,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM,
    // and the fall-through below already passes it to `jit_invoke_dispatch`.
    let vm = &*(vm_ptr as *const SharedVm);
    // The three `int` arguments arrive sign-extended; read them as `int`s.
    let sum = checksum as i32 as u32;
    if let Some(bytes) = zip_checksum_payload(vm, b, off as i32, len as i32) {
        let v = if adler {
            cratonvm_native_builtins::zip_real::zlib_adler32(sum, bytes)
        } else {
            cratonvm_native_builtins::zip_real::zlib_crc32(sum, bytes)
        };
        // The `int` result, sign-extended as every compiled `int` is.
        return i64::from(v as i32);
    }
    let args = [checksum, b, off, len];
    jit_invoke_dispatch(
        vm_ptr,
        info as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        4,
    )
}

/// `b[off..off + len]` when `b` is a live `byte[]` (the validation
/// `decimal_digits_latin1_serve` makes) and the range lies inside it; `None`
/// otherwise (null, not a `byte[]`, a negative `off` / `len`, past the end).
/// The slice stays valid while the caller runs no allocation and no safepoint.
pub unsafe fn zip_checksum_payload<'a>(
    vm: &'a SharedVm,
    b: i64,
    off: i32,
    len: i32,
) -> Option<&'a [u8]> {
    let raw = b as u64;
    if b == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return None;
    }
    let object = vm.mem.heap.is_object_address(raw as usize)?;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array
        || vm.mem.heap.element_type_of_validated(object) != ArrayElementType::Byte
    {
        return None;
    }
    let off = usize::try_from(off).ok()?;
    let len = usize::try_from(len).ok()?;
    if off.checked_add(len)? > vm.mem.heap.array_length(object) {
        return None;
    }
    let base = vm.mem.heap.array_data_ptr(object)?;
    // SAFETY: `[off, off + len)` lies inside the payload of a live `byte[]`
    // (checked just above).
    Some(std::slice::from_raw_parts((base as *const u8).add(off), len))
}


/// Thin direct-call target for JIT `invokestatic
/// java/lang/ref/Reference.reachabilityFence(Object)` sites (registered into
/// `cratonvm_jit::REACHABILITY_FENCE_DIRECT_FN` by `build_helpers`).
///
/// Second on the same census — 3 200 000 calls, ~2 per `ByteBuffer` accessor —
/// and its registered body is `black_box(args.first()); Ok(None)`, i.e. it does
/// nothing but be opaque. Paying ~160 ns of generic native funnel for that is
/// pure loss.
///
/// **Deliberately still a CALL, not an elision.** HotSpot intrinsifies
/// `reachabilityFence` to no instructions at all, but it can afford to: its
/// compiler models the fence as a liveness constraint, so the referent stays in
/// the oop map without any code. This JIT has no such model, and the ONE thing
/// the method exists for is keeping the argument reachable across a region
/// where the compiler would otherwise consider it dead. Emitting nothing would
/// silently delete that guarantee, and the failure — an object collected while
/// a native still holds its address — is unreproducible and catastrophic.
/// Passing the reference to an opaque `extern "C"` function keeps it live in
/// the argument register and on the conservative scan, exactly as the
/// registered native did, while removing the funnel. ~160 ns becomes the cost
/// of a direct `CALL`.
///
/// Returns `0`, never the `i64::MIN` sentinel. The site is a `JitDirectCall`
/// with return type `V`, and that sequence still compares RAX against
/// `i64::MIN` after the CALL; a `()` return left the compare reading whatever
/// this body last put in RAX.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_reachability_fence_direct(
    _vm_ptr: i64,
    referent: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    // The whole contract: be opaque about `referent` so nothing upstream may
    // conclude it is dead. `black_box` is what the registered native used.
    let _ = std::hint::black_box(referent);
    0
}

/// Synthetic call-site info for [`jit_object_get_class_direct`]'s decline edge.
///
/// A `const`, not a `static`: an immutable call descriptor is not process
/// state, and the one expression that takes its address in the helper below is
/// promoted to a single `'static` location, so the address the dispatcher sees
/// is stable. (`vm/tests/per_vm_state_statics_ratchet.rs` is frozen; this adds
/// no line to it.) `invoke_kind: 0` and `java/lang/Object`: the dispatcher
/// re-selects on the RECEIVER, exactly as the site would have without the bind.
pub const OBJECT_GET_CLASS_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "getClass",
    descriptor: "()Ljava/lang/Class;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokevirtual Object.getClass()` sites (JIT
/// round 17 wave 1, lane nio, NIO2-2; bound only under
/// `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT=1`).
///
/// # What it answers, and why that is the native's answer
///
/// `Object.getClass` is `ACC_NATIVE`, so there is no bytecode to shadow; the
/// registered body is `native_object_get_class`. Its common arm, for a plain
/// (non-array) object, is `get_class_mirror(display_id)` where `display_id` is
/// the stamped class unless `getclass_display_class_id` remaps it. This helper
/// answers ONLY when that remap is already memoised as "the stamped class"
/// (`getclass_display_is_cached_stamp`, the same per-thread memo the native
/// fills) and the class's mirror already exists in `class_mirrors` (the same
/// map `get_or_create_class_mirror` reads first). Those are exactly the
/// conditions of the SBR2 CALLER `LeafNativeKind::ObjectGetClass` arm, which
/// has served the MIC route since 2026-09-30; this helper removes the MIC round
/// trip in front of it.
///
/// Everything else -- an array (the native builds the array class's mirror),
/// a receiver whose class the memo has not classified yet or remaps, a class
/// with no mirror yet, a receiver that fails the heap screen -- declines to
/// `jit_invoke_dispatch`, which runs the registered native: the first call per
/// class and thread fills the memo, so the next one is served here.
///
/// A null receiver records the JEP 358 receiver action and returns the
/// sentinel, as every sibling helper and the generic dispatcher do, so the NPE
/// message is the one `invokevirtual` raises (HotSpot's `Cannot invoke
/// "Object.getClass()" because ... is null`).
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_object_get_class_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_object_get_class_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_object_get_class_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// nio20 L3 (NIO19-5): `class_id`'s mirror from `class_mirrors`, for a class
/// the CALLER has already shown to display as its stamp
/// (`getclass_display_is_cached_stamp`) -- and, while the read lock is still
/// held, that same mirror published as the class's compiled-`getClass()` word
/// (`ClassRealm::getclass_mirror_words`), so the next compiled `getClass()` of
/// this class answers with two loads instead of reaching here. Holding the
/// read lock across the publish is what keeps the word equal to the map: every
/// writer that moves or removes a mirror rewrites the word under the WRITE
/// lock. A no-op publish until a compile has allocated the table.
pub fn stamped_class_mirror_publishing_word(
    vm: &SharedVm,
    class_id: crate::classloading::ClassId,
) -> Option<ObjectRef> {
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&class_id).copied()?;
    vm.classes.getclass_mirror_words.publish(class_id, mirror);
    Some(mirror)
}

/// nio21 wave 1, lane L5 (W4-L2-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the mirror of the
/// one-dimensional primitive array class of `elem`, exactly as
/// `native_object_get_class`'s array arm answers a primitive array (its
/// reference-only arms -- lambda components, `strict_array_class_id`, the
/// per-loader synthetic mirror -- never apply): the unique class named `[I`
/// & co. (`class_id_by_name` is `find_unique_class_by_name`), then its entry in
/// `class_mirrors` (what `get_or_create_class_mirror` reads first). `None`
/// when either is absent -- the native then loads / creates it, and the next
/// call is served here. While the `class_mirrors` read lock is held the mirror
/// is published as primitive-array band `slot`'s word
/// (`GetClassMirrorWords::publish_prim_array`), so compiled `getClass()` of
/// such an array answers with one tag-byte screen and one load.
pub fn prim_array_class_mirror_publishing_word(
    vm: &SharedVm,
    elem: cratonvm_types::ArrayElementType,
    slot: usize,
) -> Option<ObjectRef> {
    use cratonvm_types::ArrayElementType as A;
    let name = match elem {
        A::Boolean => "[Z",
        A::Char => "[C",
        A::Float => "[F",
        A::Double => "[D",
        A::Byte => "[B",
        A::Short => "[S",
        A::Int => "[I",
        A::Long => "[J",
        A::Reference => return None,
    };
    let array_class_id = vm
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name(name)?;
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&array_class_id).copied()?;
    vm.classes
        .getclass_mirror_words
        .publish_prim_array(slot, array_class_id, mirror);
    Some(mirror)
}

/// nio21 wave 2, lane L5 (L5W1-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the mirror of the
/// array class over the reference component `component` (the header class id
/// of a reference array), when that answer is provably what
/// `native_object_get_class` returns for EVERY array over `component`, now
/// and later -- and, under the `class_mirrors` read lock, that mirror
/// published as the component's reference-array word.
///
/// # Why the conditions make it the native's answer in both modes
///
/// Let `name` be `[L<component>;` (or `[<component>` for an array component)
/// and `arr` the class filed under `(Bootstrap, name)` -- where
/// `synthesize_array_class_for_loader` files every array over a built-in
/// loader's component. Required: `component` is below the band (so no lambda
/// proxy: the native's lambda arm never applies), has a built-in defining
/// loader (`loader_id < 3`: the native's per-loader synthetic-mirror arm
/// never applies), `arr` is the UNIQUE class of that name, and `arr`'s
/// recorded component is `component`. Then the native's reference arms:
///
/// * `--compatible`: `strict_array_class_id` sees `Unique(arr)` with a
///   matching component and answers `None`; the by-name route
///   (`class_id_by_name` = `find_unique_class_by_name`) answers `arr`. Should
///   a user loader later define a same-named array, the name turns ambiguous
///   and `array_class_id_for_loader(name, <built-in>)` probes the bootstrap
///   key first: `arr` again.
/// * `--real-jdk`: before any defining loader exists, the by-name route
///   (`arr`); after, `array_class_id_for_loader`'s bootstrap probe (`arr`).
///
/// Bootstrap / platform / application classes are never unloaded, so `arr`
/// stays the answer; the word's mirror follows `arr`'s through the
/// collector's re-key / forget of `arr` (the owner map). Anything else -- a
/// user-loader component, a lambda component, an absent or ambiguous array
/// class, an absent mirror -- returns `None` and the call declines to the
/// native, as before.
pub fn ref_array_class_mirror_publishing_word(
    vm: &SharedVm,
    component: crate::classloading::ClassId,
) -> Option<ObjectRef> {
    if !cratonvm_jit::ir_lower::getclass_ref_array_words_enabled() {
        return None;
    }
    let cap = cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY;
    if usize::try_from(component.as_u32()).ok()? >= cap {
        return None;
    }
    let array_class_id = {
        let cm = vm.classes.class_manager.read();
        let loader = cm.get_loader_id(component)?;
        if loader.to_native_id() >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED
            || matches!(loader, cratonvm_types::ClassLoaderId::UserDefined(_))
        {
            return None;
        }
        let comp_name = std::sync::Arc::clone(&cm.get_class(component)?.name);
        let name = if comp_name.starts_with('[') {
            format!("[{comp_name}")
        } else {
            format!("[L{comp_name};")
        };
        let unique = cm.find_unique_class_by_name(&name)?;
        let boot =
            cm.loaded_class_under_exact_key(&name, cratonvm_types::ClassLoaderId::Bootstrap)?;
        if unique != boot {
            return None;
        }
        let recorded = cm
            .get_class(unique)?
            .array_info
            .as_ref()
            .map(|ai| ai.component_class_id)?;
        if recorded != component {
            return None;
        }
        unique
    };
    if usize::try_from(array_class_id.as_u32()).ok()? >= cap {
        return None;
    }
    let mirrors = vm.classes.class_mirrors.read();
    let mirror = mirrors.get(&array_class_id).copied()?;
    vm.classes
        .getclass_mirror_words
        .publish_ref_array(component, array_class_id, mirror);
    Some(mirror)
}

/// Body of [`jit_object_get_class_direct`], run under its panic guard.
pub unsafe fn jit_object_get_class_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    // nio20 wave 4, lane L2: why this call was not answered here, for the
    // `CRATONVM_DBG_JITC` census below (a compiled prefix HIT never reaches
    // this helper, so every line here is a prefix miss or an unprefixed site).
    let mut decline = "not-an-object-address";
    let mut decline_cid: u32 = 0;
    // Shape screen before any header read, as `jit_buffer_session_direct` does.
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            let object = vm.mem.heap.load_and_forward(object);
            decline = "array";
            let kind = vm.mem.heap.kind_of(object);
            // nio21 wave 1, lane L5 (W4-L2-1): a one-dimensional PRIMITIVE
            // array is answered (and its word published) here; nio21 wave 2
            // L5: so is a REFERENCE array over a built-in loader's component
            // whose array class is unambiguous
            // (`ref_array_class_mirror_publishing_word`); any other reference
            // array still declines to the native (loader / strict-mode arms).
            if kind == cratonvm_types::ObjectKind::Array
                && cratonvm_jit::ir_lower::getclass_ref_array_words_enabled()
                && vm.mem.heap.element_type_of(object)
                    == cratonvm_types::ArrayElementType::Reference
            {
                let component = vm.mem.heap.class_id_of(object);
                decline_cid = component.as_u32();
                decline = "reference-array-not-publishable";
                if let Some(mirror) = ref_array_class_mirror_publishing_word(vm, component) {
                    if let Some((thread, _guard)) = jit_thread_mut() {
                        thread.native_pending_return = Some(mirror);
                    }
                    return mirror.as_ptr() as i64;
                }
            }
            if kind == cratonvm_types::ObjectKind::Array
                && cratonvm_jit::ir_lower::getclass_prim_array_words_enabled()
            {
                let elem = vm.mem.heap.element_type_of(object);
                if let Some(slot) = cratonvm_jit::ir_lower::getclass_prim_array_slot(elem) {
                    decline = "prim-array-class-or-mirror-absent";
                    if let Some(mirror) = prim_array_class_mirror_publishing_word(vm, elem, slot) {
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            thread.native_pending_return = Some(mirror);
                        }
                        return mirror.as_ptr() as i64;
                    }
                } else if decline == "array" {
                    decline = "reference-array";
                }
            }
            if kind == cratonvm_types::ObjectKind::Object {
                let cid = vm.mem.heap.class_id_of(object);
                decline_cid = cid.as_u32();
                decline = "display-not-memoised-as-stamp";
                let vid = vm.vm_identity;
                if cratonvm_native_builtins::getclass_display_is_cached_stamp(vid, cid) {
                    decline = "no-mirror";
                    let mirror = stamped_class_mirror_publishing_word(vm, cid);
                    if let Some(mirror) = mirror {
                        if crate::runtime::env_cache::dbg_jitc() {
                            if let Some((served, declined)) =
                                vm.classes.getclass_mirror_words.note_helper_call(true)
                            {
                                eprintln!(
                                    "[cratonvm-jitc] getclass-helper served={served} \
                                     declined={declined} (last served class_id={} \
                                     word={:#x}; a published word that still reaches \
                                     here is a site with no prefix, or an id past the \
                                     table)",
                                    cid.as_u32(),
                                    vm.classes.getclass_mirror_words.get(cid),
                                );
                            }
                        }
                        // Object-return handoff root, as the leaf arm and
                        // `jit_thread_current_thread_direct` record it. The
                        // mirror is rooted by `class_mirrors` already.
                        if let Some((thread, _guard)) = jit_thread_mut() {
                            thread.native_pending_return = Some(mirror);
                        }
                        return mirror.as_ptr() as i64;
                    }
                }
            }
        }
    }
    if crate::runtime::env_cache::dbg_jitc() {
        if let Some((served, declined)) = vm.classes.getclass_mirror_words.note_helper_call(false) {
            eprintln!(
                "[cratonvm-jitc] getclass-helper served={served} declined={declined} \
                 (last decline: {decline} class_id={decline_cid})"
            );
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &OBJECT_GET_CLASS_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_identity_hash_code_direct`]'s decline
/// edge (a `const`, as [`OBJECT_GET_CLASS_INFO`] and for its reason).
pub const IDENTITY_HASH_CODE_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/System",
    method_name: "identityHashCode",
    descriptor: "(Ljava/lang/Object;)I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `invokestatic System.identityHashCode(Object)`
/// sites (W2A; bound unless `CRATONVM_JIT_IDENTITY_HASH_DIRECT=0`).
///
/// The compiled header-read prefix answers an object that already holds a hash;
/// what reaches this helper is the first call per object. That used to run the
/// full native funnel (dispatch, lean-door argument decode, the leaf callback,
/// return coercion) around ~25 ns of real work. This helper runs the leaf's own
/// steps (`NativeContext::identity_hash_code_installed`: the class-table note,
/// then the header read or lazy mint-and-CAS; no allocation, lock or GC point)
/// on the raw word, and declines to `jit_invoke_dispatch` for anything else: a
/// word that is not a heap object, or an object the header cannot answer for (a
/// monitor-held hash), which the registered native resolves as before.
///
/// `null` answers `0` as `System.identityHashCode` does.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_identity_hash_code_direct(
    vm_ptr: i64,
    object: i64,
) -> i64 {
    let f = || jit_identity_hash_code_direct_body(vm_ptr, object);
    contain(
        "jit_identity_hash_code_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_identity_hash_code_direct`], run under its panic guard.
pub unsafe fn jit_identity_hash_code_direct_body(vm_ptr: i64, object: i64) -> i64 {
    use cratonvm_native_api::NativeHeapAccess as _;
    crate::jit::conservative_roots::note_jit_boundary();
    if object == 0 {
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = object as u64;
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some(obj) = vm.mem.heap.is_object_address(raw as usize) {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let obj = vm.mem.heap.load_and_forward(obj);
                let ctx = crate::vm::NativeContextImpl { shared: vm, thread };
                if let Some(hash) = ctx.identity_hash_code_installed(obj) {
                    return i64::from(hash);
                }
            }
        }
    }
    let args = [object];
    jit_invoke_dispatch(
        vm_ptr,
        &IDENTITY_HASH_CODE_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_class_get_name_direct`]'s decline edge
/// (a `const`, as [`OBJECT_GET_CLASS_INFO`] and for its reason). `java/lang/Class`
/// is final, so the statically bound kind is the site's own call.
pub const CLASS_GET_SUPERCLASS_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Class",
    method_name: "getSuperclass",
    descriptor: "()Ljava/lang/Class;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub(in crate::jit) unsafe extern "C" fn jit_class_get_superclass_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let raw = receiver as u64;
    if raw != 0 && (raw & 0x7) == 0 && raw < (1u64 << 48) {
        let vm = &*(vm_ptr as *const SharedVm);
        if let Some(super_ptr) = vm.classes.mirror_fast_table.lookup_super(raw as usize) {
            return super_ptr as i64;
        }
        let raw_mirror = unsafe { ObjectRef::from_raw(raw as *mut u8) };
        if let Some(cid) = crate::vm::class_id_from_mirror(vm, raw_mirror) {
            if cid.as_u32() < 1_000_000 {
                if let Some((is_iface, super_opt)) = vm.classes.class_meta_words.get(cid) {
                    if is_iface {
                        vm.classes.mirror_fast_table.fill_super(raw as usize, 0);
                        return 0;
                    }
                    match super_opt {
                        None => {
                            vm.classes.mirror_fast_table.fill_super(raw as usize, 0);
                            return 0;
                        }
                        Some(super_cid) => {
                            let ptr = vm.classes.getclass_mirror_words.get(super_cid);
                            if ptr != 0 {
                                vm.classes.mirror_fast_table.fill_super(raw as usize, ptr);
                                return ptr as i64;
                            }
                        }
                    }
                }
            }
        }
    }
    let f = || jit_class_get_superclass_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_class_get_superclass_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

pub unsafe fn jit_class_get_superclass_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        let raw_mirror = unsafe { ObjectRef::from_raw(raw as *mut u8) };
        if let Some(cid) = crate::vm::class_id_from_mirror(vm, raw_mirror) {
            if cid.as_u32() < 1_000_000 {
                if let Some((is_iface, super_opt)) = vm.classes.class_meta_words.get(cid) {
                    if is_iface {
                        vm.classes.mirror_fast_table.fill_super(raw as usize, 0);
                        return 0;
                    }
                    match super_opt {
                        None => {
                            vm.classes.mirror_fast_table.fill_super(raw as usize, 0);
                            return 0;
                        }
                        Some(super_cid) => {
                            let ptr = vm.classes.getclass_mirror_words.get(super_cid);
                            if ptr != 0 {
                                vm.classes.mirror_fast_table.fill_super(raw as usize, ptr);
                                return ptr as i64;
                            }
                        }
                    }
                }
            }
        }
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            let mirror = vm.mem.heap.load_and_forward(object);
            if let Some((thread, _guard)) = jit_thread_mut() {
                let answer = {
                    let ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_class::class_get_superclass_fast(&ctx, mirror)
                };
                if let Some(super_opt) = answer {
                    return match super_opt {
                        Some(parent) => {
                            vm.classes.mirror_fast_table.fill_super(raw as usize, parent.as_ptr() as usize);
                            thread.native_pending_return = Some(parent);
                            parent.as_ptr() as i64
                        }
                        None => {
                            vm.classes.mirror_fast_table.fill_super(raw as usize, 0);
                            0
                        }
                    };
                }
            }
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &CLASS_GET_SUPERCLASS_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

pub const CLASS_GET_NAME_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Class",
    method_name: "getName",
    descriptor: "()Ljava/lang/String;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 1,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `java/lang/Class.getName()` sites (JIT round
/// 22 wave 2, lane runtime, RUNTIME22-2; bound unless
/// `CRATONVM_JIT_CLASS_GET_NAME_DIRECT=0`: default ON since R22-AB-FLIP).
///
/// `Class.getName()` is the VM's reviewed leaf `Intrinsic` (its JDK body
/// cannot run: this VM's mirrors hold the INTERNAL name in `Class.name`), and
/// on a warm class its whole answer is a memo hit
/// (`lang_class::native_class_get_name_leaf`). Compiled code reached that hit
/// through the dispatch funnel and the leaf door, ~150-500 ns against
/// HotSpot's field load. This helper asks the same two memo arms directly
/// (`lang_class::class_get_name_fast`) and answers the same, strongly held
/// `String`; every miss declines to `jit_invoke_dispatch` with the site's
/// triple, which runs the registered native as before (and fills the memo for
/// the next call). A null receiver records the JEP 358 receiver action and
/// returns the sentinel, as [`jit_object_get_class_direct`] does.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_class_get_name_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let raw = receiver as u64;
    if raw != 0 && (raw & 0x7) == 0 && raw < (1u64 << 48) {
        let vm = &*(vm_ptr as *const SharedVm);
        if let Some(name_ptr) = vm.classes.mirror_fast_table.lookup_name(raw as usize) {
            return name_ptr as i64;
        }
        let vm_id = vm.vm_identity;
        let epoch = vm.mem.heap.address_epoch();
        if let Some(name_ptr) = cratonvm_native_builtins::lang_class::class_get_name_mirror_memo_hit_raw(vm_id, raw as usize, epoch) {
            vm.classes.mirror_fast_table.fill_name(raw as usize, name_ptr);
            return name_ptr as i64;
        }
    }
    let f = || jit_class_get_name_direct_body(vm_ptr, receiver);
    let bits = contain(
        "jit_class_get_name_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_class_get_name_direct`], run under its panic guard.
pub unsafe fn jit_class_get_name_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        let vm_id = vm.vm_identity;
        let epoch = vm.mem.heap.address_epoch();
        if let Some(name_ptr) = cratonvm_native_builtins::lang_class::class_get_name_mirror_memo_hit_raw(vm_id, raw as usize, epoch) {
            vm.classes.mirror_fast_table.fill_name(raw as usize, name_ptr);
            return name_ptr as i64;
        }
        if let Some(object) = vm.mem.heap.is_object_address(raw as usize) {
            let mirror = vm.mem.heap.load_and_forward(object);
            if let Some((thread, _guard)) = jit_thread_mut() {
                let answer = {
                    let ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    cratonvm_native_builtins::lang_class::class_get_name_fast(&ctx, mirror)
                };
                if let Some(name) = answer {
                    vm.classes.mirror_fast_table.fill_name(raw as usize, name.as_ptr() as usize);
                    // Object-return handoff root, as the getClass helper
                    // records its mirror; the memo already holds `name`.
                    thread.native_pending_return = Some(name);
                    return name.as_ptr() as i64;
                }
            }
        }
    }
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &CLASS_GET_NAME_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

/// Synthetic call-site info for [`jit_buffer_session_direct`]'s decline edge.
///
/// `class_name` is the abstract base rather than a concrete buffer class, and
/// that is safe because the decline path re-dispatches on the RECEIVER: the
/// generic dispatcher resolves the same triple it would have resolved without
/// the bind, for whatever class the receiver actually is.
pub static BUFFER_SESSION_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/nio/Buffer",
    method_name: "session",
    descriptor: "()Ljdk/internal/foreign/MemorySessionImpl;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Thin direct-call target for JIT `java.nio.Buffer.session()` sites.
///
/// # What it replaces, and why that is exactly nothing
///
/// The registered shim's whole body is `Ok(Some(Value::Object(None)))` — a
/// constant null — and `--dump-native-registry` prices it at ONE crossing per
/// multi-byte heap accessor: 200 000 `session()` invocations for 200 000
/// `HeapByteBuffer.getInt(int)` calls, alongside the 200 000
/// `getIntUnaligned` crossings that do the actual read. Half the native
/// crossings on the hot accessor path were for a constant.
///
/// # `segment == null` is the bytecode's own answer, not a shadow's guess
///
/// Real `java.nio.Buffer.session()` is exactly `segment != null ?
/// segment.sessionImpl() : null` (`getfield segment; ifnull`). 2026-09-21: this
/// helper reads that same field — through
/// `resolve_field_slot_by_name_cached`, the identical cached resolver
/// `NativeContext::get_field_by_name` uses, so it can never disagree with a
/// real `getfield segment` on the same class — and when the field is null it
/// returns 0 directly. That is not standing in front of the real method the
/// way the old shim did; it IS the real method's own value for that receiver,
/// computed without a call. So it needs no class-name list, no registry
/// population and no §1.4 policy check: there is no bytecode being shadowed,
/// because this is what the bytecode computes.
///
/// When `segment` is non-null the real `sessionImpl()` call has to run — this
/// helper cannot answer it any faster than a call — so it declines
/// unconditionally to the generic dispatcher, which reaches the real bytecode.
///
/// # Why this exists (history)
///
/// Before this, the helper answered only for a class id
/// `buffer_session::class_is_served` said the OLD registered shim had already
/// run for — a real fact, but one `--real-jdk` strict policy could never
/// produce: a `Bridge`-kind native is never invoked when real bytecode is
/// authoritative (§1 rule 4), so the shim never ran, the table stayed empty,
/// and this helper declined on every single call under `--real-jdk` (census:
/// `served=0 declined=N`). The field read above needs no such census — it
/// answers from the receiver's own state, in every mode. The class-population
/// fallback below is kept only for the receiver shapes the field resolve
/// cannot reach (see its own comment); it is not expected to fire for any
/// loaded `Buffer` subclass.
///
/// # Safety
/// Called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_buffer_session_direct(
    vm_ptr: i64,
    receiver: i64,
) -> i64 {
    let f = || jit_buffer_session_direct_body(vm_ptr, receiver);
    contain(
        "jit_buffer_session_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

// A layout proof for one native-owned segment/session pair. The current scope
// field is read on every hit; no mutable address or session value is cached.
// GC/free/close retire the epoch, metadata changes retire the layout epoch,
// and the VM identity prevents reuse between embedding instances.
#[derive(Clone, Copy)]
pub struct BufferStoredSessionProof {
    pub vm: usize,
    pub epoch: u64,
    pub layout_epoch: u64,
    pub segment: usize,
    pub session: usize,
    pub slot: usize,
}

thread_local! {
    pub static BUFFER_STORED_SESSION_PROOFS: std::cell::Cell<[BufferStoredSessionProof; 4]> =
        const { std::cell::Cell::new([BufferStoredSessionProof {
            vm: 0, epoch: 0, layout_epoch: 0, segment: 0, session: 0, slot: 0,
        }; 4]) };
    pub static BUFFER_STORED_SESSION_VICTIM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub unsafe fn buffer_stored_session(vm: &SharedVm, segment: ObjectRef) -> Option<ObjectRef> {
    buffer_stored_session_with(vm, segment, || {
        let (thread, _guard) = jit_thread_mut()?;
        let ctx = crate::vm::NativeContextImpl { shared: vm, thread };
        cratonvm_native_builtins::panama::jit_carrier_stored_session(&ctx, segment)
    })
}

pub unsafe fn buffer_stored_session_with(
    vm: &SharedVm,
    segment: ObjectRef,
    resolve: impl FnOnce() -> Option<(ObjectRef, usize)>,
) -> Option<ObjectRef> {
    let segment_address = segment.as_ptr() as usize;
    let epoch = cratonvm_native_builtins::ffm_fast::epoch();
    let layout_epoch = cratonvm_classloading::class_origin_epoch();
    let proofs = BUFFER_STORED_SESSION_PROOFS.with(std::cell::Cell::get);
    for proof in &proofs {
        if proof.vm == vm.vm_identity
            && proof.epoch == epoch
            && proof.layout_epoch == layout_epoch
            && proof.segment == segment_address
        {
            if let Value::Object(Some(session)) = vm.mem.heap.get_field(segment, proof.slot) {
                if session.as_ptr() as usize == proof.session {
                    return Some(session);
                }
            }
            // A rewritten scope must be validated by its owning native again.
            break;
        }
    }
    let (session, slot) = resolve()?;
    // The getter allocates nothing and cannot reach a safepoint. Even so, only
    // publish a proof whose epochs still match the checks that established it.
    if epoch == cratonvm_native_builtins::ffm_fast::epoch()
        && layout_epoch == cratonvm_classloading::class_origin_epoch()
    {
        let victim = BUFFER_STORED_SESSION_VICTIM.with(|c| {
            let victim = c.get();
            c.set((victim + 1) % proofs.len());
            victim
        });
        let mut updated = proofs;
        updated[victim] = BufferStoredSessionProof {
            vm: vm.vm_identity,
            epoch,
            layout_epoch,
            segment: segment_address,
            session: session.as_ptr() as usize,
            slot,
        };
        BUFFER_STORED_SESSION_PROOFS.with(|c| c.set(updated));
    }
    Some(session)
}

/// Body of [`jit_buffer_session_direct`], run under its panic guard.
pub unsafe fn jit_buffer_session_direct_body(vm_ptr: i64, receiver: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        // The `invokevirtual` this helper stands in for trapped on its
        // receiver: record the same JEP-358 action every sibling direct helper
        // and the generic dispatcher record, so `getMessage()` does not differ
        // by tier (round 11; this used the bare `set_jit_pending_npe()`).
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
    let raw = receiver as u64;
    // Shape screen before any header read, as `dbb_direct_elem_addr` does:
    // this is the only thing between a fabricated argument and the class-id
    // probe below.
    if (raw & 0x7) == 0 && raw < (1u64 << 48) {
        if let Some((class_id, _)) = direct_receiver_facts(vm, raw as usize) {
            let object = ObjectRef::from_raw(raw as usize as *mut u8);
            let class_id = ClassId::new(class_id);
            // The bytecode's own field, resolved the same way `getfield
            // segment` would be — see the doc comment above for why this
            // needs no policy check.
            if let Some(slot) =
                crate::vm::vm_exec::resolve_field_slot_by_name_cached(vm, class_id, "segment")
            {
                let segment = vm.mem.heap.get_field(object, slot);
                if matches!(segment, Value::Object(None)) {
                    // `null`, exactly what `segment != null ? … : null`
                    // computes for a null segment — every caller on this path
                    // (`ScopedMemoryAccess.get*Unaligned`) already accepts it,
                    // skipping `checkValidStateRaw()` on a null session.
                    cratonvm_jit::BUFFER_SESSION_SERVED
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return 0;
                }
                if let Value::Object(Some(segment)) = segment {
                    if let Some(session) = buffer_stored_session(vm, segment) {
                        cratonvm_jit::BUFFER_SESSION_SERVED
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return session.as_ptr() as i64;
                    }
                }
                // A real segment's `sessionImpl()` remains authoritative.
                // Carrier shapes that require resolution also take the call.
            } else if cratonvm_native_builtins::buffer_session::class_is_served_for_vm(vm.vm_identity, class_id.as_u32())
                && !jit_direct_helper_refused(vm, &BUFFER_SESSION_GATE)
            {
                // Defensive fallback: a receiver whose class the field resolve
                // could not reach (should not happen for any loaded `Buffer`
                // subclass, since `segment` is declared on `Buffer` itself),
                // but which the OLD shim-population census already proved safe
                // to answer null for. See the type's doc comment for why this
                // path's own policy check (H12-1) still applies here and
                // nowhere else in this function.
                cratonvm_jit::BUFFER_SESSION_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return 0;
            }
        }
    }
    cratonvm_jit::BUFFER_SESSION_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver];
    jit_invoke_dispatch(
        vm_ptr,
        &BUFFER_SESSION_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        1,
    )
}

// ============================================================================
// ScopedMemoryAccess unaligned thin direct helpers
// ============================================================================

pub static SCOPED_MEMORY_GET_BYTE_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getByteInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J)B",
    num_jit_args: 4,
    return_type: b'B',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};
pub static SCOPED_MEMORY_PUT_BYTE_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putByteInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JB)V",
    num_jit_args: 5,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// A session verdict is published only by the checked native access. The same
// epoch and close handshake that retire segment verdicts retire this proof.
// The window remains open until after the memory operation, so a shared close
// cannot free the address between the memo lookup and the load/store.
pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_byte_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
) -> i64 {
    contain(
        "jit_scoped_memory_get_byte_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
            let mut bytes = [0u8; 1];
            if window.admits_fast_path()
                && receiver != 0
                && !jit_direct_helper_refused(vm, &SCOPED_MEMORY_GET_BYTE_INTERNAL_GATE)
                && (session == 0
                    || cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
                && unsafe_accessor_capability_admits(vm, &UNSAFE_GET_BYTE_GATE)
                && raw_accessor_read_bytes(vm, obj, offset, &mut bytes)
            {
                cratonvm_jit::SCOPED_MEMORY_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return i64::from(bytes[0] as i8);
            }
            cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let args = [receiver, session, obj, offset];
            jit_invoke_dispatch(
                vm_ptr,
                &SCOPED_MEMORY_GET_BYTE_INTERNAL_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                4,
            )
        },
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_put_byte_internal_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    value: i64,
) -> i64 {
    contain(
        "jit_scoped_memory_put_byte_internal_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        || {
            crate::jit::conservative_roots::note_jit_boundary();
            let vm = &*(vm_ptr as *const SharedVm);
            let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
            if window.admits_fast_path()
                && receiver != 0
                && !jit_direct_helper_refused(vm, &SCOPED_MEMORY_PUT_BYTE_INTERNAL_GATE)
                && (session == 0
                    || cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
                && unsafe_accessor_capability_admits(vm, &UNSAFE_PUT_BYTE_GATE)
                && raw_accessor_write_bytes(vm, obj, offset, &[value as u8])
            {
                cratonvm_jit::SCOPED_MEMORY_SERVED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return 0;
            }
            cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let args = [receiver, session, obj, offset, value];
            jit_invoke_dispatch(
                vm_ptr,
                &SCOPED_MEMORY_PUT_BYTE_INTERNAL_INFO as *const JitInvokeInfo as i64,
                args.as_ptr() as i64,
                5,
            )
        },
    )
}

pub static SCOPED_MEMORY_GET_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getIntUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getShortUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    num_jit_args: 5,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getLongUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    num_jit_args: 5,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getCharUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    num_jit_args: 5,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_INT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putIntUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_SHORT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putShortUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_LONG_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putLongUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_CHAR_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putCharUnaligned",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// `*Internal` twins -- see `ScopedMemoryOp`'s doc comment (jit/src/direct_helpers.rs)
// for why a decline must name the SAME method that was actually intercepted.

pub static SCOPED_MEMORY_GET_INT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getIntUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)I",
    num_jit_args: 5,
    return_type: b'I',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_SHORT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getShortUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)S",
    num_jit_args: 5,
    return_type: b'S',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_LONG_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getLongUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)J",
    num_jit_args: 5,
    return_type: b'J',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_GET_CHAR_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "getCharUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JZ)C",
    num_jit_args: 5,
    return_type: b'C',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_INT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putIntUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JIZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_SHORT_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putShortUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JSZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_LONG_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putLongUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JJZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub static SCOPED_MEMORY_PUT_CHAR_INTERNAL_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "jdk/internal/misc/ScopedMemoryAccess",
    method_name: "putCharUnalignedInternal",
    descriptor: "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JCZ)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// Shared fast arm of the eight `ScopedMemoryAccess.get*Unaligned{,Internal}`
/// thin helpers.
///
/// `gate` is the CALLER's, not this function's: the eight callers stand in
/// front of eight different registered rows, and a policy question asked about
/// one of them on behalf of another is the H7-1 defect one level down. It is
/// threaded rather than derived because this core has no way to tell which
/// accessor called it — the width is a slice length and the row is not.
pub unsafe fn scoped_memory_read_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    session: i64,
    obj: i64,
    offset: i64,
    out: &mut [u8],
) -> bool {
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    let unsafe_gate = match out.len() {
        2 => &UNSAFE_GET_SHORT_GATE,
        4 => &UNSAFE_GET_INT_GATE,
        8 => &UNSAFE_GET_LONG_GATE,
        _ => return false,
    };
    if !window.admits_fast_path()
        || jit_direct_helper_refused(vm, gate)
        || (session != 0
            && !cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
        || !unsafe_accessor_capability_admits(vm, unsafe_gate)
    {
        return false;
    }
    raw_accessor_read_bytes(vm, obj, offset, out)
}

/// Shared byte-copy core of [`scoped_memory_read_bytes`] and the plain
/// `Unsafe.get{Byte,Short,Int,Long}(Object,long)` thin helpers further below.
///
/// `Unsafe`'s own accessors take no `MemorySessionImpl` and have no
/// scoped-arena liveness to check -- real `Unsafe` does not check one either,
/// only `ScopedMemoryAccess` does, using its checked session verdict and
/// close window at the call site above. Declines (`false`) for anything but a primitive heap array
/// or an off-heap address, exactly as before this split, so a decline always
/// falls through to the generic dispatcher, never to a wrong answer.
pub unsafe fn raw_accessor_read_bytes(vm: &SharedVm, obj: i64, offset: i64, out: &mut [u8]) -> bool {
    if obj == 0 {
        let addr = offset;
        if addr <= 0 {
            return false;
        }
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_out(addr, out);
        } else if (addr as u64).checked_add(out.len() as u64).is_some() {
            std::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len());
            return true;
        } else {
            return false;
        }
    }
    let raw = obj as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return false;
    }
    let Some(object) = vm.mem.heap.is_object_address(raw as usize) else {
        return false;
    };
    const ABASE: usize = 16;
    if offset < ABASE as i64 {
        return false;
    }
    let start = (offset as usize) - ABASE;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = vm.mem.heap.element_type_of_validated(object);
    if et == ArrayElementType::Reference {
        return false;
    }
    let width = cratonvm_types::element_byte_size(et);
    let Some(capacity) = vm.mem.heap.array_length(object).checked_mul(width) else {
        return false;
    };
    if start
        .checked_add(out.len())
        .map_or(true, |end| end > capacity)
    {
        return false;
    }
    let Some(base) = vm.mem.heap.array_data_ptr(object) else {
        return false;
    };
    std::ptr::copy_nonoverlapping(base.add(start), out.as_mut_ptr(), out.len());
    true
}

/// Write-side twin of [`scoped_memory_read_bytes`]; `gate` is the caller's for
/// the same reason.
pub unsafe fn scoped_memory_write_bytes(
    vm: &SharedVm,
    gate: &'static DirectHelperGate,
    session: i64,
    obj: i64,
    offset: i64,
    data: &[u8],
) -> bool {
    let window = cratonvm_native_builtins::ffm_fast::AccessWindow::open();
    let unsafe_gate = match data.len() {
        2 => &UNSAFE_PUT_SHORT_GATE,
        4 => &UNSAFE_PUT_INT_GATE,
        8 => &UNSAFE_PUT_LONG_GATE,
        _ => return false,
    };
    if !window.admits_fast_path()
        || jit_direct_helper_refused(vm, gate)
        || (session != 0
            && !cratonvm_native_builtins::ffm_fast::is_validated(session as u64, false))
        || !unsafe_accessor_capability_admits(vm, unsafe_gate)
    {
        return false;
    }
    raw_accessor_write_bytes(vm, obj, offset, data)
}

/// Write-side twin of [`raw_accessor_read_bytes`]; see its doc comment.
pub unsafe fn raw_accessor_write_bytes(vm: &SharedVm, obj: i64, offset: i64, data: &[u8]) -> bool {
    if obj == 0 {
        let addr = offset;
        if addr <= 0 {
            return false;
        }
        if cratonvm_native_builtins::unsafe_arena_addr_is_tagged(addr) {
            return cratonvm_native_builtins::unsafe_arena_copy_in(addr, data);
        } else if (addr as u64).checked_add(data.len() as u64).is_some() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len());
            return true;
        } else {
            return false;
        }
    }
    let raw = obj as u64;
    if (raw & 0x7) != 0 || raw >= (1u64 << 48) {
        return false;
    }
    let Some(object) = vm.mem.heap.is_object_address(raw as usize) else {
        return false;
    };
    const ABASE: usize = 16;
    if offset < ABASE as i64 {
        return false;
    }
    let start = (offset as usize) - ABASE;
    if vm.mem.heap.kind_of(object) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = vm.mem.heap.element_type_of_validated(object);
    if et == ArrayElementType::Reference {
        return false;
    }
    let width = cratonvm_types::element_byte_size(et);
    let Some(capacity) = vm.mem.heap.array_length(object).checked_mul(width) else {
        return false;
    };
    if start
        .checked_add(data.len())
        .map_or(true, |end| end > capacity)
    {
        return false;
    }
    let Some(base) = vm.mem.heap.array_data_ptr(object) else {
        return false;
    };
    std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(start), data.len());
    true
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_int_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_int_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_int_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_int_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 4];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_INT_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i32::from_be_bytes(bytes)
        } else {
            i32::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_INT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_short_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_short_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_short_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_short_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_SHORT_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i16::from_be_bytes(bytes)
        } else {
            i16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_SHORT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_long_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_long_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_long_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_long_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 8];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_LONG_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            i64::from_be_bytes(bytes)
        } else {
            i64::from_le_bytes(bytes)
        };
        return val;
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_LONG_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_scoped_memory_get_char_direct(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    let f = || {
        jit_scoped_memory_get_char_direct_body(vm_ptr, receiver, session, obj, offset, big_endian)
    };
    contain(
        "jit_scoped_memory_get_char_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

pub unsafe fn jit_scoped_memory_get_char_direct_body(
    vm_ptr: i64,
    receiver: i64,
    session: i64,
    obj: i64,
    offset: i64,
    big_endian: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    if receiver == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let mut bytes = [0u8; 2];
    if scoped_memory_read_bytes(
        vm,
        &SCOPED_MEMORY_GET_CHAR_GATE,
        session,
        obj,
        offset,
        &mut bytes,
    ) {
        cratonvm_jit::SCOPED_MEMORY_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let val = if big_endian != 0 {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        };
        return i64::from(val);
    }
    cratonvm_jit::SCOPED_MEMORY_DECLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let args = [receiver, session, obj, offset, big_endian];
    jit_invoke_dispatch(
        vm_ptr,
        &SCOPED_MEMORY_GET_CHAR_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        5,
    )
}

