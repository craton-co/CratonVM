#[allow(unused_imports)]
use super::*;

/// Body of [`jit_concurrent_hashmap_get_direct`], run under its panic guard.
pub unsafe fn jit_concurrent_hashmap_get_direct_body(vm_ptr: i64, receiver: i64, key: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    'fast: {
        // H12-1. Both rows this ladder touches —
        // `java/util/concurrent/ConcurrentMap.get` at the call site and
        // `java/util/concurrent/ConcurrentHashMap.get`, whose `native_chm_get`
        // actually runs — are `bridge`. No compile door binds this helper
        // today (the OSR ladder does not recognise `ConcurrentMap`, and
        // `direct_native_helper_for_impl` refuses it at MethodEntry), so this
        // is the one guard here that is a backstop rather than a live fix.
        // It is written anyway, because H7-1 §4a's point is that the pair is
        // right by a coincidence of tagging that a retag is meant to change.
        //
        // After the receiver screens, for the counter reason spelled out in
        // `jit_hashmap_get_direct`.
        if receiver == 0
            || (receiver as u64 & 0x7) != 0
            || (receiver as u64) >= (1u64 << 48)
            || !jit_concurrent_hashmap_receiver_is_exact(vm, receiver)
        {
            break 'fast;
        }
        if jit_direct_helper_refused(vm, &CONCURRENT_HASHMAP_GET_GATE) {
            break 'fast;
        }
        // The `JdkConcurrentHashMap` row (interpreter round i3 wave 76, lane
        // L1; default off): the map is the JDK's, its entries are in `table`,
        // and `native_chm_get` / `jit_chm_get` read the segmented store the row
        // never builds (an empty answer). The generic dispatch below runs the
        // `ConcurrentMap.get` door, which hands the map to its own body. One
        // per-VM registry read, after the receiver screens.
        if vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkConcurrentHashMap)
        {
            break 'fast;
        }
        let Some(recv) = vm.mem.heap.is_object_address(receiver as usize) else {
            break 'fast;
        };
        let key_val = if key == 0 {
            // An explicit `null` key ARGUMENT, which `native_chm_get` answers
            // with `chm_reject_null_key`'s NPE. Do not confuse it with the
            // unrecognised-address case below — that one is not an argument
            // value at all, it is a failed validation.
            Value::Object(None)
        } else if (key as u64 & 0x7) != 0 || (key as u64) >= (1u64 << 48) {
            break 'fast;
        } else {
            match vm.mem.heap.is_object_address(key as usize) {
                Some(object) => Value::Object(Some(object)),
                // See item 1 in this function's doc comment. Was `return 0`.
                None => break 'fast,
            }
        };
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        // r9w11 chm11: a primitive-wrapper or `String` key is answered by the
        // native's own allocation-free, never-blocking prefix, without the funnel.
        if let Value::Object(Some(_)) = key_val {
            let probe = {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: vm,
                    thread: &mut *thread,
                };
                cratonvm_native_collections::jit_chm_get(&mut ctx, recv, key_val)            };
            match probe {
                Some(Value::Object(Some(object))) => {
                    thread.native_pending_return = Some(object);
                    return object.as_ptr() as i64;
                }
                Some(Value::Object(None)) => return 0,
                _ => {}
            }
        }
        let values = [Value::Object(Some(recv)), key_val];
        // Item 2 in this function's doc comment: the SAME funnel the
        // interpreter uses for the SAME callback, so the two tiers cannot
        // disagree about pinning, transitions or exception draining. Every
        // `Value::Object` in `values` was validated against this VM's heap
        // immediately above, which is exactly the precondition the
        // `_prevalidated_objects` variant is documented against.
        let result = crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            cratonvm_native_collections::native_chm_get,
            &values,
        );
        match result {
            Ok(Some(Value::Object(Some(object)))) => {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Ok(Some(Value::Object(None))) | Ok(None) => return 0,
            // A non-object `Value` out of an object-typed map is
            // out-of-contract. `get` is a read, so re-running it through the
            // dispatcher is idempotent and costs only a second lookup — the
            // reason the PUT sibling may NOT do the same thing.
            Ok(_) => break 'fast,
            Err(error) => {
                return handle_jit_dispatch_error(
                    vm,
                    thread,
                    error,
                    &CONCURRENT_HASHMAP_GET_DIRECT_INFO,
                )
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &CONCURRENT_HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Thin direct-call target for JIT `invokevirtual HashMap.get(Object)` sites
/// whose constant-pool class is exactly `java/util/HashMap` (recognition in
/// `jit::try_compile`; registered via `set_hashmap_get_direct_fn`).
///
/// Fast path: the Integer-keyed overlay probe (`jit_overlay_hashmap_get`) —
/// no Java-heap allocation, no Java dispatch, no `safe_native_call` wrapper
/// (same contract as `jit_integer_int_value_direct`). Everything else —
/// subclass receiver, non-Integer key, materialized map, redefine window —
/// falls back to the full generic dispatcher, byte-for-byte the semantics
/// the non-direct site had.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_hashmap_get_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
) -> i64 {
    let f = || jit_hashmap_get_direct_body(vm_ptr, receiver, key);
    let bits = contain(
        "jit_hashmap_get_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_hashmap_get_direct`], run under its panic guard.
pub unsafe fn jit_hashmap_get_direct_body(vm_ptr: i64, receiver: i64, key: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
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
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        // H12-1. `java/util/HashMap.get` is `bridge`, so under `JdkOnly` the
        // real `java.util.HashMap` bytecode is authoritative and this helper
        // must not answer. `break 'fast` is the route every non-exact receiver
        // already takes, and `jit_invoke_dispatch` below is policy-checked.
        //
        // Placed AFTER the receiver screens on purpose, so
        // `jit_direct_helper_jdk_only_refusals` counts calls the fast path
        // would otherwise have SERVED. In front of them it would also count
        // every call that was going to `break 'fast` anyway, and a counter that
        // inflates is a counter nobody can read a ratio out of.
        if jit_direct_helper_refused(vm, &HASHMAP_GET_GATE) {
            break 'fast;
        }
        let key_val = if key == 0 {
            Value::Object(None)
        } else {
            let kraw = key as u64;
            if (kraw & 0x7) != 0 || kraw >= (1u64 << 48) {
                break 'fast;
            }
            // The key is dereferenced for the first time downstream, by
            // `unbox_wrapper`'s `class_id_of`. Keep the arena-membership probe:
            // it is the only check between a stale/fabricated key argument and
            // that dereference.
            match vm.mem.heap.is_object_address(key as usize) {
                Some(object) => Value::Object(Some(object)),
                None => break 'fast,
            }
        };
        // `jit_hashmap_receiver_is_exact` just read this live receiver's class
        // header and matched exact `HashMap`, so a second arena membership
        // probe cannot add safety on this path.
        let recv_obj = ObjectRef::from_raw(receiver as usize as *mut u8);
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        let probe = {
            let ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_get(&ctx, recv_obj, key_val)        };
        match probe {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                // Object-return handoff root (see `jit_integer_value_of_direct`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            // The overlay stores whatever Value was put; an object-typed map
            // returning a non-object Value is out-of-contract — take the
            // full dispatcher rather than guessing an encoding.
            Some(Ok(Some(_))) => break 'fast,
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_GET_DIRECT_INFO)
            }
            None => {
                // r9w11 chm11: probed AFTER the overlay, which answers only keys
                // that unbox to an `int` -- this cache holds `String` keys only, so
                // the two are disjoint and an `Integer` key no longer pays for it.
                // The native HashMap path maintains a GC-remapped, modCount-guarded
                // String-node cache. Probe it directly here so hot dynamic lowercase
                // keys avoid entering the GC-safe native wrapper at all.
                if let Value::Object(Some(key_object)) = key_val {
                    let cached = {
                        let mut ctx = crate::vm::NativeContextImpl {
                            shared: vm,
                            thread: &mut *thread,
                        };
                        ctx.hashmap_string_node_cache_get_object(recv_obj, key_object)
                    };
                    if let Some(Value::Object(Some(object))) = cached {
                        thread.native_pending_return = Some(object);
                        return object.as_ptr() as i64;
                    }
                    if let Some(Value::Object(None)) = cached {
                        return 0;
                    }
                }
                let values = [Value::Object(Some(recv_obj)), key_val];
                match crate::vm::safe_native_call_prevalidated_objects(
                    vm,
                    thread,
                    cratonvm_native_collections::native_hashmap_get_exact,
                    &values,
                ) {
                    Ok(Some(Value::Object(Some(object)))) => {
                        let result = object.as_ptr() as i64;
                        thread.native_pending_return = Some(object);
                        return result;
                    }
                    Ok(Some(Value::Object(None))) | Ok(None) => return 0,
                    Ok(_) => break 'fast,
                    Err(error) => {
                        return handle_jit_dispatch_error(
                            vm,
                            thread,
                            error,
                            &HASHMAP_GET_DIRECT_INFO,
                        )
                    }
                }
            }
        }
    }
    let args = [receiver, key];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_GET_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

/// Compact-Latin1 sibling: `StringLatin1.toLowerCase(String, byte[], Locale)`,
/// so `value` is the receiver's backing array (unused — the implementation
/// reads the String) and `locale` the third argument.
///
/// Both entries delegate to `lang_string::jit_string_to_lower_case`, the same
/// implementation the interpreted native uses. They used to carry a private copy
/// of the ASCII/Unicode fold that ignored `locale` entirely, so a
/// `toLowerCase(TURKISH)` call silently changed its answer when its caller
/// tiered up — the interpreter said `tıtle`, the compiled code `title`.
///
/// That fix was one layer too shallow: the delegate called `string_case_impl`,
/// BELOW the null-`Locale` check, so `toLowerCase((Locale) null)` threw
/// interpreted and answered the default-locale string compiled. Measured on
/// OpenJDK 25.0.3+9, the oracle throws on the 200 000th warm iteration too.
/// See `docs/feature-designs/jit-ir-optimizations.md`.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
pub(in crate::jit) unsafe extern "C" fn jit_string_latin1_to_lower_direct(
    vm_ptr: i64,
    source: i64,
    _value: i64,
    locale: i64,
) -> i64 {
    let f = || jit_string_latin1_to_lower_direct_body(vm_ptr, source, _value, locale);
    let bits = contain(
        "jit_string_latin1_to_lower_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_string_latin1_to_lower_direct`], run under its panic guard.
pub unsafe fn jit_string_latin1_to_lower_direct_body(
    vm_ptr: i64,
    source: i64,
    _value: i64,
    locale: i64,
) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    // Round 11 (lane `rt`): every shape this fast arm cannot serve goes to the
    // generic dispatcher, as `jit_hashmap_get_direct`'s does. These arms used
    // to `return 0` — a null String the Java method never produces for these
    // inputs (a null `str` with a null `Locale` must throw; an unvalidatable
    // pointer or a missing JIT thread is this helper's problem, not the
    // program's). Called only before `jit_thread_mut` is taken below, so the
    // dispatcher can take it itself.
    let dispatch = || {
        let args = [source, _value, locale];
        jit_invoke_dispatch(
            vm_ptr,
            &STRING_LATIN1_LOWER_DIRECT_INFO as *const JitInvokeInfo as i64,
            args.as_ptr() as i64,
            3,
        )
    };
    if source == 0 {
        return dispatch();
    }
    let Some(source) = vm.mem.heap.is_object_address(source as usize) else {
        return dispatch();
    };
    // A `locale` of 0 is an explicit `null` Locale ARGUMENT, not "no argument":
    // the only descriptor bound to this helper,
    // `StringLatin1.toLowerCase(Ljava/lang/String;[BLjava/util/Locale;)`, has a
    // MANDATORY third parameter, so "absent" is not a state this site can be
    // in. `lang_string::jit_string_to_lower_case` now goes through the same
    // `string_case_native` the interpreted native does and throws for it —
    // E18-1 §1. It used to sit one layer below that check and answer the
    // default-locale string, so the same source line threw while interpreted
    // and stopped throwing once its caller tiered up.
    let locale_obj = if locale == 0 {
        None
    } else {
        // A non-zero address the heap does not recognise gets the same
        // treatment `source` gets six lines above: bail, do not guess.
        let Some(obj) = vm.mem.heap.is_object_address(locale as usize) else {
            return dispatch();
        };
        Some(obj)
    };
    let Some((thread, _guard)) = jit_thread_mut() else {
        return dispatch();
    };
    // Scoped so `thread` can be re-borrowed for the error path below, exactly
    // as `jit_hashmap_get_direct` does.
    let values = [
        Value::Object(Some(source)),
        Value::Object(None),
        Value::Object(locale_obj),
    ];
    let outcome = crate::vm::safe_native_call(
        vm,
        thread,
        cratonvm_native_builtins::lang_string::native_string_latin1_to_lower_case,
        &values,
    );
    let result = match outcome {
        Ok(Some(Value::Object(Some(object)))) => object,
        Ok(Some(Value::Object(None))) | Ok(None) => return 0,
        Ok(Some(_)) => return 0,
        // The JDK's NullPointerException for a null Locale arrives here. The
        // old code could only report it as `None`, i.e. as a null String.
        Err(error) => {
            return handle_jit_dispatch_error(vm, thread, error, &STRING_LATIN1_LOWER_DIRECT_INFO)
        }
    };
    thread.native_pending_return = Some(result);
    result.as_ptr() as i64
}

/// PUT sibling of [`jit_hashmap_get_direct`] — see its doc for the contract.
///
/// # H7-1: one implementation, and no arm that mutates and then re-dispatches
///
/// This used to call `cratonvm_native_collections::jit_overlay_hashmap_put`
/// — a `pub` re-export of `try_hm_int_fast_put` — as its own private first
/// stage, and hand every other outcome back to `jit_invoke_dispatch`. Two
/// consequences, the second of which is a wrong answer:
///
/// * It was a **second entry into the map's write path** that the registry has
///   never heard of, which is the population `H4-1` §1c and `H0-3` are about.
///   `native_hashmap_put_exact`'s FIRST statement is that same
///   `try_hm_int_fast_put`, so calling the native loses no fast path — it
///   loses the private door.
/// * `Some(Ok(Some(<non-object Value>)))` fell to `break 'fast`, i.e. to
///   `jit_invoke_dispatch`, **after the overlay insert had already happened**.
///   The generic path then re-executed the put and returned the value this
///   call had just written as if it were the previous mapping.
///   `HashMap.put`'s return value is the previous mapping, so that is a
///   silently wrong answer, and it is reachable: the overlay's stored `Value`
///   comes from whoever wrote it, and `H4-1` §1a counts 42 direct Rust
///   `native_map_put_pub` call sites that are not obliged to store an object.
///   The GET sibling may re-dispatch on this arm because a read is idempotent;
///   a write is not, and the two were written as if they were the same shape.
///
/// The exact-`java/util/HashMap` receiver guard above is what makes
/// `native_hashmap_put_exact` the right callee rather than the registered
/// `native_map_put`: for that receiver `native_map_put_evict`'s
/// `is_bare_java_lang_object` / `is_unmod_wrapper` / `ht_reject_null_*` /
/// CHM / LHM / TreeMap ladder is all no-ops, and its `CF_EXACT_HASHMAP` arm is
/// `native_hashmap_put_exact`'s body line for line. That is the same identity
/// `hashmap_native_callback` already relies on for the site-cache door, and
/// the same shape [`jit_hashmap_get_direct`]'s own fallback arm already uses.
///
/// **Cost: that paragraph's falsifier fired (r9w12).** It used to read "Cost,
/// stated rather than measured — no binary carrying this has been built. The
/// overlay-servable put now pays one `safe_native_call_prevalidated_objects`
/// funnel entry it did not pay before ... Falsified by an A/B on one binary
/// showing a map-put-dominated workload regressing beyond noise." It was
/// measured on the 2026-09-20 dev build and it does regress: `BoxProbe put`
/// (10 M `HashMap<Integer,Integer>.put`, cached boxes) put **22.67 %** of its
/// samples in `safe_native_call_impl`, more than twice the 10.57 % in
/// `try_hm_int_fast_put` — the work the funnel was wrapping.
///
/// So the overlay probe is back in front of the funnel, without the defect
/// that removed it: see the r9w12 block in the body for why the arm that
/// inserted and then re-dispatched cannot recur, and why a `None` from the
/// probe means nothing was written. The funnel still serves every non-overlay
/// outcome, which is where `map_hash_key` (arbitrary Java `hashCode()`) lives,
/// so nothing that needed the pin ring, the `NativeRunning` transition or the
/// exception drain has lost them.
///
/// SAFETY: called only from JIT-compiled code with a live `vm_ptr`.
/// Direct call helper for `java/lang/reflect/Constructor.newInstance([Ljava/lang/Object;)Ljava/lang/Object;`.
pub(crate) unsafe extern "C" fn jit_ctor_new_instance_direct(
    vm_ptr: i64,
    ctor_raw: i64,
    args_raw: i64,
) -> i64 {
    let f = || jit_ctor_new_instance_direct_body(vm_ptr, ctor_raw, args_raw);
    let bits = contain(
        "jit_ctor_new_instance_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    native_return_handed_to_compiled_code();
    bits
}

unsafe fn jit_ctor_new_instance_direct_body(vm_ptr: i64, ctor_raw: i64, args_raw: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    if ctor_raw == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    if let Some(obj_ptr) = crate::jit::helpers::dispatch::ctor_new_instance_fast(
        vm_ptr,
        ctor_raw,
        args_raw,
        true,
    ) {
        return obj_ptr;
    }
    let args = [ctor_raw, args_raw];
    if let Some(obj_ptr) = crate::jit::helpers::dispatch::ctor_new_instance_redirect(
        vm_ptr,
        &CTOR_NEW_INSTANCE_DIRECT_INFO,
        &args,
    ) {
        if obj_ptr != 0 && obj_ptr != i64::MIN {
            if let Some((thread, _guard)) = jit_thread_mut() {
                thread.native_pending_return =
                    Some(ObjectRef::from_raw(obj_ptr as usize as *mut u8));
            }
        }
        return obj_ptr;
    }
    jit_invoke_dispatch(
        vm_ptr,
        &CTOR_NEW_INSTANCE_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    )
}

pub(in crate::jit) unsafe extern "C" fn jit_hashmap_put_direct(
    vm_ptr: i64,
    receiver: i64,
    key: i64,
    value: i64,
) -> i64 {
    let f = || jit_hashmap_put_direct_body(vm_ptr, receiver, key, value);
    let bits = contain(
        "jit_hashmap_put_direct",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    bits
}

/// Body of [`jit_hashmap_put_direct`], run under its panic guard.
pub unsafe fn jit_hashmap_put_direct_body(vm_ptr: i64, receiver: i64, key: i64, value: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    // SAFETY: vm_ptr originates from JIT code compiled against this live VM.
    let vm = &*(vm_ptr as *const SharedVm);
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
    'fast: {
        let rraw = receiver as u64;
        if (rraw & 0x7) != 0 || rraw >= (1u64 << 48) {
            break 'fast;
        }
        if !jit_hashmap_receiver_is_exact(vm, receiver) {
            break 'fast;
        }
        // H12-1. `java/util/HashMap.put` is `bridge`; see the sibling comment
        // in `jit_hashmap_get_direct`, including why this sits after the
        // receiver screens. This one matters more than the read: the OSR door
        // binds it, and a put is a MUTATION, so a strict-mode execution of this
        // body writes through CratonVM's native into a map the interpreter
        // reads with real bytecode.
        if jit_direct_helper_refused(vm, &HASHMAP_PUT_GATE) {
            break 'fast;
        }
        let mut vals = [Value::Object(None); 2];
        for (slot, raw) in [(0usize, key), (1usize, value)] {
            if raw == 0 {
                continue;
            }
            let bits = raw as u64;
            if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                break 'fast;
            }
            // Key and value are dereferenced downstream for the first time
            // (`unbox_wrapper`'s `class_id_of` on the key, and the value once
            // it is read back out). Keep the arena-membership probe: it is the
            // only check between a stale/fabricated argument and that
            // dereference.
            match vm.mem.heap.is_object_address(raw as usize) {
                Some(object) => vals[slot] = Value::Object(Some(object)),
                None => break 'fast,
            }
        }
        // Exact-class screening above already dereferenced and validated this
        // live HashMap receiver.
        let recv_obj = ObjectRef::from_raw(receiver as usize as *mut u8);
        let Some((thread, _guard)) = jit_thread_mut() else {
            break 'fast;
        };
        // r9w12: the overlay probe, in front of the funnel and with the H7-1
        // defect kept out.
        //
        // H7-1 removed a `jit_overlay_hashmap_put` prefix from this body for
        // two reasons, and only the SECOND of them was about the prefix
        // existing: the `Some(Ok(Some(<non-object Value>)))` arm fell through
        // to `jit_invoke_dispatch` AFTER the overlay insert had already
        // happened, so the generic path re-applied the put and returned the
        // value this call had just written as the previous mapping. That is a
        // wrong answer and it is not reintroduced here: every `Some(..)` arm
        // below RETURNS, and the non-object arm answers `0` -- byte for byte
        // what the funnel arm below already answers for the same shape.
        //
        // The first reason was "a second entry into the map's write path the
        // registry has never heard of". The GET sibling has carried exactly
        // that shape since perf/halfgap-20260717 and is the precedent: the
        // probe IS `try_hm_int_fast_put`, i.e. `native_hashmap_put_exact`'s
        // own first statement, so there is one implementation of the overlay
        // insert and this is a second CALLER of it, not a second copy.
        //
        // The contract for calling it outside `safe_native_call` is
        // `jit_overlay_hashmap_put`'s own: the overlay insert touches only the
        // Rust-side shard table (whose entries the GC scans as roots via
        // `gc_overlay_roots_for_collection`), so it allocates nothing on the
        // Java heap, dispatches no Java and cannot throw. Nothing can move
        // across it, which is what lets the arguments go unpinned here -- the
        // same argument `jit_overlay_hashmap_get` makes.
        //
        // A `None` means the overlay wrote NOTHING: every `return None` in
        // `try_hm_int_fast_put` is ahead of its `entries.insert`, and the one
        // that leaves a default-constructed (empty) state behind is idempotent
        // -- the funnel's own call re-takes the same branch. So the fallback
        // below re-runs a put that has not happened yet.
        //
        // MEASURED, and this is the A/B `jit_hashmap_put_direct`'s "Cost,
        // stated rather than measured" paragraph asked for: on the 2026-09-20
        // dev build, `BoxProbe put` (10 M `HashMap<Integer,Integer>.put` of
        // cached boxes) spent **22.67 %** of its samples in
        // `safe_native_call_impl` -- the funnel entry that paragraph said an
        // overlay-servable put "now pays ... it did not pay before" -- against
        // 10.57 % in `try_hm_int_fast_put` itself. That is the regression
        // beyond noise it named as its own falsifier.
        let overlay = {
            let mut ctx = crate::vm::NativeContextImpl {
                shared: vm,
                thread: &mut *thread,
            };
            cratonvm_native_collections::jit_overlay_hashmap_put(
                &mut ctx, recv_obj, vals[0], vals[1],
            )
        };
        match overlay {
            Some(Ok(Some(Value::Object(Some(object))))) => {
                // Object-return handoff root (see `jit_integer_value_of_direct`).
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
            // Out-of-contract, and the H7-1 §2c arm: the put has ALREADY been
            // applied, so this must NOT re-dispatch. `0` is the same reply the
            // funnel arm below gives for the same shape.
            Some(Ok(Some(_))) => {
                debug_assert!(
                    false,
                    "java/util/HashMap.put overlay returned a non-object Value; a non-Java \
                     writer stored a primitive under this key (H4-1 §1a)"
                );
                return 0;
            }
            Some(Err(error)) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_PUT_DIRECT_INFO)
            }
            // Nothing written -- the general path below is still the whole put.
            None => {}
        }
        // H7-1: the ONE implementation, through the same funnel the
        // interpreter uses for the same receiver. `native_hashmap_put_exact`
        // opens with `try_hm_int_fast_put`, so the overlay fast path is
        // unchanged; what is gone is the JIT's private entry into it and the
        // arm that inserted and then re-dispatched. Every `Value::Object` in
        // `vals` was validated against this VM's heap above, which is the
        // `_prevalidated_objects` precondition.
        let values = [Value::Object(Some(recv_obj)), vals[0], vals[1]];
        match crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            cratonvm_native_collections::native_hashmap_put_exact,
            &values,
        ) {
            Ok(Some(Value::Object(Some(object)))) => {
                thread.native_pending_return = Some(object);
                return object.as_ptr() as i64;
            }
            Ok(Some(Value::Object(None))) | Ok(None) => return 0,
            // Out-of-contract: an object-typed map answered with a non-object
            // `Value`. The put has ALREADY been applied by the call above, so
            // re-dispatching would apply it twice and return the value this
            // call wrote instead of the previous mapping — the defect this
            // rewrite exists to remove. There is no `L`-shaped encoding of a
            // primitive `Value`, so answer `null`: it is the one reply that
            // does not mutate a second time, and it is what the same match
            // already answers for `Value::Object(None)`.
            Ok(Some(_)) => {
                debug_assert!(
                    false,
                    "java/util/HashMap.put returned a non-object Value; a non-Java writer \
                     stored a primitive under this key (H4-1 §1a)"
                );
                return 0;
            }
            Err(error) => {
                return handle_jit_dispatch_error(vm, thread, error, &HASHMAP_PUT_DIRECT_INFO)
            }
        }
    }
    let args = [receiver, key, value];
    jit_invoke_dispatch(
        vm_ptr,
        &HASHMAP_PUT_DIRECT_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        3,
    )
}

/// §4 census wrapper around [`call_integer_native_raw_inner`].
///
/// `Some` is "this edge delivered the native's semantics" — which includes the
/// two arms that deliberately never touch `entry.callback` (the TLAB wrapper
/// allocation and the direct `intValue` field read). Those are open-coded
/// copies of the registered intrinsic, so from the census's point of view the
/// native was dispatched; not counting them would leave exactly the kind of
/// unverifiable path the acceptance criterion is written against. Every `None`
/// return is an argument-shape guard that runs before any of it.
#[inline]
pub fn call_integer_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    entry: IntegerNativeDispatchCache,
) -> Option<i64> {
    let result = call_integer_native_raw_inner(vm, thread, info, args_slice, entry);
    if result.is_some() {
        count_jit_native_dispatch(vm, entry.native_id);
    }
    result
}

