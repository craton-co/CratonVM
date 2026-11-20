use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_COLLIDE: usize = 0x27A0_0001;
const VM_FREED: usize = 0x27A0_0002;
const VM_A: usize = 0x27A0_0003;
const VM_B: usize = 0x27A0_0004;
const VM_LSN: usize = 0x27A0_0005;

struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            forget_vm_parked_tls_sockets(vm);
            crate::t27_tls::forget_vm_tls_state(vm);
            crate::forget_vm_lock_keys(vm);
        }
    }
}

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the lock-key registry and the mock hash the
    // address and never dereference it. Every address ends in 0.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

fn ctx_in(vm: usize) -> MockNativeContext {
    let c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    c
}

fn mac(algo: &str) -> MacState {
    MacState {
        algo: algo.to_string(),
        key: vec![1],
        data: Vec::new(),
        initialized: true,
    }
}

/// Two live objects of one VM with one identity hash keep separate rows
/// in every table family: `Mac` state, `SSLSocket` client-auth /
/// endpoint-algorithm / staged trust scope, the factory rows and the
/// initialised mark, and the p68 anchors.
#[test]
fn two_live_same_hash_objects_keep_separate_rows() {
    let _teardown = Teardown(&[VM_COLLIDE]);
    let c = ctx_in(VM_COLLIDE);
    let (x, y) = (obj(0x0027_A100), obj(0x1_0027_A100));
    assert_eq!(c.identity_hash_code(x), c.identity_hash_code(y), "premise");
    let (kx, ky) = (ssl_obj_key(&c, x), ssl_obj_key(&c, y));
    assert_ne!(kx, ky);
    assert_eq!(ssl_obj_key(&c, x), kx, "a key is stable per object");

    // `Mac` state.
    assert_eq!((mac_state_key(&c, x), mac_state_key(&c, y)), (kx, ky));
    mac_state_table()
        .lock()
        .unwrap()
        .insert(kx, mac("HmacSHA256"));
    mac_state_table()
        .lock()
        .unwrap()
        .insert(ky, mac("HmacSHA1"));
    let algo = |o: ObjectRef| {
        let k = mac_state_key(&c, o);
        mac_state_table()
            .lock()
            .unwrap()
            .get(&k)
            .map(|s| s.algo.clone())
    };
    assert_eq!(algo(x).as_deref(), Some("HmacSHA256"));
    assert_eq!(algo(y).as_deref(), Some("HmacSHA1"));

    // `SSLSocket` rows.
    ssl_sock_auth_update(&c, x, |s| s.1 = 1);
    assert_eq!(ssl_sock_auth_get(&c, x), (0, 1, 0));
    assert_eq!(
        ssl_sock_auth_get(&c, y),
        (0, 0, 0),
        "y read x's client-auth row"
    );
    assert_eq!((ssl_sock_key(&c, x), ssl_sock_key(&c, y)), (kx, ky));
    ssl_sock_endpoint_alg()
        .lock()
        .insert(kx, "HTTPS".to_string());
    let alg_y = ssl_sock_endpoint_alg().lock().get(&ky).cloned();
    assert_eq!(alg_y, None);
    stash_pending_ssl_socket_connect_ctx(&c, x, vec![b"x".to_vec()], Some(1), None);
    stash_pending_ssl_socket_connect_ctx(&c, y, vec![b"y".to_vec()], Some(2), None);
    let (roots_x, tm_x, _, _) = take_pending_ssl_socket_connect_ctx(&c, x);
    let (roots_y, tm_y, _, _) = take_pending_ssl_socket_connect_ctx(&c, y);
    assert_eq!(
        (roots_x, tm_x),
        (vec![b"x".to_vec()], Some(1)),
        "x got y's trust scope"
    );
    assert_eq!((roots_y, tm_y), (vec![b"y".to_vec()], Some(2)));

    // Factory rows and the initialised gate.
    kmf_keystore_id_by_identity().lock().insert(kx, 5);
    kmf_key_manager_cache().lock().insert(kx, (5, 50));
    tmf_tm_id_by_identity().lock().insert(ky, 60);
    let ks_y = kmf_keystore_id_by_identity().lock().get(&ky).copied();
    assert_eq!(ks_y, None, "y resolved x's keystore");
    let tm_x = tmf_tm_id_by_identity().lock().get(&kx).copied();
    assert_eq!(tm_x, None, "x resolved y's trust manager");
    mark_jsse_factory_initialized(&c, x);
    assert!(require_jsse_factory_initialized(&c, x, "KeyManagerFactoryImpl").is_ok());
    assert!(
        require_jsse_factory_initialized(&c, y, "KeyManagerFactoryImpl").is_err(),
        "y passed the gate on x's init"
    );

    // p68 anchors.
    assert_ne!(p68_trust_roots_key(&c, x), p68_trust_roots_key(&c, y));
}

