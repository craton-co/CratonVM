use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::Read as _;

/// The owner of the sockets the older tests create. No test tears this
/// identity down; the teardown tests below use identities of their own,
/// because the registry is shared by every test in this binary and they
/// run in parallel.
const TEST_VM: usize = 0xB14_5000;

/// Helper: allocate a socket directly through `Socket::new` and stash it
/// in our registry, returning the assigned id. Exercises the same code
/// path `socketCreate` would take, without needing a NativeContext.
fn fresh_stream_socket() -> i32 {
    let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    register_socket(
        TEST_VM,
        SocketState {
            socket: s,
            so_timeout_ms: None,
            is_listening: false,
            is_connected: false,
            is_stream: true,
            closed: false,
        },
    )
}

#[test]
fn socket_registry_basic_roundtrip() {
    let id = fresh_stream_socket();
    assert!(id > 0);
    with_socket::<_, ()>(id, |s| {
        assert!(s.is_stream);
        assert!(!s.closed);
        assert!(!s.is_listening);
        Ok(())
    })
    .unwrap();
    drop_socket(id);
    let err = with_socket::<_, ()>(id, |_| Ok(())).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("not found"), "got {msg}");
}

#[test]
fn options_roundtrip_tcp_nodelay_and_keepalive() {
    let id = fresh_stream_socket();
    // TCP_NODELAY on
    with_socket::<_, ()>(id, |s| {
        s.socket.set_nodelay(true).unwrap();
        assert!(s.socket.nodelay().unwrap());
        // SO_KEEPALIVE on
        s.socket.set_keepalive(true).unwrap();
        assert!(s.socket.keepalive().unwrap());
        // SO_REUSEADDR on
        s.socket.set_reuse_address(true).unwrap();
        assert!(s.socket.reuse_address().unwrap());
        Ok(())
    })
    .unwrap();
    drop_socket(id);
}

#[test]
fn end_to_end_listen_and_accept_on_loopback() {
    // Bind a server on 127.0.0.1:0, set SO_REUSEADDR + listen(50),
    // connect a client, send 5 bytes, accept, read, assert equality.
    let server = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    server.set_reuse_address(true).unwrap();
    server
        .bind(&SockAddr::from(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            0,
        ))))
        .unwrap();
    server.listen(50).unwrap();
    let bound = server.local_addr().unwrap().as_socket().unwrap();

    let server_id = register_socket(
        TEST_VM,
        SocketState {
            socket: server,
            so_timeout_ms: None,
            is_listening: true,
            is_connected: false,
            is_stream: true,
            closed: false,
        },
    );

    let server_thread = std::thread::spawn(move || {
        let g = registry().read();
        let s = g.sockets.get(&server_id).unwrap();
        let (mut accepted, _peer) = s.socket.accept().unwrap();
        drop(g);
        // socket2 returns its own Socket for `accept`; convert to a
        // Read-capable handle.
        let mut buf = [0u8; 5];
        accepted.read_exact(&mut buf).unwrap();
        buf
    });

    let mut client = std::net::TcpStream::connect(bound).unwrap();
    client.write_all(b"hello").unwrap();
    client.flush().unwrap();

    let bytes = server_thread.join().unwrap();
    assert_eq!(&bytes, b"hello");
    drop_socket(server_id);
}

#[test]
fn so_linger_off_returns_minus_one() {
    let id = fresh_stream_socket();
    let v = with_socket(id, |s| {
        s.socket.set_linger(None).unwrap();
        get_option(s, SO_LINGER)
    })
    .unwrap();
    assert_eq!(v, Some(Value::Int(-1)));
    drop_socket(id);
}

#[test]
fn so_timeout_persists_in_state() {
    let id = fresh_stream_socket();
    with_socket::<_, ()>(id, |s| {
        s.so_timeout_ms = Some(750);
        s.socket
            .set_read_timeout(Some(Duration::from_millis(750)))
            .unwrap();
        Ok(())
    })
    .unwrap();
    let v = with_socket(id, |s| get_option(s, SO_TIMEOUT)).unwrap();
    assert_eq!(v, Some(Value::Int(750)));
    drop_socket(id);
}

#[test]
fn close_marks_state_and_subsequent_call_errors() {
    let id = fresh_stream_socket();
    // Mark closed by hand (mirroring what socket_close0 does).
    {
        let mut g = registry().write();
        let s = g.sockets.get_mut(&id).unwrap();
        s.closed = true;
        let _ = s.socket.shutdown(std::net::Shutdown::Both);
    }
    let err = with_socket(id, |s| get_option(s, TCP_NODELAY)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("Socket closed"), "got {msg}");
    drop_socket(id);
}

#[test]
fn unknown_option_rejected() {
    let id = fresh_stream_socket();
    let err = with_socket(id, |s| get_option(s, 0xdead_beefu32 as i32)).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("unknown cmd"), "got {msg}");
    drop_socket(id);
}