#[inline]
pub fn call_integer_native_raw_inner(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    entry: IntegerNativeDispatchCache,
) -> Option<i64> {
    if args_slice.len() != 1 {
        return None;
    }
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        let value = args_slice[0] as i32;
        // Round 11 (lane `rt`): beyond the box cache means beyond
        // `IntegerCache.high`, not beyond 127 — the same bound, and the same
        // "unresolved means take the native" rule, as
        // `jit_integer_value_of_direct`. This twin fresh-allocated everything
        // past 127, so with `-Djava.lang.Integer.IntegerCache.high=1000`,
        // `Integer.valueOf(500) == Integer.valueOf(500)` was true interpreted
        // and false through this compiled route.
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
            });
            if let Some(class_id) = cached_class {
                // Young-pressure relief for this cached fast path, which
                // deliberately bypasses the `safe_native_call` boundary (and
                // its young-pressure GC hook): the only argument here is a
                // primitive `i32`, so initiating the orchestrated GC is
                // exactly as safe as `jit_new_object`'s slow-path GC — no raw
                // object pointers are held across it. Without this, a
                // boxing-dominated compiled loop keeps spilling wrappers into
                // old gen until `alloc_young_initialized` hard-aborts.
                if vm.mem.heap.young_spill_pressure() {
                    // `|| old_gen_needs_gc()`: same reasoning as the
                    // `safe_native_call` hook this mirrors — the young trigger
                    // cannot see pressure that has gone into old gen, which is
                    // where every spill lands. See `vm_exec.rs`.
                    if !crate::runtime::interpreter::gc_overhead_limit_exceeded(vm)
                        && (vm.mem.heap.needs_gc_for_jit_allocation()
                            || vm.mem.heap.old_gen_needs_gc())
                    {
                        // zr z3/t2: its own site name -- a pressure verdict,
                        // not a failed allocation
                        // (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
                        crate::runtime::interpreter::maybe_gc_forced_pub_at(
                            vm,
                            thread,
                            "jit-box-pressure",
                        );
                    }
                    vm.mem.heap.clear_young_spill_pressure();
                }
                // zr z3/lt: fallible on ZGC with a collect-and-retry; a refusal
                // falls through to the native callback below, whose funnel
                // throws the OutOfMemoryError (this arm used to abort).
                if let Some(object) = jit_box_cold_alloc(vm, &mut *thread, class_id) {
                    use cratonvm_native_api::NativeContext as _;
                    let mut ctx = crate::vm::NativeContextImpl {
                        shared: vm,
                        thread: &mut *thread,
                    };
                    ctx.set_field(object, 0, Value::Int(value));
                    // Mirror `safe_native_call`'s object-return handoff root.
                    // A peer STW cannot collect this active JIT mutator until
                    // its next safepoint, but publishing the pending value
                    // preserves the existing root contract and diagnostic
                    // visibility.
                    ctx.thread.native_pending_return = Some(object);
                    return Some(object.as_ptr() as i64);
                }
            }
        }
    }
    if matches!(entry.kind, IntegerNativeKind::IntValue) {
        let raw = args_slice[0] as u64;
        if raw == 0 || (raw & 0x7) != 0 || raw >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(raw as usize)?;
        // `intrinsic_integer_int_value` delegates to
        // `native_wrapper_int_value`, whose complete behavior is a read of
        // wrapper field 0 and `Int`-or-zero normalization. The receiver is
        // already heap-validated above and this operation cannot allocate or
        // safepoint, so entering `safe_native_call` adds only rooting/panic/
        // dispatch overhead on every unbox in a compiled loop.
        return Some(match vm.mem.heap.get_field(object, 0) {
            Value::Int(value) => value as i64,
            _ => 0,
        });
    }
    let arg = match entry.kind {
        IntegerNativeKind::ValueOf => Value::Int(args_slice[0] as i32),
        IntegerNativeKind::IntValue => unreachable!("handled above"),
    };
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        entry.callback,
        std::slice::from_ref(&arg),
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    if matches!(entry.kind, IntegerNativeKind::ValueOf) {
        if let Some(Value::Object(Some(object))) = result {
            INTEGER_WRAPPER_CLASS_CACHE.with(|cache| {
                cache.set(Some((
                    vm.vm_identity,
                    vm.mem.heap.class_id_of(object).as_u32(),
                )))
            });
        }
    }
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

#[inline]
pub fn hashmap_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => Some(3),
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => Some(2),
        _ => None,
    }
}

/// Resolve the exact-layout `HashMap` fast-path native for this site, subject
/// to Real-JDK policy. Called once per call site, at cache-fill time.
///
/// The callbacks here are **hard-coded VM-side implementations**, not registry
/// lookups — which is precisely why this needed routing: nothing about the
/// bare `match` could tell whether `java/util/HashMap.put` is registered as a
/// reviewed `Intrinsic`, an ordinary `Bridge` in front of real JDK bytecode,
/// or a `SyntheticStub` that §1.3 forbids invoking outright.
/// [`admit_jit_fast_native`] asks the registry for the kind and lets
/// `resolve_native_dispatch_wave1` answer; `Compatible` keeps the exact
/// callback this function always returned.
#[inline]
pub fn hashmap_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    // JIT round 20 wave 14 (lane cjavac): under the `--compatible`
    // `jdk-hash-collections` row the map is the JDK's own (`table`), the
    // `HashMap` triples are not registered (so `admit_jit_fast_native` below
    // has no kind to ask and would admit), and these natives read stand-in
    // stores the row leaves empty -- a JDK-built map's `get` answered `null`.
    // No fast path: the generic tail runs the JDK body, as the interpreter
    // does. See `hashmap_gate_under_jdk_model`. `--real-jdk` keeps its own
    // registry-kind decision below unchanged.
    if !crate::vm::dispatch_policy(vm).is_jdk_only()
        && vm
            .natives
            .native_methods
            .aligned(cratonvm_types::HotSpotAlignment::JdkHashCollections)
    {
        return None;
    }
    let callback = match (info.method_name, info.descriptor) {
        ("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;") => {
            cratonvm_native_collections::native_hashmap_put_exact
        }
        ("get", "(Ljava/lang/Object;)Ljava/lang/Object;") => {
            cratonvm_native_collections::native_hashmap_get_exact        }
        _ => return None,
    };
    admit_jit_fast_native(
        vm,
        "java/util/HashMap",
        info.method_name,
        info.descriptor,
        callback,
    )
}

#[inline]
pub fn matcher_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("find", "()Z") | ("start", "()I") | ("end", "()I") | ("group", "()Ljava/lang/String;") => {
            Some(1)
        }
        ("find", "(I)Z")
        | ("start", "(I)I")
        | ("end", "(I)I")
        | ("group", "(I)Ljava/lang/String;") => Some(2),
        _ => None,
    }
}

/// The pre-existing, policy-free `Matcher` fast-path lookup: the operator's
/// `native_matcher_find()` opt-out and a two-`&'static str` match, nothing
/// else. No registry access, no lock, no hashing.
///
/// This is the callback half of the decision. It is never a dispatch verdict on
/// its own — every caller pairs it with an admission, either
/// [`matcher_native_callback`] (cache-fill sites) or
/// [`admit_matcher_leaf_native`] (the per-dispatch exact-receiver leaf).
#[inline]
pub fn matcher_native_callback_uncached(
    info: &JitInvokeInfo,
) -> Option<cratonvm_native_api::NativeCallback> {
    if !crate::runtime::env_cache::native_matcher_find() {
        return None;
    }
    cratonvm_native_builtins::matcher_realjdk_native_callback(info.method_name, info.descriptor)
}

/// The class the eight `Matcher` leaf triples are registered under.
pub const MATCHER_CLASS: &str = "java/util/regex/Matcher";

/// Dense index of the (method, descriptor) pair the exact-receiver `Matcher`
/// leaf serves, or `None` for anything else.
///
/// The eight rows are exactly the triples [`matcher_native_arg_count`] admits.
/// Keep the two in step: a pair with an index but no arg count would memoize an
/// admission that `call_matcher_native_raw` then declines, and a pair with an
/// arg count but no index would dispatch uncounted — which is the defect this
/// index exists to close.
#[inline]
pub fn matcher_site_index(info: &JitInvokeInfo) -> Option<usize> {
    Some(match (info.method_name, info.descriptor) {
        ("find", "()Z") => 0,
        ("start", "()I") => 1,
        ("end", "()I") => 2,
        ("group", "()Ljava/lang/String;") => 3,
        ("find", "(I)Z") => 4,
        ("start", "(I)I") => 5,
        ("end", "(I)I") => 6,
        ("group", "(I)Ljava/lang/String;") => 7,
        _ => return None,
    })
}

/// The `(method_name, descriptor)` for a [`matcher_site_index`], so the memo
/// cells can be filled without re-deriving the strings from a `JitInvokeInfo`
/// that the caller may not have.
pub const MATCHER_SITE_TRIPLES: [(&str, &str); 8] = [
    ("find", "()Z"),
    ("start", "()I"),
    ("end", "()I"),
    ("group", "()Ljava/lang/String;"),
    ("find", "(I)Z"),
    ("start", "(I)I"),
    ("end", "(I)I"),
    ("group", "(I)Ljava/lang/String;"),
];

/// How many memo cells the exact-receiver `Matcher` leaf uses: one per
/// [`MATCHER_SITE_TRIPLES`] row, indexed by [`matcher_site_index`] and nothing
/// else (ONE CELL, ONE TRIPLE, the invariant
/// [`cratonvm_native_api::NativeCallSite`] documents and `debug_assert!`s).
///
/// nio21 wave 8 L2: the cells were a process `static` array
/// (`MATCHER_LEAF_SITES`). Correct across VMs, because a memo carries the
/// registry generation and that is banded per registry, but per-VM state in
/// a process static, so two VMs thrashed it on every regex-hot dispatch.
/// They now live per VM, `JitRealm::direct_helper_gates.matcher_leaf_site(i)`.
/// The *policy* half of the decision was already per VM
/// (`NativeRealm::matcher_leaf_admission`, see [`admit_matcher_leaf_native`]).
pub const MATCHER_LEAF_SITE_COUNT: usize = MATCHER_SITE_TRIPLES.len();

/// `NativeRealm::matcher_leaf_admission` verdicts.
pub const MATCHER_ADMISSION_REFUSED: u64 = 1;
pub const MATCHER_ADMISSION_ADMITTED: u64 = 2;

/// Dispatches actually served by the exact-receiver `Matcher` leaf, reported by
/// `CRATONVM_INTRINSIC_STATS=1` beside the other compiled-code native counters.
///
/// It exists because the claim this leaf's memo was built to fix — "this edge
/// is uncounted for the §4 census" — is a claim about a RATE, and nothing
/// measured it. Without this number, "the leaf served the call and did not
/// count it" and "the leaf declined and the generic tail served it, counting it
/// properly" are indistinguishable from the census alone: both leave the
/// registry's total looking right on the workloads where the leaf never fires.
/// A zero here on a regex-hot run says the leaf is not the path that run takes
/// — which is a different fact from the census being complete, and only this
/// counter can tell the two apart.
///
/// Per-thread since round 11 (lane `rt`): it was a process-global `AtomicU64`
/// bumped with a `lock xadd` on every leaf-served dispatch — i.e. on every
/// compiled `Matcher` call on a regex-hot workload, from every thread. See
/// [`JitCounterBlock`].
///
/// Dispatches served by the compiled exact-receiver `Matcher` leaf this run.
pub fn matcher_leaf_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.matcher_leaf_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// OSRENTRY6-2 (JIT round 19 proposal wave 7, lane interp7): this VM's OSR
/// door direct-bind refusal census, `(reason, count)` largest first, with a
/// slow-door `None` classified by its cause instead of
/// `callee-not-yet-compiled` (`jit_bridge::osr_door_bind_refusal_reasons`).
/// For the launcher's end-of-run census.
pub fn osr_door_bind_refusal_reasons(shared: &SharedVm) -> Vec<(&'static str, u64)> {
    crate::runtime::interpreter::jit_bridge::osr_door_bind_refusal_reasons(shared)
}

/// Policy admission + census handle for the exact-receiver `Matcher` leaf,
/// memoized per triple so it can run on a per-dispatch path.
///
/// # What this replaces, and why the old shape was a gap
///
/// This leaf is the one hot by-name native fast path in this file that
/// re-decides on every dispatch: deciding used to be two `&'static str`
/// compares, so the three memoized siblings' cache-fill admission had nowhere
/// to live. Wave 1 therefore hoisted the Real-JDK question to a single
/// `dispatch_policy(vm).is_jdk_only()` gate and **skipped the leaf wholesale**
/// under strict. That was safe — a disabled optimization cannot invoke a
/// `SyntheticStub` — but it left two things undone, both of which this closes:
///
/// * **`Compatible` dispatched here uncounted.** Every `find`/`start`/`end`/
///   `group` served by the leaf was missing from the §4 census, and an
///   uncounted path is an unverifiable one.
/// * **Strict lost the fast route** rather than being admitted to it. These
///   natives are registered `NativeKind::Intrinsic` (see the
///   `keep_real_matcher_find_fastpath` banner in `native-builtins/src/lib.rs`),
///   so §1.4's reviewed-intrinsic exception admits them; skipping was strictly
///   conservative, not correct-by-policy.
///
/// # The cost, which is what made the old shape necessary
///
/// Warm, this is: one dense index from two `&'static str` compares, one relaxed
/// `AtomicU64` load with a generation compare (the `NativeCallSite`), and — in
/// strict mode only — one more relaxed load with a generation compare. No lock,
/// no allocation, and above all no string hashing. The three-string registry
/// hash and, in strict mode, the class-manager read lock plus hierarchy walk
/// inside `jit_fast_native_has_bytecode` are paid once per triple per registry
/// generation.
///
/// `Compatible` does not consult the per-VM memo at all: its admission is a
/// policy field read and a `Some`, so a memo would cost more than it saves.
#[inline]
pub fn admit_matcher_leaf_native(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let callback = matcher_native_callback_uncached(info)?;
    let index = matcher_site_index(info)?;
    let registry = &vm.natives.native_methods;
    let id = vm.jit.direct_helper_gates.matcher_leaf_site(index).resolve(
        registry,
        MATCHER_CLASS,
        info.method_name,
        info.descriptor,
    );
    if !crate::vm::dispatch_policy(vm).is_jdk_only() {
        return Some((callback, id));
    }
    let generation = u64::from(registry.generation());
    let cell = &vm.natives.matcher_leaf_admission[index];
    let memo = cell.load(std::sync::atomic::Ordering::Relaxed);
    if memo >> 32 == generation {
        return if memo & 0xffff_ffff == MATCHER_ADMISSION_ADMITTED {
            Some((callback, id))
        } else {
            None
        };
    }
    let admitted = jdk_only_admit_jit_fast_native(
        vm,
        MATCHER_CLASS,
        info.method_name,
        info.descriptor,
        callback,
        id,
    );
    let verdict = if admitted.is_some() {
        MATCHER_ADMISSION_ADMITTED
    } else {
        MATCHER_ADMISSION_REFUSED
    };
    cell.store(
        (generation << 32) | verdict,
        std::sync::atomic::Ordering::Relaxed,
    );
    admitted
}

/// Resolve the real-layout `Matcher` fast-path native for this site, subject
/// to Real-JDK policy. Called once per call site, at cache-fill time.
///
/// The `native_matcher_find()` env gate stays the FIRST question asked: it is
/// the operator's existing opt-out and Real-JDK must never make a disabled
/// fast path reappear. Policy is then layered on top of that verdict, never
/// under it.
///
/// These natives are registered `NativeKind::Intrinsic` (see the long
/// `keep_real_matcher_find_fastpath` banner in `native-builtins/src/lib.rs`),
/// so §1.4's reviewed-intrinsic exception admits them and the 19x real-JDK
/// `Matcher.find` fast path survives `--real-jdk` intact on this cached route.
/// That answer comes out of the registry, not out of an assumption here —
/// which is the point.
#[inline]
pub fn matcher_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let callback = matcher_native_callback_uncached(info)?;
    admit_jit_fast_native(
        vm,
        "java/util/regex/Matcher",
        info.method_name,
        info.descriptor,
        callback,
    )
}

#[inline]
pub fn stringbuilder_native_arg_count(info: &JitInvokeInfo) -> Option<usize> {
    match (info.method_name, info.descriptor) {
        ("append", "(I)Ljava/lang/StringBuilder;")
        | ("append", "(C)Ljava/lang/StringBuilder;")
        | ("append", "(Ljava/lang/String;)Ljava/lang/StringBuilder;") => Some(2),
        ("toString", "()Ljava/lang/String;") | ("length", "()I") => Some(1),
        _ => None,
    }
}

/// Resolve the registered StringBuilder native for this site ONCE (the
/// registry's 3-string hash) — cached per callsite afterwards, exactly like
/// the HashMap/Matcher kinds. Returns `None` (no caching, generic dispatch)
/// when no native is registered for the triple.
///
/// This was a bare `NativeMethodRegistry::find`, which discards the
/// `NativeKind` — so it could not tell a reviewed intrinsic from a
/// `SyntheticStub` and could not honour §1.3 or §1.4. It now resolves through
/// `resolve_id` + `callback_of`, whose semantics the registry documents as
/// *identical* to `find` (descriptor-quirk fallback included), so `Compatible`
/// mode gets the same callback from the same lookup — plus the
/// `NativeMethodId` the census needs and the kind policy needs.
#[inline]
pub fn stringbuilder_native_callback(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let registry = &vm.natives.native_methods;
    let id = registry.resolve_id("java/lang/StringBuilder", info.method_name, info.descriptor)?;
    let callback = registry.callback_of(id)?;
    admit_jit_fast_native_resolved(
        vm,
        "java/lang/StringBuilder",
        info.method_name,
        info.descriptor,
        callback,
        Some(id),
    )
}

/// Invoke a cached StringBuilder native from raw JIT argument slots.
#[inline]
pub fn call_stringbuilder_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = stringbuilder_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }
    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        match info.descriptor.as_bytes().get(1) {
            // int / char parameter — the natives take Value::Int for both.
            Some(b'I') | Some(b'C') => values[1] = Value::Int(args_slice[1] as i32),
            Some(b'L') => {
                let raw = args_slice[1];
                // `append((String) null)` must append "null" — the generic
                // path (appendNull routing) owns that; don't serve it here.
                if raw == 0 {
                    return None;
                }
                let bits = raw as u64;
                if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
                    return None;
                }
                let object = vm.mem.heap.is_object_address(bits as usize)?;
                values[1] = Value::Object(Some(object));
            }
            _ => return None,
        }
    }
    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Can a `java/util/regex/Matcher` receiver reach this call site at all?
