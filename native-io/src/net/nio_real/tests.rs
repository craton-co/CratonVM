use super::*;

#[test]
fn ip_text_parses_every_shape_read_inet_address_text_emits() {
    assert_eq!(
        parse_ip_text("127.0.0.1"),
        Some(("127.0.0.1".parse().unwrap(), 0))
    );
    assert_eq!(parse_ip_text("::1"), Some(("::1".parse().unwrap(), 0)));
    assert_eq!(
        parse_ip_text("fe80::1%3"),
        Some(("fe80::1".parse().unwrap(), 3))
    );
    assert_eq!(parse_ip_text("not-an-ip"), None);
}

#[test]
fn a_v6_socket_gets_v4_mapped_and_the_v4_wildcard_becomes_any() {
    let (sa, len) = sockaddr_for("127.0.0.1".parse().unwrap(), 0, 9, true).unwrap();
    assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_in6>());
    // SAFETY: `sockaddr_for` wrote a `sockaddr_in6` at the start of `sa`.
    let sin6 =
        unsafe { &*((&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()) };
    assert_eq!(sin6.sin6_family as i32, libc::AF_INET6);
    assert_eq!(u16::from_be(sin6.sin6_port), 9);
    assert_eq!(
        std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr),
        "::ffff:127.0.0.1".parse::<std::net::Ipv6Addr>().unwrap()
    );
    let (sa, _) = sockaddr_for("0.0.0.0".parse().unwrap(), 0, 0, true).unwrap();
    // SAFETY: as above.
    let sin6 =
        unsafe { &*((&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()) };
    assert_eq!(
        std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr),
        std::net::Ipv6Addr::UNSPECIFIED
    );
}

#[test]
fn a_v4_socket_refuses_a_real_v6_address() {
    assert_eq!(
        sockaddr_for("::1".parse().unwrap(), 0, 1, false).err(),
        Some(libc::EAFNOSUPPORT)
    );
    assert!(sockaddr_for("::ffff:10.0.0.1".parse().unwrap(), 0, 1, false).is_ok());
}

/// An armed interruptible wait stops on the interrupt flag with the marker,
/// an unarmed one does not look at it (a `java.net.Socket` read is not
/// interruptible).
#[test]
fn an_armed_wait_stops_on_the_interrupt_and_an_unarmed_one_ignores_it() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (_a, _) = l.accept().unwrap();
    let id = register_handle(NetSocketHandle::Stream(Arc::new(c.try_clone().unwrap())));
    let mut buf = [0u8; 4];
    let yes = || true;
    let e = net_read_close_aware_intr(id, &c, &mut buf, Some(&yes)).unwrap_err();
    assert!(is_nio_interrupted(&e));
    let e = net_write_close_aware_intr(id, &c, b"x", Some(&yes)).unwrap_err();
    assert!(is_nio_interrupted(&e));
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let uid = register_handle(NetSocketHandle::Datagram(Arc::new(
        udp.try_clone().unwrap(),
    )));
    let e = dgram_wait_intr(uid, &udp, NET_POLLIN, -1, Some(&yes)).unwrap_err();
    assert!(is_nio_interrupted(&e));
    // Unarmed: a bounded wait simply times out.
    assert!(!dgram_wait_intr(uid, &udp, NET_POLLIN, 30, None).unwrap());
    close_net_fd(id);
    close_net_fd(uid);
}

#[test]
fn errno_text_drops_the_os_error_suffix() {
    assert_eq!(errno_text(libc::ECONNREFUSED), "Connection refused");
}

#[test]
fn a_datagram_id_owns_a_real_socket_and_translates_to_it() {
    use std::os::fd::AsRawFd;
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let raw = udp.as_raw_fd();
    let id = register_handle(NetSocketHandle::Datagram(Arc::new(udp)));
    assert!(id >= 0x4000_0000);
    assert_eq!(nio_os_fd(id).map(|f| f.raw), Some(raw));
    assert!(datagram_of(id).is_some());
    close_net_fd(id);
    assert!(nio_os_fd(id).is_none());
    assert!(datagram_of(id).is_none());
}