/// The collection that finds one of two colliders dead drops exactly its
/// rows -- through `lib.rs::gc_sweep_lock_keys`, so the hook's wiring is
/// covered too -- and offers the manager ids its factory rows named.
#[test]
fn a_freed_key_drops_its_rows_and_only_its_rows() {
    let _teardown = Teardown(&[VM_FREED]);
    let c = ctx_in(VM_FREED);
    let (dead, live) = (obj(0x0027_B100), obj(0x1_0027_B100));
    let (kd, kl) = (ssl_obj_key(&c, dead), ssl_obj_key(&c, live));
    for (o, k) in [(dead, kd), (live, kl)] {
        mac_state_table()
            .lock()
            .unwrap()
            .insert(k, mac("HmacSHA256"));
        ssl_sock_auth_update(&c, o, |s| s.2 = 1);
        ssl_sock_endpoint_alg()
            .lock()
            .insert(k, "HTTPS".to_string());
        stash_pending_ssl_socket_connect_ctx(&c, o, vec![b"r".to_vec()], None, None);
        kmf_keystore_id_by_identity().lock().insert(k, 5);
        kmf_live_km_id_by_identity().lock().insert(k, 51);
        tmf_tm_id_by_identity().lock().insert(k, 61);
        mark_jsse_factory_initialized(&c, o);
        p68_ctx_trust_roots_table()
            .lock()
            .insert(k, vec![b"a".to_vec()]);
        ssl_layered_socket_owners().lock().insert(k);
    }
    let parked = |sock_key: u64| PendingConnectSocket {
        host: "localhost".into(),
        port: 1,
        extra_roots: Vec::new(),
        java_tm_key: None,
        max_protocol: None,
        client_identity: None,
        tcp: None,
        vm: VM_FREED,
        sock_key,
    };
    let (pid_dead, pid_live) = (
        stash_pending_connect_socket(parked(kd.1)),
        stash_pending_connect_socket(parked(kl.1)),
    );
    handshake_listeners()
        .lock()
        .insert(kd, vec![(7, 0x27B1), (8, 0x27B2)]);

    let live_addr = addr(live);
    crate::gc_sweep_lock_keys(VM_FREED, &|x| x == live_addr);

    let present = |k: SslObjKey| {
        let mac = mac_state_table().lock().unwrap().contains_key(&k);
        let auth = ssl_sock_auth_state().lock().contains_key(&k);
        let alg = ssl_sock_endpoint_alg().lock().contains_key(&k);
        let staged = pending_ssl_socket_connect_ctx_table()
            .lock()
            .contains_key(&k);
        let ks = kmf_keystore_id_by_identity().lock().contains_key(&k);
        let live_km = kmf_live_km_id_by_identity().lock().contains_key(&k);
        let tm = tmf_tm_id_by_identity().lock().contains_key(&k);
        let init = jsse_factory_initialized().lock().contains(&k);
        let p68 = p68_ctx_trust_roots_table().lock().contains_key(&k);
        let layered = ssl_layered_socket_owners().lock().contains(&k);
        [mac, auth, alg, staged, ks, live_km, tm, init, p68, layered]
    };
    assert_eq!(present(kd), [false; 10], "the dead object kept a row");
    assert_eq!(present(kl), [true; 10], "the live collider lost a row");
    assert!(
        take_pending_connect_socket(pid_dead).is_none(),
        "the dead socket's parked row"
    );
    assert!(
        take_pending_connect_socket(pid_live).is_some(),
        "the live socket's parked row"
    );
    assert!(!handshake_listeners().lock().contains_key(&kd));
    let queued = dead_socket_listener_roots().lock().get(&VM_FREED).cloned();
    assert_eq!(
        queued,
        Some(vec![0x27B1, 0x27B2]),
        "the dead socket's listener roots"
    );
    // The queue is dropped by the teardown (`forget_vm_tls_state`). The
    // dead rows' manager ids (51, 61) were offered; the VM's next TLS
    // sweep keeps them, because the live collider's rows still name them.
    crate::t27_tls::gc_sweep_tls_rows(VM_FREED, &|x| x == live_addr);
    assert!(factory_rows_name_manager_id(VM_FREED, true, 51));
    assert!(factory_rows_name_manager_id(VM_FREED, false, 61));
}