///
/// `Matcher` is `final`, so its instances reach a virtual/interface site only
/// through `Matcher` itself or a supertype that declares the leaf triples,
/// which is `MatchResult` (`Object` declares none of them). Any other static
/// class rules the exact-receiver leaf out without looking at the receiver.
#[inline]
pub fn matcher_leaf_site_class(info: &JitInvokeInfo) -> bool {
    info.class_name == MATCHER_CLASS || info.class_name == "java/util/regex/MatchResult"
}

#[inline]
pub fn is_exact_matcher_class(vm: &SharedVm, class_id: ClassId) -> bool {
    let vm_key = vm.vm_identity;
    let raw_class_id = class_id.as_u32();
    if MATCHER_CLASS_CACHE.with(|cache| cache.get() == Some((vm_key, raw_class_id))) {
        return true;
    }

    let is_exact = vm
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|class| class.name.as_ref() == "java/util/regex/Matcher")
        .unwrap_or(false);
    if is_exact {
        MATCHER_CLASS_CACHE.with(|cache| cache.set(Some((vm_key, raw_class_id))));
    }
    is_exact
}

#[inline]
pub fn call_object_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    entry: NativeDispatchCache,
) -> Option<i64> {
    let result = match entry.kind {
        ObjectNativeKind::HashMap => {
            call_hashmap_native_raw(vm, thread, info, receiver_ref, args_slice, entry.callback)
        }
        ObjectNativeKind::Matcher => call_matcher_native_raw(
            vm,
            thread,
            info,
            receiver_ref,
            args_slice,
            entry.callback,
            entry.native_id,
        ),
        ObjectNativeKind::StringBuilder => call_stringbuilder_native_raw(
            vm,
            thread,
            info,
            receiver_ref,
            args_slice,
            entry.callback,
        ),
    };
    // §4 census, counted on the dispatching edge.
    //
    // `Some` is exactly "the native ran": each of the three leaves returns
    // `None` only from an argument-shape guard taken BEFORE
    // `safe_native_call_prevalidated_objects`, and returns `Some` on both the
    // `Ok` and the `Err` (native threw) arms after it. Counting here rather
    // than inside each leaf keeps the census on one line instead of threaded
    // through three signatures, and cannot count a call that did not happen.
    if result.is_some() {
        count_jit_native_dispatch(vm, entry.native_id);
    }
    result
}

/// Invoke a cached HashMap bridge directly from raw JIT argument slots.
#[inline]
pub fn call_hashmap_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
) -> Option<i64> {
    let expected_len = match args_slice.len() {
        len @ (2 | 3) => len,
        _ => return None,
    };

    let mut values = [Value::Object(None); 3];
    values[0] = Value::Object(Some(receiver_ref));
    for (index, &raw) in args_slice[1..].iter().enumerate() {
        if raw == 0 {
            continue;
        }
        let bits = raw as u64;
        if (bits & 0x7) != 0 || bits >= (1u64 << 48) {
            return None;
        }
        let object = vm.mem.heap.is_object_address(bits as usize)?;
        values[index + 1] = Value::Object(Some(object));
    }

    let result = match crate::vm::safe_native_call_prevalidated_objects(
        vm,
        thread,
        callback,
        &values[..expected_len],
    ) {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Invoke a cached real-layout Matcher native from raw JIT argument slots.
#[inline]
pub fn call_matcher_native_raw(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    args_slice: &[i64],
    callback: cratonvm_native_api::NativeCallback,
    native_id: Option<cratonvm_native_api::NativeMethodId>,
) -> Option<i64> {
    let expected_len = matcher_native_arg_count(info)?;
    if args_slice.len() != expected_len {
        return None;
    }

    let mut values = [Value::Object(None); 2];
    values[0] = Value::Object(Some(receiver_ref));
    if expected_len == 2 {
        values[1] = Value::Int(args_slice[1] as i32);
    }

    let call = if native_id.is_some_and(|id| vm.natives.native_methods.is_leaf_id(id))
        && cratonvm_native_builtins::matcher_realjdk_leaf_search_cached(
            vm.vm_identity,
            receiver_ref.as_ptr() as usize,
        ) {
        crate::vm::safe_native_call_leaf_prevalidated(vm, thread, callback, &values[..expected_len])
    } else {
        crate::vm::safe_native_call_prevalidated_objects(
            vm,
            thread,
            callback,
            &values[..expected_len],
        )
    };
    let result = match call {
        Ok(value) => value,
        Err(error) => return Some(handle_jit_dispatch_error(vm, thread, error, info)),
    };
    Some(match result {
        Some(Value::Int(value)) => value as i64,
        Some(Value::Long(value)) => value,
        Some(Value::Float(value)) => value.to_bits() as i64,
        Some(Value::Double(value)) => value.to_bits() as i64,
        Some(Value::Object(Some(object))) => object.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => 0,
    })
}

/// Specialize a lambda proxy's erased `Function.apply(Object)` call when its
/// implementation is a concrete `(int) -> double` virtual/interface target.
///
/// The generic lambda route allocates vectors, reparses descriptors, and builds
/// an interpreter frame for each application. TDigest invokes this shape in
/// its quantile/cdf numeric kernels, so dispatch directly to the already-JITed
/// receiver method and only retain the Java-mandated boxed Double result.
// SAFETY: called by the JIT's adapter fold, at a run whose four members the
// caller's constant pool proves (`lambda_int_to_double_runs_proven`).
pub unsafe extern "C" fn jit_lambda_int_to_double(vm_ptr: i64, proxy_raw: i64, index: i64) -> i64 {
    let f = || jit_lambda_int_to_double_body(vm_ptr, proxy_raw, index);
    contain(
        "jit_lambda_int_to_double",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// Body of [`jit_lambda_int_to_double`], run under its panic guard.
pub unsafe fn jit_lambda_int_to_double_body(vm_ptr: i64, proxy_raw: i64, index: i64) -> i64 {
    crate::jit::conservative_roots::note_jit_boundary();
    jit_safepoint_flush_satb(vm_ptr);
    let vm = &*(vm_ptr as *const SharedVm);
    // r9-vmside: every arm below that used to `return f64::NAN.to_bits()`
    // ANSWERED the Java expression `((Double) f.apply(Integer.valueOf(i)))
    // .doubleValue()` with NaN — a null `f`, a `Function` that is not the one
    // lambda shape this peephole knows, a lambda whose capture is null, a
    // `get` that threw. The peephole is chosen by the call site's constant
    // pool (`cratonvm_jit::lambda_int_to_double_runs_proven`; by method NAME,
    // `Dist.quantile*`/`Dist.cdf*`, until interpreter round i1 wave 10), not
    // by proving the receiver's type, so "not the expected shape" is an
    // ordinary input, and a NaN is a silent wrong answer in a numeric kernel. Each arm now gives the Java answer: an
    // NPE for a null receiver, and the generic `apply` for everything else.
    if proxy_raw == 0 {
        set_jit_pending_npe_action(
            crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
        );
        return i64::MIN;
    }
    // Interpreter-only mode (`jvmti_requires_interpreter`): the direct route
    // below calls a compiled `get(I)D` and skips the lambda's own frames, so
    // no MethodEntry/MethodExit or breakpoint would be delivered for either.
    // The generic `apply` goes through `jit_invoke_dispatch`, which stands down
    // the same way. Interpreter round i1 wave 8, L7.
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm) {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    }
    let Some(proxy) = vm.mem.heap.is_object_address(proxy_raw as usize) else {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    };
    // Class 0 for an array (`jit_dispatch_class`): no lambda call site.
    let proxy_class_id = jit_dispatch_class_validated(vm, proxy)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    // Read BEFORE anything below is resolved, so a memo stamped with it saw
    // every publication and definition it counts (`LambdaGetMemo`).
    let memo_stamp = lambda_get_memo_stamp(vm);
    let memo_key = (vm.vm_identity, proxy_class_id.as_u32());
    let shape_ok = match lambda_get_memo_shape(memo_key, memo_stamp) {
        Some(shape_ok) => shape_ok,
        None => {
            let impl_class_name = lambda_int_to_double_shape(vm, proxy_class_id);
            let shape_ok = impl_class_name.is_some();
            lambda_get_memo_store(
                memo_key,
                LambdaGetMemo {
                    stamp: memo_stamp,
                    impl_class_name,
                    route: None,
                },
            );
            shape_ok
        }
    };
    if !shape_ok {
        return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
    }
    let mut receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        // A null capture: the generic `apply` raises the NPE its body would.
        _ => return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index),
    };
    // Class 0 for an array (`jit_dispatch_class`), which names no class, so
    // the compiled-`get` lookup below declines rather than finding the
    // COMPONENT's `get(I)D`.
    let receiver_class_id = jit_dispatch_class(vm, receiver)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    // JVMS §5.4.6: the direct route below compiles and calls the `get(I)D` the
    // lenient walk from the receiver's class finds. Where selection against
    // the method handle's owner picks another declaration (a private hider, or
    // a public method in another package that does not override a
    // package-private `get`), the generic `apply` runs instead. The guard is
    // dropped before either route runs (the generic one dispatches Java).
    //
    // Memoized per `(proxy class, receiver class)` with the proxy's shape
    // (`LambdaGetMemo`): this resolution, the by-name `jit_cache` probe and a
    // declined compile used to be repeated on every element of the kernel.
    let compiled = match lambda_get_memo_route(memo_key, memo_stamp, receiver_class_id) {
        Some(LambdaGetRoute::Generic) => {
            return lambda_int_to_double_generic(vm, vm_ptr, proxy_raw, index);
        }
        Some(LambdaGetRoute::Direct(compiled)) => compiled,
        None => match resolve_lambda_get_route(
            vm,
            proxy,
            proxy_class_id,
            receiver_class_id,
            &mut receiver,
            memo_key,
            memo_stamp,
        ) {
            Ok(compiled) => compiled,
            Err(proxy_word) => {
                return lambda_int_to_double_generic(vm, vm_ptr, proxy_word, index);
            }
        },
    };
    if let Some(compiled) = compiled {
        let args = [receiver.as_ptr() as i64, index as i32 as i64];
        // The scalar helper is entered by a JIT caller that already owns the
        // active-root chain entry. Mirror the established virtual-dispatch
        // cache and re-enter this leaf directly: a second guard would mutate
        // that chain (and invalidate its scan cache) for every element.
        // Finished through `lambda_get_call_then_route`: the compiled `get`'s
        // sentinel is serviced here, never handed up raw (round 12 wave 2).
        if let Some(result) = lambda_get_call_then_route(vm, receiver_class_id, &args, || {
            try_call_compiled_entry_reentrant(
                compiled.entry_ptr() as usize,
                compiled.needs_context(),
                vm_ptr,
                &args,
            )
        }) {
            return result;
        }
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        return f64::NAN.to_bits() as i64;
    };
    match crate::vm::invoke_on_class_shared(
        vm,
        thread,
        receiver_class_id,
        "get",
        "(I)D",
        &[Value::Object(Some(receiver)), Value::Int(index as i32)],
    ) {
        Ok(Some(Value::Double(value))) => value.to_bits() as i64,
        // r9-vmside: `get` threw (`IndexOutOfBoundsException` from an
        // out-of-range `list.get(i)`, say). Park it and return the sentinel
        // the compiled caller's `D`-width check tests for, exactly as the
        // dispatch helper does — it used to become NaN.
        Err(error) => {
            handle_jit_dispatch_error(vm, thread, error, &LAMBDA_INT_TO_DOUBLE_APPLY_INFO)
        }
        _ => f64::NAN.to_bits() as i64,
    }
}

/// The call site [`lambda_get_call_then_route`] finishes a compiled `get(I)D`
/// under: a receiver-dispatched `get(I)D` with no constant-pool context. Every
/// service it is handed to resolves the callee from the RECEIVER's class
/// (`invoke_kind` 0; `declaring_class_id` 0, so `jit_site_owner` asks no
/// selection question: the direct route already asked it,
/// `resolve_lambda_get_route`), and its static address keeps the per-site
/// memos those services key on (`callee_handler_template_key`, the
/// virtual-target memo) on one stable identity. `class_name` only names the
/// site in diagnostics: with a receiver present nothing resolves by it.
pub static LAMBDA_GET_DIRECT_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Object",
    method_name: "get",
    descriptor: "(I)D",
    num_jit_args: 2,
    return_type: b'D',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// [`LAMBDA_GET_DIRECT_INFO`] with the receiver's class as its loader context,
/// for [`try_resume_trapped_callee`] only. That function resolves the stash's
/// declaring class through `declaring_class_id`'s loader and, at `0`, through
/// the bootstrap loader alone, which refuses every application `get` (the
/// frame is then re-run from entry instead of resumed). The receiver's class
/// sees the declaring class: it is that class or one of its supertypes. Built
/// per call, so it must never key a memo; the resume keys none.
pub fn lambda_get_resume_site(receiver_class_id: ClassId) -> JitInvokeInfo {
    JitInvokeInfo {
        class_name: LAMBDA_GET_DIRECT_INFO.class_name,
        method_name: LAMBDA_GET_DIRECT_INFO.method_name,
        descriptor: LAMBDA_GET_DIRECT_INFO.descriptor,
        num_jit_args: LAMBDA_GET_DIRECT_INFO.num_jit_args,
        return_type: LAMBDA_GET_DIRECT_INFO.return_type,
        invoke_kind: LAMBDA_GET_DIRECT_INFO.invoke_kind,
        declaring_class_id: receiver_class_id.as_u32(),
        owner_class_id: 0,
    }
}

/// Precise resume of a PURE deopt that a directly called compiled `get(I)D`
/// left stashed: rebuild its frame at the trap and run it to completion, as
/// the dispatch helper's compiled arms do. `None`, stash untouched, when there
/// is no stash, when a throwable is pending beside it (the general service in
/// [`lambda_get_call_then_route`] owns that shape), or when the resume refuses.
///
/// # Safety
/// As [`try_resume_trapped_callee`]: `thread` is this thread's exclusive borrow.
pub unsafe fn resume_trapped_lambda_get(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver_class_id: ClassId,
) -> Option<i64> {
    if receiver_class_id.as_u32() == 0
        || !cratonvm_jit::deopt::has_last_deopt()
        || jit_pending_exception_is_set()
    {
        return None;
    }
    let site = lambda_get_resume_site(receiver_class_id);
    try_resume_trapped_callee(vm, thread, &site, receiver_class_id, None)
}

/// Call the compiled `get(I)D` the direct route chose through `call`, and
/// finish the call the way the dispatch helper finishes its own compiled arms
/// (`call_compiled_then_route`). `None` when `call` made no call.
///
/// Round 12 wave 2 (lane osrfix): this route used to hand the callee's raw
/// result back. A compiled `get` that TRAPPED (an uncommon trap on the branch
/// it had never taken, typically the `throw` arm of its own bounds check)
/// answers the `i64::MIN` sentinel with its reconstructed frame stashed, and
/// only the helper between the two frames can resume that frame. Handed up
/// raw, the stash reached the compiled caller's sink as a FOREIGN frame: an
/// OSR'd caller raised `InternalError` ("a callee's stashed frame reached the
/// OSR sink",
/// r12w1-orch-osr-catch-around-lambda-peephole-throws-internalerror), a
/// method-entry caller re-ran from bci 0. Now a pure deopt is resumed at its
/// trap ([`resume_trapped_lambda_get`]), and everything else -- a throw routed
/// through `get`'s own handler, a frameless trap, a declined stash re-run from
/// entry -- takes [`route_implicit_exc_through_callee`], as for any
/// receiver-dispatched compiled callee. A throw that escapes `get` stays
/// parked beside the sentinel for the caller's `D`-width check.
///
/// The receiver is pinned across the call and the routing, which can re-run
/// `get` with it after a collection moved it
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
///
/// # Safety
/// Called from [`jit_lambda_int_to_double_body`] with no `jit_thread_mut`
/// borrow live; `call` enters `receiver_class_id`'s live, unwrapped `get(I)D`
/// body with `args`, which is `[receiver, index]`.
pub unsafe fn lambda_get_call_then_route(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    args: &[i64; 2],
    call: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    if !lambda_get_service_enabled() {
        return call();
    }
    let info = &LAMBDA_GET_DIRECT_INFO;
    let pins =
        jit_thread_mut().map(|(thread, _guard)| CompileArgPins::pin_raw(vm, thread, info, args));
    let routed = call().map(|rc| {
        if rc != i64::MIN {
            return rc;
        }
        if let Some(v) = jit_thread_mut()
            .and_then(|(thread, _guard)| resume_trapped_lambda_get(vm, thread, receiver_class_id))
        {
            return v;
        }
        route_implicit_exc_through_callee(vm, info, args, receiver_class_id, rc, pins.as_ref())
    });
    if let Some(pins) = pins {
        // As `call_compiled_then_route`: every nested entry restores the
        // thread pointer, so this finds it.
        if let Some((thread, _guard)) = jit_thread_mut() {
            pins.release(thread);
        }
    }
    routed
}

/// `CRATONVM_JIT_LAMBDA_GET_SERVICE` (default on; `0` hands the compiled
/// `get`'s raw result back as before round 12 wave 2, sentinel and stash
/// included). Cached once per process.
pub fn lambda_get_service_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// The unmemoized half of [`jit_lambda_int_to_double_body`]'s route choice for
/// `receiver` (a `receiver_class_id` instance captured by `proxy`): the
/// JVMS §5.4.6 selection test, the compiled `get(I)D` probe, and the one
/// compile the first call asks for. `Ok(body)` is the direct route (`None`:
/// the interpreted `get`); `Err(proxy word)` is the generic `apply`, on the
/// proxy's current address. Records its answer in `LAMBDA_GET_MEMO` except
/// when the capture read back null after the compile. `receiver` is re-read
/// when the compile may have moved it.
// SAFETY: called only from `jit_lambda_int_to_double_body`, on this thread's
// live JIT frame (`jit_thread_mut`); `proxy` was validated by the caller.
pub unsafe fn resolve_lambda_get_route(
    vm: &SharedVm,
    proxy: ObjectRef,
    proxy_class_id: ClassId,
    receiver_class_id: ClassId,
    receiver: &mut ObjectRef,
    memo_key: (usize, u32),
    memo_stamp: LambdaGetStamp,
) -> Result<Option<std::sync::Arc<cratonvm_jit::CompiledMethod>>, i64> {
    let Some(impl_class_name) = lambda_get_memo_impl_class(memo_key, memo_stamp)
        .or_else(|| lambda_int_to_double_shape(vm, proxy_class_id))
    else {
        return Err(proxy.as_ptr() as i64);
    };
    let class_name = {
        let cm = vm.classes.class_manager.read();
        let name = cm.get_class(receiver_class_id).and_then(|class| {
            let selection_moves = cm
                .find_class_by_name_for_class(&impl_class_name, receiver_class_id)
                .and_then(|owner| {
                    crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                        cm.class_store(),
                        receiver_class_id,
                        owner,
                        "get",
                        "(I)D",
                    )
                })
                .is_some();
            (!selection_moves).then(|| class.name.clone())
        });
        name
    };
    let Some(class_name) = class_name else {
        lambda_get_memo_note_route(
            memo_key,
            memo_stamp,
            receiver_class_id,
            LambdaGetRoute::Generic,
        );
        return Err(proxy.as_ptr() as i64);
    };
    let mut compiled = {
        let cache = vm.jit.jit_cache.read();
        cache
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // `java.util.Vector.get(int)` is `synchronized`, so this route is
            // one `Vector` receiver away from calling a monitor-bearing body
            // with no monitor. See `CompiledMethod::requires_wrapped_entry`.
            .filter(|compiled| !compiled.requires_wrapped_entry)
    };
    if compiled.is_none() {
        // This direct scalar route bypasses the normal bytecode invocation
        // counter, so publish the receiver-specialized getter once here.
        //
        // The compile may load classes, i.e. run Java and collect. `proxy` is
        // this helper's own copy, so it is pinned across the compile and
        // re-read (see [`CompileArgPins`]); `load_and_forward` was the repair
        // here, and it needs a forwarding word at the old address that ZGC's
        // slide does not leave.
        let mut pinned_proxy = [Value::Object(Some(proxy))];
        let pins = jit_thread_mut()
            .map(|(thread, _guard)| CompileArgPins::pin_values(vm, thread, &pinned_proxy));
        let _ = crate::runtime::interpreter::try_jit_compile_callee_for_class(
            vm,
            receiver_class_id,
            &class_name,
            "get",
            "(I)D",
            true,
        );
        if let Some(pins) = pins {
            if let Some((thread, _guard)) = jit_thread_mut() {
                pins.release_values(thread, &mut pinned_proxy);
            }
        }
        compiled = vm
            .jit
            .jit_cache
            .read()
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // r9-vmside: the same filter as the first probe. This re-probe had
            // lost it, so the FIRST call on a cold `Vector` receiver — the one
            // that compiles `get` — entered the `synchronized` body with no
            // monitor, exactly what the comment on the first probe forbids.
            .filter(|compiled| !compiled.requires_wrapped_entry);
        // `receiver` is this helper's own copy of the proxy's field: re-read it
        // through the re-read proxy rather than call `get` on a pre-move
        // address.
        let proxy = match pinned_proxy[0] {
            Value::Object(Some(proxy)) => proxy,
            _ => proxy,
        };
        *receiver = match vm.mem.heap.get_field(proxy, 0) {
            Value::Object(Some(captured)) => captured,
            _ => return Err(proxy.as_ptr() as i64),
        };
    }
    // A declined compile is remembered too: until the stamp moves (any
    // publication, definition or redefinition), this pair takes the
    // interpreted `get` without asking the compiler again on every element.
    lambda_get_memo_note_route(
        memo_key,
        memo_stamp,
        receiver_class_id,
        LambdaGetRoute::Direct(compiled.clone()),
    );
    Ok(compiled)
}

/// The impl class of `proxy_class_id`'s lambda call site when it is the one
/// shape [`jit_lambda_int_to_double`] routes directly: a one-capture
/// `Function.apply(Object)` whose implementation is a virtual or interface
/// `get(I)D`. `None` for every other proxy.
pub fn lambda_int_to_double_shape(
    vm: &SharedVm,
    proxy_class_id: ClassId,
) -> Option<std::sync::Arc<str>> {
    let call_site = vm.classes.lambda_call_site_for(proxy_class_id)?;
    let shape = call_site.functional_interface.as_ref() == "java/util/function/Function"
        && call_site.sam_method_name.as_ref() == "apply"
        && call_site.sam_descriptor.as_ref() == "(Ljava/lang/Object;)Ljava/lang/Object;"
        && call_site.capture_types.len() == 1
        && call_site.impl_handle.member_name.as_ref() == "get"
        && call_site.impl_handle.descriptor.as_ref() == "(I)D"
        && matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        );
    shape.then(|| call_site.impl_handle.class_name.clone())
}

