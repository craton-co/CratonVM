use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

/// Two live contexts of ONE VM with one identity hash (the mock's hash is
/// the address truncated to `i32`): A's "not initialised" mark does not
/// refuse B, B's `init` does not clear A's, and a dead context's mark goes
/// with its lock key.
#[test]
fn same_hash_contexts_keep_their_own_marks() {
    const VM: usize = 0xD29C_0A01;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (a_addr, b_addr) = (0x1_0D29_C000usize, 0x2_0D29_C000usize);
    // SAFETY: never dereferenced; the mock hashes the address.
    let a = unsafe { ObjectRef::from_raw(a_addr as *mut u8) };
    let b = unsafe { ObjectRef::from_raw(b_addr as *mut u8) };
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two contexts share an identity hash"
    );

    mark_context_uninitialized(&ctx, a);
    mark_context_uninitialized(&ctx, b);
    mark_context_initialized(&ctx, b);
    assert!(
        require_context_initialized(&ctx, a).is_err(),
        "B's init cleared A's mark"
    );
    assert!(
        require_context_initialized(&ctx, b).is_ok(),
        "A's mark refused B"
    );

    let ka = crate::existing_weak_lock_key(&ctx, a).expect("A was keyed");
    crate::gc_sweep_lock_keys(VM, &|addr| addr == b_addr);
    assert!(
        !uninitialized_contexts().lock().contains(&(VM, ka)),
        "a dead context's mark goes with its key"
    );
    crate::forget_vm_lock_keys(VM);
    forget_vm_uninitialized_contexts(VM);
}
