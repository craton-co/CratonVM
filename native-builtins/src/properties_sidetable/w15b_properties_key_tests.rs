use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

const PW: &str = "hibernate.connection.password";

/// The page's `Properties` scenario: VM B's `new Properties()` with VM A's
/// identity hash used to be handed A's key, so the two shared one side map
/// (Hibernate's `maskOut` bleed, across VMs).
#[test]
fn two_vms_properties_with_one_identity_hash_keep_separate_side_maps() {
    const VM_A: usize = 0xB150_0001;
    const VM_B: usize = 0xB150_0002;
    let mut a = ctx_for(VM_A);
    let mut b = ctx_for(VM_B);
    let pa = obj(0x1_B15A_0000);
    let pb = obj(0x2_B15A_0000);
    assert_eq!(a.identity_hash_code(pa), b.identity_hash_code(pb));

    put_kv(&mut a, pa, PW, "secret");
    put_kv(&mut b, pb, PW, "****");
    assert_ne!(key_for(&a, pa), key_for(&b, pb));
    assert_eq!(get_kv(&a, pa, PW).as_deref(), Some("secret"));
    assert_eq!(get_kv(&b, pb, PW).as_deref(), Some("****"));
    mark_system_props(&b, pb);
    assert!(is_system_props(&b, pb));
    assert!(!is_system_props(&a, pa), "B's marker leaked into A");
    {
        let queues = weak_track_queues().lock();
        assert!(queues.contains_key(&VM_A) && queues.contains_key(&VM_B));
    }

    // Teardown of A takes A's rows and A's queue handle, nothing of B's.
    crate::forget_vm_lock_keys(VM_A);
    assert!(!weak_track_queues().lock().contains_key(&VM_A));
    assert!(weak_track_queues().lock().contains_key(&VM_B));
    assert_eq!(get_kv(&b, pb, PW).as_deref(), Some("****"));
    assert!(is_system_props(&b, pb));
    // A's object keys afresh (the registry never re-mints) and is empty.
    assert_eq!(get_kv(&a, pa, PW), None);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
    assert!(!weak_track_queues().lock().contains_key(&VM_B));
}

/// A dead `Properties` object's rows go with its key at the sweep that
/// finds it dead, and a later object with its identity hash inherits none
/// of them (the old registry rebound the dead slot to it).
#[test]
fn a_dead_properties_rows_go_with_its_key() {
    const VM: usize = 0xB150_0003;
    let mut c = ctx_for(VM);
    let dead = obj(0x1_B15B_0000);
    let reborn = obj(0x2_B15B_0000);
    put_kv(&mut c, dead, PW, "secret");
    mark_system_props(&c, dead);
    let kd = key_for(&c, dead);
    assert!(table().lock().contains_key(&kd));

    assert!(crate::gc_sweep_lock_keys(VM, &|_| false) >= 1);
    assert!(
        !table().lock().contains_key(&kd),
        "side map outlived its key"
    );
    assert!(!system_props_keys().lock().contains(&kd));
    assert!(
        !weak_track_registry()
            .lock()
            .values()
            .any(|e| e.props_key == kd),
        "the weak-track row of the dead WeakReference stayed"
    );

    assert_ne!(key_for(&c, reborn), kd);
    assert_eq!(get_kv(&c, reborn, PW), None);
    assert!(!is_system_props(&c, reborn));
    crate::forget_vm_lock_keys(VM);
}

/// A live `Properties` object that a collection moves keeps its rows: the
/// epilogue re-addresses its slot, and the next sweep judges it there.
#[test]
fn a_moved_properties_keeps_its_rows() {
    const VM: usize = 0xB150_0004;
    let mut c = ctx_for(VM);
    let (old, new) = (0x1_B15C_0000usize, 0x3_B15C_0000usize);
    put_kv(&mut c, obj(old), PW, "secret");
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(get_kv(&c, obj(new), PW).as_deref(), Some("secret"));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(get_kv(&c, obj(new), PW).as_deref(), Some("secret"));
    crate::forget_vm_lock_keys(VM);
}

thread_local! {
    static POLLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn count_polls(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "poll" {
        POLLS.with(|p| p.set(p.get() + 1));
        return Some(Ok(None));
    }
    None
}

fn registered_since_drain(vm: usize) -> u32 {
    weak_track_queues()
        .lock()
        .get(&vm)
        .map(|q| q.registered_since_drain)
        .unwrap_or(0)
}

/// With the side map swept, `put_kv_units` no longer reaches the cap that
/// used to be the only thing that drained the weak-track queue; the
/// registration path drains it every `WEAK_TRACK_DRAIN_EVERY` objects.
#[test]
fn registrations_drain_the_weak_track_queue_periodically() {
    const VM: usize = 0xB150_0005;
    let mut c = ctx_for(VM);
    c.set_invoke_virtual_hook(count_polls);
    POLLS.with(|p| p.set(0));
    let n = WEAK_TRACK_DRAIN_EVERY as usize;
    for i in 0..n - 1 {
        put_kv(&mut c, obj(0x1_B15D_0000 + i * 0x10), "k", "v");
    }
    assert_eq!(registered_since_drain(VM), WEAK_TRACK_DRAIN_EVERY - 1);
    assert_eq!(POLLS.with(|p| p.get()), 0);
    // A second write to a tracked object registers nothing.
    put_kv(&mut c, obj(0x1_B15D_0000), "k2", "v2");
    assert_eq!(registered_since_drain(VM), WEAK_TRACK_DRAIN_EVERY - 1);
    put_kv(&mut c, obj(0x1_B15D_0000 + (n - 1) * 0x10), "k", "v");
    assert_eq!(registered_since_drain(VM), 0);
    assert_eq!(
        POLLS.with(|p| p.get()),
        1,
        "the Nth registration drains once"
    );
    crate::forget_vm_lock_keys(VM);
}