/// What [`jit_lambda_int_to_double`] chose for one receiver class of one
/// lambda proxy class.
#[derive(Clone)]
pub enum LambdaGetRoute {
    /// The generic `apply`: selection picks another `get(I)D` than the
    /// receiver's lenient walk (JVMS §5.4.6), or the receiver's class is
    /// unknown.
    Generic,
    /// The direct route: the compiled, unwrapped `get(I)D`, or `None` (no body
    /// was published and the compile declined): the interpreted `get`.
    Direct(Option<std::sync::Arc<cratonvm_jit::CompiledMethod>>),
}

/// `(process JIT cache generation, class-definition epoch of the VM's class
/// store, the VM's redefinition epoch)`. Every answer a [`LambdaGetMemo`]
/// holds is a function of these: a compiled body is found by exact key
/// (a publication, replacement or withdrawal moves the generation), the
/// shape and the selection test by class ids and names (a definition or an
/// unload moves the epoch), and a redefinition moves the last.
pub type LambdaGetStamp = (u64, u64, u32);

/// One lambda proxy class's resolution in [`jit_lambda_int_to_double`], valid
/// while the stamp holds (round 12 wave 1, lane rt; item 4's first bullet of
/// `r11-rt-helper-hot-paths-repeat-work-per-call`). Before it, every element
/// of a `Dist.quantile`/`cdf` kernel re-did the call-site lookup (a read
/// lock), the selection test (the class-manager read lock and an `Arc<str>`
/// clone), a by-name `jit_cache` probe, and, when `get` could not compile, the
/// compile request itself.
pub struct LambdaGetMemo {
    pub stamp: LambdaGetStamp,
    /// `Some(impl class)` when the proxy's call site is the one shape.
    pub impl_class_name: Option<std::sync::Arc<str>>,
    /// The route for the last receiver class seen: these kernels are
    /// monomorphic, so one slot per proxy class is enough.
    pub route: Option<(u32, LambdaGetRoute)>,
}

/// Entries kept per thread before the memo is emptied (it is keyed on class
/// ids, so a program with many lambda classes must not grow it without bound).
pub const LAMBDA_GET_MEMO_CAP: usize = 64;

thread_local! {
    /// `(vm_identity, proxy ClassId) -> LambdaGetMemo`. Not a
    /// `site_keyed_memos!` member: no `JitInvokeInfo` address is in the key,
    /// and a class id is covered by the stamp's class-definition epoch.
    // addr-keyed: not an address -- VM identity + class id.
    pub static LAMBDA_GET_MEMO: std::cell::RefCell<rustc_hash::FxHashMap<(usize, u32), LambdaGetMemo>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// `CRATONVM_JIT_LAMBDA_GET_MEMO`: default ON; `0` / `false` / `off`
/// resolves on every call, as before round 12.
pub fn lambda_get_memo_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// The stamp, read BEFORE anything it validates is resolved.
pub fn lambda_get_memo_stamp(vm: &SharedVm) -> LambdaGetStamp {
    (
        cratonvm_jit::jit_cache_generation(),
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        vm.jit.jit_cache.redefine_epoch(),
    )
}

/// The memoized shape verdict for `key`, if a memo valid at `stamp` holds one.
pub fn lambda_get_memo_shape(key: (usize, u32), stamp: LambdaGetStamp) -> Option<bool> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .map(|memo| memo.impl_class_name.is_some())
    })
}

/// The memoized impl class name for `key` at `stamp`.
pub fn lambda_get_memo_impl_class(
    key: (usize, u32),
    stamp: LambdaGetStamp,
) -> Option<std::sync::Arc<str>> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .and_then(|memo| memo.impl_class_name.clone())
    })
}

/// The memoized route for `receiver_class_id` under `key` at `stamp`.
pub fn lambda_get_memo_route(
    key: (usize, u32),
    stamp: LambdaGetStamp,
    receiver_class_id: ClassId,
) -> Option<LambdaGetRoute> {
    if !lambda_get_memo_enabled() {
        return None;
    }
    LAMBDA_GET_MEMO.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|memo| memo.stamp == stamp)
            .and_then(|memo| memo.route.as_ref())
            .filter(|(cid, _)| *cid == receiver_class_id.as_u32())
            .map(|(_, route)| route.clone())
    })
}

/// Replace `key`'s memo.
pub fn lambda_get_memo_store(key: (usize, u32), memo: LambdaGetMemo) {
    if !lambda_get_memo_enabled() {
        return;
    }
    LAMBDA_GET_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= LAMBDA_GET_MEMO_CAP && !m.contains_key(&key) {
            m.clear();
        }
        m.insert(key, memo);
    });
}

/// Record `route` for `receiver_class_id` in `key`'s memo, if that memo is
/// still the one resolved at `stamp` (a nested call on this thread may have
/// replaced it with a newer one while the compile ran Java).
pub fn lambda_get_memo_note_route(
    key: (usize, u32),
    stamp: LambdaGetStamp,
    receiver_class_id: ClassId,
    route: LambdaGetRoute,
) {
    if !lambda_get_memo_enabled() {
        return;
    }
    LAMBDA_GET_MEMO.with(|m| {
        if let Some(memo) = m.borrow_mut().get_mut(&key) {
            if memo.stamp == stamp {
                memo.route = Some((receiver_class_id.as_u32(), route));
            }
        }
    });
}

/// Synthetic call site for [`lambda_int_to_double_generic`]: the erased
/// `Function.apply(Object)` the peephole replaced, dispatched on its receiver.
pub static LAMBDA_INT_TO_DOUBLE_APPLY_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/util/function/Function",
    method_name: "apply",
    descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
    num_jit_args: 2,
    return_type: b'L',
    invoke_kind: 2,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// HotSpot's JEP 358 message for `((Double) f.apply(i)).doubleValue()` when
/// `apply` returned null (`L7LambdaIntToDoubleFold.java`).
pub const LAMBDA_INT_TO_DOUBLE_NULL_RESULT_NPE: &str =
    "Cannot invoke \"java.lang.Double.doubleValue()\" \
     because the return value of \"java.util.function.Function.apply(Object)\" is null";

/// The Java semantics [`jit_lambda_int_to_double`] stands in for, for every
/// receiver its fast shape does not cover:
/// `((Double) f.apply(Integer.valueOf(i))).doubleValue()`.
///
/// Four steps, each with the exception the bytecode would raise: box the
/// index (`Integer.valueOf`, through its own direct helper), dispatch `apply`
/// on the receiver through the generic dispatcher (an exception it throws is
/// parked and answered with the sentinel), `checkcast Double` (a
/// `ClassCastException`), `doubleValue()` on null (an NPE). Every failure
/// returns `i64::MIN` with the signal parked, which the compiled caller's
/// `D`-width post-invoke check disambiguates through `dispatch_threw`.
///
/// # Safety
/// Called only from [`jit_lambda_int_to_double_body`], i.e. from compiled code
/// with a live `vm_ptr`. `proxy_raw` may be stale after a collection;
/// `jit_invoke_dispatch` forwards its reference arguments.
#[cold]
#[inline(never)]
pub unsafe fn lambda_int_to_double_generic(
    vm: &SharedVm,
    vm_ptr: i64,
    proxy_raw: i64,
    index: i64,
) -> i64 {
    // `Integer.valueOf(i)`. Its direct helper returns `i64::MIN` with the
    // exception parked when boxing fails.
    //
    // Boxing an index outside the `Integer` cache ALLOCATES, and an allocation
    // can collect and move the proxy. `proxy_raw` is this helper's own copy —
    // the compiled caller's frame slot is fixed up, this local is not — and
    // `jit_invoke_dispatch`'s argument forwarding only recovers an object
    // while its old address still holds a forwarding pointer, which fresh
    // allocation into the reclaimed space can overwrite. Pin a validated proxy
    // in the thread's `native_pin_roots` (scanned and remapped) across the box
    // and re-read it (r11-rt-lambda-proxy-raw-pointer-held-across-boxing).
    // The thread borrow is released before boxing, which takes its own.
    let pin_idx = vm
        .mem
        .heap
        .is_object_address(proxy_raw as usize)
        .and_then(|proxy| {
            jit_thread_mut().map(|(thread, _guard)| {
                let idx = thread.native_pin_roots.len();
                thread.native_pin_roots.push(proxy);
                idx
            })
        });
    let boxed = jit_integer_value_of_direct(vm_ptr, index as i32 as i64);
    let proxy_raw = match pin_idx {
        Some(idx) => match jit_thread_mut() {
            Some((thread, _guard)) => {
                let current = thread
                    .native_pin_roots
                    .get(idx)
                    .map_or(proxy_raw, |proxy| proxy.as_ptr() as usize as i64);
                thread.native_pin_roots.truncate(idx);
                current
            }
            None => proxy_raw,
        },
        None => proxy_raw,
    };
    if boxed == i64::MIN {
        return i64::MIN;
    }
    // The box is this helper's own raw local: `jit_integer_value_of_direct`
    // no longer leaves it rooted in `native_pending_return` once it returns
    // (handoff-w36b), and nothing else names it until the dispatch below has
    // forwarded its arguments into the callee's frame. Pin it across that
    // entry (its SATB flush can meet a collection), exactly as the proxy was
    // pinned across the box.
    let boxed_pin = vm.mem.heap.is_object_address(boxed as usize).and_then(|b| {
        jit_thread_mut().map(|(thread, _guard)| {
            let idx = thread.native_pin_roots.len();
            thread.native_pin_roots.push(b);
            idx
        })
    });
    let args = [proxy_raw, boxed];
    let result = jit_invoke_dispatch(
        vm_ptr,
        &LAMBDA_INT_TO_DOUBLE_APPLY_INFO as *const JitInvokeInfo as i64,
        args.as_ptr() as i64,
        2,
    );
    if let Some(idx) = boxed_pin {
        if let Some((thread, _guard)) = jit_thread_mut() {
            thread.native_pin_roots.truncate(idx);
        }
    }
    if result == i64::MIN {
        // `apply` threw, or the dispatcher refused: its signal is parked.
        return i64::MIN;
    }
    if result == 0 {
        // `doubleValue()` on the null `apply` returned. The compiled frame
        // reports the bci of the fold's first instruction (the `apply`), where
        // an action-only JEP 358 message cannot be derived, so the message is
        // spelled here: HotSpot's, computed at the `invokevirtual doubleValue`.
        let Some((thread, _guard)) = jit_thread_mut() else {
            set_jit_pending_npe_action(
                crate::runtime::exceptions::helpful_npe::jit_action::INVOKE_RECEIVER,
            );
            return i64::MIN;
        };
        if let crate::error::MethodCallFailed::ExceptionThrown(exc) =
            crate::runtime::exceptions::throw_runtime_error(
                vm,
                thread,
                crate::error::RuntimeError::NullPointerException {
                    message: Some(LAMBDA_INT_TO_DOUBLE_NULL_RESULT_NPE.to_string()),
                },
            )
        {
            set_jit_pending_exception(thread, exc);
        }
        return i64::MIN;
    }
    let Some(obj) = vm.mem.heap.is_object_address(result as usize) else {
        // Not an object the heap recognises — nothing to cast or unbox.
        return f64::NAN.to_bits() as i64;
    };
    // An array is never a `Double`, though a `Double[]`'s header names
    // `Double` (`jit_dispatch_class`); it takes the refusal below, which used
    // to read "field 0" out of the array instead.
    let obj_class_name = match jit_dispatch_class_validated(vm, obj) {
        JitDispatchClass::Class(obj_class_id) => vm
            .classes
            .class_manager
            .read()
            .get_class(obj_class_id)
            .map(|c| c.name.to_string()),
        JitDispatchClass::JavaLangObject => Some(
            crate::runtime::interpreter::cce_display_class_name(vm, obj, "java/lang/Object"),
        ),
    };
    if obj_class_name.as_deref() == Some("java/lang/Double") {
        // `java.lang.Double` is final, so the name is the whole `checkcast`.
        if let Value::Double(d) = vm.mem.heap.get_field(obj, 0) {
            return d.to_bits() as i64;
        }
        return f64::NAN.to_bits() as i64;
    }
    // `checkcast java/lang/Double` refused.
    let Some((thread, _guard)) = jit_thread_mut() else {
        return f64::NAN.to_bits() as i64;
    };
    // JIT round 21 wave 25, lane hexname (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
    // the receiver by its BOTTOM class's hidden flag (a hidden class keeps its
    // `/0x<hex>` tail, an ordinary `p/0x2A` is dotted); the bare `replace`
    // stays the fallback and the `=0` text.
    let shown = obj_class_name.as_deref().unwrap_or("<unknown>");
    let message = format!(
        "class {} cannot be cast to class java.lang.Double",
        crate::runtime::exceptions::external_name_by_bottom_class_or(
            shown,
            obj_class_name.as_deref().and_then(|name| {
                crate::runtime::exceptions::bottom_class_hidden(vm, vm.mem.heap.class_id_of(obj), name)
            }),
            |name| name.replace('/', "."),
        )
    );
    if let crate::error::MethodCallFailed::ExceptionThrown(exc) =
        crate::runtime::exceptions::throw_runtime_error(
            vm,
            thread,
            crate::error::RuntimeError::ClassCastException { message },
        )
    {
        set_jit_pending_exception(thread, exc);
    }
    i64::MIN
}

/// t1 wave 4 (lane Y2): while an inline-cache publication runs under
/// `JdkOnly`, route the JIT's refusal recorder to THIS VM's
/// `--real-jdk-report` sinks.
///
/// The publication APIs (`JitMICSlot::update`, `JitPICSlot::install`) carry a
/// `jdk_only` bit and no VM, and their refusal
/// (`record_jdk_only_ic_native_refusal` in `jit/src/lib.rs`) writes to the
/// innermost `JdkOnlySinkScope` on the thread, falling back to the process
/// sinks. A mutator-thread IC miss is outside any compile scope, so before
/// this every such refusal fed one process counter that every VM's report
/// read (AGENTS.md: no process globals for compatibility state). Opening the
/// scope at each VM publication site keeps the API unchanged.
///
/// `None` under `Compatible`: the refusal branch is unreachable there (it is
/// gated on the same bit), so the thread-local write is skipped entirely. Bind
/// the result to a named `_scope` so it lives across the publication calls.
#[inline]
pub fn jdk_only_ic_publication_scope(
    vm: &SharedVm,
    jdk_only: bool,
) -> Option<cratonvm_jit::jdk_only_sink::JdkOnlySinkScope<'_>> {
    jdk_only.then(|| {
        cratonvm_jit::jdk_only_sink::JdkOnlySinkScope::enter(vm.jit.jit_cache.jdk_only_sinks())
    })
}

/// Give this SAM call site an inline-cache entry of its own.
///
/// The Rust arm below answers a lambda dispatch in ~190 ns; a named class's
/// call site answers one in ~12, because after its first miss the inline cache
/// holds the callee and the emitted cascade never re-enters Rust
/// (`mic_calls=1` across 2 200 000 dispatches, `CRATONVM_DBG=mic-prof`). The
/// only thing keeping a lambda out of that slot was an argument shuffle — the
/// call site has `(proxy, samArgs…)` and the impl wants `(captures…, samArgs…)`
/// — so `lambda_adapter` emits a thunk that performs the shuffle, reads any
/// captures out of the proxy, and tail-jumps; the slot holds THAT.
///
/// Two questions are asked here, and the rest belong to the emitter:
///
/// * **no `checkcast`** — a generic call site's cast is a class-hierarchy
///   question, not a register move.
/// * **a static impl**, asserted rather than assumed because the thunk drops the
///   receiver outright and nothing downstream would notice if it mattered.
///
/// Whether the captures' types, the proxy's layout and the arity permit an
/// emission is `lambda_adapter_entry`'s to decide — it is what would have to
/// emit them. A `None` from it leaves this site on the Rust arm for good,
/// because the install is claimed before the attempt rather than after it; and
/// every refusal it can make is a property of the SITE rather than of the
/// moment, so nothing is given up by asking once.
///
/// That last sentence was NOT true while a reference capture could also be
/// refused for the collector's read-barrier state, which is a property of the
/// instant: a site that happened to ask during an armed cycle lost its thunk
/// permanently. That refusal is gone — it was both inert by default and
/// unnecessary, see `lambda_adapter_entry` — so the "ask once" shape is now
/// unqualified. Claiming after the attempt instead would trade it for
/// re-asking (a lock and a layout lookup) on every dispatch a refused site ever
/// serves, which is the shape of the 202 000 re-installs
/// `LambdaJitSite::adapter_slots` exists to prevent.
///
/// Both slots are written, because the emitted cascade prefers the PIC when the
/// codegen allocated one and never consults the MIC in that case.
///
/// Everything after installation is the ordinary cached-call path: the
/// cascade's own null and `ObjectKind::Object` guards run before the call, its
/// `i64::MIN` callee-deopt check after it, and an escaping exception is left in
/// `jit_pending_exception` for the caller's post-invoke check. This arm
/// participates in none of it any more, which is the point.

