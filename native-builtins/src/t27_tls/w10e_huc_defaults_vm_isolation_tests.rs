use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xE10A_0001;
const VM_B: usize = 0xE10A_0002;
const VM_C: usize = 0xE10A_0003;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Clears one test's VMs even when an assertion fails, so a parallel test
/// never sees their rows.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

#[test]
fn huc_defaults_and_connection_captures_belong_to_the_capturing_vm() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);

    // One identity hash in both VMs (the mock's hash is the address
    // truncated to `i32`).
    let mut sslctx_a = at(0x1_E10A_2000);
    let mut sslctx_b = at(0x2_E10A_2000);
    let key_a = ctx_obj_key(&mut a, sslctx_a).unwrap();
    let key_b = ctx_obj_key(&mut b, sslctx_b).unwrap();
    ctx_identity_table()
        .lock()
        .insert(key_a, ("cert-a".to_string(), "key-a".to_string()));
    ctx_trust_roots_table().lock().insert(
        key_a,
        TlsTrustRoots {
            root_ders: vec![b"root-a".to_vec()],
            revocation: None,
        },
    );
    ctx_trust_roots_table().lock().insert(
        key_b,
        TlsTrustRoots {
            root_ders: vec![b"root-b".to_vec()],
            revocation: None,
        },
    );

    capture_huc_ssl_context(&mut a, &mut sslctx_a).unwrap();
    capture_huc_ssl_context(&mut b, &mut sslctx_b).unwrap();

    // Each VM reads back its own default policy.
    assert_eq!(
        huc_default_client_identity_in_vm(VM_A),
        Some(("cert-a".to_string(), "key-a".to_string()))
    );
    assert_eq!(huc_default_client_identity_in_vm(VM_B), None);
    let roots = |vm| huc_default_trust_roots_in_vm(vm).map(|r| r.root_ders);
    assert_eq!(roots(VM_A), Some(vec![b"root-a".to_vec()]));
    assert_eq!(roots(VM_B), Some(vec![b"root-b".to_vec()]));
    assert!(
        huc_default_tls_in_vm(VM_B)
            .and_then(|c| c.client_config.clone())
            .is_some(),
        "an anonymous context still gets a stable ClientConfig"
    );

    // A later capture in B does not change A's policy.
    capture_huc_ssl_context(&mut b, &mut sslctx_b).unwrap();
    assert_eq!(roots(VM_A), Some(vec![b"root-a".to_vec()]));

    // An instance-scoped capture is visible only to its own VM's
    // connection with that hash, and leaves B's default alone.
    let (conn_a, conn_b) = (at(0x1_E10A_3000), at(0x2_E10A_3000));
    capture_huc_ssl_context_for_connection(&mut b, conn_b, &mut sslctx_b).unwrap();
    assert!(huc_tls_for_connection(&b, conn_b).is_some());
    assert!(huc_tls_for_connection(&a, conn_a).is_none());
    assert_eq!(roots(VM_B), Some(vec![b"root-b".to_vec()]));

    // The engine tables are torn down with the VM too.
    let engine_id = engine_id_or_alloc(&a, at(0x1_E10A_4000));
    assert!(engine_registry().read().contains_key(&engine_id));

    forget_vm_tls_state(VM_A);
    assert!(huc_default_tls_in_vm(VM_A).is_none());
    assert!(!engine_registry().read().contains_key(&engine_id));
    assert!(!ctx_identity_table().lock().contains_key(&key_a));
    assert!(!ctx_trust_roots_table().lock().contains_key(&key_a));
    assert_eq!(roots(VM_B), Some(vec![b"root-b".to_vec()]));
    assert!(huc_tls_for_connection(&b, conn_b).is_some());
}

/// The per-VM bound evicts the OLDEST connection capture, not all of
/// them (the table used to clear itself at 256).
#[test]
fn connection_capture_bound_evicts_only_the_oldest() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_C);
    let mut sslctx = at(0x3_E10A_5000);
    let key = ctx_obj_key(&mut c, sslctx).unwrap();
    ctx_trust_roots_table()
        .lock()
        .insert(key, TlsTrustRoots::default());
    let conn = |i: usize| at(0x3_E10B_0000 + i * 8);
    for i in 0..=HUC_CONNECTION_CAPTURES_PER_VM {
        capture_huc_ssl_context_for_connection(&mut c, conn(i), &mut sslctx).unwrap();
    }
    assert!(
        huc_tls_for_connection(&c, conn(0)).is_none(),
        "oldest evicted"
    );
    for i in 1..=HUC_CONNECTION_CAPTURES_PER_VM {
        assert!(
            huc_tls_for_connection(&c, conn(i)).is_some(),
            "row {i} kept"
        );
    }
}