/// Regression for the P1 lock-contention bug: a blocking `accept()` must
/// NOT hold the registry lock for its duration. We reproduce the shape of
/// the (no-SO_TIMEOUT) accept path — clone the listener out under a short
/// lock, then block on the clone — and assert that a concurrent
/// `with_socket` write on another socket succeeds *while* the accept is
/// still blocked waiting for a connection. Before the fix the read guard
/// was held across `accept()`, so this `with_socket` would block until a
/// client connected (or forever).
#[test]
fn blocking_accept_does_not_hold_registry_lock() {
    use std::sync::mpsc;

    // Server listener registered like socketCreate/Bind/Listen would.
    let server = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    server.set_reuse_address(true).unwrap();
    server
        .bind(&SockAddr::from(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            0,
        ))))
        .unwrap();
    server.listen(50).unwrap();
    let bound = server.local_addr().unwrap().as_socket().unwrap();
    let server_id = register_socket(
        TEST_VM,
        SocketState {
            socket: server,
            so_timeout_ms: None,
            is_listening: true,
            is_connected: false,
            is_stream: true,
            closed: false,
        },
    );

    // A second, unrelated socket whose registry entry we'll touch while
    // the accept is blocked.
    let other_id = fresh_stream_socket();

    let (started_tx, started_rx) = mpsc::channel::<()>();
    let accept_thread = std::thread::spawn(move || {
        // Mirror socket_accept's no-timeout path: clone out under a short
        // lock, release, then block on the clone.
        let listener = {
            let g = registry().read();
            let s = g.sockets.get(&server_id).unwrap();
            s.socket.try_clone().unwrap()
        };
        started_tx.send(()).unwrap();
        let (_accepted, _peer) = listener.accept().unwrap();
    });

    // Wait until the accept thread has cloned-out and is about to block.
    started_rx.recv().unwrap();

    // While the accept blocks, a write-locking registry op on the OTHER
    // socket must complete promptly (it would deadlock under the old code).
    with_socket::<_, ()>(other_id, |s| {
        s.is_connected = true;
        Ok(())
    })
    .unwrap();

    // Now unblock the accept so the thread can finish.
    let mut client = std::net::TcpStream::connect(bound).unwrap();
    client.write_all(b"x").unwrap();
    client.flush().unwrap();
    accept_thread.join().unwrap();

    drop_socket(server_id);
    drop_socket(other_id);
}

/// A bound, listening loopback socket, registered for `vm`, and its port.
fn listening_socket_for(vm: usize) -> (i32, u16) {
    let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    let addr = SockAddr::from(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
    s.bind(&addr).unwrap();
    s.listen(8).unwrap();
    let port = s.local_addr().unwrap().as_socket().unwrap().port();
    let id = register_socket(
        vm,
        SocketState {
            socket: s,
            so_timeout_ms: None,
            is_listening: true,
            is_connected: false,
            is_stream: true,
            closed: false,
        },
    );
    (id, port)
}

/// gc-common w14-b (`common-w13d-rustls-and-plain-socket-registries-outlive-their-vm`):
/// tearing VM A down closes A's sockets (its port can be bound again) and
/// leaves VM B's alone; a close drops the owner row with the socket.
#[test]
fn forget_vm_plain_sockets_closes_only_that_vms_rows() {
    const VM_A: usize = 0xB14_5A01;
    const VM_B: usize = 0xB14_5B01;
    let (a_listener, port_a) = listening_socket_for(VM_A);
    let a_plain = {
        let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        register_socket(
            VM_A,
            SocketState {
                socket: s,
                so_timeout_ms: None,
                is_listening: false,
                is_connected: false,
                is_stream: true,
                closed: false,
            },
        )
    };
    let (b_listener, _) = listening_socket_for(VM_B);
    {
        let g = registry().read();
        assert_eq!(g.owners.get(&a_listener), Some(&VM_A));
        assert_eq!(g.owners.get(&a_plain), Some(&VM_A));
        assert_eq!(g.owners.get(&b_listener), Some(&VM_B));
    }

    forget_vm_plain_sockets(VM_A);

    {
        let g = registry().read();
        assert!(
            !g.sockets.contains_key(&a_listener),
            "A's listener survived"
        );
        assert!(!g.sockets.contains_key(&a_plain), "A's socket survived");
        assert!(
            !g.owners.values().any(|v| *v == VM_A),
            "A's owner rows survived"
        );
        assert!(
            g.sockets.contains_key(&b_listener),
            "B's listener was closed"
        );
    }
    // The port is released.
    std::net::TcpListener::bind(("127.0.0.1", port_a))
        .expect("A's port must be free after teardown");

    // A close removes the owner row together with the socket.
    drop_socket(b_listener);
    assert!(!registry().read().owners.contains_key(&b_listener));
    forget_vm_plain_sockets(VM_B);
}

/// The id counter used to be taken unchecked, so after a wrap a new socket
/// could be handed the id of one still open, and the insert closed it.
#[test]
fn next_free_id_skips_zero_and_live_rows() {
    let mut g = SocketRegistry {
        next_id: i32::MAX,
        ..SocketRegistry::default()
    };
    let live = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    g.sockets.insert(
        1,
        SocketState {
            socket: live,
            so_timeout_ms: None,
            is_listening: false,
            is_connected: false,
            is_stream: true,
            closed: false,
        },
    );
    assert_eq!(next_free_id(&mut g), i32::MAX);
    // The counter wrapped; 0 is reserved and 1 is live.
    assert_eq!(next_free_id(&mut g), 2);
    g.next_id = 0;
    assert_eq!(next_free_id(&mut g), 2, "0 is never minted and 1 is live");
}
