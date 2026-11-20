use super::*;

const VM_A: usize = 0xD15C_0A01;
const VM_B: usize = 0xD15C_0B01;

/// Two VMs' contexts with one identity hash: A's uninitialised context is
/// refused and B's initialised one served, in both orders; teardown drops
/// only the named VM's rows.
#[test]
fn marks_are_per_vm_in_both_orders() {
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    // SAFETY: never dereferenced; the mock hashes the address.
    let obj = unsafe { ObjectRef::from_raw(0x0d15_c000_0100usize as *mut u8) };

    mark_context_uninitialized(&a, obj);
    mark_context_initialized(&b, obj);
    assert!(
        require_context_initialized(&a, obj).is_err(),
        "B's getDefault cleared A's mark"
    );
    assert!(require_context_initialized(&b, obj).is_ok());

    mark_context_initialized(&a, obj);
    mark_context_uninitialized(&a, obj);
    mark_context_initialized(&b, obj);
    assert!(require_context_initialized(&a, obj).is_err());
    assert!(
        require_context_initialized(&b, obj).is_ok(),
        "A's mark refused B's context"
    );

    mark_context_uninitialized(&b, obj);
    forget_vm_uninitialized_contexts(VM_A);
    assert!(
        require_context_initialized(&a, obj).is_ok(),
        "A's row survived teardown"
    );
    assert!(
        require_context_initialized(&b, obj).is_err(),
        "teardown dropped B's row"
    );
    forget_vm_uninitialized_contexts(VM_B);
}
