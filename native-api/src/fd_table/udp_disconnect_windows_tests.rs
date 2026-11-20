use super::FileDescriptorTable as FdTable;

const PEER: &str = "127.0.0.1:9"; // discard; UDP connect needs no listener

fn connect_then_disconnect(t: &FdTable, fd: super::FdId, what: &str) {
    t.udp_connect(fd, PEER)
        .unwrap_or_else(|e| panic!("{what}: connect failed: {e}"));
    assert!(
        t.udp_peer_addr(fd).is_ok(),
        "{what}: precondition -- connected sockets report a peer"
    );
    t.udp_disconnect(fd)
        .unwrap_or_else(|e| panic!("{what}: disconnect failed: {e}"));
    assert!(
        t.udp_peer_addr(fd).is_err(),
        "{what}: after disconnect the OS must report NO peer, got {:?}",
        t.udp_peer_addr(fd)
    );
}

/// The DatagramChannel shape: dual-stack IPv6. The old 16-byte
/// `AF_UNSPEC` sockaddr was too short for it, and Winsock refused with
/// WSAEFAULT (10014).
#[test]
fn a_dual_stack_socket_actually_disconnects() {
    let t = FdTable::new();
    let fd = t.open_udp_dual_stack().expect("dual-stack UDP socket");
    connect_then_disconnect(&t, fd, "dual-stack");
}

/// A plain IPv4 socket. A REGRESSION GUARD, not a caught bug: the old
/// `AF_UNSPEC` form already disconnected IPv4 correctly (measured -- this
/// test passes against it), and IPv4 now takes an explicit `AF_INET` zero
/// address instead. This pins that the change did not break it. Only the
/// dual-stack case above failed on the old code.
#[test]
fn an_ipv4_socket_actually_disconnects() {
    let t = FdTable::new();
    let fd = t.open_udp(Some("127.0.0.1:0")).expect("IPv4 UDP socket");
    connect_then_disconnect(&t, fd, "ipv4");
}