fn mega_cast_miss_info_ptr(info: &JitInvokeInfo) -> usize {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static INTERNED: OnceLock<Mutex<HashMap<(&'static str, &'static str, usize, u8, u8), usize>>> =
        OnceLock::new();
    let map = INTERNED.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (
        info.method_name,
        info.descriptor,
        info.num_jit_args,
        info.return_type,
        info.invoke_kind,
    );
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    *guard.entry(key).or_insert_with(|| {
        let leaked = Box::leak(Box::new(JitInvokeInfo {
            class_name: "",
            method_name: info.method_name,
            descriptor: info.descriptor,
            num_jit_args: info.num_jit_args,
            return_type: info.return_type,
            invoke_kind: info.invoke_kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }));
        leaked as *const JitInvokeInfo as usize
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn publish_mega_cast_thunk(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_class_id: ClassId,
    code: &cratonvm_jit::RetainedCode,
    vm_ptr: i64,
    sam_args: usize,
    guards: &[cratonvm_jit::lambda_adapter::CastGuard],
    epoch_seen: u32,
) {
    let Some(mega_entry) = cratonvm_jit::lambda_adapter::lambda_mega_cast_adapter_entry(
        vm.jit.jit_cache.lambda_adapters(),
        receiver_class_id.as_u32(),
        code.arc(),
        site.capture_descs(),
        sam_args,
        guards,
        cratonvm_jit::lambda_adapter::CastMissSite {
            helper: jit_invoke_virtual_mic as *const () as usize,
            vm_ptr: vm_ptr as usize,
            info_ptr: mega_cast_miss_info_ptr(info),
            mic_ptr: 0,
            pic_ptr: 0,
        },
    ) else {
        return;
    };
    install_mega_dispatch_way(
        vm,
        info,
        Some(pic),
        receiver_class_id.as_u32(),
        mega_entry as u64,
        code.needs_context(),
        epoch_seen,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn install_lambda_inline_cache(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    code: &cratonvm_jit::RetainedCode,
    vm_ptr: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) {
    if !crate::runtime::env_cache::jit_lambda_adapter() {
        return;
    }
    // JIT round 20 wave 12 (lane qLam): a site with a Rust-side adapter (box /
    // unbox, native target) has no machine thunk; the direct arm serves it.
    if site.rust_only() {
        return;
    }
    // Read before the thunk is resolved, for the shared-table publication
    // below (`install_mega_dispatch_way`'s redefinition rule).
    let epoch_seen = vm.jit.jit_cache.redefine_epoch();
    if site.num_captures() != 0 && !crate::runtime::env_cache::jit_lambda_capture_adapter() {
        return;
    }
    // JIT round 20 wave 12 (lane qLam, DISP20-3): an INSTANCE-method impl
    // site carries the receiver guard its thunk bakes (the site exists only
    // under `CRATONVM_JIT_LAMBDA_INSTANCE_THUNK`); every other non-static
    // impl is refused as before.
    let receiver_guard = site.receiver_thunk_guard();
    if !site.is_static_impl() && receiver_guard.is_none() {
        return;
    }
    // nio21 w18 L2: a site that replays bridge `checkcast`s gets a CAST thunk
    // (`cratonvm_jit::lambda_adapter::lambda_cast_adapter_entry`) once every
    // cast has a class this site PROVED passes; until then nothing is claimed,
    // so a later call (whose arguments prove one) installs it. The thunk's
    // miss edge is THIS site's resolving helper call, so it is installed into
    // this site's MIC/PIC only. `CRATONVM_JIT_LAMBDA_CAST_THUNK=0` restores
    // the refusal (read here, once per claim, never per call).
    let cast_guards = if site.has_checkcasts() {
        let Some(guards) = site.cast_thunk_guards() else {
            return;
        };
        // The switch is read AFTER the per-site claim below, never here: a
        // site whose install is refused for good (an over-budget arity) comes
        // back on every call, and w18 read the flag 2.4 M times on `hdr`.
        if mic_ptr == 0 {
            return;
        }
        Some(guards)
    } else {
        None
    };
    // JIT round 20 wave 12 (lane qLam, DISP20-3): a receiver thunk always has
    // a miss edge (its receiver guard), so it needs the MIC even with no cast,
    // and is therefore always built as a guarded thunk (`cast_guards` is
    // `Some`, possibly empty -- which also keeps it out of the shared
    // megamorphic table below). An unbound receiver's own bridge cast is
    // implied by the exact-class guard and dropped; a proved class other than
    // the receiver's could never pass both, so that site gets no thunk.
    let cast_guards = match receiver_guard {
        None => cast_guards,
        Some(receiver) => {
            if mic_ptr == 0 {
                return;
            }
            let mut guards = cast_guards.unwrap_or_default();
            if receiver.source == cratonvm_jit::lambda_adapter::ReceiverSource::SamArg0 {
                if guards
                    .iter()
                    .any(|g| g.sam_index == 0 && g.class_id != receiver.class_id)
                {
                    return;
                }
                guards.retain(|g| g.sam_index != 0);
            }
            Some(guards)
        }
    };
    // A thunk tail-jumps to the impl and returns straight to its compiled
    // caller, so no Rust runs on that path and the constant-return probe
    // CANNOT see those calls. Say so -- in strict mode by refusing the thunk,
    // otherwise by counting what is going unseen, so a clean
    // `site_const_mismatch` is not mistaken for a clean bill of health.
    if site.const_return().is_some() && crate::runtime::env_cache::jit_lambda_const_probe() {
        if crate::runtime::env_cache::jit_lambda_const_probe_strict() {
            return;
        }
        crate::runtime::interpreter::const_probe_note_opaque();
    }
    if !site.claim_adapter_install(mic_ptr, pic_ptr) {
        return;
    }
    // nio21 w19 L2: the cast thunk's switch, once per claim (see above).
    // (A receiver thunk is under its own switch, read when its site was built.)
    if cast_guards.is_some()
        && receiver_guard.is_none()
        && false
    {
        return;
    }
    // `total_args` is captures plus SAM arguments; the emitter wants them apart,
    // because only the SAM arguments arrive in registers.
    let sam_args = site.total_args() - site.num_captures();
    if let Some(receiver) = receiver_guard {
        // JIT round 20 wave 12 (lane qLam): the receiver thunk.
        let guards = cast_guards.as_deref().unwrap_or(&[]);
        let Some(entry) = cratonvm_jit::lambda_adapter::lambda_receiver_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
            receiver,
            guards,
            cratonvm_jit::lambda_adapter::CastMissSite {
                helper: jit_invoke_virtual_mic as *const () as usize,
                vm_ptr: vm_ptr as usize,
                info_ptr: info as *const JitInvokeInfo as usize,
                mic_ptr: mic_ptr as usize,
                pic_ptr: pic_ptr as usize,
            },
        ) else {
            return;
        };
        install_lambda_receiver_thunk(vm, site, receiver_class_id, code, mic_ptr, pic_ptr, entry);
        return;
    }
    let entry = match &cast_guards {
        None => cratonvm_jit::lambda_adapter::lambda_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
        ),
        // Cast: addresses carried as the helpers' `i64` words.
        Some(guards) => cratonvm_jit::lambda_adapter::lambda_cast_adapter_entry(
            vm.jit.jit_cache.lambda_adapters(),
            receiver_class_id.as_u32(),
            code.arc(),
            site.capture_descs(),
            sam_args,
            guards,
            cratonvm_jit::lambda_adapter::CastMissSite {
                helper: jit_invoke_virtual_mic as *const () as usize,
                vm_ptr: vm_ptr as usize,
                info_ptr: info as *const JitInvokeInfo as usize,
                mic_ptr: mic_ptr as usize,
                pic_ptr: pic_ptr as usize,
            },
        ),
    };
    let Some(entry) = entry else {
        return;
    };
    let class_id = receiver_class_id.as_u32();
    // nio21 w18 L2: a cast thunk's site comes back here on every cast MISS
    // (an argument class other than the proved one). When two such sites of
    // one proxy alternate, the per-site latch above re-claims on each miss;
    // a site whose MIC already holds exactly this thunk is left untouched
    // rather than re-stored (the MIC's first line is the one the cascade
    // loads on every call).
    if cast_guards.is_some() && mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if mic
            .cached_class_id
            .load(std::sync::atomic::Ordering::Acquire)
            == class_id
            && mic.cached_entry().0 == entry as u64
        {
            return;
        }
    }
    let class_name = site.impl_class_name();
    let needs_ctx = code.needs_context();
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    // Run the thunk once before anything dispatches through it. See
    // `const_thunk_self_test`: for a constant-returning impl the answer is
    // known from the bytecode, so a wrong thunk is caught deterministically
    // here rather than statistically later.
    if crate::runtime::env_cache::jit_lambda_const_probe()
        && !const_thunk_self_test(site, entry, needs_ctx, vm_ptr, receiver_ref)
    {
        return;
    }
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let mut installed = false;
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        // MEGA5-18-4: a gate-first megamorphic site never reads its MIC.
        if !mic_publication_skipped(pic_ptr) {
            mic.update(class_id, class_name, entry as u64, needs_ctx, jdk_only);
        }
        installed = true;
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        let mic_serves = mic_ptr != 0
            && pic_way_redundant_with_mic(&*(mic_ptr as *const JitMICSlot), pic, class_id);
        if !mic_serves && pic.install(class_id, class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(
                vm,
                crate::threading::thread_state::current_thread_id().map(crate::ThreadId),
            );
        }
        installed = true;
        // Round 12 wave 3 (lane mega2, proposal W2-1 step 3): and the VM's
        // shared megamorphic table, under this SAM site's selector, so every
        // other site of the same SAM method whose own ways overflow reaches
        // the thunk from its hashed stub instead of from this helper. The
        // thunk is a function of (proxy class, SAM name and descriptor) alone
        // (`lambda_jit_site`), both of which the key carries, and it is
        // exactly what this site's PIC just published. Only an impl without
        // an exception table: the table's Rust reader services a sentinel
        // through `handle_rust_armed_callee_sentinel`, which this arm
        // deliberately does not use for a lambda (see its sentinel note).
        // nio21 w18 L2: never a cast thunk -- its miss edge names THIS site.
        if mega_table_publish_lambda_enabled()
            && cast_guards.is_none()
            && site.cached_impl().exception_table.is_empty()
        {
            install_mega_dispatch_way(
                vm,
                info,
                Some(pic),
                class_id,
                entry as u64,
                needs_ctx,
                epoch_seen,
            );
        } else if let Some(guards) = cast_guards.as_deref() {
            if mega_table_publish_lambda_enabled()
                && receiver_guard.is_none()
                && site.cached_impl().exception_table.is_empty()
            {
                publish_mega_cast_thunk(
                    vm,
                    site,
                    info,
                    pic,
                    receiver_class_id,
                    code,
                    vm_ptr,
                    sam_args,
                    guards,
                    epoch_seen,
                );
            }
        }
    }
    if installed {
        crate::runtime::interpreter::lambda_site_bump_adapter(site.num_captures());
        if mic_prof::enabled() {
            // The call counts AT THE MOMENT OF INSTALL. `site_adapters`
            // alone cannot tell "installed early and served" from
            // "installed after the workload was over", which is exactly
            // what an intermittent `site_direct` failure asks.
            let (fast_returns, site_direct, _) =
                crate::runtime::interpreter::lambda_jit_engagement();
            eprintln!(
                "[cratonvm-jitc] lambda-adapter installed class_id={class_id} \
                 captures={} sam_args={sam_args} entry={entry:#x} impl={} \
                 at site_direct={site_direct} fast_returns={fast_returns}",
                site.num_captures(),
                class_name,
            );
        }
    }
}

/// JIT round 20 wave 12, lane qLam (DISP20-3): publish an instance-method
/// impl site's RECEIVER thunk (`lambda_receiver_adapter_entry`) into THIS call
/// site's MIC / PIC under the proxy's class id -- the tail of
/// [`install_lambda_inline_cache`] for a cast thunk, which it is a kind of: its
/// miss edge names this site, so it is never published to the shared
/// megamorphic table, and a slot that already holds exactly this thunk is not
/// re-stored. The constant-return self-test does not apply (an instance site
/// has no constant return; see `build_lambda_instance_jit_site`).
///
/// A site whose receivers alternate between two classes holds one of their
/// thunks at a time; the other class misses into the Rust direct arm, which
/// serves it (its own site has already spent its install claim).
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn install_lambda_receiver_thunk(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    proxy_class_id: ClassId,
    code: &cratonvm_jit::RetainedCode,
    mic_ptr: i64,
    pic_ptr: i64,
    entry: usize,
) {
    let class_id = proxy_class_id.as_u32();
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if mic
            .cached_class_id
            .load(std::sync::atomic::Ordering::Acquire)
            == class_id
            && mic.cached_entry().0 == entry as u64
        {
            return;
        }
    }
    let class_name = site.impl_class_name();
    let needs_ctx = code.needs_context();
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let mut installed = false;
    if mic_ptr != 0 {
        let mic = &*(mic_ptr as *const JitMICSlot);
        if !mic_publication_skipped(pic_ptr) {
            mic.update(class_id, class_name, entry as u64, needs_ctx, jdk_only);
        }
        installed = true;
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        let mic_serves = mic_ptr != 0
            && pic_way_redundant_with_mic(&*(mic_ptr as *const JitMICSlot), pic, class_id);
        if !mic_serves && pic.install(class_id, class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(
                vm,
                crate::threading::thread_state::current_thread_id().map(crate::ThreadId),
            );
        }
        installed = true;
    }
    if installed {
        crate::runtime::interpreter::lambda_site_bump_adapter(site.num_captures());
        // The positive control (`CRATONVM_DBG_MIC_PROF=1`): one line per install.
        if mic_prof::enabled() {
            let (fast_returns, site_direct, _) =
                crate::runtime::interpreter::lambda_jit_engagement();
            eprintln!(
                "[cratonvm-jitc] lambda-receiver-thunk installed class_id={class_id} \
                 receiver={:?} captures={} entry={entry:#x} impl={}.{} \
                 at site_direct={site_direct} fast_returns={fast_returns}",
                site.receiver_thunk_guard(),
                site.num_captures(),
                class_name,
                site.impl_method_name(),
            );
        }
    }
}

/// Withdraw what a dispatch just published into its inline caches if a class
/// redefinition landed while it was resolving the target.
///
/// `jit_invoke_virtual_mic` stamps the MIC with the redefinition epoch BEFORE
/// it resolves and publishes. A redefinition that lands between the stamp and
/// the publication is invisible to that stamp, and the published entry is then
/// served by the inline machine-code cascade, which never re-enters the helper
/// to notice. Re-reading the epoch after publishing closes the window: the
/// entry is withdrawn, and the next miss finds the stamp stale, flushes and
/// re-resolves against the redefined class.
pub fn withdraw_ic_publication_if_redefined(
    vm: &SharedVm,
    epoch_at_resolution: u32,
    mic: &JitMICSlot,
    pic: Option<&JitPICSlot>,
) {
    if vm.jit.jit_cache.redefine_epoch() == epoch_at_resolution {
        return;
    }
    mic.clear_compiled_entry();
    if let Some(pic) = pic {
        pic.clear_entries();
    }
}

/// A compiled caller's SAM call, served straight from the lambda call site's
/// own cached target.
///
/// This is the arm that gives a lambda call site what every named-class call
/// site already had. `jit_invoke_virtual_mic` is entered ONCE per named-class
/// site — after that the inline cascade in `jit/src/x64.rs` calls the callee
/// from machine code and never returns to Rust (`mic_calls=1` over 2 200 000
/// dispatches on `probes/SamHotLoopProbe.java`). A lambda receiver is caught by
/// the arm above this one, which returns before the inline cache is ever
/// reached, so its site can never be cached: `mic_calls=2197000`,
/// `hit_entry=0`, ~819 cycles a call.
///
/// The inline cache cannot hold a lambda — its cascade passes the caller's own
/// argument registers straight through, and a SAM call's registers are not the
/// impl's (the proxy receiver has to go, the captures have to arrive). So the
/// site is cached one level out, in Rust, and this arm spends it: read the
/// captures out of the proxy's fields, put the SAM's own already-decoded raw
/// arguments after them, and call the compiled impl through the same
/// reentrant primitive the monomorphic hit path uses.
///
/// Deliberately identical to the MIC hit path in everything that is not the
/// argument shuffle — same `try_call_compiled_entry_reentrant_owned`, same
/// `i64::MIN` deopt-sentinel handling, same "an escaping exception is left in
/// `jit_pending_exception` for the compiled caller's own post-invoke check".
/// A `None` from it means the callee's arity exceeds the register tables, and
/// the caller falls through to the generic path with nothing done.
///
/// See known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md.
pub unsafe fn try_lambda_site_direct_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    receiver_ref: ObjectRef,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    vm_ptr: i64,
    ic_slots: Option<(i64, i64)>,
) -> Option<i64> {
    const MAX_DIRECT_ARGS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;
    // Interpreter-only mode (`jvmti_requires_interpreter`): this arm enters a
    // compiled impl, which posts no MethodEntry/MethodExit and stops at no
    // breakpoint. Declining leaves the call to the generic lambda route, whose
    // one-shot door stands down the same way. Checked before the inline-cache
    // install below, so nothing is published for the machine-code cascade
    // either. Interpreter round i1 wave 6, L7. The i54-L1 retirement: also
    // while a C JVMTI env lists lambda proxy frames, which only the generic
    // route records (`interpreter::lambda_proxy_frames_armed`).
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        || crate::runtime::interpreter::lambda_proxy_frames_armed(vm)
    {
        return None;
    }
    let site = match crate::runtime::interpreter::lambda_jit_site(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    ) {
        Some(site) => site,
        // JIT round 20 wave 12 (lane qLam, DISP20-3): an INSTANCE-method impl,
        // keyed by the call's own receiver class (`CRATONVM_JIT_LAMBDA_INSTANCE_THUNK`).
        None => crate::runtime::interpreter::lambda_instance_jit_site(
            vm,
            receiver_class_id,
            receiver_ref,
            args_slice,
            info.method_name,
            info.descriptor,
        )?,
    };
    crate::runtime::interpreter::lambda_site_bump_calls();
    // `args_slice[0]` is the proxy receiver; the SAM's own arguments follow it.
    if !site.direct_enabled() {
        // This site's body deoptimized once already; the interpreter's one-shot
        // path owns it from here (see `LambdaJitSite::direct_disabled`).
        crate::runtime::interpreter::lambda_site_bump_deopted();
        return None;
    }
    if args_slice.is_empty() || site.total_args() > MAX_DIRECT_ARGS {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    // JIT round 20 wave 12 (lane qLam; `CRATONVM_JIT_LAMBDA_SITE_BOXING`,
    // `CRATONVM_JIT_LAMBDA_SITE_NATIVE`): a site with a Rust-side adapter.
    // Its unboxed SAM arguments replace the caller's words for everything
    // below (the impl, and the handler / implicit-route rebuilds, which decode
    // by the impl's own descriptor), it never installs a machine thunk, and a
    // native target is called here and now. The caller's own `args_slice` is
    // untouched, so a decline hands the general path the boxed originals.
    let mut adapted_words = [0i64; MAX_DIRECT_ARGS + 1];
    // JIT r20 wave 13 (lane huntGC2): the caller's own words, which the
    // collector keeps current. `adapted_words` is a Rust local no collector
    // sees, so after the impl ran (a GC point) the routes below re-derive
    // their words from these (`lambda_site_args_after_call`).
    let caller_args = args_slice;
    let (args_slice, ic_slots) = match site.adapt() {
        None => (args_slice, ic_slots),
        Some(adapt) => {
            let words = adapted_words.get_mut(..args_slice.len())?;
            words.copy_from_slice(args_slice);
            for &(k, prim, wrapper) in adapt.unbox_list() {
                let raw = *args_slice.get(1 + k)?;
                if raw == 0 {
                    // LambdaMetafactory's class raises the NPE; the general
                    // path does too.
                    return None;
                }
                // SAFETY: a non-zero reference word of the compiled caller is
                // a live object, as everywhere else on this arm.
                let obj = ObjectRef::from_raw(raw as *mut u8);
                if vm.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array
                    || vm.mem.heap.class_id_of(obj) != wrapper
                {
                    // Not exactly the wrapper: the general path's cast decides.
                    crate::runtime::interpreter::lambda_site_bump_refused();
                    return None;
                }
                words[1 + k] = crate::runtime::interpreter::lambda_unbox_bits(vm, prim, obj)?;
            }
            (&adapted_words[..args_slice.len()], None)
        }
    };
    if let Some(native_info) = site.adapt().and_then(|adapt| adapt.native_info()) {
        return lambda_site_native_call(
            vm,
            thread,
            &site,
            receiver_ref,
            info,
            args_slice,
            caller_args,
            native_info,
        );
    }
    let sam_args = &args_slice[1..];
    if site.num_captures() + sam_args.len() != site.total_args() {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    let Some(code) = crate::runtime::interpreter::lambda_jit_site_code(vm, &site) else {
        // Still interpreted. The warmup counter on the generic path is what
        // gets it compiled; until then this arm has nothing to call.
        crate::runtime::interpreter::lambda_site_bump_no_code();
        return None;
    };

    if !crate::runtime::interpreter::lambda_jit_site_checkcasts_pass(vm, &site, sam_args) {
        // A cast that would throw. Decline, and let the generic path raise the
        // `ClassCastException` it has always raised, message and all.
        crate::runtime::interpreter::lambda_site_bump_refused();
        return None;
    }
    if let Some((mic_ptr, pic_ptr)) = ic_slots {
        install_lambda_inline_cache(
            vm,
            &site,
            info,
            receiver_ref,
            receiver_class_id,
            &code,
            vm_ptr,
            mic_ptr,
            pic_ptr,
        );
    }
    let mut jit_args = [0i64; MAX_DIRECT_ARGS];
    crate::runtime::interpreter::lambda_jit_site_capture_args(
        vm,
        &site,
        receiver_ref,
        &mut jit_args,
    );
    jit_args[site.num_captures()..site.total_args()].copy_from_slice(sam_args);
    let entry = code.entry_ptr() as usize;
    let needs_ctx = code.needs_context();
    let Some(rc) = try_call_compiled_entry_reentrant_owned(
        &code,
        entry,
        needs_ctx,
        vm_ptr,
        &jit_args[..site.total_args()],
    ) else {
        // The callee's arity is past what the register tables cover. Nothing
        // ran; the generic path takes it.
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    };
    if rc == i64::MIN {
        // `i64::MIN` is either a deopt sentinel or a `long` that really is
        // `Long.MIN_VALUE`, and only the out-of-band signals can tell them
        // apart. Draining them is safe here BECAUSE this arm called exactly one
        // method: whatever they say happened, happened inside this site's impl.
        //
        // The shared sentinel handler is not used for this, and the reason is
        // no longer that it CANNOT be: `try_resume_trapped_callee` used to
        // identify the trapped callee by the CALL SITE's name alone, which for
        // a SAM call is `apply` and never the `lambda$...` body that actually
        // trapped, so it refused the resume and minted an orphan
        // (`lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908`).
        // It now asks the metafactory's `impl_handle` as a second identity and
        // would answer correctly. This arm still owns the case, because it owns
        // something that helper does not: `site.disable_direct()`, the latch
        // that keeps every LATER call to this site off the direct arm. Draining
        // the signals here is what makes that decision possible.
        let sig = take_all_jit_signals(thread);
        let stashed = cratonvm_jit::deopt::take_last_deopt_with_point();
        // Interpreter round i5 wave 103, lane L2: the impl's body was retired
        // after this site picked it, and its not-entrant stub answered the
        // call through the VM, where the throwable already left the impl
        // (`DrainedJitSignals::escaped_a_redispatch_of`). Its table must not be
        // searched again (no pc: a typed row would match by class alone).
        if stashed.is_none() && !site.cached_impl().exception_table.is_empty() && {
            let imp = site.cached_impl();
            sig.escaped_a_redispatch_of(&imp.method_name, &imp.method_descriptor)
        } {
            restash_jit_signals(thread, sig);
            crate::runtime::interpreter::lambda_site_bump_direct();
            return Some(rc);
        }
        // nio21 wave 21 L1: an EXPLICIT throwable leaving an impl with its own
        // table (admitted only under `CRATONVM_JIT_LAMBDA_HANDLER_IMPL`) is
        // offered to that table first; restashed below, it would leave the
        // impl past its `catch` / `finally`.
        if let (None, Some(exc)) = (stashed.as_ref(), sig.exception) {
            if !site.cached_impl().exception_table.is_empty() {
                // Interpreter round i5 wave 102, lane L2: the impl's own
                // stamp only (`DrainedJitSignals::athrow_bci_owned_by`).
                // (Fields, not the method: `sig.exception` is moved out.)
                let athrow_bci = {
                    let imp = site.cached_impl();
                    throw_stamp_owned_by(
                        sig.athrow_bci,
                        sig.athrow_owner,
                        method_identity_key(
                            &imp.class_name,
                            &imp.method_name,
                            &imp.method_descriptor,
                        ),
                    )
                };
                // Interpreter round i5 wave 105, lane L2: NO stamp at all
                // (not a refused foreign one) means the VM re-posted the
                // throwable after the impl's bytecode ran
                // (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`).
                let throw_pc = if athrow_bci >= 0 {
                    athrow_bci as usize
                } else if sig.athrow_bci == -1 {
                    crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
                } else {
                    usize::MAX
                };
                // JIT r20 wave 13 (lane huntGC2): the words as they are NOW.
                let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
                let routed = match lambda_site_args_after_call(vm, &site, caller_args, &mut fresh) {
                    Some(now) => lambda_route_thrown_through_impl(
                        vm, thread, &site, info, now, exc, throw_pc,
                    ),
                    None => {
                        clear_lambda_impl_exceptional_frame(vm, thread, site.cached_impl());
                        Err(exc)
                    }
                };
                let bits = match routed {
                    Ok(bits) => bits,
                    Err(exc) => {
                        set_jit_pending_exception(thread, exc);
                        i64::MIN
                    }
                };
                crate::runtime::interpreter::lambda_site_bump_direct();
                // qLam: an adapted site boxes the handler's value.
                return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
            }
        }
        if lambda_direct_sentinel_is_not_a_deopt(
            &sig,
            stashed.is_some(),
            !site.cached_impl().exception_table.is_empty(),
        ) {
            // Not a deopt: an escaping exception (which propagates through
            // `jit_pending_exception`, exactly as it does out of the MIC hit
            // path) or a genuine `Long.MIN_VALUE`. Put every signal back the
            // way it was found and hand the value on.
            restash_jit_signals(thread, sig);
            crate::runtime::interpreter::lambda_site_bump_direct();
            screen_const_return(&site, rc, "jit-direct-minsentinel");
            return Some(rc);
        }
        // An implicit exception in an impl WITH its own exception table
        // (interpreter round i1 wave 4, L7). Routed through that table at the
        // throw site, exactly as `route_implicit_exc_through_callee` does for a
        // named callee, instead of being read as a deopt: the decline below
        // hands the call to the generic path, which re-runs the impl FROM
        // ENTRY and commits everything the compiled body did before the trap a
        // second time. Neither outcome is a deopt, so the site keeps its
        // direct arm. Only a routing this function cannot decide falls through.
        //
        // Round 13 wave 8 (lane replay4): such a fall-through is not a bare
        // trap but an implicit exception whose handler could not be resumed;
        // its re-run is judged at the throw bci (`-1`: unknown) below.
        let implicit_throw_bci =
            (sig.npe || sig.aioobe.is_some() || sig.arithmetic).then_some(sig.athrow_bci);
        // JIT r20 wave 13 (lane huntGC2): an adapted site's words re-derived
        // from the caller's CURRENT words (the impl ran: a GC point); `None`
        // (cannot happen for words that adapted before the call) keeps the
        // signals for the deopt path below, as an undecided route does.
        let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
        let route_args = lambda_site_args_after_call(vm, &site, caller_args, &mut fresh);
        if let (None, Some(now)) = (stashed.as_ref(), route_args) {
            match lambda_direct_route_implicit(vm, thread, &site, info, now, sig) {
                // No constant-return screen: a body with a handler is never a
                // bare constant (`LambdaJitSite::const_return`).
                LambdaImplicitRoute::Completed(bits) => {
                    crate::runtime::interpreter::lambda_site_bump_direct();
                    // qLam: an adapted site boxes the handler's value.
                    return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
                }
                LambdaImplicitRoute::Declined => {}
            }
        }
        // A deopt. Its signals describe an attempt that is being abandoned and
        // are dropped with it, and this site leaves the direct arm for good so
        // every later call goes through the interpreter's one-shot.
        //
        // But "the body did not complete" is NOT "the body did nothing". A
        // deopt sentinel means the compiled body ran up to `rframe.bci` and
        // stopped there; returning `None` here hands the call to the generic
        // path below, which re-enters the impl FROM ENTRY and therefore
        // re-executes every side effect already committed before the trap. That
        // is the hibernate-reactive `reactiveRemove`-fires-twice defect: one
        // `ArrayLoop.next()` dispatch, two deletes, `--nojit` clean,
        // `CRATONVM_JIT_LAMBDA_SITE=0` clean. See
        // `docs/feature-designs/concurrent-gc-maturation.md`
        // section 8.
        //
        // The reconstructed frame is what makes resuming from the trap point
        // possible, so spend it instead of dropping it — through the same
        // resume sink the interpreter's one-shot door uses, keyed on this
        // site's OWN impl identity (`site.cached_impl()`), which is exactly the
        // identity `try_resume_trapped_callee` could not supply. The stashing
        // point's address goes with it (round 11 wave 10), so a foreign frame
        // from a superseded body is not charged to the fresh one.
        //
        // Interpreter round i1 wave 15, lane L3: an impl that left at a
        // back-edge poll because the interpreter-only mode is on keeps the
        // direct arm. The arm already declines while the mode holds (the
        // `jvmti_requires_interpreter` test above), and the latch is for
        // good: set here, it would outlive the agent's request.
        let left_for_the_interpreter = stashed.as_ref().is_some_and(|(rframe, cause, point)| {
            crate::runtime::interpreter::stashed_exit_left_for_the_interpreter(
                vm, rframe, *cause, *point,
            )
        });
        if !left_for_the_interpreter {
            site.disable_direct();
        }
        crate::runtime::interpreter::lambda_site_bump_deopted();
        if let Some((rframe, cause, point_addr)) = stashed {
            let frames_depth_on_entry = thread.frames.len();
            match crate::runtime::interpreter::resume_deopted_body_at_point(
                vm,
                thread,
                &code,
                site.cached_impl(),
                &rframe,
                cause,
                point_addr,
                frames_depth_on_entry,
                "lambda-site-direct",
            ) {
                Ok(Some(value)) => {
                    crate::runtime::interpreter::lambda_site_bump_resumed();
                    let bits = jit_abi_bits_of(value);
                    screen_const_return(&site, bits, "jit-direct-resumed");
                    // qLam: an adapted site boxes the resumed frame's value.
                    return Some(lambda_site_adapt_return(vm, thread, &site, info, bits));
                }
                Ok(None) => {
                    // No resume was possible; the owning method has been
                    // de-speculated instead. Falling through re-runs the impl
                    // FROM ENTRY, which is only legitimate when that replay is
                    // observably equivalent to the attempt that trapped.
                    //
                    // This arm used to fall through unconditionally, with a
                    // counter and a comment ("side effects and all — counted so
                    // the residual is visible rather than silent") standing in
                    // for the check. A counter makes a double-execution
                    // countable, not correct: the compiled body ran up to
                    // `rframe.bci` and committed everything before it, so for
                    // an impl that stores, does I/O, takes a monitor or calls
                    // back into Java, the re-run commits all of it a second
                    // time. That is the hibernate-reactive
                    // `reactiveRemove`-fires-twice defect named above, reached
                    // by the one path the fix for it did not cover.
                    //
                    // The interpreter's own deopt sink has asked this question
                    // for exactly this reason since the spliced-bci fix, and
                    // the two ends are supposed to "ask ONE predicate ... so
                    // they cannot drift apart again". They had drifted: this
                    // end never asked.
                    //
                    // Both halves are answerable here: the impl's bytecode is
                    // `site.cached_impl().code`, and the spliced half is the
                    // artifact's own `spliced_bodies_side_effect_free` — `code`
                    // is a `RetainedCode` keep-alive that derefs to the
                    // `CompiledMethod` this call entered.
                    crate::runtime::interpreter::lambda_site_bump_unresumable();
                    // Round 12 wave 3 (lane replay, sinks patch 2): the stash
                    // form reads the point's first-arrival bit, and answers a
                    // point the impl's artifact does not own (a nested
                    // callee's trap) with the whole-body rule instead of a bci
                    // of another method.
                    //
                    // Round 13 wave 1 (lane replay,
                    // `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`
                    // items 2 and 3): counted in the door census and refused
                    // exactly where the interpreter's one-shot door into the
                    // same impl refuses (`jit_bridge::door_rerun_or_refuse`):
                    // an own stash whose replay is unsound, of a class never
                    // redefined. A foreign stash (a nested callee's trap no
                    // call-site service claimed) and a hot-swapped impl re-run,
                    // as at that door; before, this arm alone raised
                    // `InternalError` for both.
                    let refuse = {
                        let impl_method = site.cached_impl();
                        let (cause, exact) = crate::runtime::interpreter::door_rerun_verdict(
                            &impl_method.code,
                            &code,
                            &impl_method.class_name,
                            &impl_method.method_name,
                            &impl_method.method_descriptor,
                            Some((&rframe, point_addr)),
                        );
                        crate::runtime::interpreter::note_door_rerun(cause, exact);
                        crate::runtime::interpreter::retire_lambda_impl_after_rerun(
                            vm,
                            impl_method,
                            exact,
                        );
                        // Round 13 wave 3 (lane replay2; R13-2): the doors'
                        // one predicate, which also honours their switch
                        // `CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES`. Round 13
                        // wave 8 (lane replay4): every cause, as at the doors
                        // (a foreign stash too), and a refusal is counted as
                        // one instead of as the re-run it replaced.
                        let refused = crate::runtime::interpreter::unsound_rerun_refused(
                            cause,
                            exact,
                            || class_was_redefined(vm, impl_method.declaring_class_id),
                        );
                        if refused {
                            crate::runtime::interpreter::note_door_rerun_refused(cause);
                        }
                        refused
                    };
                    if refuse {
                        // Refuse rather than duplicate. `InternalError` is the
                        // same diagnosis the interpreter sink raises through
                        // `VmError::Internal`, delivered as a Java throwable
                        // because this path returns into compiled code and has
                        // no error channel of its own.
                        //
                        // `i64::MIN` is the return value for the same reason the
                        // `ExceptionThrown` arm below uses it:
                        // `emit_post_invoke_exception_check` compares RAX
                        // against `i64::MIN` and consults `dispatch_threw` only
                        // on that comparison, so any other word — `0`
                        // especially — leaves the exception parked and unclaimed.
                        let msg = format!(
                            "precise deoptimization unavailable for the lambda impl \
                             {}.{} at bci {}; refusing side-effecting replay",
                            site.impl_class_name(),
                            site.impl_method_name(),
                            rframe.bci,
                        );
                        // Round 12 wave 5 (lane replay3): through
                        // `throw_runtime_error`, whose heap-exhaustion rule
                        // answers with the preallocated `OutOfMemoryError`, and
                        // the singleton again for any other build failure. The
                        // old fallback returned the sentinel with NOTHING
                        // pending, which the compiled caller reads as a genuine
                        // return value (`i64::MIN`, or a bogus reference).
                        let exc = match crate::runtime::exceptions::throw_runtime_error(
                            vm,
                            thread,
                            crate::error::RuntimeError::InternalError { message: msg },
                        ) {
                            crate::error::MethodCallFailed::ExceptionThrown(exc) => Some(exc),
                            crate::error::MethodCallFailed::InternalError(_) => {
                                *vm.mem.singleton_oom.read()
                            }
                        };
                        if let Some(exc) = exc {
                            set_jit_pending_exception(thread, exc);
                        }
                        // Falling through would re-run a side-effecting body,
                        // so the sentinel is returned even in the one case left
                        // with nothing to park (no singleton yet: VM start-up).
                        return Some(i64::MIN);
                    }
                }
                Err(crate::error::MethodCallFailed::ExceptionThrown(exc)) => {
                    // Park it for the compiled caller's own post-invoke check —
                    // and return the sentinel that check actually tests for.
                    //
                    // This used to return `0`, described as "the null/zero
                    // sentinel". **There is no such sentinel.**
                    // `emit_post_invoke_exception_check` compares `RAX` against
                    // `i64::MIN` and consults `dispatch_threw` only on that
                    // comparison; `0` is a perfectly ordinary null reference
                    // return and the check keeps it. The parked exception then
                    // sat unclaimed while compiled code carried on with a null.
                    //
                    // It looked correct for a year because of what usually
                    // FOLLOWS a SAM call: unboxing the result
                    // (`Integer.intValue`) was a CALL, and that call crossed
                    // into Rust and delivered the pending exception a moment
                    // later at a site that could route it. Making the box/unbox
                    // intrinsic default-ON removed the call — the unbox became
                    // an inline load with a null-receiver guard — and the
                    // exception escaped its own `catch`. See
                    // `internal/fixed-bugs/jit-superseded-implicit-npe-leak-FIXED-20260903.md`.
                    //
                    // `i64::MIN` is right for every return type: for a
                    // reference it cannot be a valid heap address, and for the
                    // `J`/`D`/`F` shapes where it IS a representable value the
                    // check disambiguates through `dispatch_threw`, which finds
                    // exactly the signal parked on the line above.
                    set_jit_pending_exception(thread, exc);
                    crate::runtime::interpreter::lambda_site_bump_resumed();
                    return Some(i64::MIN);
                }
                Err(failed) => {
                    crate::runtime::interpreter::lambda_site_bump_unresumable();
                    // Round 12 wave 5 (lane replay3): an internal failure out
                    // of the resume (the resumed frame's own `VmError`, or a
                    // heap too full to build even the re-materialisation
                    // `OutOfMemoryError`) happened AFTER the attempt ran, so
                    // the `return None` below re-ran the impl from entry and
                    // committed its prefix again. Raised at this call instead,
                    // through the helpers' one conversion.
                    if lambda_resume_failure_raises() {
                        stash_helper_failure(
                            vm,
                            thread,
                            failed,
                            &"lambda impl resume failed",
                            true,
                            true,
                        );
                        return Some(i64::MIN);
                    }
                }
            }
        } else if true {
            // The bare sentinel: the generic path re-runs the impl from entry.
            // Counted as the one-shot door counts its own (`Frameless`); round
            // 13 wave 1, lane replay. This was the one uncounted re-run left.
            let impl_method = site.cached_impl();
            let (cause, exact) = match implicit_throw_bci {
                // Round 13 wave 8 (lane replay4): an implicit exception whose
                // handler could not be resumed. The throwing instruction (an
                // array access, a division, a field access) commits nothing
                // and re-executes, so the prefix rule at its bci answers;
                // `-1` (not stamped) is the whole-body rule. Round 13 wave 12
                // (lane replay6): with the edges the entered body's local
                // handlers can take on the way to the throw.
                Some(bci) => (
                    crate::runtime::interpreter::DoorRerunCause::CalleeException,
                    crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_after(
                        &impl_method.code,
                        Some(&*code),
                        u32::try_from(bci).unwrap_or(u32::MAX),
                    ),
                ),
                None => crate::runtime::interpreter::door_rerun_verdict(
                    &impl_method.code,
                    &code,
                    &impl_method.class_name,
                    &impl_method.method_name,
                    &impl_method.method_descriptor,
                    None,
                ),
            };
            crate::runtime::interpreter::note_door_rerun(cause, exact);
            crate::runtime::interpreter::retire_lambda_impl_after_rerun(vm, impl_method, exact);
            // Round 13 wave 8 (lane replay4): refused when unsound, as the
            // one-shot door's frameless arm refuses it
            // (`CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES`; an implicit
            // exception's, `CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES`).
            if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
                class_was_redefined(vm, impl_method.declaring_class_id)
            }) {
                crate::runtime::interpreter::note_door_rerun_refused(cause);
                let msg = format!(
                    "precise deoptimization unavailable for the lambda impl {}.{} after a \
                     trap with no frame; refusing side-effecting replay",
                    site.impl_class_name(),
                    site.impl_method_name(),
                );
                let exc = match crate::runtime::exceptions::throw_runtime_error(
                    vm,
                    thread,
                    crate::error::RuntimeError::InternalError { message: msg },
                ) {
                    crate::error::MethodCallFailed::ExceptionThrown(exc) => Some(exc),
                    crate::error::MethodCallFailed::InternalError(_) => {
                        *vm.mem.singleton_oom.read()
                    }
                };
                if let Some(exc) = exc {
                    set_jit_pending_exception(thread, exc);
                }
                return Some(i64::MIN);
            }
        }
        return None;
    }
    crate::runtime::interpreter::lambda_site_bump_direct();
    screen_const_return(&site, rc, "jit-direct");
    // qLam: an adapted site boxes the impl's 32-bit return.
    Some(lambda_site_adapt_return(vm, thread, &site, info, rc))
}

