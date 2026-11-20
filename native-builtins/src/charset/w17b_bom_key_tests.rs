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

/// The page's first scenario: VM B's `UTF-16` encoder with the identity
/// hash of VM A's, which already wrote its BOM, must still write its own.
/// The old registry had no VM in its slots and handed B A's key.
#[test]
fn a_second_vms_encoder_with_one_identity_hash_still_writes_its_bom() {
    const VM_A: usize = 0xB17B_0001;
    const VM_B: usize = 0xB17B_0002;
    let a = ctx_for(VM_A);
    let b = ctx_for(VM_B);
    let (ea, eb) = (obj(0x1_B17D_0010), obj(0x2_B17D_0010));
    assert_eq!(a.identity_hash_code(ea), b.identity_hash_code(eb));
    mark_bom_written(&a, ea);
    assert!(bom_already_written(&a, ea));
    assert!(!bom_already_written(&b, eb), "B inherited A's BOM state");
    let ka = bom_key_for(&a, ea);
    assert_ne!(ka, bom_key_for(&b, eb));

    mark_bom_written(&b, eb);
    crate::forget_vm_lock_keys(VM_A);
    assert!(bom_already_written(&b, eb), "A's teardown took B's row");
    assert!(
        !bom_written_set().lock().contains(&ka),
        "A's teardown left A's row"
    );
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// A dead encoder's row goes with its key, and a later encoder with its
/// identity hash starts a fresh session (the old registry rebound the dead
/// slot to it, so its first `encode()` left the BOM out).
#[test]
fn a_dead_encoders_bom_row_goes_with_its_key() {
    const VM: usize = 0xB17B_0003;
    let c = ctx_for(VM);
    let dead = obj(0x1_B17E_0010);
    mark_bom_written(&c, dead);
    let kd = bom_key_for(&c, dead);
    assert!(bom_written_set().lock().contains(&kd));
    let addr = dead.as_ptr() as usize;
    assert!(crate::gc_sweep_lock_keys(VM, &|a| a != addr) >= 1);
    assert!(
        !bom_written_set().lock().contains(&kd),
        "the dead encoder's row outlived it"
    );

    let reborn = obj(0x2_B17E_0010);
    assert_ne!(bom_key_for(&c, reborn), kd);
    assert!(!bom_already_written(&c, reborn));
    crate::forget_vm_lock_keys(VM);
}

/// A live encoder that a collection moves keeps its session: no second
/// BOM after the move. `reset()` (`clear_bom_state`) at the new address
/// still re-arms it.
#[test]
fn a_moved_encoder_keeps_its_bom_session() {
    const VM: usize = 0xB17B_0004;
    let c = ctx_for(VM);
    let (old, new) = (0x1_B17F_0010usize, 0x3_B17F_0010usize);
    mark_bom_written(&c, obj(old));
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(old, new);
    crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == old, &moved);
    assert!(bom_already_written(&c, obj(new)));
    crate::gc_sweep_lock_keys(VM, &|a| a == new);
    assert!(bom_already_written(&c, obj(new)));
    clear_bom_state(&c, obj(new));
    assert!(!bom_already_written(&c, obj(new)));
    crate::forget_vm_lock_keys(VM);
}
