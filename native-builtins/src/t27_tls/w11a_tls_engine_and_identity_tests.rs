use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xA11A_0001;
const VM_B: usize = 0xA11A_0002;
const VM_C: usize = 0xA11A_0003;
const VM_D: usize = 0xA11A_0004;
/// `a_minted_engine_starts_from_the_default_state`'s own VM: it used to
/// share `VM_A` with the identity test, whose `forget_vm_tls_state(VM_A)`
/// (and teardown) could drop this test's engine row mid-assertion when
/// the two ran in parallel (gc-common w25-b).
const VM_E: usize = 0xA11A_0005;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key only, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Clears one test's VMs even when an assertion fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

/// Removes hand-inserted engine cells even when an assertion fails.
struct EngineIds(Vec<i32>);
impl Drop for EngineIds {
    fn drop(&mut self) {
        let mut registry = engine_registry().write();
        for id in &self.0 {
            registry.remove(id);
        }
    }
}

fn identity(tag: &str) -> RuntimeTlsIdentity {
    RuntimeTlsIdentity {
        cert_pem: format!("cert-{tag}"),
        key_pem: format!("key-{tag}"),
        client_ca_pem: None,
    }
}

#[test]
fn the_runtime_identity_belongs_to_the_vm_that_installed_it() {
    let _teardown = Teardown(&[VM_A, VM_B, VM_C]);
    set_runtime_tls_identity(VM_A, Some(identity("a")));
    assert!(
        runtime_tls_identity(VM_B).is_none(),
        "B must not serve A's key"
    );
    set_runtime_tls_identity(VM_B, Some(identity("b")));
    assert_eq!(runtime_tls_identity(VM_A).unwrap().key_pem, "key-a");
    assert_eq!(runtime_tls_identity(VM_B).unwrap().key_pem, "key-b");

    // The keystore path installs into its own VM only. VM C has no
    // identity yet, so even a candidate that does not build is installed
    // (the unchanged first-install rule).
    install_identity_from_der(VM_C, &[0x30, 0x03, 0x02, 0x01, 0x00], &[vec![1, 2, 3]]);
    assert!(runtime_tls_identity(VM_C)
        .unwrap()
        .cert_pem
        .contains("BEGIN CERTIFICATE"));
    assert_eq!(runtime_tls_identity(VM_A).unwrap().key_pem, "key-a");
    assert_eq!(runtime_tls_identity(VM_B).unwrap().key_pem, "key-b");

    // An engine's default server config reads its own VM's slot.
    let none = default_engine_server_config(VM_D, &[], false)
        .err()
        .expect("VM D has no identity");
    assert!(none.contains("No TLS key/cert configured"), "{none}");
    let a_err = default_engine_server_config(VM_A, &[], false)
        .err()
        .expect("cert-a is not a PEM");
    assert!(
        !a_err.contains("No TLS key/cert configured"),
        "VM A's identity was found: {a_err}"
    );

    // Clearing never creates a row, and teardown drops only its VM.
    set_runtime_tls_identity(VM_D, None);
    assert!(!RUNTIME_TLS_IDENTITY.has_row(VM_D));
    forget_vm_tls_state(VM_A);
    assert!(runtime_tls_identity(VM_A).is_none());
    assert_eq!(runtime_tls_identity(VM_B).unwrap().key_pem, "key-b");
}

#[test]
fn a_minted_engine_starts_from_the_default_state() {
    let _teardown = Teardown(&[VM_E]);
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_E);
    // A collected engine and the fresh one `createSSLEngine` just
    // allocated at its address, before any sweep freed the dead one's key:
    // same object key (a weak lock key since gc-common w26-b, matched by
    // address; a LIVE same-hash engine elsewhere has its own, see
    // `w26b_tls_object_key_tests`).
    let engine = at(0x1_A11A_1000);
    let key = engine_objref_key(&a, engine);
    let old_id = engine_id_or_alloc(&a, engine);
    assert_eq!(with_engine(old_id, |s| s.vm), Some(VM_E));
    with_engine(old_id, |s| {
        s.closed_inbound = true;
        s.closed_outbound = true;
        s.peer_cert_chain_der = vec![vec![0xDE, 0xAD]];
        s.negotiated_alpn = Some("h2".to_string());
    });
    engine_session_table()
        .lock()
        .insert((key, SessionPhase::Negotiated), at(0xA11A_2000));
    engine_alpn_selector_table()
        .lock()
        .insert(key, at(0xA11A_3000));

    // The mint point: `createSSLEngine`'s first engine native.
    set_engine_trust_roots_override(&a, engine);
    let new_id = engine_id_or_alloc(&a, engine);
    assert_ne!(new_id, old_id);
    assert!(!engine_registry().read().contains_key(&old_id));
    let fresh = with_engine(new_id, |s| {
        (
            s.closed_inbound,
            s.closed_outbound,
            s.peer_cert_chain_der.is_empty(),
            s.negotiated_alpn.is_none(),
            s.conn.is_none(),
            s.vm,
        )
    });
    assert_eq!(fresh, Some((false, false, true, true, true, VM_E)));
    assert!(!engine_session_table().lock().keys().any(|(k, _)| *k == key));
    assert!(!engine_alpn_selector_table().lock().contains_key(&key));
}

/// Engine B's `wrap` completes while engine A's lock is held -- the shape
/// of A parked mid-`process_new_packets`. Under the process-wide registry
/// write guard B waited for A.
#[test]
fn one_engines_lock_does_not_stall_another_engine() {
    let id_a = engine_alloc_id();
    let id_b = engine_alloc_id();
    let _ids = EngineIds(vec![id_a, id_b]);
    engine_insert(id_a, EngineState::default());
    engine_insert(id_b, EngineState::default());

    let cell_a = engine_cell(id_a).expect("A has a cell");
    let parked = cell_a.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let produced = with_engine(id_b, |s| {
            s.outbound.extend_from_slice(b"record");
            s.outbound.len()
        });
        let _ = tx.send(produced);
    });
    let got = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(parked);
    worker.join().expect("worker");
    assert_eq!(
        got.expect("engine B must not wait for engine A's lock"),
        Some(6)
    );
    // A is still reachable once released.
    assert_eq!(with_engine(id_a, |s| s.outbound.len()), Some(0));
}