/// Two VMs that key an object at one address stay separate: VM A's
/// lock-key teardown (`forget_vm_lock_keys`, which fans the freed keys
/// out to the hook) and its TLS teardown take only A's rows.
#[test]
fn two_vms_stay_separate() {
    let _teardown = Teardown(&[VM_A, VM_B]);
    let (a, b) = (ctx_in(VM_A), ctx_in(VM_B));
    let o = obj(0x0027_C100);
    let (ka, kb) = (ssl_obj_key(&a, o), ssl_obj_key(&b, o));
    assert_ne!(ka, kb);
    for (c, k, algo) in [(&a, ka, "HmacSHA256"), (&b, kb, "HmacSHA1")] {
        mac_state_table().lock().unwrap().insert(k, mac(algo));
        ssl_sock_auth_update(c, o, |s| s.0 = 1);
        kmf_keystore_id_by_identity().lock().insert(k, 9);
    }
    assert_eq!(ssl_sock_auth_get(&b, o), (1, 0, 0));

    crate::forget_vm_lock_keys(VM_A);
    assert!(!mac_state_table().lock().unwrap().contains_key(&ka));
    assert!(!ssl_sock_auth_state().lock().contains_key(&ka));
    assert!(!kmf_keystore_id_by_identity().lock().contains_key(&ka));
    let algo_b = mac_state_table()
        .lock()
        .unwrap()
        .get(&kb)
        .map(|s| s.algo.clone());
    assert_eq!(
        algo_b.as_deref(),
        Some("HmacSHA1"),
        "A's teardown took B's Mac"
    );
    assert_eq!(
        ssl_sock_auth_get(&b, o),
        (1, 0, 0),
        "A's teardown took B's socket row"
    );
    assert_eq!(
        kmf_keystore_id_by_identity().lock().get(&kb).copied(),
        Some(9)
    );

    forget_vm_factory_identity_rows(VM_B);
    assert!(!mac_state_table().lock().unwrap().contains_key(&kb));
    assert!(!kmf_keystore_id_by_identity().lock().contains_key(&kb));
}

/// `add` / `removeHandshakeCompletedListener` with two listeners that
/// share an identity hash: both are registered (the second used to be
/// taken for a duplicate, its root released), and removing one leaves
/// the other registered and rooted.
#[test]
fn same_hash_listeners_are_two_listeners() {
    let _teardown = Teardown(&[VM_LSN]);
    let mut registry = NativeMethodRegistry::new();
    register_p68_ssl(&mut registry);
    let find = |name: &str| {
        registry
            .find(
                "javax/net/ssl/SSLSocket",
                name,
                "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
            )
            .expect("SSLSocket listener native should be registered")
    };
    let (add, remove) = (
        find("addHandshakeCompletedListener"),
        find("removeHandshakeCompletedListener"),
    );
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_LSN);
    let sock = ctx.fresh_object_ref();
    let (l1, l2) = (obj(0x0027_D100), obj(0x1_0027_D100));
    assert_eq!(
        ctx.identity_hash_code(l1),
        ctx.identity_hash_code(l2),
        "premise"
    );
    let base = ctx.global_root_count();
    for l in [l1, l2] {
        add(
            &mut ctx,
            &[Value::Object(Some(sock)), Value::Object(Some(l))],
        )
        .unwrap();
    }
    assert_eq!(
        ctx.global_root_count(),
        base + 2,
        "the second listener was dropped"
    );
    // Re-adding the same listener is still idempotent.
    add(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(l1))],
    )
    .unwrap();
    assert_eq!(ctx.global_root_count(), base + 2);

    remove(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(l1))],
    )
    .unwrap();
    assert_eq!(ctx.global_root_count(), base + 1);
    let sock_key = ssl_sock_key(&ctx, sock);
    let rows = handshake_listeners().lock().get(&sock_key).map(|e| e.len());
    assert_eq!(rows, Some(1), "removing l1 removed l2");
    remove(
        &mut ctx,
        &[Value::Object(Some(sock)), Value::Object(Some(l2))],
    )
    .unwrap();
    assert_eq!(ctx.global_root_count(), base);
    assert!(
        remove(
            &mut ctx,
            &[Value::Object(Some(sock)), Value::Object(Some(l2))]
        )
        .is_err(),
        "a listener removed twice"
    );
}
