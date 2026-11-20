use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn resolve_localhost_returns_at_least_one_address() {
    let ips = resolve_addrs("localhost").unwrap();
    assert!(!ips.is_empty(), "should resolve localhost");
    // At least one of the returned addresses should be a loopback.
    assert!(
        ips.iter().any(|ip| ip.is_loopback()),
        "expected a loopback in {ips:?}",
    );
}

#[test]
fn resolve_ipv4_literal_short_circuits() {
    let ips = resolve_addrs("127.0.0.1").unwrap();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
}

#[test]
fn resolve_ipv6_literal_short_circuits() {
    let ips = resolve_addrs("::1").unwrap();
    assert_eq!(ips, vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]);
}

#[test]
fn resolve_bracketed_ipv6_short_circuits() {
    let ips = resolve_addrs("[::1]").unwrap();
    assert_eq!(ips, vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]);
}

#[test]
fn resolve_empty_host_errors() {
    let err = resolve_addrs("").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("UnknownHost"), "got {msg}");
}

#[test]
fn resolve_garbage_host_errors() {
    // RFC 6761 .invalid never resolves; per spec, getaddrinfo MUST fail.
    let err = resolve_addrs("definitely-does-not-exist.invalid").unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("UnknownHost"), "got {msg}");
}

#[test]
fn ip_from_bytes_handles_v4_v6_and_garbage() {
    let v4 = ip_from_bytes(&[1, 2, 3, 4]);
    assert_eq!(v4, Some(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
    let v6_in: [u8; 16] = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    let v6 = ip_from_bytes(&v6_in);
    match v6 {
        Some(IpAddr::V6(addr)) => assert_eq!(addr.octets(), v6_in),
        _ => panic!("expected v6"),
    }
    assert_eq!(ip_from_bytes(&[1, 2]), None);
    assert_eq!(ip_from_bytes(&[0u8; 8]), None);
}

#[test]
fn ptr_lookup_localhost_or_unknown() {
    // PTR for 127.0.0.1 is platform-dependent. Either we get back a
    // hostname (Linux: "localhost", macOS: "localhost", Windows: the
    // computer name) or we get an EAI_NONAME error if the OS has no PTR.
    // Both outcomes are valid; we just check the call doesn't panic.
    let r = ptr_lookup(&IpAddr::V4(Ipv4Addr::LOCALHOST));
    match r {
        Ok(name) => assert!(!name.is_empty(), "ptr returned empty name"),
        Err(_) => {} // EAI_NONAME — acceptable on hermetic CI runners.
    }
}

#[test]
fn anchor_function_compiles() {
    _anchor_getaddrinfo();
}

#[test]
fn ipv4_available_on_developer_host() {
    // Every host with a working network stack has 127.0.0.1. If this
    // probe ever returns false on a CI runner, the runner is so locked
    // down it can't bind UDP — at which point the rest of net_phase_e
    // is broken too. We assert the typical case.
    assert!(probe_ipv4_available(), "127.0.0.1 UDP bind must work");
    assert!(ipv4_available_cached(), "cached probe must agree");
}

#[test]
fn ipv6_supported_does_not_panic() {
    // ::1 may or may not be available depending on host config; we just
    // assert the probe runs without panicking and the cache returns the
    // same answer twice.
    let first = ipv6_supported_cached();
    let second = ipv6_supported_cached();
    assert_eq!(first, second, "cache must be stable");
}

#[test]
fn ipv4_available_cache_is_stable() {
    let a = ipv4_available_cached();
    let b = ipv4_available_cached();
    assert_eq!(a, b, "ipv4_available cache must memoize");
}

#[test]
fn ipv4_ipv6_probe_natives_cover_jdk17_and_jdk25_owners() {
    let mut r = NativeMethodRegistry::new();
    register_inet_address_real(&mut r);

    for owner in [INET_ADDRESS, INET_IMPL_FACTORY] {
        assert!(
            r.find(owner, "isIPv6Supported", "()Z").is_some(),
            "{owner}.isIPv6Supported must be registered"
        );
        assert!(
            r.find(owner, "isIPv4Available", "()Z").is_some(),
            "{owner}.isIPv4Available must be registered"
        );
    }
}