/// JIT r20 wave 13 (lane huntGC2): the words a lambda site's post-call routes
/// (`lambda_route_thrown_through_impl`, `lambda_direct_route_implicit`) decode
/// the impl's incoming values from, AFTER the impl ran. A site without an
/// adapter: the caller's own words, which the collector keeps current. An
/// adapted site (`CRATONVM_JIT_LAMBDA_SITE_BOXING`): the unboxed copy rebuilt
/// from those current words into `buf` -- the copy taken before the call is a
/// Rust local no collector updates, so its proxy and reference SAM words are
/// stale once a collection in the impl moved them. A wrapper is immutable,
/// so the unboxed values are the ones the impl was entered with. `None` when
/// a word no longer adapts (unreachable for words that adapted before).
///
/// SAFETY: as [`try_lambda_site_direct_call`]; `caller_args` is the compiled
/// caller's argument words for this SAM call, proxy receiver first.
pub(crate) unsafe fn lambda_site_args_after_call<'a>(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    caller_args: &'a [i64],
    buf: &'a mut [i64; crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS + 1],
) -> Option<&'a [i64]> {
    let Some(adapt) = site.adapt() else {
        return Some(caller_args);
    };
    let words = buf.get_mut(..caller_args.len())?;
    words.copy_from_slice(caller_args);
    for &(k, prim, wrapper) in adapt.unbox_list() {
        let raw = *caller_args.get(1 + k)?;
        if raw == 0 {
            return None;
        }
        // SAFETY: a non-zero reference word of the compiled caller is a live
        // object, as in `try_lambda_site_direct_call`.
        let obj = ObjectRef::from_raw(raw as *mut u8);
        if vm.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array
            || vm.mem.heap.class_id_of(obj) != wrapper
        {
            return None;
        }
        *words.get_mut(1 + k)? = crate::runtime::interpreter::lambda_unbox_bits(vm, prim, obj)?;
    }
    buf.get(..caller_args.len())
}

/// JIT round 20 wave 12, lane qLam (`CRATONVM_JIT_LAMBDA_SITE_BOXING`): the
/// value an adapted lambda site hands its compiled caller -- the impl's raw
/// 32-bit return boxed through the wrapper's `valueOf`, as LambdaMetafactory's
/// class boxes it. `i64::MIN` (an escaping exception is parked; a 32-bit
/// return is never that word) and every non-boxing site's bits pass through. A
/// throw out of `valueOf` itself is delivered as any helper's is.
pub(crate) unsafe fn lambda_site_adapt_return(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    bits: i64,
) -> i64 {
    let Some(prim) = site.adapt().and_then(|adapt| adapt.box_return()) else {
        return bits;
    };
    if bits == i64::MIN {
        return bits;
    }
    // JIT round 23 wave 3b, lane L2 (`CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX`, default
    // ON, `=0` restores the native funnel): an `Integer` box is the compiled
    // `Integer.valueOf` thin helper's own body on the thread this arm holds --
    // the box cache's objects for `-128..=IntegerCache.high`, a TLAB bump past
    // it, the canonical native only for a cold slot -- instead of a
    // `safe_native_call` of the Intrinsic per call (the caller-loader guard,
    // native state, alloc-wrapper; page
    // `r23-w2-boxing-method-ref-call-costs-microseconds-FIXED-20261021.md`). Same
    // identity rules, so the same objects -- but only where `box_primitive`
    // itself would call that Intrinsic (r23 w3 review): `lambda_box_intrinsic`
    // on, the native admitted for the compiled `valueOf` binds, and `Integer`
    // initialized. Otherwise the by-name `valueOf` (the JDK body, which also
    // initializes the class) stays authoritative, as before.
    if prim == 'I'
        && lambda_arm_tlab_box_enabled(vm)
        && vm.config.lambda_box_intrinsic
        && crate::runtime::interpreter::invoke::box_value_of_intrinsic(vm, 'I').is_some()
    {
        // Cast: JIT ABI -- the int category travels sign-extended.
        let value = i64::from(bits as i32);
        // A fresh object is parked in `native_pending_return` by the body, as
        // `lambda_box_return_bits` parks it; the helper wrapper this arm runs
        // under drops the handoff root once compiled code has the value.
        return jit_integer_value_of_direct_body_on(
            vm as *const SharedVm as i64,
            value,
            Some(&mut *thread),
        );
    }
    match crate::runtime::interpreter::lambda_box_return_bits(vm, thread, prim, bits) {
        Ok(boxed) => boxed,
        Err(error) => handle_jit_dispatch_error(vm, thread, error, info),
    }
}

/// `CRATONVM_JIT_LAMBDA_ARM_TLAB_BOX` (JIT round 23 wave 3b, lane L2; perf,
/// default ON, `=0` the kill switch): read once per VM
/// (`VmConfig::lambda_arm_tlab_box`). See [`lambda_site_adapt_return`].
#[inline]
pub fn lambda_arm_tlab_box_enabled(vm: &SharedVm) -> bool {
    vm.config.lambda_arm_tlab_box
}

/// JIT round 20 wave 12, lane qLam (`CRATONVM_JIT_LAMBDA_SITE_NATIVE`): an
/// instance site whose target is the receiver class's reviewed `Intrinsic`
/// (`lambda_native_instance_target`): the call is a compiled `invokevirtual`
/// of that method on the receiver, served by the JIT's native site cache under
/// the site's own call-site record -- the door that re-checks the registry
/// generation, redefinition, the capability gate and `--real-jdk` admission
/// on its own and declines (`None`: the general path runs the call, nothing
/// having run here) whenever it would not serve. `args_slice` is the
/// (adapted) caller words, proxy first; the impl's words are the receiver
/// (capture 0, or SAM argument 0) and the rest, as for a compiled impl.
///
/// JIT round 20 (lane fixLN): those words are a Rust local no collector
/// updates, and the door's site-cache fill can run Java (a GC point). So the
/// door is asked `WarmOnly` (it declines rather than resolve); a cold site is
/// filled with nothing but the receiver word held (`FillOnly`), and the words
/// are re-materialised from `caller_args` -- the compiled caller's own words,
/// which the collector keeps current -- before the warm call.
pub(crate) unsafe fn lambda_site_native_call(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    proxy: ObjectRef,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    caller_args: &[i64],
    native_info: &'static JitInvokeInfo,
) -> Option<i64> {
    const MAX_DIRECT_ARGS: usize = crate::runtime::interpreter::jit_bridge::JIT_ABI_MAX_JAVA_ARGS;
    let sam_args = args_slice.get(1..)?;
    let total = site.total_args();
    if total > MAX_DIRECT_ARGS || site.num_captures() + sam_args.len() != total {
        crate::runtime::interpreter::lambda_site_bump_arity();
        return None;
    }
    if !crate::runtime::interpreter::lambda_jit_site_checkcasts_pass(vm, site, sam_args) {
        // A cast that would throw: the general path raises it.
        crate::runtime::interpreter::lambda_site_bump_refused();
        return None;
    }
    let mut words = [0i64; MAX_DIRECT_ARGS];
    crate::runtime::interpreter::lambda_jit_site_capture_args(vm, site, proxy, &mut words);
    words[site.num_captures()..total].copy_from_slice(sam_args);
    if words[0] == 0 {
        // The receiver is null (the site lookup screens it; kept local).
        return None;
    }
    let key = jit_site_key(vm.vm_identity, native_info as *const JitInvokeInfo as usize);
    let mut cold = false;
    let served = try_jit_site_cached_native_dispatch_mode(
        vm,
        Some(&mut *thread),
        native_info,
        key,
        &words[..total],
        0,
        NativeSiteDoorMode::WarmOnly(&mut cold),
    );
    let bits = match served {
        Some(bits) => bits,
        // Declined for a reason other than a cold site: the general path.
        None if !cold => return None,
        None => {
            // Fill the site holding only the receiver word, which the fill
            // reads (for its class) before any resolution and never after.
            let receiver_word = [words[0]];
            let _ = try_jit_site_cached_native_dispatch_mode(
                vm,
                None,
                native_info,
                key,
                &receiver_word,
                0,
                NativeSiteDoorMode::FillOnly,
            );
            // The words as they are NOW: the fill was a GC point.
            let mut fresh = [0i64; MAX_DIRECT_ARGS + 1];
            let now = lambda_site_args_after_call(vm, site, caller_args, &mut fresh)?;
            let sam_args = now.get(1..)?;
            let proxy_raw = *now.first()?;
            if proxy_raw == 0 || site.num_captures() + sam_args.len() != total {
                return None;
            }
            words = [0i64; MAX_DIRECT_ARGS];
            crate::runtime::interpreter::lambda_jit_site_capture_args(
                vm,
                site,
                ObjectRef::from_raw(proxy_raw as *mut u8),
                &mut words,
            );
            words[site.num_captures()..total].copy_from_slice(sam_args);
            if words[0] == 0 {
                return None;
            }
            // Still cold (the registry generation moved under the fill):
            // decline, nothing having run here.
            let mut cold = false;
            try_jit_site_cached_native_dispatch_mode(
                vm,
                Some(&mut *thread),
                native_info,
                key,
                &words[..total],
                0,
                NativeSiteDoorMode::WarmOnly(&mut cold),
            )?
        }
    };
    crate::runtime::interpreter::lambda_site_bump_direct();
    Some(lambda_site_adapt_return(vm, thread, site, info, bits))
}

/// `CRATONVM_JIT_LAMBDA_RESUME_FAILURE_RAISES`, **default ON** (`=0` restores
/// the pre-round-12-wave-5 fall-through, which re-ran the impl from entry):
/// whether `try_lambda_site_direct_call` raises an internal failure out of a
/// trapped impl's resume at the call instead of re-running the impl. Read
/// per call (j27 wave 7, lane T1IC: was a process `OnceLock`); the path is
/// cold.
pub fn lambda_resume_failure_raises() -> bool {
    true
}



