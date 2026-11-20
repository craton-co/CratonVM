use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const VM_STAGE: usize = 0xD16_0101;
const VM_PARK: usize = 0xD16_0201;
const VM_PARK_OTHER: usize = 0xD16_0202;
const VM_LSN: usize = 0xD16_0301;

/// Drops every process-wide row the test's VMs left, on every exit path.
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

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// The zero-arg `createSocket()` staged a trust scope (and a context's
/// TrustManager key) for a socket that is never connected. While the
/// socket lives the row stays; the collection that finds it dead drops
/// it, so the parked-key scan stops pinning the context's rows.
/// (gc-common w27-a: the lock-key sweep, by the socket's weak lock key.)
#[test]
fn w16d_a_dead_unconnected_socket_takes_its_staged_scope() {
    let _teardown = Teardown(&[VM_STAGE]);
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_STAGE);
    let (sock, live) = (c.fresh_object_ref(), c.fresh_object_ref());
    // The staging key is the socket key.
    assert_eq!(pending_ssl_connect_key(&c, sock), ssl_sock_key(&c, sock));
    const TM_KEY: u64 = 0x0D16_0101_0D16_0101;
    stash_pending_ssl_socket_connect_ctx(&c, sock, vec![b"dead".to_vec()], Some(TM_KEY), None);
    stash_pending_ssl_socket_connect_ctx(&c, live, vec![b"live".to_vec()], None, None);
    assert!(parked_sockets_name_ctx_key(TM_KEY));
    let dead_key = ssl_sock_key(&c, sock);

    let (sock_addr, live_addr) = (addr(sock), addr(live));
    crate::gc_sweep_lock_keys(VM_STAGE, &|x| x == sock_addr || x == live_addr);
    assert!(
        parked_sockets_name_ctx_key(TM_KEY),
        "a live socket lost its staged scope"
    );

    crate::gc_sweep_lock_keys(VM_STAGE, &|x| x == live_addr);
    assert!(
        !parked_sockets_name_ctx_key(TM_KEY),
        "the dead socket still pins its context's TrustManager key"
    );
    assert!(!pending_ssl_socket_connect_ctx_table()
        .lock()
        .contains_key(&dead_key));
    assert_eq!(
        take_pending_ssl_socket_connect_ctx(&c, live).0,
        vec![b"live".to_vec()],
        "the live socket's scope must survive"
    );
}

/// `connect()` parked a real, established connection; the socket dies
/// before its first I/O. The collection that finds it dead drops the row
/// and closes the stream (the peer reads end-of-stream), and another VM's
/// parked row is left alone.
#[test]
fn w16d_a_dead_connected_socket_closes_its_parked_connection() {
    use std::io::Read as _;
    let _teardown = Teardown(&[VM_PARK, VM_PARK_OTHER]);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let (mut peer, _) = listener.accept().expect("accept");
    peer.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("timeout");

    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(VM_PARK);
    let sock = c.fresh_object_ref();
    // What `new13_ssl_socket_connect` records on entry.
    let sock_key = ssl_sock_key(&c, sock).1;
    let parked =
        |vm: usize, sock_key: u64, tcp: Option<std::net::TcpStream>| PendingConnectSocket {
            host: "127.0.0.1".into(),
            port,
            extra_roots: Vec::new(),
            java_tm_key: None,
            max_protocol: None,
            client_identity: None,
            tcp,
            vm,
            sock_key,
        };
    let pid = stash_pending_connect_socket(parked(VM_PARK, sock_key, Some(client)));
    // Another VM's row under the same key NUMBER: the hook matches the VM
    // too (a key belongs to one VM, but a row names its VM).
    let other = stash_pending_connect_socket(parked(VM_PARK_OTHER, sock_key, None));

    // Alive: nothing moves.
    let sock_addr = addr(sock);
    crate::gc_sweep_lock_keys(VM_PARK, &|x| x == sock_addr);
    assert!(pending_connect_sockets().lock().contains_key(&pid));

    // Dead: the row goes and the connection is closed.
    crate::gc_sweep_lock_keys(VM_PARK, &|_| false);
    assert!(
        take_pending_connect_socket(pid).is_none(),
        "the dead socket's row survived"
    );
    let mut buf = [0u8; 8];
    match peer.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("unexpected {n} bytes from a parked, unused connection"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            panic!("the parked connection of a dead socket is still open")
        }
        // Reset / aborted: closed too.
        Err(_) => {}
    }
    assert!(
        take_pending_connect_socket(other).is_some(),
        "another VM's parked socket was dropped"
    );
}

