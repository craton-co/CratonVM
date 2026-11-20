use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identity: no other test uses it.
const VM: usize = 0x29E0_1908;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_jboss_module_state(VM);
        crate::forget_vm_lock_keys(VM);
    }
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn recorded(ctx: &dyn NativeContext, module: ObjectRef) -> Option<Vec<String>> {
    let key = existing_module_packages_key(ctx, module)?;
    module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
}

#[test]
fn same_hash_modules_keep_their_own_packages() {
    let _forget = Forget;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    // 4 GiB apart: one mock identity hash (address truncated to i32).
    let (a, b) = (at(0x1_29E0_1900), at(0x2_29E0_1900));
    assert_eq!(ctx.identity_hash_code(a), ctx.identity_hash_code(b));

    module_packages_record(
        module_packages_key(&ctx, a).expect("minted"),
        vec!["w29e.a".to_string()],
    );
    assert_eq!(recorded(&ctx, b), None, "B was never recorded");
    module_packages_record(
        module_packages_key(&ctx, b).expect("minted"),
        vec!["w29e.b".to_string()],
    );
    assert_eq!(recorded(&ctx, a), Some(vec!["w29e.a".to_string()]));
    assert_eq!(recorded(&ctx, b), Some(vec!["w29e.b".to_string()]));

    // A dies: the lock-key sweep frees its key and drops its row only.
    let b_addr = b.as_ptr() as usize;
    crate::gc_sweep_lock_keys(VM, &|x| x == b_addr);
    assert_eq!(recorded(&ctx, a), None);
    assert_eq!(recorded(&ctx, b), Some(vec!["w29e.b".to_string()]));
    assert!(!module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(&(vm, _), v)| vm == VM && v == &vec!["w29e.a".to_string()]));
}
