use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM: usize = 0x29E0_1801;

struct Teardown;
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(VM);
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

#[test]
fn builtin_loader_namespaces_are_allocated_not_hashed() {
    let _t = Teardown;
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (a, b) = (at(0x1_29E0_1800), at(0x2_29E0_1800));
    assert_eq!(ctx.identity_hash_code(a), ctx.identity_hash_code(b));

    let ns_a = proxy_builtin_namespace_id(&mut ctx, a);
    let ns_b = proxy_builtin_namespace_id(&mut ctx, b);
    assert_ne!(ns_a, ns_b, "one hash, two loaders, two namespaces");
    assert!(ns_a >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
    assert_ne!(ns_a, ctx.identity_hash_code(a) as u32);
    assert_eq!(proxy_builtin_namespace_id(&mut ctx, a), ns_a, "stable");

    let key_a = crate::existing_weak_lock_key(&ctx, a).expect("keyed");
    let b_addr = b.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == b_addr);
    assert!(!proxy_builtin_namespace_ids().lock().contains_key(&key_a));
    assert_eq!(proxy_builtin_namespace_id(&mut ctx, b), ns_b);
}
