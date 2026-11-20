use super::*;

#[test]
fn a_v4_address_on_a_v6_socket_is_v4_mapped_and_the_wildcard_is_any() {
    let sa = sockaddr_bytes("127.0.0.1".parse().unwrap(), 0, 9, true).unwrap();
    assert_eq!(sa.len(), 28);
    assert_eq!(u16::from_ne_bytes([sa[0], sa[1]]), AF_INET6 as u16);
    assert_eq!(u16::from_be_bytes([sa[2], sa[3]]), 9);
    assert_eq!(
        &sa[8..24],
        &"::ffff:127.0.0.1"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets()
    );
    let any = sockaddr_bytes("0.0.0.0".parse().unwrap(), 0, 0, true).unwrap();
    assert_eq!(&any[8..24], &[0u8; 16]);
}

#[test]
fn a_v4_socket_refuses_a_real_v6_address_and_takes_a_mapped_one() {
    assert_eq!(
        sockaddr_bytes("::1".parse().unwrap(), 0, 1, false).err(),
        Some(WSAEAFNOSUPPORT)
    );
    let sa = sockaddr_bytes("::ffff:10.0.0.1".parse().unwrap(), 0, 1, false).unwrap();
    assert_eq!(sa.len(), 16);
    assert_eq!(u16::from_ne_bytes([sa[0], sa[1]]), AF_INET as u16);
    assert_eq!(&sa[4..8], &[10, 0, 0, 1]);
}

/// An unbound dual-stack datagram socket is AF_INET6 although `getsockname` refuses it on Winsock, and
/// binds to a v4 loopback address (v4-mapped) like HotSpot's does.
#[test]
fn an_unbound_datagram_socket_remembers_its_family_and_binds_to_loopback() {
    let mut ctx = crate::test_support::MockNativeContext::new();
    let fd = match net_socket0_datagram(&mut ctx, true, false) {
        Some(Ok(Some(Value::Int(fd)))) => fd,
        other => panic!("socket0 failed: {other:?}"),
    };
    let udp = datagram_of(fd).expect("registered");
    assert!(udp_is_v6(&udp));
    net_bind0_datagram(&mut ctx, fd, &udp, "127.0.0.1", 0).expect("bind");
    assert_ne!(udp.local_addr().unwrap().port(), 0);
    close_net_fd(fd);
}