/// Check a SAM call's result against the constant its impl body must return.
///
/// Off unless `CRATONVM_JIT_LAMBDA_CONST_PROBE` is set, and a no-op for the
/// overwhelming majority of sites, whose bodies are not constants. See
/// `const_int_return_of`.
pub(crate) fn screen_const_return(
    site: &crate::runtime::interpreter::LambdaJitSite,
    raw: i64,
    arm: &str,
) {
    if !crate::runtime::env_cache::jit_lambda_const_probe() {
        return;
    }
    let Some(expected) = site.const_return() else {
        return;
    };
    crate::runtime::interpreter::const_probe_screen(
        expected,
        raw,
        site.impl_class_name(),
        site.impl_method_name(),
        arm,
    );
}

/// Encode a resumed frame's return `Value` into the raw JIT-ABI register bit
/// pattern a compiled caller expects.
///
/// The inverse of `execute_jit_call_oneshot`'s argument decode, and the same
/// table: `void` and `null` are both the zero word, a float/double travels as
/// its bit pattern, and a reference as its raw address.
pub fn jit_abi_bits_of(value: Option<Value>) -> i64 {
    match value {
        Some(Value::Int(x)) => x as i64, // Cast: JIT ABI -- i64 register convention
        Some(Value::Long(x)) => x,
        Some(Value::Float(x)) => x.to_bits() as i64, // Cast: JIT ABI -- float bits to i64
        Some(Value::Double(x)) => x.to_bits() as i64, // Cast: JIT ABI -- double bits to i64
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64, // Cast: JIT ABI -- pointer to i64
        Some(Value::Object(None)) | None => 0,
        Some(_) => 0,
    }
}

/// The direct lambda arm's classification of an `i64::MIN` return: `true` when
/// it is NOT a deopt — an escaping throwable, a genuine `Long.MIN_VALUE`, or an
/// implicit exception the impl cannot catch — so the drained signals are put
/// back and the value handed on; `false` for a deopt, which latches the site
/// off the direct arm and resumes (or declines to re-run) the body.
///
/// The implicit clause is interpreter round i1 wave 3 (L7). A trapping stub
/// raises the deopt flag BESIDE its implicit one (`jit_throw_arithmetic`,
/// `jit_throw_aioobe`, `jit_npe_with_action`), and with no stash that pair
/// means "the body threw NPE / AIOOBE / `/ by zero`" and nothing else. Read as
/// a deopt, the signal was dropped and the generic path re-ran the impl FROM
/// ENTRY, committing every side effect before the throw a second time
/// (`L7JitDoorOutcomes.java`'s lambda, one extra
/// `sideEffects++` per site). Propagated, the compiled caller's own post-invoke
/// check builds and routes it, as for any other callee. An impl WITH an
/// exception table may catch it itself, so it answers `false` here and the
/// caller routes it through that table ([`lambda_direct_route_implicit`], wave
/// 4) before it treats anything as a deopt.
pub fn lambda_direct_sentinel_is_not_a_deopt(
    sig: &DrainedJitSignals,
    stashed: bool,
    impl_has_handlers: bool,
) -> bool {
    lambda_direct_sentinel_is_not_a_deopt_as(
        sig,
        stashed,
        impl_has_handlers,
        true,
    )
}

/// [`lambda_direct_sentinel_is_not_a_deopt`] with
/// `CRATONVM_JIT_LAMBDA_IMPLICIT_WITHOUT_DEOPT_ROUTES` given. Pure.
///
/// JIT round 23 wave 2, lane L3
/// (`docs/feature-designs/real-jdk-mode.md`):
/// not every implicit signal comes with the deopt flag. `jit_getfield` /
/// `jit_putfield` on a null receiver (the slow path an optimizing body's
/// implicit null check recovers into) raise the NPE flag ALONE. The old
/// expression `(implicit && !impl_has_handlers) || !sig.deopt` then read an
/// NPE out of a handler-bearing impl as "not a deopt" and restashed it for
/// the compiled caller, so it left the impl past its own `catch`
/// (`W1L3Probe.loopNpe`: `try { sink += NULL_HOLDER.x; } catch (NPE e)`,
/// called through a SAM once its C2 body superseded the C1 one). With
/// `routes` on, an implicit signal decides by the table alone: an impl with
/// handlers answers `false`, and the caller routes it through that table
/// ([`lambda_direct_route_implicit`]); the deopt flag only separates a bare
/// deopt from a genuine `Long.MIN_VALUE` when no signal is set.
pub fn lambda_direct_sentinel_is_not_a_deopt_as(
    sig: &DrainedJitSignals,
    stashed: bool,
    impl_has_handlers: bool,
    routes: bool,
) -> bool {
    if sig.exception.is_some() {
        return true;
    }
    if stashed {
        return false;
    }
    let implicit = sig.npe || sig.aioobe.is_some() || sig.arithmetic;
    if routes && implicit {
        return !impl_has_handlers;
    }
    (implicit && !impl_has_handlers) || !sig.deopt
}



/// What [`lambda_direct_route_implicit`] did with an implicit exception.
pub enum LambdaImplicitRoute {
    /// The call is over: the impl's handler returned these result bits, or the
    /// throwable escaped the impl and is parked for the compiled caller with
    /// the `i64::MIN` sentinel its post-invoke check tests for.
    Completed(i64),
    /// Nothing was decided (no implicit signal, no table, the throwable could
    /// not be built, or the table search cannot be trusted): the caller takes
    /// its pre-existing path.
    Declined,
}

/// Route an implicit exception raised by a directly called lambda impl that
/// has its OWN exception table through that table (interpreter round i1 wave
/// 4, L7) — the direct arm's counterpart of
/// [`route_implicit_exc_through_callee`]'s serviced arm, and the same machinery:
/// [`materialize_implicit_signal`] builds the throwable, and
/// `run_jit_callee_handler` resumes the impl at the covering handler (from the
/// reason-9 frame its body published, when there is one).
///
/// `sig` is the drained signal record; it is consumed. The impl's incoming
/// arguments are re-derived from `args_slice` and the proxy's fields AFTER the
/// throwable is allocated, so a collection there cannot leave them stale — the
/// same source the generic fallback decodes from.
///
/// SAFETY: as [`try_lambda_site_direct_call`]; `args_slice` is the compiled
/// caller's argument slots for this SAM call, proxy receiver first.
pub(crate) unsafe fn lambda_direct_route_implicit(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    sig: DrainedJitSignals,
) -> LambdaImplicitRoute {
    let implicit = implicit_signal_of(sig.aioobe, sig.npe, sig.arithmetic);
    let cached = site.cached_impl();
    if implicit == ImplicitSignal::None
        || sig.exception.is_some()
        || cached.exception_table.is_empty()
    {
        return LambdaImplicitRoute::Declined;
    }
    // `jit_throw_aioobe` stamps its own bci; the other two do not, and the
    // search then relies on the reason-9 frame, or its pc-unknown rule.
    // Interpreter round i5 wave 102, lane L2: the impl's own stamp only.
    let athrow_bci = sig.athrow_bci_owned_by(
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
    );
    let throw_pc = if athrow_bci >= 0 {
        athrow_bci as usize
    } else {
        usize::MAX
    };
    let Some(exc) =
        materialize_implicit_signal(vm, thread, implicit, sig.trap_frames, sig.npe_action)
    else {
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return LambdaImplicitRoute::Declined;
    };
    // Pinned across the argument decode and the handler search, both of which
    // can reach a safepoint (see `try_run_callee_handler`).
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let Some(args) = lambda_impl_incoming_values(vm, site, args_slice) else {
        thread.native_pin_roots.truncate(pin_base);
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return LambdaImplicitRoute::Declined;
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, cached, throw_pc, exc, &args,
    );
    thread.native_pin_roots.truncate(pin_base);
    match res {
        Ok(Ok(value)) => LambdaImplicitRoute::Completed(jit_abi_bits_of(value)),
        Ok(Err(e)) => {
            LambdaImplicitRoute::Completed(handle_jit_dispatch_error(vm, thread, e, info))
        }
        // The impl's table does not cover the throw: it escapes the impl, and
        // the compiled caller's own check routes it -- an escaping exception,
        // not a deopt.
        Err((crate::runtime::interpreter::CalleeHandlerMiss::NotCaught, exc)) => {
            set_jit_pending_exception(thread, exc);
            // The impl is gone; its frame can never be claimed.
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            LambdaImplicitRoute::Completed(i64::MIN)
        }
        Err(_) => {
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            LambdaImplicitRoute::Declined
        }
    }
}

/// nio21 wave 21 L1
/// (`r21-lambda-handler-impl-sam-calls-round-trip-through-rust-FIXED-20261010.md`):
/// offer a throwable that left a handler-bearing lambda impl's compiled body
/// to THAT impl's exception table, at `throw_pc` (or the reason-9 frame its
/// body published), with the impl's incoming values (`lambda_impl_incoming_values`:
/// the proxy's captures, then the SAM arguments). The explicit-throwable twin
/// of [`lambda_direct_route_implicit`], and the same machinery
/// (`run_jit_callee_handler`) a named callee's handler runs through
/// ([`try_run_callee_handler`]).
///
/// `Ok(bits)`: the impl's handler ran and the call is over (its result, or
/// `i64::MIN` with whatever the handler's own code left pending). `Err(exc)`:
/// the table does not take it (or cannot be searched); `exc` is the throwable
/// to propagate, re-read from its pin, and the impl's stashed frame is
/// dropped. A miss is what a named callee's miss is: the throwable leaves.
///
/// SAFETY: as [`lambda_direct_route_implicit`]; `args_slice` holds the
/// compiled caller's CURRENT argument words for this SAM call, proxy first.
pub(crate) unsafe fn lambda_route_thrown_through_impl(
    vm: &SharedVm,
    thread: &mut JvmThread,
    site: &crate::runtime::interpreter::LambdaJitSite,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    exc: ObjectRef,
    throw_pc: usize,
) -> Result<i64, ObjectRef> {
    let cached = site.cached_impl();
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let Some(args) = lambda_impl_incoming_values(vm, site, args_slice) else {
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        clear_lambda_impl_exceptional_frame(vm, thread, cached);
        return Err(exc);
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, cached, throw_pc, exc, &args,
    );
    thread.native_pin_roots.truncate(pin_base);
    match res {
        Ok(Ok(value)) => Ok(jit_abi_bits_of(value)),
        Ok(Err(e)) => Ok(handle_jit_dispatch_error(vm, thread, e, info)),
        Err((_, exc)) => {
            clear_lambda_impl_exceptional_frame(vm, thread, cached);
            Err(exc)
        }
    }
}

/// nio21 wave 21 L1: the lambda site of a SAM call whose receiver is a lambda
/// proxy with a HANDLER-BEARING impl, for the callee-deopt service; `None`
/// for every other receiver (and always while
/// `CRATONVM_JIT_LAMBDA_HANDLER_IMPL` is off, which is the only way such a
/// site exists). A receiver-dispatched site only (`invoke_kind` 0 / 2).
pub(crate) fn lambda_handler_site_for_service(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
) -> Option<std::rc::Rc<crate::runtime::interpreter::LambdaJitSite>> {
    if !crate::runtime::exceptions::lambda_handler_impl_enabled()
        || !matches!(info.invoke_kind, 0 | 2)
        || receiver_class_id.as_u32() == 0
    {
        return None;
    }
    let site = match crate::runtime::interpreter::lambda_jit_site(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
    ) {
        Some(site) => site,
        // W4A item 1: an INSTANCE impl's site is keyed by the receiver's class
        // too, read from the caller's current words (proxy first).
        None => {
            let proxy = vm
                .mem
                .heap
                .is_object_address(*args_slice.first()? as usize)?;
            crate::runtime::interpreter::lambda_instance_jit_site(
                vm,
                receiver_class_id,
                proxy,
                args_slice,
                info.method_name,
                info.descriptor,
            )?
        }
    };
    (!site.cached_impl().exception_table.is_empty()).then_some(site)}

/// Interpreter round i5 wave 104, lane L2 (page
/// `i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range`):
/// has the EXPLICIT throwable the MIC door drained already been offered to
/// the lambda impl's table, so that [`lambda_route_thrown_through_impl`] must
/// not offer it again?
///
/// * The not-entrant record names the IMPL (`jit_not_entrant_entry_body`
///   records the body it re-dispatched, while this door's `info` names the
///   SAM method, so `take_redispatch_escape_for(info)` never matched it): the
///   VM's invoke path ran the impl and searched its table at the real pc. The
///   throwable is unstamped, so the route would search with the pc unknown
///   and a typed row would match on class alone, re-running the impl's
///   handler (the `pc-unknown typed match` census lines of
///   `L2W102ForeignThrowStamp`: its shapes are method references).
///   The same rule as [`DrainedJitSignals::escaped_a_redispatch_of`].
/// * The stamp belongs to ANOTHER known method: the last compiled exit was
///   not the impl's (the proxy's own compiled body, say, whose call to the
///   impl was serviced -- and the impl's table searched -- at that inner
///   site). Its pc is no pc of the impl; offering it would range-check the
///   impl's table against a foreign bci.
///
/// An implicit signal is left to the route as before.
pub(crate) fn explicit_throwable_already_left_impl(
    signals: &DrainedJitSignals,
    escape_record: u64,
    site: &crate::runtime::interpreter::LambdaJitSite,
) -> bool {
    if signals.exception.is_none() {
        return false;
    }
    let imp = site.cached_impl();
    if signals.athrow_bci < 0 {
        // A record is only ever written with the switch on; asked again so
        // `=0` restores the old behaviour exactly.
        return not_entrant_escape_enabled()
            && redispatch_escape_record_names(
                escape_record,
                &imp.method_name,
                &imp.method_descriptor,
            );
    }
    // `athrow_owner` is `0` (unknown) under `CRATONVM_JIT_THROW_STAMP_OWNER=0`.
    signals.athrow_owner != 0
        && signals.athrow_owner
            != method_identity_key(&imp.class_name, &imp.method_name, &imp.method_descriptor)
}

/// The impl's incoming arguments for a direct SAM call: the proxy's captures,
/// then the SAM arguments decoded by the impl's own parameter types. `None` for
/// a shape this cannot spell (an unbound method reference, whose receiver is a
/// SAM argument rather than a capture, or an arity mismatch).
///
/// SAFETY: as [`lambda_direct_route_implicit`].
pub(crate) unsafe fn lambda_impl_incoming_values(
    vm: &SharedVm,
    site: &crate::runtime::interpreter::LambdaJitSite,
    args_slice: &[i64],
) -> Option<JitDecodedArgs> {
    let cached = site.cached_impl();
    let proxy = vm
        .mem
        .heap
        .is_object_address(*args_slice.first()? as usize)?;
    let mut values = JitDecodedArgs::new();
    for i in 0..site.num_captures() {
        values.push(vm.mem.heap.get_field(proxy, i));
    }
    // An instance impl's receiver is its first capture and is not a parameter.
    // An UNBOUND reference (`Type::m`, no captures) has it as SAM argument 0.
    let unbound = site.num_captures() == 0 && !cached.is_static;
    let captured_params = if unbound {
        0
    } else {
        site.num_captures()
            .checked_sub(usize::from(!cached.is_static))?
    };
    let mut tags = DescriptorParamIter::new(&cached.method_descriptor).skip(captured_params);
    let mut sam_words = args_slice[1..].iter();
    if unbound {
        // A null or foreign receiver word cannot be spelled: decline.
        values.push(Value::Object(Some(
            vm.mem.heap.is_object_address(*sam_words.next()? as usize)?,
        )));
    }
    for &raw in sam_words {
        let value = match tags.next()? {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(raw as i32),
            b'J' => Value::Long(raw),
            b'F' => Value::Float(f32::from_bits(raw as u32)),
            b'D' => Value::Double(f64::from_bits(raw as u64)),
            _ if raw == 0 => Value::Object(None),
            _ => Value::Object(vm.mem.heap.is_object_address(raw as usize)),
        };
        values.push(value);
    }
    if tags.next().is_some() {
        return None;
    }
    Some(values)
}

/// Drop the reason-9 frame a lambda impl's compiled body published, if the one
/// stashed names that impl: its attempt is being abandoned, and a stale frame
/// is a later drain's mis-claim. Same rule as [`clear_own_exceptional_frame`],
/// keyed on the impl rather than on a call site's SAM name.
///
/// JIT round 17 wave 3, lane syncdoor3: the locks the dropped frame names
/// (a `synchronized` impl's handed method monitor, a block lock) are released
/// under the exceptional hand-over's switches ([`drop_exceptional_frame_of`]),
/// as [`clear_own_exceptional_frame`] releases them.
pub fn clear_lambda_impl_exceptional_frame(
    vm: &SharedVm,
    thread: &mut JvmThread,
    cached: &crate::classloading::resolution::CachedBytecodeMethod,
) {
    if let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() {
        if exceptional_frame_key_names(&key, &cached.method_name, &cached.method_descriptor) {
            let _ = drop_exceptional_frame_of(vm, thread, &key, bci, "abandoned lambda impl");
        }
    }
}

