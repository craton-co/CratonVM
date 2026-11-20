use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xB27B_0C01;
const VM_B: usize = 0xB27B_0C02;
const VM_C: usize = 0xB27B_0C03;
const VM_D: usize = 0xB27B_0C04;
const VM_E: usize = 0xB27B_0C05;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn ctx_in(vm: usize) -> MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Forgets this test's VMs only, even when an assertion fails: the rows
/// first (as `forget_vm_native_root_stores` orders it), then the keys.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_http_url_connection_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn result(status: i32) -> RealResult {
    RealResult {
        status,
        headers: Vec::new(),
        body: vec![0x42; 64],
        reason: String::new(),
        truncated: false,
    }
}

fn method_of(ctx: &MockNativeContext, carrier: ObjectRef) -> Option<String> {
    real_req_of(ctx, carrier, |r| r.method.clone())
}

fn has_peer_row(ctx: &MockNativeContext, carrier: ObjectRef) -> bool {
    huc_existing_obj_key(ctx, carrier)
        .is_some_and(|k| https_peer_info().lock().unwrap().contains_key(&k))
}

/// Two live real carriers of one VM with one identity hash: separate
/// request, result and handshake rows, and a mint-time forget of one
/// (`real_forget`) leaves the other's in-flight request alone.
#[test]
fn two_live_same_hash_carriers_keep_separate_rows() {
    let _teardown = Teardown(&[VM_A]);
    let mut a = ctx_in(VM_A);
    let (c1, c2) = (at(0x1_B27B_C000), at(0x2_B27B_C000));
    assert_eq!(
        a.identity_hash_code(c1),
        a.identity_hash_code(c2),
        "premise: the two carriers share an identity hash"
    );
    with_real_req(&a, c1, |r| r.method = "PUT".to_string());
    assert_eq!(
        method_of(&a, c2),
        None,
        "a live collider does not send the first one's method"
    );
    with_real_req(&a, c2, |r| r.method = "DELETE".to_string());
    assert_eq!(method_of(&a, c1).as_deref(), Some("PUT"));
    assert_eq!(method_of(&a, c2).as_deref(), Some("DELETE"));

    let k1 = huc_existing_obj_key(&a, c1).expect("filed");
    real_results_with(VM_A, |t| {
        t.insert(k1, result(200));
    });
    assert!(real_is_connected(&a, c1));
    assert!(!real_is_connected(&a, c2), "the collider has not performed");

    record_https_peer_info(&a, Some(c1), &[vec![0x30u8]], "TLS_AES_128_GCM_SHA256");
    assert!(has_peer_row(&a, c1));
    assert!(
        !has_peer_row(&a, c2),
        "the collider has no handshake: `https_ensure_exchanged` runs its own"
    );

    real_forget(&mut a, c2);
    assert_eq!(method_of(&a, c2), None);
    assert_eq!(
        method_of(&a, c1).as_deref(),
        Some("PUT"),
        "the live collider's request stays"
    );
    assert!(real_is_connected(&a, c1));
}

