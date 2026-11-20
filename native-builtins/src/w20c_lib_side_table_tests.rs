use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::Ordering::Relaxed;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing below dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn slots_of(vm: usize) -> usize {
    lock_key_registry()
        .lock()
        .buckets
        .values()
        .map(|b| b.iter().filter(|s| s.vm == vm).count())
        .sum()
}

fn retired_of(vm: usize) -> usize {
    netty_queue_retired_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(owner, _)| *owner == vm)
        .count()
}

fn memo_has(key: usize) -> bool {
    message_bytes_to_chars_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
}

fn queue_has(key: usize) -> bool {
    netty_jctools_queue_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
}

fn offer(ctx: &mut crate::test_utils::MockNativeContext, q: ObjectRef, e: ObjectRef) {
    let args = [Value::Object(Some(q)), Value::Object(Some(e))];
    let r = crate::messaging_shims::native_netty_mpsc_offer(ctx, &args).expect("offer");
    assert_eq!(r, Some(Value::Int(1)));
}

fn poll(ctx: &mut crate::test_utils::MockNativeContext, q: ObjectRef) -> Option<ObjectRef> {
    let args = [Value::Object(Some(q))];
    match crate::messaging_shims::native_netty_mpsc_poll(ctx, &args).expect("poll") {
        Some(Value::Object(o)) => o,
        other => panic!("poll answered {other:?}"),
    }
}

fn size(ctx: &mut crate::test_utils::MockNativeContext, q: ObjectRef) -> i32 {
    let args = [Value::Object(Some(q))];
    match crate::messaging_shims::native_netty_mpsc_size(ctx, &args).expect("size") {
        Some(Value::Int(n)) => n,
        other => panic!("size answered {other:?}"),
    }
}

/// The page's retire test: one identity hash in two VMs, distinct rows in
/// every table, and a VM's teardown takes only its own.
#[test]
fn two_vms_objects_with_one_identity_hash_keep_distinct_rows() {
    const VM_A: usize = 0x7ff2_0c01;
    const VM_B: usize = 0x7ff2_0c09;
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let o = obj(0x1_20C0_0010);
    assert_eq!(a.identity_hash_code(o), b.identity_hash_code(o));

    // BufferedInputStream mark / reset.
    buffered_input_stream_mark(&a, o, 4);
    assert!(buffered_input_stream_rewind(&a, o));
    assert!(
        !buffered_input_stream_rewind(&b, o),
        "B's stream has no mark"
    );
    assert!(buffered_input_stream_can_delegate_bulk(&b, o));
    assert!(!buffered_input_stream_can_delegate_bulk(&a, o));

    // JUL `useParentHandlers`.
    jul_logger_set_use_parent_handlers(&a, o, false);
    assert_eq!(jul_logger_use_parent_handlers_flag(&a, o), Some(false));
    assert_eq!(jul_logger_use_parent_handlers_flag(&b, o), None);

    // `MessageBytes.toChars` memo: B's recycle must not drop A's row.
    let ka = lib_side_key(&a, o);
    message_bytes_to_chars_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ka, (7, 3, 0, 0));
    message_bytes_forget_chars_memo(&b, o);
    assert!(memo_has(ka), "B's recycle dropped A's memo row");
    assert!(
        existing_weak_lock_key(&b, o).is_none(),
        "B's reads and removes minted a key"
    );

    // LongAdder cell strips.
    let cells_a = long_adder_cells(&mut a, o);
    cells_a.cells[0].fetch_add(5, Relaxed);
    assert_eq!(long_adder_cells_sum(&mut b, o), 0, "B summed A's strip");
    assert_eq!(long_adder_cells_sum(&mut a, o), 5);
    let cells_b = long_adder_cells(&mut b, o);
    assert!(!std::sync::Arc::ptr_eq(&cells_a, &cells_b));
    assert_ne!(lib_side_key(&b, o), ka);

    // Netty jctools queue.
    let elem = obj(0x1_20C0_0110);
    offer(&mut a, o, elem);
    assert_eq!(size(&mut b, o), 0, "B sees A's element");
    assert_eq!(poll(&mut b, o), None);
    assert_eq!(size(&mut a, o), 1);
    assert_eq!(poll(&mut a, o), Some(elem));

    // A's teardown takes A's rows only.
    forget_vm_lock_keys(VM_A);
    forget_vm_netty_queue_state(VM_A);
    assert_eq!(slots_of(VM_A), 0);
    assert_eq!(retired_of(VM_A), 0);
    assert!(!memo_has(ka));
    assert!(!buffered_input_stream_rewind(&a, o));
    assert_eq!(jul_logger_use_parent_handlers_flag(&a, o), None);
    assert!(long_adder_existing_cells(&a, o).is_none());
    assert!(
        long_adder_existing_cells(&b, o).is_some(),
        "A's teardown took B's strip"
    );
    forget_vm_lock_keys(VM_B);
    forget_vm_netty_queue_state(VM_B);
    assert_eq!(slots_of(VM_B), 0);
}

