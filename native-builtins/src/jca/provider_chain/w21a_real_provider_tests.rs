use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities and provider name: no other test uses them.
const W21A_VM_A: usize = 0x0A21_4008;
const W21A_VM_B: usize = 0x0A21_5008;
const W21A_PROVIDER: &str = "W21aTestProvider";

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_real_provider_roots(W21A_VM_A);
        forget_vm_real_provider_roots(W21A_VM_B);
        // gc-common w22-b: `security_remove_provider` below now writes B's
        // own provider registry; do not leave it for the process.
        forget_vm_provider_registry(W21A_VM_A);
        forget_vm_provider_registry(W21A_VM_B);
    }
}

#[test]
fn w21a_real_providers_are_per_vm_and_forgotten_with_the_vm() {
    let _forget = Forget;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W21A_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W21A_VM_B);
    let prov_a = a.fresh_object_ref();
    // B's global-ref slot 1 holds an unrelated object: the same index VM
    // A's handle for its provider gets in A's table.
    let unrelated_b = b.fresh_object_ref();
    let unrelated_handle = b.add_global_root(unrelated_b);

    remember_real_provider(&mut a, W21A_PROVIDER, prov_a);
    assert_eq!(resolve_real_provider(&mut a, W21A_PROVIDER), Some(prov_a));
    assert_eq!(
        resolve_real_provider(&mut b, W21A_PROVIDER),
        None,
        "VM B must not resolve VM A's handle in its own table"
    );

    let prov_b = b.fresh_object_ref();
    remember_real_provider(&mut b, W21A_PROVIDER, prov_b);
    assert_eq!(
        b.resolve_global_root(unrelated_handle),
        Some(unrelated_b),
        "B's addProvider must not release the root of B's that A's row names"
    );
    assert_eq!(resolve_real_provider(&mut b, W21A_PROVIDER), Some(prov_b));
    assert_eq!(resolve_real_provider(&mut a, W21A_PROVIDER), Some(prov_a));

    // `Security.removeProvider` in B drops B's row only.
    let name = b.create_string(W21A_PROVIDER);
    let r = security_remove_provider(&mut b, &[Value::Object(Some(name))]);
    assert!(matches!(r, Ok(None)), "removeProvider answered {r:?}");
    assert_eq!(resolve_real_provider(&mut b, W21A_PROVIDER), None);
    assert_eq!(b.resolve_global_root(unrelated_handle), Some(unrelated_b));
    assert_eq!(resolve_real_provider(&mut a, W21A_PROVIDER), Some(prov_a));

    forget_vm_real_provider_roots(W21A_VM_A);
    assert!(!REAL_PROVIDERS.has_row(W21A_VM_A));
    assert_eq!(resolve_real_provider(&mut a, W21A_PROVIDER), None);
}

#[test]
fn w21a_real_provider_cache_rows_go_with_their_vm() {
    const W21A_VM_C: usize = 0x0A21_6008;
    const W21A_VM_D: usize = 0x0A21_7008;
    real_provider_cache()
        .lock()
        .insert((W21A_VM_C, W21A_PROVIDER.to_string()), 7);
    real_provider_cache()
        .lock()
        .insert((W21A_VM_D, W21A_PROVIDER.to_string()), 7);
    forget_vm_real_provider_roots(W21A_VM_C);
    let (c_row, d_row) = {
        let cache = real_provider_cache().lock();
        (
            cache.contains_key(&(W21A_VM_C, W21A_PROVIDER.to_string())),
            cache.contains_key(&(W21A_VM_D, W21A_PROVIDER.to_string())),
        )
    };
    assert!(!c_row, "the disposed VM's row is dropped");
    assert!(d_row, "another VM's row stays");
    forget_vm_real_provider_roots(W21A_VM_D);
}