/// The lock-key sweep frees a dead carrier's key: every row filed under it
/// goes (`lib.rs::sweep_lock_keys` -> `forget_huc_obj_keys`), a live
/// stream's association with the dead carrier goes too, and the dead
/// carrier's request-body root is parked, then released by the VM's next
/// body-stream call. A live carrier's rows stay.
#[test]
fn a_freed_key_drops_every_row_of_its_carrier() {
    let _teardown = Teardown(&[VM_B]);
    let mut b = ctx_in(VM_B);
    let (dead, live, stream) = (at(0xB27B_D000), at(0xB27B_D100), at(0xB27B_D200));
    with_real_req(&b, dead, |r| r.do_output = true);
    with_real_req(&b, live, |r| r.do_output = true);
    let k_dead = huc_existing_obj_key(&b, dead).expect("filed");
    let k_live = huc_existing_obj_key(&b, live).expect("filed");
    real_results_with(VM_B, |t| {
        t.insert(k_dead, result(200));
    });
    record_https_peer_info(&b, Some(dead), &[vec![0x30u8]], "TLS_AES_128_GCM_SHA256");
    note_response_stream(&b, stream, Some(dead));
    let k_stream = huc_existing_obj_key(&b, stream).expect("noted");
    let conn = register_synthetic_conn(
        &b,
        dead,
        ConnState {
            status: 200,
            response_body: vec![0x5A; 128],
            response_headers: Vec::new(),
            body_consumed: false,
            truncated: false,
        },
    );
    let baos = b.fresh_object_ref();
    real_body_stream_insert(&mut b, k_dead, baos);
    let root = REAL_BODY_STREAMS
        .peek(VM_B, |t| t.rows.get(&k_dead).map(|r| r.root))
        .flatten()
        .expect("the body row");

    let (live_addr, stream_addr) = (live.as_ptr() as usize, stream.as_ptr() as usize);
    crate::gc_sweep_lock_keys(VM_B, &|x| x == live_addr || x == stream_addr);

    assert!(
        huc_existing_obj_key(&b, dead).is_none(),
        "the key was freed"
    );
    assert_eq!(
        real_results_peek(VM_B, |t| t.contains_key(&k_dead)),
        Some(false)
    );
    assert_eq!(
        real_reqs_peek(VM_B, |t| t.contains_key(&k_dead)),
        Some(false)
    );
    assert_eq!(
        real_reqs_peek(VM_B, |t| t.contains_key(&k_live)),
        Some(true)
    );
    assert!(!https_peer_info().lock().unwrap().contains_key(&k_dead));
    assert!(
        !https_response_streams()
            .lock()
            .unwrap()
            .contains_key(&k_stream),
        "a live stream's association with a dead carrier goes"
    );
    assert_eq!(
        CONN_REGISTRY.peek(VM_B, |r| r.get(conn).is_some()),
        Some(false)
    );
    assert_eq!(
        REAL_BODY_STREAMS.peek(VM_B, |t| t.rows.contains_key(&k_dead)),
        Some(false)
    );
    if root != 0 {
        assert_eq!(
            REAL_BODY_STREAMS.peek(VM_B, |t| t.orphaned_roots.contains(&root)),
            Some(true),
            "the dead carrier's body root waits for a context"
        );
    }
    real_body_stream_forget(&mut b, None);
    assert_eq!(
        REAL_BODY_STREAMS.peek(VM_B, |t| t.orphaned_roots.is_empty()),
        Some(true)
    );
    if root != 0 {
        assert_eq!(b.resolve_global_root(root), None, "released");
    }
}

/// The per-read observers mint no key for a stream they know nothing
/// about, and the BAOS observer answers at once with no live upload.
#[test]
fn the_stream_observers_mint_nothing() {
    let _teardown = Teardown(&[VM_C]);
    let mut c = ctx_in(VM_C);
    let (baos, bais) = (at(0xB27B_E000), at(0xB27B_E100));
    assert!(matches!(
        huc_live_baos_event(&mut c, baos, BaosEvent::Flush),
        Ok(false)
    ));
    assert!(huc_existing_obj_key(&c, baos).is_none());
    assert!(
        huc_live_bais_event(&mut c, bais, cratonvm_native_api::registry::BaisEvent::Eof)
            .is_ok()
    );
    assert!(huc_existing_obj_key(&c, bais).is_none());
}

/// One address in two VMs never shares a key; one VM's lock-key teardown
/// drops exactly its own rows.
#[test]
fn two_vms_stay_separate() {
    let _teardown = Teardown(&[VM_D, VM_E]);
    let (d, e) = (ctx_in(VM_D), ctx_in(VM_E));
    let obj = at(0x1_B27B_F000);
    with_real_req(&d, obj, |r| r.method = "D".to_string());
    with_real_req(&e, obj, |r| r.method = "E".to_string());
    let (kd, ke) = (
        huc_existing_obj_key(&d, obj).expect("filed"),
        huc_existing_obj_key(&e, obj).expect("filed"),
    );
    assert_ne!(kd, ke);
    assert_eq!(method_of(&d, obj).as_deref(), Some("D"));
    assert_eq!(method_of(&e, obj).as_deref(), Some("E"));

    crate::forget_vm_lock_keys(VM_D);
    assert_eq!(real_reqs_peek(VM_D, |t| t.contains_key(&kd)), Some(false));
    assert_eq!(method_of(&e, obj).as_deref(), Some("E"));
}
