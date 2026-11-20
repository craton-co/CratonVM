use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; nothing on these paths dereferences it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_for(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn permits(ctx: &dyn NativeContext, s: ObjectRef) -> (i32, i32) {
    let st = sem_side_state(ctx, s);
    (st.permits, st.fair)
}

/// The page's `sem` scenario: VM B's `Semaphore` with VM A's identity hash
/// used to share A's permits.
#[test]
fn two_vms_semaphores_with_one_identity_hash_keep_separate_permits() {
    const VM_A: usize = 0xB162_0001;
    const VM_B: usize = 0xB162_0002;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (sa, sb) = (obj(0x1_B16A_1000), obj(0x2_B16A_1000));
    assert_eq!(a.identity_hash_code(sa), b.identity_hash_code(sb));
    sem_set_side_state(
        &a,
        sa,
        SemState {
            permits: 5,
            fair: 1,
        },
    );
    assert_eq!(permits(&b, sb), (0, 0), "B saw A's permits");
    sem_set_side_state(
        &b,
        sb,
        SemState {
            permits: 2,
            fair: 0,
        },
    );
    assert_eq!(permits(&a, sa), (5, 1));
    assert_eq!(permits(&b, sb), (2, 0));

    crate::forget_vm_lock_keys(VM_A);
    assert_eq!(permits(&b, sb), (2, 0));
    assert_eq!(permits(&a, sa), (0, 0));
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// A dead `Semaphore`'s state goes with its key, and a later one with its
/// identity hash starts empty (the old registry rebound the dead slot).
#[test]
fn a_dead_semaphores_state_goes_with_its_key() {
    const VM: usize = 0xB162_0003;
    let c = ctx_for(VM);
    let dead = obj(0x1_B16B_1000);
    sem_set_side_state(
        &c,
        dead,
        SemState {
            permits: 7,
            fair: 1,
        },
    );
    let kd = sem_obj_key_for(&c, dead);
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(!sem_states_by_obj().lock().contains_key(&kd));
    let reborn = obj(0x2_B16B_1000);
    assert_ne!(sem_obj_key_for(&c, reborn), kd);
    assert_eq!(permits(&c, reborn), (0, 0));
    crate::forget_vm_lock_keys(VM);
}

/// A live `Semaphore` that a collection moves keeps its permits.
#[test]
fn a_moved_semaphore_keeps_its_permits() {
    const VM: usize = 0xB162_0004;
    let c = ctx_for(VM);
    let (old, new) = (0x1_B16C_1000usize, 0x3_B16C_1000usize);
    sem_set_side_state(
        &c,
        obj(old),
        SemState {
            permits: 3,
            fair: 0,
        },
    );
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert_eq!(permits(&c, obj(new)), (3, 0));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert_eq!(permits(&c, obj(new)), (3, 0));
    crate::forget_vm_lock_keys(VM);
}
