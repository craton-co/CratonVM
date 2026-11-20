use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_A: usize = 0xB26B_0001;
const VM_B: usize = 0xB26B_0002;
const VM_C: usize = 0xB26B_0003;
const VM_D: usize = 0xB26B_0004;
const VM_E: usize = 0xB26B_0005;
const VM_F: usize = 0xB26B_0006;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: a key / address only, never dereferenced (the mock hashes
    // the address).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn ctx_in(vm: usize) -> crate::test_utils::MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

/// Forgets this test's VMs only (TLS state first, then the lock keys, as
/// `forget_vm_native_root_stores` orders it), even when an assertion
/// fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

/// Two live engines of one VM with one identity hash: distinct keys,
/// distinct engine ids, and neither sees the other's rustls state,
/// sessions or selector.
#[test]
fn two_live_same_hash_engines_keep_separate_state() {
    let _teardown = Teardown(&[VM_A]);
    let a = ctx_in(VM_A);
    let (e1, e2) = (at(0x1_B26B_1000), at(0x2_B26B_1000));
    assert_eq!(
        a.identity_hash_code(e1),
        a.identity_hash_code(e2),
        "premise: the two engines share an identity hash"
    );
    let (k1, k2) = (engine_objref_key(&a, e1), engine_objref_key(&a, e2));
    assert_ne!(k1, k2, "a live collider has a key of its own");
    assert!(objref_key_owned_by(k1, VM_A) && objref_key_owned_by(k2, VM_A));
    let (id1, id2) = (engine_id_or_alloc(&a, e1), engine_id_or_alloc(&a, e2));
    assert_ne!(
        id1, id2,
        "the second engine must not drive the first's connection"
    );
    assert_eq!(engine_id_or_alloc(&a, e1), id1, "stable per engine");
    with_engine(id1, |s| s.negotiated_alpn = Some("h2".to_string()));
    assert_eq!(with_engine(id2, |s| s.negotiated_alpn.clone()), Some(None));
    engine_session_table()
        .lock()
        .insert((k1, SessionPhase::Negotiated), at(0xB26B_1100));
    engine_alpn_selector_table()
        .lock()
        .insert(k1, at(0xB26B_1200));
    assert!(!engine_session_table()
        .lock()
        .contains_key(&(k2, SessionPhase::Negotiated)));
    assert!(!engine_alpn_selector_table().lock().contains_key(&k2));
    // The same holds for a session's byte rows and a socket's.
    let (s1, s2) = (at(0x1_B26B_1300), at(0x2_B26B_1300));
    record_client_peer_chain(&a, s1, vec![vec![0xB2]]);
    assert!(existing_objref_key(&a, s2).is_none(), "s2 was never keyed");
    stash_sock_alpn(&a, s1, "h2".to_string());
    assert_eq!(lookup_sock_alpn(&a, s1).as_deref(), Some("h2"));
    assert_eq!(lookup_sock_alpn(&a, s2), None);
    // A lookup leaves no slot behind.
    assert!(existing_objref_key(&a, s2).is_none());
}

/// `createSSLEngine`'s mint point on a new engine that shares a LIVE
/// engine's identity hash leaves the live engine's rows alone.
#[test]
fn minting_a_new_engine_keeps_a_live_colliders_rows() {
    let _teardown = Teardown(&[VM_B]);
    let b = ctx_in(VM_B);
    let (live, fresh) = (at(0x1_B26B_2000), at(0x2_B26B_2000));
    let live_id = engine_id_or_alloc(&b, live);
    let live_key = engine_objref_key(&b, live);
    with_engine(live_id, |s| s.closed_outbound = true);
    engine_session_table()
        .lock()
        .insert((live_key, SessionPhase::Negotiated), at(0xB26B_2100));
    engine_alpn_selector_table()
        .lock()
        .insert(live_key, at(0xB26B_2200));

    set_engine_trust_roots_override(&b, fresh);
    let fresh_id = engine_id_or_alloc(&b, fresh);
    assert_ne!(fresh_id, live_id);
    assert_eq!(engine_id_or_alloc(&b, live), live_id);
    assert!(engine_registry().read().contains_key(&live_id));
    assert_eq!(with_engine(live_id, |s| s.closed_outbound), Some(true));
    assert_eq!(with_engine(fresh_id, |s| s.closed_outbound), Some(false));
    assert!(engine_session_table()
        .lock()
        .contains_key(&(live_key, SessionPhase::Negotiated)));
    assert!(engine_alpn_selector_table().lock().contains_key(&live_key));
}

/// A key the lock-key sweep frees (its object died) takes every row filed
/// under it, in every family; a live object's rows stay.
#[test]
fn a_freed_key_drops_its_rows() {
    let _teardown = Teardown(&[VM_C]);
    let c = ctx_in(VM_C);
    let (engine, sock, ses, params) = (
        at(0xB26B_3000),
        at(0xB26B_3100),
        at(0xB26B_3200),
        at(0xB26B_3300),
    );
    let live_sock = at(0xB26B_3400);
    let id = engine_id_or_alloc(&c, engine);
    let k_engine = engine_objref_key(&c, engine);
    engine_alpn_selector_table()
        .lock()
        .insert(k_engine, at(0xB26B_3010));
    stash_sock_alpn(&c, sock, "h2".to_string());
    stash_sss_enabled_protocols(&c, sock, vec!["TLSv1.3".to_string()]);
    set_ssl_server_socket_state(&c, sock, ssl_server_socket_state_miss());
    stash_sock_alpn(&c, live_sock, "http/1.1".to_string());
    forget_session_rows(&c, ses);
    record_client_peer_chain(&c, ses, vec![vec![0xB2, 0x6B]]);
    let k_params = engine_objref_key(&c, params);
    sslparams_alpn_table()
        .lock()
        .insert(k_params, vec!["h2".to_string()]);
    let (k_sock, k_ses) = (
        existing_objref_key(&c, sock).expect("filed"),
        existing_objref_key(&c, ses).expect("filed"),
    );

    // Only `live_sock` survives this VM's collection.
    let live = addr(live_sock);
    crate::gc_sweep_lock_keys(VM_C, &|x| x == live);

    assert!(
        existing_objref_key(&c, engine).is_none(),
        "the key was freed"
    );
    assert!(!engine_table().lock().contains_key(&k_engine));
    assert!(!engine_registry().read().contains_key(&id));
    assert!(!engine_alpn_selector_table().lock().contains_key(&k_engine));
    assert!(!sock_alpn_table().lock().contains_key(&k_sock));
    assert!(!sss_enabled_protocols_table().lock().contains_key(&k_sock));
    assert!(!ssl_server_socket_states().lock().contains_key(&k_sock));
    assert!(!session_peer_certs_table().lock().contains_key(&k_ses));
    assert!(!sslparams_alpn_table().lock().contains_key(&k_params));
    assert_eq!(lookup_sock_alpn(&c, live_sock).as_deref(), Some("http/1.1"));
}

/// Two VMs' objects at one address (and so one hash) never share a key;
/// one VM's teardown drops exactly its own rows.
#[test]
fn two_vms_stay_separate() {
    let _teardown = Teardown(&[VM_D, VM_E]);
    let (d, e) = (ctx_in(VM_D), ctx_in(VM_E));
    let sock = at(0x1_B26B_4000);
    stash_sock_alpn(&d, sock, "d".to_string());
    stash_sock_alpn(&e, sock, "e".to_string());
    let (kd, ke) = (
        existing_objref_key(&d, sock).expect("filed"),
        existing_objref_key(&e, sock).expect("filed"),
    );
    assert_ne!(kd, ke);
    assert!(objref_key_owned_by(kd, VM_D) && !objref_key_owned_by(kd, VM_E));
    assert_eq!(lookup_sock_alpn(&d, sock).as_deref(), Some("d"));
    assert_eq!(lookup_sock_alpn(&e, sock).as_deref(), Some("e"));

    forget_vm_tls_state(VM_D);
    crate::forget_vm_lock_keys(VM_D);
    assert!(!sock_alpn_table().lock().contains_key(&kd));
    assert!(!objref_key_owned_by(kd, VM_D));
    assert_eq!(lookup_sock_alpn(&e, sock).as_deref(), Some("e"));
}

/// `HttpsURLConnection.setSSLSocketFactory` is scoped to ONE connection:
/// a live connection with the same identity hash does not inherit it, and
/// the capture goes when its connection dies.
#[test]
fn a_connection_capture_is_its_connections_alone() {
    let _teardown = Teardown(&[VM_F]);
    let mut f = ctx_in(VM_F);
    let mut sslctx = at(0x3_B26B_5000);
    let key = ctx_obj_key(&mut f, sslctx).unwrap();
    ctx_trust_roots_table()
        .lock()
        .insert(key, TlsTrustRoots::default());
    let (conn, collider) = (at(0x1_B26B_5100), at(0x2_B26B_5100));
    capture_huc_ssl_context_for_connection(&mut f, conn, &mut sslctx).unwrap();
    assert!(huc_tls_for_connection(&f, conn).is_some());
    assert!(
        huc_tls_for_connection(&f, collider).is_none(),
        "a same-hash connection must not get another's instance policy"
    );
    // The connection dies: its capture goes with it.
    gc_sweep_tls_rows(VM_F, &|x| x != addr(conn));
    assert!(huc_tls_for_connection(&f, conn).is_none());
}