/// The page's dead-key test: a dead object's rows go with its key, a live
/// one's stay, and a dead queue's element handles are released by the
/// VM's next queue call.
#[test]
fn a_dead_objects_rows_go_with_its_key() {
    const VM: usize = 0x7ff2_0c11;
    let mut c = ctx_for(VM);
    let dead = obj(0x1_20C1_0010);
    let live = obj(0x1_20C1_1010);
    let elem = obj(0x1_20C1_2010);
    for o in [dead, live] {
        buffered_input_stream_mark(&c, o, 8);
        jul_logger_set_use_parent_handlers(&c, o, false);
        let k = lib_side_key(&c, o);
        message_bytes_to_chars_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, (1, 1, 0, 0));
        long_adder_cells(&mut c, o).cells[1].fetch_add(3, Relaxed);
    }
    offer(&mut c, dead, elem);
    let dead_key = existing_weak_lock_key(&c, dead).expect("keyed");
    let live_key = existing_weak_lock_key(&c, live).expect("keyed");
    assert!(queue_has(dead_key));
    assert_eq!(c.global_root_count(), 1);

    let live_addr = live.as_ptr() as usize;
    assert_eq!(gc_sweep_lock_keys(VM, &|a| a == live_addr), 1);

    assert!(existing_weak_lock_key(&c, dead).is_none());
    assert!(!buffered_input_stream_rewind(&c, dead));
    assert_eq!(jul_logger_use_parent_handlers_flag(&c, dead), None);
    assert!(!memo_has(dead_key));
    assert!(long_adder_existing_cells(&c, dead).is_none());
    assert!(!queue_has(dead_key));
    assert_eq!(retired_of(VM), 1, "the dead queue's handle awaits release");

    assert!(buffered_input_stream_rewind(&c, live));
    assert_eq!(jul_logger_use_parent_handlers_flag(&c, live), Some(false));
    assert!(memo_has(live_key));
    assert_eq!(long_adder_cells_sum(&mut c, live), 3);

    // The VM's next queue call releases the orphaned handle.
    assert_eq!(poll(&mut c, live), None);
    assert_eq!(retired_of(VM), 0);
    assert_eq!(c.global_root_count(), 0);

    forget_vm_lock_keys(VM);
    forget_vm_netty_queue_state(VM);
    assert!(!memo_has(live_key));
    assert!(long_adder_existing_cells(&c, live).is_none());
    assert_eq!(slots_of(VM), 0);
}

/// `netty_queue_drain_retired` used to release every VM's retired handles
/// through the calling VM's context.
#[test]
fn a_vms_retired_queue_handles_are_released_only_by_that_vm() {
    const VM_A: usize = 0x7ff2_0c21;
    const VM_B: usize = 0x7ff2_0c29;
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let (qa, qb, e) = (obj(0x1_20C2_0010), obj(0x1_20C2_1010), obj(0x1_20C2_2010));
    offer(&mut a, qa, e);
    assert_eq!(poll(&mut a, qa), Some(e));
    assert_eq!(retired_of(VM_A), 1);
    assert_eq!(a.global_root_count(), 1, "released on A's next queue call");

    offer(&mut b, qb, e);
    assert_eq!(retired_of(VM_A), 1, "B's call took A's retired handle");

    assert_eq!(poll(&mut a, qa), None);
    assert_eq!(retired_of(VM_A), 0);
    assert_eq!(a.global_root_count(), 0);

    forget_vm_lock_keys(VM_A);
    forget_vm_lock_keys(VM_B);
    forget_vm_netty_queue_state(VM_A);
    forget_vm_netty_queue_state(VM_B);
    assert_eq!(retired_of(VM_B), 0);
}

/// Discovery: `Thread.setContextClassLoader(null)` in one VM no longer
/// makes another VM's thread with the same identity hash answer `null`,
/// and the row goes with the thread.
#[test]
fn explicit_null_context_loader_rows_are_per_vm_and_weak() {
    const VM_A: usize = 0x7ff2_0c31;
    const VM_B: usize = 0x7ff2_0c39;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let t = obj(0x1_20C3_0010);
    let live = obj(0x1_20C3_1010);
    set_thread_context_loader_explicit_null(&a, t, true);
    set_thread_context_loader_explicit_null(&a, live, true);
    assert!(thread_context_loader_is_explicit_null(&a, t));
    assert!(
        !thread_context_loader_is_explicit_null(&b, t),
        "leaked across VMs"
    );
    set_thread_context_loader_explicit_null(&b, t, false);
    assert!(
        thread_context_loader_is_explicit_null(&a, t),
        "B's clear reached A"
    );
    assert!(
        existing_weak_lock_key(&b, t).is_none(),
        "a clear minted a key"
    );

    let live_addr = live.as_ptr() as usize;
    assert_eq!(gc_sweep_lock_keys(VM_A, &|x| x == live_addr), 1);
    assert!(!thread_context_loader_is_explicit_null(&a, t));
    assert!(thread_context_loader_is_explicit_null(&a, live));
    forget_vm_lock_keys(VM_A);
    forget_vm_lock_keys(VM_B);
    assert!(!thread_context_loader_is_explicit_null(&a, live));
}

/// The first mapping in a VM initialises `MappingMatch` with every object
/// argument pinned and releases the pins; once the class is initialised
/// the arguments come back borrowed and nothing is pinned.
#[test]
fn mapper_args_initialise_mapping_match_first_and_release_their_pins() {
    // Its own VM identity: the "already initialised" memo is per thread
    // and per VM, and 0 (every other mock) is never memoised.
    let mut c = ctx_for(0x7ff2_0c51);
    let o = c.fresh_object_ref();
    let args = [Value::Object(Some(o)), Value::Int(3), Value::Object(None)];
    let pins = c.native_pin_count_for_test();
    assert!(c.class_id_by_name(MAPPING_MATCH_CLASS).is_none());

    let first = mapper_args_with_mapping_match_ready(&mut c, &args);
    assert!(matches!(first, std::borrow::Cow::Owned(_)));
    assert_eq!(&*first, &args[..]);
    assert_eq!(c.native_pin_count_for_test(), pins, "a pin leaked");
    assert!(c.class_id_by_name(MAPPING_MATCH_CLASS).is_some());

    let second = mapper_args_with_mapping_match_ready(&mut c, &args);
    assert!(matches!(second, std::borrow::Cow::Borrowed(_)));
    assert_eq!(c.native_pin_count_for_test(), pins);
}
