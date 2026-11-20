use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W22A_VM_A: usize = 0x0A22_6A08;
const W22A_VM_B: usize = 0x0A22_6B08;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in [W22A_VM_A, W22A_VM_B] {
            forget_vm_jboss_module_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn packages_of(key: ModulePackagesKey) -> Option<Vec<String>> {
    module_packages_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 5, and the package table found beside it): a Module's recorded
/// package set is keyed by its VM (and, since gc-common w29-e, its weak
/// lock key rather than its identity hash), so the same object address in
/// another VM finds nothing of it; a VM's teardown drops its boot-layer
/// memo row and its package rows, and no other VM's.
#[test]
fn w22a_module_state_is_per_vm_and_forgotten_with_the_vm() {
    let _forget = Forget;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W22A_VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W22A_VM_B);
    let module = a.fresh_object_ref();
    let key_a = module_packages_key(&a, module).expect("minted");
    assert_eq!(key_a.0, W22A_VM_A);
    module_packages_record(key_a, vec!["w22a.a".to_string()]);
    assert_eq!(
        existing_module_packages_key(&b, module),
        None,
        "the same address in VM B is not VM A's Module"
    );
    let key_b = module_packages_key(&b, module).expect("minted");
    assert_ne!(key_a, key_b);
    module_packages_record(key_b, vec!["w22a.b".to_string()]);
    assert_eq!(packages_of(key_a), Some(vec!["w22a.a".to_string()]));

    {
        let mut memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
        memo.insert(W22A_VM_A, 11);
        memo.insert(W22A_VM_B, 12);
    }
    forget_vm_jboss_module_state(W22A_VM_A);
    assert_eq!(packages_of(key_a), None);
    assert_eq!(packages_of(key_b), Some(vec!["w22a.b".to_string()]));
    {
        let memo = boot_layer_memo().lock().unwrap_or_else(|e| e.into_inner());
        assert!(!memo.contains_key(&W22A_VM_A));
        assert_eq!(memo.get(&W22A_VM_B), Some(&12));
    }
    forget_vm_jboss_module_state(W22A_VM_B);
    assert_eq!(packages_of(key_b), None);
    assert!(!boot_layer_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&W22A_VM_B));
}