/// Put a drained [`DrainedJitSignals`] back exactly as it was found.
///
/// The direct lambda arm has to DRAIN the signals to tell a deopt sentinel from
/// a `long` equal to `Long.MIN_VALUE`, and when the answer is "not a deopt" the
/// signals still belong to the compiled caller's own post-invoke checks.
pub fn restash_jit_signals(thread: &mut JvmThread, mut sig: DrainedJitSignals) {
    if let Some(exc) = sig.exception {
        set_jit_pending_exception(thread, exc);
    }
    // Same rule as `handle_compiled_callee_deopt_sentinel`'s restore: the
    // frames belong to the trap, not to this drain, so put back the ones that
    // were taken instead of sampling a fresh (shallower) stack.
    // ONE `trap_frames` slot, so exactly ONE of these three may restore it
    // and the rest come back flag-only. Writing it more than once is not a
    // tidiness point: every one of these setters ASSIGNS the slot, so a
    // second call with `None` would wipe the frames the first put back.
    // JVMS does not let one frame have two of these pending at once, so
    // the priority never arbitrates anything real -- it is written out so
    // the next reader does not have to derive that from three `take()`s.
    let frames = sig.trap_frames.take();
    if sig.npe {
        restash_jit_pending_npe(sig.npe_action, frames);
        if sig.arithmetic {
            stash_jit_pending_arithmetic();
        }
        if let Some((index, length)) = sig.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if sig.arithmetic {
        restash_jit_pending_arithmetic(frames);
        if let Some((index, length)) = sig.aioobe {
            stash_jit_pending_aioobe(index, length);
        }
    } else if let Some((index, length)) = sig.aioobe {
        restash_jit_pending_aioobe(index, length, frames);
    }
    if sig.deopt {
        set_jit_deopt_pending();
    }
}

/// The static call site [`try_fast_lambda_int_to_double_apply`] services a
/// trapped compiled `Double.valueOf(D)` box under (round 12 wave 2,
/// `r12w2-osrfix-fast-lambda-apply-hands-up-callee-stash-patch`, item c).
pub static DOUBLE_VALUE_OF_BOX_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "java/lang/Double",
    method_name: "valueOf",
    descriptor: "(D)Ljava/lang/Double;",
    num_jit_args: 1,
    return_type: b'L',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

// SAFETY: the JIT invoke metadata and the validated proxy receiver originate
// from the active interpreted frame; callers fall back to interpretation on a miss.
pub unsafe fn try_fast_lambda_int_to_double_apply(
    vm: &SharedVm,
    thread: &mut JvmThread,
    proxy: ObjectRef,
    proxy_class_id: ClassId,
    info: &JitInvokeInfo,
    args: &[i64],
) -> Result<Option<i64>, crate::error::MethodCallFailed> {
    if info.method_name != "apply"
        || info.descriptor != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || args.len() != 2
    {
        return Ok(None);
    }
    // Enters a compiled `get(I)D` (and `Double.valueOf`): stand down in
    // interpreter-only mode, and while a C JVMTI env lists lambda proxy
    // frames, as `try_lambda_site_direct_call` does.
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm)
        || crate::runtime::interpreter::lambda_proxy_frames_armed(vm)
    {
        return Ok(None);
    }
    let call_site = match vm.classes.lambda_call_site_for(proxy_class_id) {
        Some(call_site) => call_site,
        None => return Ok(None),
    };
    if call_site.sam_method_name.as_ref() != "apply"
        || call_site.sam_descriptor.as_ref() != "(Ljava/lang/Object;)Ljava/lang/Object;"
        || call_site.capture_types.len() != 1
        || call_site.impl_handle.member_name.as_ref() != "get"
        || call_site.impl_handle.descriptor.as_ref() != "(I)D"
        || !matches!(
            call_site.impl_handle.kind,
            crate::classloading::resolution::MethodHandleKind::InvokeVirtual
                | crate::classloading::resolution::MethodHandleKind::InvokeInterface
        )
    {
        return Ok(None);
    }
    let index = match vm.mem.heap.is_object_address(args[1] as usize) {
        Some(index_box) => match vm.mem.heap.get_field(index_box, 0) {
            Value::Int(index) => index,
            _ => return Ok(None),        },
        None => return Ok(None),
    };
    let receiver = match vm.mem.heap.get_field(proxy, 0) {
        Value::Object(Some(receiver)) => receiver,
        _ => return Ok(None),
    };
    // Class 0 for an array (`jit_dispatch_class`), which names no class, so
    // the compiled-`get` lookup below declines rather than finding the
    // COMPONENT's `get(I)D`.
    let receiver_class_id = jit_dispatch_class(vm, receiver)
        .class_id()
        .unwrap_or_else(|| ClassId::new(0));
    let class_name = match vm.classes.class_manager.read().get_class(receiver_class_id) {
        Some(class) => class.name.clone(),
        None => return Ok(None),
    };
    // JVMS §5.4.6, as the peephole's `resolve_lambda_get_route` asks it: where
    // selection against the method handle's owner picks another `get(I)D` than
    // the receiver's lenient walk, the generic dispatch runs instead (round 12
    // wave 2, `r12w2-osrfix-fast-lambda-apply-hands-up-callee-stash-patch`;
    // the peephole's generic route lands here, so without this its own
    // selection test was undone).
    let selection_moves = {
        let cm = vm.classes.class_manager.read();
        cm.find_class_by_name_for_class(&call_site.impl_handle.class_name, receiver_class_id)
            .and_then(|owner| {
                crate::runtime::resolve::selection::receiver_dispatch_moves_to(
                    cm.class_store(),
                    receiver_class_id,
                    owner,
                    "get",
                    "(I)D",
                )
            })
            .is_some()
    };
    if selection_moves {
        return Ok(None);
    }
    let compiled = {
        let cache = vm.jit.jit_cache.read();
        cache
            .get(&class_name, "get", "(I)D", receiver_class_id)
            // The `Vector.get` case again — see the sibling route above.
            .filter(|compiled| !compiled.requires_wrapped_entry)
    };
    let Some(compiled) = compiled else {
        return Ok(None);
    };
    // A compiled leaf may itself contain a safe dispatch helper. That is not a
    // reason to discard a receiver-guarded entry: JitEntryGuard covers its
    // roots and the nested helper preserves normal Java dispatch semantics.
    let jit_args = [receiver.as_ptr() as i64, index as i64];
    let vm_ptr = vm as *const _ as i64;
    // Round 12 wave 2 (lane osrfix's patch, applied by lane calls): root the
    // receiver across the call and its service (the service can re-run `get`
    // with it), and retire a stale deopt flag first, as
    // `call_compiled_entry_under_owner` does, so a flag after the call is this
    // callee's own.
    let pins = CompileArgPins::pin_raw(vm, thread, &LAMBDA_GET_DIRECT_INFO, &jit_args);
    let _ = take_jit_deopt_pending();
    let _guard = crate::jit::conservative_roots::JitEntryGuard::enter_with_compiled_at(
        &*compiled,
        Some(thread.frames.len()),
    );
    let bits = if compiled.needs_context() {
        compiled.try_call_with_context(vm_ptr, &jit_args)
    } else {
        compiled.try_call(&jit_args)
    };
    // Service the callee's sentinel HERE, as the peephole's direct route does
    // (`lambda_get_call_then_route`): a pure deopt is resumed at its trap, then
    // the MIC door's service routes a throw through `get`'s own handler, or
    // re-runs a frameless trap or a declined stash. What it cannot service
    // keeps the sentinel, which the first arm below hands up with its signals
    // in place. `CRATONVM_JIT_LAMBDA_GET_SERVICE=0` hands it up raw, as before.
    let bits = bits.map(|bits| {
        if bits == i64::MIN && jit_dispatch_threw() != 0 && lambda_get_service_enabled() {
            resume_trapped_lambda_get(vm, thread, receiver_class_id)
                .or_else(|| {
                    handle_rust_armed_callee_sentinel(
                        vm,
                        thread,
                        &LAMBDA_GET_DIRECT_INFO,
                        receiver_class_id,
                        &jit_args,
                        Some(&pins),
                    )
                })
                .unwrap_or(i64::MIN)        } else {
            bits        }    });
    pins.release(thread);
    let value = match bits {
        // r9-vmside: the compiled `get(I)D` threw or deopted. It parked the
        // signal (`jit_pending_exception`, an implicit-trap flag, the deopt
        // flag or a stashed frame) and answered with the sentinel — whose bits
        // are the double `-0.0`. This arm used to box that as
        // `Double.valueOf(-0.0)` and hand the compiled `apply` caller a real
        // object, so its post-invoke check never fired: the exception an
        // out-of-range `list.get(i)` must raise came back as `-0.0`, and the
        // parked signal was left for an unrelated later drain. Hand the
        // sentinel up instead — `apply` returns a reference, for which
        // `i64::MIN` is unambiguous, and the peek leaves every signal in place
        // for the caller's own drain. A legitimate `-0.0` sets no signal and is
        // still boxed below.
        Ok(bits) if bits == i64::MIN && jit_dispatch_threw() != 0 => return Ok(Some(i64::MIN)),
        Ok(bits) => f64::from_bits(bits as u64),
        Err(_) => return Ok(None),
    };
    let box_args = [value.to_bits() as i64];
    if let Some((_callee_pin, entry, needs_context)) =
        crate::runtime::interpreter::try_jit_compile_callee(
            vm,
            "java/lang/Double",
            "valueOf",
            "(D)Ljava/lang/Double;",
            true,
        )
    {
        if let Some(boxed) =
            try_call_compiled_entry_reentrant(entry, needs_context, vm_ptr, &box_args)
        {
            if boxed != i64::MIN || !lambda_get_service_enabled() {
                return Ok(Some(boxed));
            }
            // A trap in the compiled `valueOf` stashed its frame: service it
            // here rather than hand the stash up (osrfix's patch, item c).
            // Bootstrap class, so `try_resume_trapped_callee`'s by-name lookup
            // (declaring_class_id 0) finds it; a parked throwable (an OOM)
            // keeps the sentinel, which is unambiguous for a reference return.
            return Ok(Some(
                handle_rust_armed_callee_sentinel(
                    vm,
                    thread,
                    &DOUBLE_VALUE_OF_BOX_INFO,
                    ClassId::new(0),
                    &box_args,
                    None,
                )
                .unwrap_or(i64::MIN),
            ));
        }
    }
    let boxed = crate::vm::invoke_or_native(
        vm,
        thread,
        "java/lang/Double",
        "valueOf",
        "(D)Ljava/lang/Double;",
        &[Value::Double(value)],
    )?;
    Ok(Some(match boxed {
        Some(Value::Object(Some(obj))) => obj.as_ptr() as i64,
        Some(Value::Object(None)) | None => 0,
        _ => return Ok(None),
    }))
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// info_ptr must point to a live JitInvokeInfo. args_ptr/num_args form a valid i64 slice.
// mic_ptr must point to a live JitMICSlot used for monomorphic inline cache dispatch.
// pic_ptr, when non-zero, must point to a live JitPICSlot co-allocated with the MIC at
// the same call site; the helper populates its 4-way entries via `install` so the next
// invocation hits the inline cascade emitted in `jit/src/x64.rs`.
// Transmutes within this function convert cached JIT entry pointers to function pointers
// matching the compiled method's extern "C" calling convention.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn jit_invoke_virtual_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // t1 wave 2, lane P: a site the compile door could not judge is judged at
    // its first compiled execution (JVMS §5.4.4; `jit_site_access_guard`).
    if !jit_site_access_repeat(vm_ptr, info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
            jit_site_access_guard(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // nio21 wave 15, lane L1: a statically bound site the optimizing tier gave
    // an inline cache (`CRATONVM_JIT_IR_BOUND_SITE_IC`). Never virtual
    // selection: see `jit_invoke_bound_site_mic`. Unconditional (not behind the
    // planner's switch): a kind-1 info can only reach this helper through that
    // plan, and the virtual arms below would select an override for it.
    if info_ptr != 0 && (*(info_ptr as *const JitInvokeInfo)).invoke_kind == 1 {
        return jit_invoke_bound_site_mic(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr);
    }
    // nio21 wave 16, lane L4 (`CRATONVM_JIT_IR_IC_PROFILE_SEED`): a starved
    // site the optimizing planner armed records the receiver this miss serves
    // ([`note_ic_profile_seed`]). One null test for every other slot. Record
    // only: a panic here must not throw into the call.
    if vm_ptr != 0 && mic_ptr != 0 && args_ptr != 0 && num_args > 0 {
        if let Some(seed) = (*(mic_ptr as *const JitMICSlot)).open_profile_seed() {
            let note = || {
                note_ic_profile_seed(vm_ptr, seed, args_ptr);
                0i64
            };
            let _ = contain("ic_profile_seed", OnPanic::RecordFor { vm_ptr }, 0, note);
        }
    }
    // Interpreter round i3 wave 81, lane L3 (`CRATONVM_REFLECT_DOOR_PHASE_STATS`,
    // a measurement instrument, default off): the reflective doors' census
    // splits its `gap` at this helper's entry and at its return (the route a
    // compiled `Method.invoke` / `Constructor.newInstance` takes). Off: one
    // registry field load per call.
    let door_census = vm_ptr != 0
        && (*(vm_ptr as *const SharedVm))
            .natives
            .native_methods
            .reflect_door_phase_stats();
    if door_census {
        cratonvm_native_builtins::reflect_door_gate_stamp(
            cratonvm_native_builtins::ReflectDoorGatePoint::HelperEntry,
        );
    }
    // `r16-ffm-plain-downcall-costs-microseconds` (FFM217-2): a bound direct
    // downcall site.
    // JIT round 20 (lane stash): both doors under the helper's panic scope.
    // JIT round 22 (lane memory, MEM20-6): asked first. Its miss is one
    // thread-local slot compare keyed by the site; the field lane below parses
    // the site descriptor and probes its own table before it can decline, and
    // that was ~3 % of every bound downcall (`perf`, `FfmHot seg`).
    // JIT round 21, lane native (FFL16-1 / FUNNEL2-19-4,
    // `CRATONVM_JIT_FFM_DIRECT_DOOR_EXACT_ONLY`): the door's table is filled
    // only for `invokeExact` sites (`try_jit_ffm_downcall_bound`'s gate), so
    // every other call skips its panic scope and table probe.
    if vm_ptr != 0 && info_ptr != 0 && args_ptr != 0 && num_args >= 1 {
        let info = &*(info_ptr as *const JitInvokeInfo);
        if info.invoke_kind == 0 && info.declaring_class_id != 0 {
            let vm = &*(vm_ptr as *const SharedVm);
            let mut ctor_cid = vm.jit.reflect_link_ids.ctor_class
                .load(std::sync::atomic::Ordering::Relaxed);
            if ctor_cid == 0
                && info.class_name == "java/lang/reflect/Constructor"
                && info.method_name == "newInstance"
            {
                ctor_cid = info.declaring_class_id;
                vm.jit.reflect_link_ids.ctor_class
                    .store(ctor_cid, std::sync::atomic::Ordering::Relaxed);
            }
            if ctor_cid != 0
                && info.declaring_class_id == ctor_cid
                && info.method_name == "newInstance"
                && info.descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
                && num_args == 2
                && cratonvm_native_builtins::lang_invoke::jit_reflect_ctor_link_enabled()
            {
                if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
                    let args = std::slice::from_raw_parts(args_ptr as *const i64, 2);
                    crate::jit::helpers::dispatch::ctor_new_instance_redirect(vm_ptr, info, args)
                }) {
                    native_return_handed_to_compiled_code();
                    return bits;
                }
            }
        }
        if info.invoke_kind == 0
            && info.method_name == "invokeExact"
            && info.declaring_class_id != 0
        {
            let vm = &*(vm_ptr as *const SharedVm);
            let a0 = vm.jit.reflect_link_ids.accessors[0]
                .load(std::sync::atomic::Ordering::Relaxed);
            let a1 = vm.jit.reflect_link_ids.accessors[1]
                .load(std::sync::atomic::Ordering::Relaxed);
            if a0 != 0 && info.declaring_class_id == a0
                && cratonvm_native_builtins::lang_invoke::jit_reflect_accessor_link_enabled()
            {
                if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
                    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
                    crate::jit::helpers::dispatch::reflect_accessor_link_redirect(vm_ptr, info, args)
                }) {
                    native_return_handed_to_compiled_code();
                    return bits;
                }
            } else if a1 != 0 && info.declaring_class_id == a1
                && cratonvm_native_builtins::lang_invoke::jit_reflect_ctor_link_enabled()
            {
                if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
                    let args = std::slice::from_raw_parts(args_ptr as *const i64, num_args as usize);
                    crate::jit::helpers::dispatch::reflect_ctor_link_redirect(vm_ptr, info, args)
                }) {
                    native_return_handed_to_compiled_code();
                    return bits;
                }
            }
        }
    }
    if ffm_direct_door_may_serve(info_ptr) {
        if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
            try_ffm_direct_door(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // 2026-10-18 (`CRATONVM_JIT_MH_REAL_FIELD_LANE`, default ON): a warm
    // real field accessor (or proven `bindTo` of one) the interpreter's
    // before-linker lanes serve, served without the native funnel. The two
    // doors are disjoint (the FFM door takes a bound downcall carrier only,
    // filed by its own bind; neither the field lane nor the lean door below
    // serves one), so their order changes no answer.
    if info_ptr != 0
        && (*(info_ptr as *const JitInvokeInfo)).class_name == "java/lang/invoke/MethodHandle"
    {
        if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
            try_jit_mh_real_field_lane(vm_ptr, info_ptr, args_ptr, num_args)
        }) {
            return bits;
        }
    }
    // SBR5 lane ncall: a site already known to be a plain callback native.
    if let Some(bits) = contain_door("jit_invoke_virtual_mic", vm_ptr, || {
        try_lean_native_door(vm_ptr, info_ptr, args_ptr, num_args, true)
    }) {
        // Wave 83, lane L3: a reflective door filed in the lean table
        // (`CRATONVM_JIT_REFLECT_DOOR_LEAN_SITE`) returns here; the census's
        // `gap_out` needs this return's stamp too.
        if door_census {
            cratonvm_native_builtins::reflect_door_gate_stamp(
                cratonvm_native_builtins::ReflectDoorGatePoint::HelperExit,
            );
        }
        return bits;
    }
    let suppress_mic = vm_ptr != 0
        && info_ptr != 0
        && cratonvm_native_builtins::lang_invoke::jit_reflect_ctor_link_enabled()
        && {
            let info = &*(info_ptr as *const JitInvokeInfo);
            let ctor_cid = (&*(vm_ptr as *const SharedVm))
                .jit
                .reflect_link_ids
                .ctor_class
                .load(std::sync::atomic::Ordering::Relaxed);
            info.invoke_kind == 0
                && ctor_cid != 0
                && info.declaring_class_id == ctor_cid
                && info.method_name == "newInstance"
                && info.descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
        };
    if suppress_mic {
        let f = || jit_invoke_dispatch_body(vm_ptr, info_ptr, args_ptr, num_args);
        let bits = contain(
            "jit_invoke_virtual_mic_suppressed",
            OnPanic::Throw { vm_ptr },
            i64::MIN,
            f,
        );
        native_return_handed_to_compiled_code();
        if door_census {
            cratonvm_native_builtins::reflect_door_gate_stamp(
                cratonvm_native_builtins::ReflectDoorGatePoint::HelperExit,
            );
        }
        return bits;
    }
    let f = || jit_invoke_virtual_mic_body(vm_ptr, info_ptr, args_ptr, num_args, mic_ptr, pic_ptr);
    let bits = contain(
        "jit_invoke_virtual_mic",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    );
    // handoff-w36b: see `drop_native_return_handed_to_compiled_code`.
    native_return_handed_to_compiled_code();
    if door_census {
        cratonvm_native_builtins::reflect_door_gate_stamp(
            cratonvm_native_builtins::ReflectDoorGatePoint::HelperExit,
        );
    }
    bits
}

/// nio21 wave 15, lane L1: the inline-cache miss of a STATICALLY BOUND
/// instance site (`invoke_kind == 1`), which the optimizing tier plans with a
/// MIC/PIC pair when its direct bind found no compiled callee at compile time
/// (`cratonvm_jit` `ir_bound_site_ic_planned`, `CRATONVM_JIT_IR_BOUND_SITE_IC`;
/// `r21-unbound-static-site-is-blind-dispatch-for-the-body-life-FIXED-20261009.md`).
///
/// 1. When this thread's `DISPATCH_CACHE` already names a current compiled
///    body for the site, publish that body into the site's MIC (and PIC) under
///    the receiver's class ([`publish_bound_site_entry`]). The inline cascade
///    then CALLs it raw from the next call on, exactly as it CALLs a virtual
///    callee: no Rust frame, no quiescence pair, no frame-chain push, no
///    argument pinning, no `TLS` map probes.
/// 2. Run the call through [`jit_invoke_dispatch`], unchanged: this is the call
///    the site made before it had a cache (the blind dispatch), so every
///    semantic of the site (the NPE of a null receiver, the substituted owner,
///    natives, synchronized callees, the tier-up compile that fills
///    `DISPATCH_CACHE`, exception routing) is the old one.
///
/// Why the cache is sound for a statically bound site: its target does not
/// depend on the receiver's class at all, so keying the entry by that class
/// only narrows when it is used. Every class that reaches the site gets the
/// same body. Never published to the shared megamorphic table, whose keys are
/// VIRTUAL selection, and never through the virtual arms of
/// [`jit_invoke_virtual_mic_body`], which would select an override.
///
/// SAFETY: as [`jit_invoke_virtual_mic`]: pointers supplied by the compiled
/// caller's inline-cache slow path for the current live VM.
// JIT round 23 wave 2f (lane L1): out of line, so `jit_invoke_virtual_mic`'s frame
// (live under every virtual compiled call it serves) does not reserve this arm's
// locals.
#[inline(never)]
pub unsafe fn jit_invoke_bound_site_mic(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) -> i64 {
    // M8-1: an inline-cache quiescent point before any probe of ours (the
    // dispatch below asks it again; it is idempotent).
    cratonvm_jit::jit_ic_quiescent_point();
    if vm_ptr != 0 && mic_ptr != 0 && num_args > 0 && args_ptr != 0 {
        // Publication only: a panic here must not throw into the call.
        let publish = || {
            publish_bound_site_entry(vm_ptr, info_ptr, args_ptr, mic_ptr, pic_ptr);
            0i64
        };
        let _ = contain("jit_invoke_bound_site_mic", OnPanic::RecordFor { vm_ptr }, 0, publish);
    }
    jit_invoke_dispatch(vm_ptr, info_ptr, args_ptr, num_args)
}

/// [`jit_invoke_bound_site_mic`]'s publication half. Publishes nothing unless
/// every one of these holds, so a site that cannot be published stays the
/// dispatch call it was (correct, only slower):
///
/// * the compiled-entry switches are on and nothing stands the compiled arms
///   down (a redefinition quiescence, interpreter-only mode), with the same
///   memo flushes the dispatch body runs before its `DISPATCH_CACHE` consult;
/// * the MIC's redefinition stamp is current (a moved epoch flushes the slot,
///   restamps it and publishes nothing this call, as the virtual arm does);
/// * this thread's `DISPATCH_CACHE` names a current (not superseded, not
///   retired), owned body for the site, without an `invokedynamic` trap and
///   not requiring a wrapped entry. Such a row exists only after the dispatch
///   body's loader-faithful fill entered that body for this very site;
/// * a callee with an exception table only while those are publishable at
///   all (`mic_publish_exception_table_callees`), with the site's rerun flag
///   set first, as `mark_site_rerun_pins_before_publish` does;
/// * the receiver is a plain object (an array's header id is its component's).
///
/// The receiver word is read before any safepoint of this helper, so it is the
/// address the compiled caller holds, which its own cascade just read the
/// header of.
pub unsafe fn publish_bound_site_entry(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    mic_ptr: i64,
    pic_ptr: i64,
) {
    if !direct_static_compiled_callee_entry_enabled()
        || !direct_virtual_compiled_callee_entry_enabled()
    {
        return;
    }
    let vm = &*(vm_ptr as *const SharedVm);
    let info = &*(info_ptr as *const JitInvokeInfo);
    let mic = &*(mic_ptr as *const JitMICSlot);
    let epoch_now = vm.jit.jit_cache.redefine_epoch();
    if mic
        .redefine_epoch
        .load(std::sync::atomic::Ordering::Acquire)
        != epoch_now
    {
        if mic
            .redefine_epoch
            .swap(epoch_now, std::sync::atomic::Ordering::AcqRel)
            != epoch_now
        {
            mic.clear_compiled_entry();
            if pic_ptr != 0 {
                (*(pic_ptr as *const JitPICSlot)).clear_entries();
            }
        }
        return;
    }
    if crate::runtime::interpreter::jvmti_requires_interpreter(vm) {
        return;
    }
    flush_class_identity_dispatch_memos(vm);
    let info_key = jit_site_key(vm.vm_identity, info_ptr as usize);
    if site_quiesced_for_redefinition(vm, info, info_key) {
        return;
    }
    flush_raw_entry_dispatch_caches();
    let found = DISPATCH_CACHE.with(|dc| {
        let dc = dc.try_borrow().ok()?;
        let c = dc.get(&info_key).filter(|c| !c.names_a_stale_body())?;
        let body = std::sync::Arc::clone(c.owner.as_ref()?.arc());
        (c.entry != 0 && !body.has_indy_trap && !body.requires_wrapped_entry).then_some((
            c.entry,
            c.needs_context,
            c.rerun_pins,
            body,
        ))
    });
    // `_callee_pin` holds the body across the publications: `update` /
    // `install` take their own keep-alive by resolving the entry, which can
    // only succeed while the artifact is alive.
    let Some((entry, needs_ctx, rerun_pins, _callee_pin)) = found else {
        return;
    };
    if rerun_pins && !mic_publish_exception_table_callees() {
        return;
    }
    let receiver_bits = *(args_ptr as *const i64) as u64;
    if receiver_bits == 0 || (receiver_bits & 0x7) != 0 || receiver_bits >= (1u64 << 48) {
        return;
    }
    let receiver_ref = ObjectRef::from_raw(receiver_bits as usize as *mut u8);
    let Some(receiver_class) = jit_dispatch_class(vm, receiver_ref).class_id() else {
        return;
    };
    // Interpreter round i7 wave 137, lane L6 (review of wave 136 L2,
    // `CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`, default OFF): this
    // publication runs BEFORE the dispatch call that makes the interface
    // sender's receiver check, from a `DISPATCH_CACHE` row an earlier PASSING
    // receiver filled. A receiver class the check refuses is never published:
    // its next call would hit the way and run the body with no
    // `IllegalAccessError`. One per-VM bool read with the switch off.
    if info.invoke_kind == 1
        && iface_special_receiver_refusal_message(
            vm,
            info,
            std::slice::from_raw_parts(args_ptr as *const i64, 1),
        )
        .is_some()
    {
        return;
    }
    let receiver_cid = receiver_class.as_u32();
    let class_name: String = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(receiver_class) {
            Some(class) => class.name.to_string(),
            None => return,
        }
    };
    if rerun_pins && !mic.callee_rerun_pins() {
        // Before the publication, so a reader that finds the entry finds the
        // flag (`JitMICSlot::mark_callee_rerun_pins`).
        mic.mark_callee_rerun_pins();
    }
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    mic.update(receiver_cid, &class_name, entry as u64, needs_ctx, jdk_only);
    let pic = (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot));
    if let Some(pic) = pic {
        if pic.install(receiver_cid, &class_name, entry as u64, needs_ctx, jdk_only) {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(vm, None);
        }
    }
    // The re-check AFTER the install (item 4 of
    // `docs/feature-designs/profile-guided-inlining.md`,
    // JIT r22 wave 3): a redefinition that moved the epoch between the stamp
    // compare above and the `update` / `install` withdraws what was just
    // published, so no hit can run the pre-redefinition body past the next
    // call. The stamp is read before the install and this after it.
    withdraw_ic_publication_if_redefined(vm, epoch_now, mic, pic);
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] bound-site-ic published {}.{}{} for receiver class {} ({}) entry={:#x}",
            info.class_name, info.method_name, info.descriptor, receiver_cid, class_name, entry
        );
    }
}

