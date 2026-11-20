use super::*;
use std::net::Ipv4Addr;

// The socket-lifecycle tests that used to live here (`register/remove
// balances`, `send/receive roundtrip via registry`, `set/get broadcast via
// registry`, `parked receive does not block the registry`) exercised this
// module's own `dgram_registry`, which no longer exists — `ctx.fd_table()`
// owns the socket now. The behaviour they covered is exercised end-to-end
// by `UdpPathProbe.java` against a real VM, which the old
// registry could never satisfy: nothing populated it, so `send` failed with
// "no socket id" on every real call.

#[test]
fn multicast_group_bookkeeping_is_per_fd() {
    let fd_a: FdId = 4001;
    let fd_b: FdId = 4002;
    let group = IpAddr::V4(Ipv4Addr::new(239, 1, 2, 3));
    let other = IpAddr::V4(Ipv4Addr::new(239, 4, 5, 6));
    let iface = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    record_join(fd_a, group, iface);
    record_join(fd_a, other, iface);
    record_join(fd_b, group, iface);
    assert_eq!(joined_groups().read().unwrap().get(&fd_a).unwrap().len(), 2);

    // Leaving one group on one fd must not touch the same group on another.
    record_leave(fd_a, group);
    assert_eq!(joined_groups().read().unwrap().get(&fd_a).unwrap().len(), 1);
    assert_eq!(joined_groups().read().unwrap().get(&fd_b).unwrap().len(), 1);

    // The last leave drops the fd's entry entirely rather than leaving an
    // empty Vec behind for every channel that ever joined.
    record_leave(fd_a, other);
    assert!(!joined_groups().read().unwrap().contains_key(&fd_a));
    record_leave(fd_b, group);
    assert!(!joined_groups().read().unwrap().contains_key(&fd_b));
}

/// JIT r23 wave 3 review: a close forgets every group row of its fd, and
/// only of its fd.
#[test]
fn forget_joined_groups_drops_one_fds_rows() {
    let fd_a: FdId = 4011;
    let fd_b: FdId = 4012;
    let group = IpAddr::V4(Ipv4Addr::new(239, 7, 8, 9));
    let iface = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    record_join(fd_a, group, iface);
    record_join(fd_b, group, iface);
    forget_joined_groups(fd_a);
    assert!(!joined_groups().read().unwrap().contains_key(&fd_a));
    assert!(joined_groups().read().unwrap().contains_key(&fd_b));
    forget_joined_groups(fd_b);
    assert!(!joined_groups().read().unwrap().contains_key(&fd_b));
}

#[test]
fn h3a_send_blocks_link_local_metadata_v4() {
    let metadata: SocketAddr = "169.254.169.254:80".parse().unwrap();
    assert!(
        check_outbound_target(metadata).is_err(),
        "cloud-metadata IPv4 must stay blocked for UDP send"
    );
    let ecs_metadata: SocketAddr = "169.254.170.2:80".parse().unwrap();
    assert!(
        check_outbound_target(ecs_metadata).is_err(),
        "AWS ECS task-role-credentials address must stay blocked for UDP send"
    );
    // NARROWED 2026-09-17: an ordinary same-link address elsewhere in
    // 169.254.0.0/16 is NOT a metadata endpoint and must be admitted —
    // this is what broke H2's TestAutoServer auto-discovery. See
    // link-local-outbound-policy-blocks-legitimate-local-services-RETIRED-20260917.md.
    let same_link: SocketAddr = "169.254.1.1:53".parse().unwrap();
    assert!(
        check_outbound_target(same_link).is_ok(),
        "non-metadata link-local IPv4 must be allowed for UDP send"
    );
}

#[test]
fn h3a_send_blocks_link_local_metadata_v6() {
    let metadata: SocketAddr = "[fd00:ec2::254]:80".parse().unwrap();
    assert!(
        check_outbound_target(metadata).is_err(),
        "cloud-metadata IPv6 must stay blocked for UDP send"
    );
}

#[test]
fn h3a_send_allows_loopback() {
    let loopback: SocketAddr = "127.0.0.1:9".parse().unwrap();
    assert!(
        check_outbound_target(loopback).is_ok(),
        "loopback must remain reachable"
    );
}

#[test]
fn wp37_localhost_resolves_for_send() {
    // `send` accepts a hostname target; make sure the resolver used by the
    // decode path still yields a loopback address for `localhost`.
    let resolved: Vec<SocketAddr> = ("localhost", 0u16)
        .to_socket_addrs()
        .expect("resolve localhost")
        .collect();
    let any_loopback = resolved.iter().any(|a| a.ip().is_loopback());
    assert!(any_loopback, "expected loopback in {resolved:?}");
}