/// Listeners registered on a socket that dies without `close()`: the row
/// goes with the socket, the roots are queued, and the VM's next listener
/// native releases them. A live socket's listener is untouched.
#[test]
fn w16d_a_dead_sockets_listeners_stop_being_roots() {
    let _teardown = Teardown(&[VM_LSN]);
    let mut registry = NativeMethodRegistry::new();
    register_p68_ssl(&mut registry);
    let add = registry
        .find(
            "javax/net/ssl/SSLSocket",
            "addHandshakeCompletedListener",
            "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
        )
        .expect("SSLSocket.addHandshakeCompletedListener native should be registered");
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_LSN);
    let (dead_sock, live_sock) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let (l1, l2, l3, l4) = (
        ctx.fresh_object_ref(),
        ctx.fresh_object_ref(),
        ctx.fresh_object_ref(),
        ctx.fresh_object_ref(),
    );
    let base = ctx.global_root_count();
    for (sock, listener) in [(dead_sock, l1), (dead_sock, l2), (live_sock, l3)] {
        add(
            &mut ctx,
            &[Value::Object(Some(sock)), Value::Object(Some(listener))],
        )
        .unwrap();
    }
    assert_eq!(ctx.global_root_count(), base + 3);

    // Taken before the sweep (minting after it would answer a fresh key).
    let (dead_key, live_key) = (ssl_sock_key(&ctx, dead_sock), ssl_sock_key(&ctx, live_sock));
    // The listeners are global roots, so they survive; only the socket
    // dies (gc-common w27-a: the lock-key sweep drops its row).
    let live: Vec<usize> = [live_sock, l1, l2, l3].iter().map(|o| addr(*o)).collect();
    crate::gc_sweep_lock_keys(VM_LSN, &|x| live.contains(&x));
    assert!(!handshake_listeners().lock().contains_key(&dead_key));
    assert!(handshake_listeners().lock().contains_key(&live_key));
    assert_eq!(
        ctx.global_root_count(),
        base + 3,
        "a sweep has no ctx: the roots wait for the next native"
    );

    add(
        &mut ctx,
        &[Value::Object(Some(live_sock)), Value::Object(Some(l4))],
    )
    .unwrap();
    assert_eq!(ctx.global_root_count(), base + 2, "l3 and l4 only");
    assert!(dead_socket_listener_roots().lock().get(&VM_LSN).is_none());
}

fn mac_row() -> MacState {
    MacState {
        algo: "HmacSHA256".to_string(),
        key: vec![7],
        data: Vec::new(),
        initialized: true,
    }
}

/// At the cap, `getInstance` evicts only the calling VM's rows: a busy
/// VM no longer evicts another VM's live `Mac` state (whose keys are
/// numerically lower), and a VM with no rows of its own evicts nothing.
/// A local table: no process-wide state.
#[test]
fn w16d_mac_eviction_stays_inside_the_calling_vm() {
    const QUIET: usize = 0x1;
    const BUSY: usize = 0x2;
    const NEWCOMER: usize = 0x3;
    // gc-common w27-a: `(VM, lock key)` keys; the numbers stand in for
    // lock keys.
    let key = |vm: usize, n: u64| -> SslObjKey { (vm, n) };
    let mut t: std::collections::HashMap<SslObjKey, MacState> =
        std::collections::HashMap::new();
    for i in 0..MAC_STATE_MAX_ENTRIES as u64 {
        t.insert(key(QUIET, i + 1), mac_row());
        t.insert(key(BUSY, i + 1), mac_row());
    }
    mac_state_evict_for_vm_if_needed(&mut t, key(NEWCOMER, 1));
    assert_eq!(
        t.len(),
        2 * MAC_STATE_MAX_ENTRIES,
        "a VM under the cap evicted"
    );

    let fresh = key(BUSY, MAC_STATE_MAX_ENTRIES as u64 + 1);
    mac_state_evict_for_vm_if_needed(&mut t, fresh);
    let quiet_rows = t.keys().filter(|k| k.0 == QUIET).count();
    let busy_rows = t.keys().filter(|k| k.0 == BUSY).count();
    assert_eq!(quiet_rows, MAC_STATE_MAX_ENTRIES, "the quiet VM lost rows");
    assert!(busy_rows <= MAC_STATE_MAX_ENTRIES / 2, "{busy_rows}");
}
