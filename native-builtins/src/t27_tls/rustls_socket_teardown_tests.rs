use super::test_fixtures::*;
use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Register a loopback rustls listener the way `create_ssl_server_socket`
/// does, owned by `vm` (or by nobody), and answer its id and port.
fn listener_for(vm: Option<usize>) -> (i32, u16) {
    let config =
        build_server_config_single_cert(SERVER_CRT_PEM, SERVER_KEY_PEM, &[], false, None)
            .expect("server config");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let entry = TlsServerListenerEntry {
        listener,
        config: TlsServerConfig::Rustls(config),
        local_port: port,
        closed: Arc::new(AtomicBool::new(false)),
    };
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.listeners.insert(id, entry);
    if let Some(vm) = vm {
        reg.owners.insert(id, vm);
    }
    (id, port)
}

/// Two VMs each hold a listener; VM A also a stashed layered socket and
/// `SSLServerSocket` side rows. Tearing A down closes A's listener (its
/// port binds again, a parked accept loop sees `closed`), drops its
/// pending and side rows, and leaves B's rows and an unowned row alone.
/// A claim through `servlet::s2_claim_for_vm` with the offset id lands
/// here.
#[test]
fn forget_vm_rustls_sockets_closes_only_that_vms_rows() {
    const VM_A: usize = 0xB14_7A01;
    const VM_B: usize = 0xB14_7B01;
    let (lid_a, port_a) = listener_for(Some(VM_A));
    let (lid_b, _) = listener_for(Some(VM_B));
    let (lid_claimed, _) = listener_for(None);
    let (lid_unowned, _) = listener_for(None);
    crate::servlet::s2_claim_for_vm(VM_A, crate::servlet::RUSTLS_SOCK_ID_BASE + lid_claimed);
    let closed_a = sreg().lock().listeners[&lid_a].closed.clone();

    const PENDING_A: i32 = 0x0B14_7A01;
    const PENDING_B: i32 = 0x0B14_7B01;
    let pending = |vm: usize| PendingLayeredSocket {
        stream: PendingLayeredStream::DialFresh {
            host: "127.0.0.1".to_string(),
            port: 1,
        },
        extra_roots: Vec::new(),
        use_java_trust_manager: false,
        client_identity: None,
        enabled_ciphers: Vec::new(),
        enabled_protocols: Vec::new(),
        server_identity: None,
        host: "127.0.0.1".to_string(),
        client_mode: true,
        vm,
        sock_key: None,
    };
    pending_layered_sockets()
        .lock()
        .insert(PENDING_A, pending(VM_A));
    pending_layered_sockets()
        .lock()
        .insert(PENDING_B, pending(VM_B));
    // gc-common w26-b: the rows are keyed by the socket's weak lock key,
    // and teardown asks the key registry which VM owns it, so mint real
    // keys in each VM (and forget them at the end).
    let mut ctx_a = crate::test_utils::mock_ctx();
    ctx_a.set_vm_identity(VM_A);
    let mut ctx_b = crate::test_utils::mock_ctx();
    ctx_b.set_vm_identity(VM_B);
    let sock_a = ctx_a.fresh_object_ref();
    let sock_b = ctx_b.fresh_object_ref();
    let key_a = gc_stable_objref_key(&ctx_a, sock_a);
    let key_b = gc_stable_objref_key(&ctx_b, sock_b);
    sss_client_auth_states().lock().insert(key_a, (1, 0));
    sss_client_auth_states().lock().insert(key_b, (1, 0));

    {
        let reg = sreg().lock();
        assert_eq!(reg.owners.get(&lid_a), Some(&VM_A));
        assert_eq!(reg.owners.get(&lid_b), Some(&VM_B));
        assert_eq!(
            reg.owners.get(&lid_claimed),
            Some(&VM_A),
            "the claim landed"
        );
        assert_eq!(reg.owners.get(&lid_unowned), None);
    }

    let streams = forget_vm_rustls_sockets(VM_A);
    assert!(streams.is_empty(), "A had no streams: {streams:?}");

    {
        let reg = sreg().lock();
        assert!(!reg.listeners.contains_key(&lid_a), "A's listener survived");
        assert!(
            !reg.listeners.contains_key(&lid_claimed),
            "the claimed listener survived"
        );
        assert!(
            !reg.owners.values().any(|v| *v == VM_A),
            "A's owner rows survived"
        );
        assert!(
            reg.listeners.contains_key(&lid_b),
            "B's listener was closed"
        );
        assert!(
            reg.listeners.contains_key(&lid_unowned),
            "an unowned listener was closed"
        );
    }
    assert!(
        closed_a.load(Ordering::SeqCst),
        "a parked accept must see the close"
    );
    assert!(!pending_layered_sockets().lock().contains_key(&PENDING_A));
    assert!(pending_layered_sockets().lock().contains_key(&PENDING_B));
    assert!(!sss_client_auth_states().lock().contains_key(&key_a));
    assert!(sss_client_auth_states().lock().contains_key(&key_b));
    // The port is released.
    TcpListener::bind(("127.0.0.1", port_a)).expect("A's port must be free after teardown");

    forget_vm_rustls_sockets(VM_B);
    assert!(!sreg().lock().listeners.contains_key(&lid_b));
    assert!(!pending_layered_sockets().lock().contains_key(&PENDING_B));
    assert!(!sss_client_auth_states().lock().contains_key(&key_b));
    rustls_listener_close(lid_unowned);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// An accepted stream inherits its listener's VM, a client stream is
/// claimed through the offset id, and teardown closes both and answers
/// their ids (for the caller's read-ahead). A repeated close of a stale
/// id leaves a listener that has since taken that id alone.
#[test]
fn accepted_and_claimed_streams_go_with_their_vm() {
    const VM: usize = 0xB14_7C01;
    let (lid, port) = listener_for(Some(VM));
    let acceptor = std::thread::spawn(move || {
        rustls_server_accept_within(lid, Some(std::time::Duration::from_secs(20)))
    });
    let mut roots = RootCertStore::empty();
    for c in parse_cert_chain_pem(CA_CRT_PEM).expect("ca") {
        roots.add(c).expect("root");
    }
    let client_config = build_client_config(roots, &[], None).expect("client config");
    let cid = rustls_client_connect(client_config, "localhost", port).expect("client connect");
    let sid = match acceptor.join().expect("acceptor thread") {
        Ok(id) => id,
        Err(AcceptFailure::TimedOut) => panic!("accept timed out"),
        Err(AcceptFailure::Failed(e)) => panic!("accept failed: {e}"),
    };
    {
        let reg = sreg().lock();
        assert_eq!(
            reg.owners.get(&sid),
            Some(&VM),
            "the accepted stream inherits"
        );
        assert_eq!(reg.owners.get(&cid), None, "a client stream starts unowned");
    }
    crate::servlet::s2_claim_for_vm(VM, crate::servlet::RUSTLS_SOCK_ID_BASE + cid);
    assert_eq!(sreg().lock().owners.get(&cid), Some(&VM));

    let mut streams = forget_vm_rustls_sockets(VM);
    streams.sort_unstable();
    let mut expected = vec![cid, sid];
    expected.sort_unstable();
    assert_eq!(streams, expected);
    {
        let reg = sreg().lock();
        assert!(!reg.listeners.contains_key(&lid));
        assert!(!reg.client_streams.contains_key(&cid));
        assert!(!reg.server_streams.contains_key(&sid));
        assert!(!reg.owners.values().any(|v| *v == VM));
    }

    // A stale close must not unown a listener that now holds the id.
    let (lid2, _) = listener_for(Some(VM));
    rustls_stream_close(lid2);
    assert_eq!(sreg().lock().owners.get(&lid2), Some(&VM));
    rustls_listener_close(lid2);
    assert!(!sreg().lock().owners.contains_key(&lid2));
}
