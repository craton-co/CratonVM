use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::net::TcpListener;
use std::sync::Mutex;

// The policy + timeout cells are process-global. The tests that
// mutate them must serialize so a parallel run doesn't see another
// test's override.
static MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn rejects_aws_imds_v4() {
    let _g = MUTEX.lock().unwrap();
    reset_policy();
    let res = policy_connect("169.254.169.254:80");
    match res {
        Err(PolicyConnectError::Denied(msg)) => {
            assert!(msg.contains("169.254"), "msg={msg}");
        }
        other => panic!("expected Denied, got {other:?}"),
    }
}

#[test]
fn rejects_aws_imds_v6() {
    let _g = MUTEX.lock().unwrap();
    reset_policy();
    let res = policy_connect("[fd00:ec2::254]:80");
    assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
}

#[test]
fn rejects_v4_link_local_neighbour() {
    // ECS task-role-credentials endpoint sits at 169.254.170.2,
    // *not* .169.254 — the policy must catch the whole /16.
    let _g = MUTEX.lock().unwrap();
    reset_policy();
    let res = policy_connect("169.254.170.2:80");
    assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
}

#[test]
fn times_out_on_blackhole_test_net() {
    // RFC 5737 TEST-NET-1 is reserved; nothing should ever answer.
    let _g = MUTEX.lock().unwrap();
    reset_policy();
    set_connect_timeout(Duration::from_millis(200));
    let start = std::time::Instant::now();
    let res = policy_connect("192.0.2.1:80");
    let elapsed = start.elapsed();
    // Reset timeout for other tests.
    set_connect_timeout(Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS));
    // Some platforms return AddrNotAvailable / network-unreachable
    // immediately instead of timing out; we accept either outcome
    // so the test isn't flaky on CI runners with restrictive
    // egress. What we DO require is that we didn't sit there for
    // the OS default (~75 s on Linux, longer on Windows).
    match res {
        Err(PolicyConnectError::Io(e)) => {
            assert!(
                elapsed < Duration::from_secs(5),
                "took {:?} which is longer than the configured 200 ms cap: err={e}",
                elapsed
            );
        }
        Err(PolicyConnectError::Denied(d)) => panic!("unexpected denial: {d}"),
        Ok(_) => panic!("blackhole connect somehow succeeded"),
    }
}

#[test]
fn allows_localhost_by_default() {
    let _g = MUTEX.lock().unwrap();
    reset_policy();
    // Start a real listener so the connect actually succeeds.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = format!("127.0.0.1:{port}");
    let _accept_thread = std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let res = policy_connect(&target);
    assert!(
        res.is_ok(),
        "default policy should allow localhost; got {res:?}"
    );
}

#[test]
fn custom_policy_overrides_default() {
    let _g = MUTEX.lock().unwrap();
    // Custom policy: deny everything.
    fn deny_all(_t: &str) -> PolicyDecision {
        PolicyDecision::Deny("test policy denies all".to_string())
    }
    set_policy(deny_all);
    let res = policy_connect("127.0.0.1:9");
    assert!(matches!(res, Err(PolicyConnectError::Denied(_))));
    // And restore the default so we don't poison other tests.
    reset_policy();
    // And confirm the default came back.
    // (We can't reach a real host without networking; rely on the
    // localhost test above for that side. Here we just verify the
    // policy is no longer denying 127.0.0.1.)
    match (load_policy())("127.0.0.1:1") {
        PolicyDecision::Allow => {}
        other => panic!("expected default Allow after reset, got {other:?}"),
    }
}

#[test]
fn connect_timeout_round_trips_through_setter() {
    let _g = MUTEX.lock().unwrap();
    let original = connect_timeout();
    set_connect_timeout(Duration::from_millis(1234));
    assert_eq!(connect_timeout(), Duration::from_millis(1234));
    // ZERO acts as "reset to default" (still finite).
    set_connect_timeout(Duration::ZERO);
    assert_eq!(
        connect_timeout(),
        Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS)
    );
    // Restore.
    set_connect_timeout(original);
}

#[test]
fn host_part_strips_brackets_and_port() {
    assert_eq!(host_part("127.0.0.1:80"), "127.0.0.1");
    assert_eq!(host_part("[::1]:80"), "::1");
    assert_eq!(host_part("example.com:443"), "example.com");
    assert_eq!(host_part("bare"), "bare");
}

/// B4 (2026-06-10): a bare, *unbracketed* IPv6 literal (multiple colons,
/// no port) must be returned whole so it parses as an `IpAddr`. The old
/// `rsplit_once(':')` split it on an internal colon (`fe80::1` → `fe80:`),
/// making it unparseable and silently `Allow`ed by `default_policy`.
#[test]
fn host_part_handles_bare_ipv6_literal() {
    assert_eq!(host_part("fe80::1"), "fe80::1");
    assert_eq!(host_part("::1"), "::1");
    assert_eq!(host_part("fd00:ec2::254"), "fd00:ec2::254");
    // Bracketed literal *without* a port still strips the brackets.
    assert_eq!(host_part("[fe80::1]"), "fe80::1");
    // And each of these now parses as a real IpAddr (the whole point).
    assert!(host_part("fe80::1").parse::<IpAddr>().is_ok());
    assert!(host_part("fd00:ec2::254").parse::<IpAddr>().is_ok());
}

/// B4 end-to-end: an unbracketed AWS-metadata IPv6 literal must be denied
/// by the default policy (previously slipped through because `host_part`
/// mangled it). `default_policy` is consulted directly so the test does
/// not depend on networking.
///
/// NARROWED 2026-09-17: `fe80::1` is an ordinary link-local address, not
/// a metadata endpoint, and must now be Allowed — see
/// `link_local_narrowing_allows_non_metadata_same_link_addresses` below.
#[test]
fn default_policy_denies_bare_ipv6_metadata() {
    // Unbracketed AWS-metadata IPv6 literal — exactly the input the old
    // `host_part` mangled into `Allow`.
    assert!(matches!(
        default_policy("fd00:ec2::254"),
        PolicyDecision::Deny(_)
    ));
    // The bracketed-with-port form must keep working too.
    assert!(matches!(
        default_policy("[fd00:ec2::254]:80"),
        PolicyDecision::Deny(_)
    ));
    // A public IPv6 literal is still allowed (no false positives).
    assert!(matches!(
        default_policy("2001:4860:4860::8888"),
        PolicyDecision::Allow
    ));
}

/// V2 (2026-06-10): the opt-in private-net classifier covers loopback
/// (`127/8`, `::1`) and the RFC1918 ranges (`10/8`, `172.16/12`,
/// `192.168/16`) plus IPv6 ULA (`fc00::/7`) and IPv4-mapped private v6.
/// These functions are gated on `CRATONVM_BLOCK_PRIVATE_NETS` in
/// `default_policy`; here we test the pure classifier so the assertion
/// does not race the process-global env cache.
#[test]
fn private_net_classifier_covers_loopback_and_rfc1918() {
    let denied = [
        "127.0.0.1",
        "127.255.255.255",
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "::1",
        "fc00::1",
        "fd12:3456::1",
        "::ffff:10.0.0.1", // IPv4-mapped private v6
        "::ffff:127.0.0.1",
    ];
    for s in denied {
        let ip: IpAddr = s.parse().unwrap();
        assert!(
            is_private_or_loopback_ip(&ip),
            "expected private/loopback: {s}"
        );
    }
    let allowed = [
        "8.8.8.8",
        "1.1.1.1",
        "172.15.0.1", // just below the /12
        "172.32.0.1", // just above the /12
        "192.169.0.1",
        "2001:4860:4860::8888",
        "::ffff:8.8.8.8", // IPv4-mapped public v6
    ];
    for s in allowed {
        let ip: IpAddr = s.parse().unwrap();
        assert!(!is_private_or_loopback_ip(&ip), "expected public: {s}");
    }
}

/// The private-net block must NOT overlap or weaken the always-on
/// cloud-metadata block: the known metadata addresses stay denied
/// regardless of the opt-in flag.
#[test]
fn private_net_classifier_excludes_link_local_metadata() {
    // 169.254.169.254 is handled by the metadata classifier, not the
    // private classifier — verify it is not mis-bucketed.
    let imds: IpAddr = "169.254.169.254".parse().unwrap();
    assert!(!is_private_or_loopback_ip(&imds));
    assert!(is_link_local_metadata_ip(&imds));
}

/// SSRF FIX (2026-06-21): IPv4-mapped IPv6 literals must NOT bypass the
/// always-on cloud-metadata block. `::ffff:169.254.169.254` (AWS IMDS)
/// and `::ffff:169.254.170.2` (ECS task-role creds) parse as `IpAddr::V6`
/// but tunnel a v4 metadata address — they previously skipped the v4
/// check and were allowed through.
///
/// NARROWED 2026-09-17: a mapped address elsewhere in the /16 (the old
/// bottom/top-of-range probes) is NOT a metadata endpoint and must now be
/// allowed.
#[test]
fn link_local_block_unwraps_ipv4_mapped_v6() {
    let blocked = [
        "::ffff:169.254.169.254", // AWS / GCP / Azure IMDS
        "::ffff:169.254.170.2",   // ECS task-role credentials
    ];
    for s in blocked {
        let ip: IpAddr = s.parse().unwrap();
        assert!(
            is_link_local_metadata_ip(&ip),
            "IPv4-mapped metadata address must be blocked: {s}"
        );
    }
    // A mapped *public* v4 address, and a mapped address elsewhere in
    // 169.254.0.0/16 that is not a known metadata endpoint, must both be
    // allowed (no false positives).
    let allowed = [
        "::ffff:8.8.8.8",
        "::ffff:169.254.0.0",
        "::ffff:169.254.255.255",
    ];
    for s in allowed {
        let ip: IpAddr = s.parse().unwrap();
        assert!(
            !is_link_local_metadata_ip(&ip),
            "non-metadata mapped address must be allowed: {s}"
        );
    }
}

/// The deprecated IPv4-compatible form `::a.b.c.d` (RFC 4291 §2.5.5.1)
/// must also be unwrapped — otherwise it is a second tunnel past the v4
/// link-local block. `::` and `::1` must NOT be misread as embedded v4.
#[test]
fn link_local_block_unwraps_ipv4_compatible_v6() {
    // `::169.254.169.254` — note Rust formats embedded-v4 zeros-high as a
    // bare v6 literal, so we build it from segments to be explicit.
    let imds_compat = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0xa9fe, 0xa9fe);
    assert_eq!(
        v6_to_ipv4_compatible(&imds_compat),
        Some(Ipv4Addr::new(169, 254, 169, 254))
    );
    assert!(is_link_local_metadata_ip(&IpAddr::V6(imds_compat)));

    // A compatible *public* v4 (e.g. 8.8.8.8 == 0x0808:0808) is allowed.
    let public_compat = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0x0808, 0x0808);
    assert_eq!(
        v6_to_ipv4_compatible(&public_compat),
        Some(Ipv4Addr::new(8, 8, 8, 8))
    );
    assert!(!is_link_local_metadata_ip(&IpAddr::V6(public_compat)));

    // `::` (unspecified) and `::1` (loopback) are not v4 tunnels.
    assert_eq!(v6_to_ipv4_compatible(&Ipv6Addr::UNSPECIFIED), None);
    assert_eq!(v6_to_ipv4_compatible(&Ipv6Addr::LOCALHOST), None);
    // And they must not be classified as link-local metadata.
    assert!(!is_link_local_metadata_ip(&IpAddr::V6(
        Ipv6Addr::UNSPECIFIED
    )));
    assert!(!is_link_local_metadata_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
}

/// End-to-end: the default policy denies an IPv4-mapped IMDS literal in
/// both bracketed-with-port and bare forms — exactly the inputs that
/// previously slipped through the always-on block.
#[test]
fn default_policy_denies_ipv4_mapped_metadata() {
    assert!(matches!(
        default_policy("[::ffff:169.254.169.254]:80"),
        PolicyDecision::Deny(_)
    ));
    assert!(matches!(
        default_policy("::ffff:169.254.170.2"),
        PolicyDecision::Deny(_)
    ));
}

/// DNS-ALIAS (2026-06-21): the per-IP classifier shared by the literal
/// and resolution paths must give identical verdicts — a hostname alias
/// can never reach anything a literal IP could not. Tested directly so
/// the assertion does not race the process-global env caches.
#[test]
fn classify_ip_matches_literal_policy_for_metadata() {
    let imds: IpAddr = "169.254.169.254".parse().unwrap();
    assert!(matches!(classify_ip(&imds), PolicyDecision::Deny(_)));
    let ecs: IpAddr = "169.254.170.2".parse().unwrap();
    assert!(matches!(classify_ip(&ecs), PolicyDecision::Deny(_)));
    // IPv4-mapped metadata is blocked through the shared classifier too.
    let mapped: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
    assert!(matches!(classify_ip(&mapped), PolicyDecision::Deny(_)));
    // A public address passes (no false positives).
    let public: IpAddr = "8.8.8.8".parse().unwrap();
    assert!(matches!(classify_ip(&public), PolicyDecision::Allow));
}

/// DNS-ALIAS resolution plumbing, exercised offline: a *literal* IP
/// passed as the "host" string is resolved by `ToSocketAddrs` WITHOUT a
/// DNS query (it short-circuits), so we can assert the resolve+classify
/// machinery blocks a metadata address and passes a public one without
/// touching the network. This is the same path a real hostname that
/// resolves to these addresses would take.
#[test]
fn resolve_and_classify_blocks_metadata_offline() {
    // 169.254.169.254 is always blocked (link-local metadata, no flag).
    match resolve_and_classify_host("169.254.169.254") {
        Some(PolicyDecision::Deny(msg)) => {
            assert!(msg.contains("resolves to blocked address"), "msg={msg}");
            assert!(msg.contains("169.254.169.254"), "msg={msg}");
        }
        other => panic!("expected Deny for IMDS, got {other:?}"),
    }
    // A public literal resolves but is not blocked → None (caller Allows).
    assert_eq!(resolve_and_classify_host("8.8.8.8"), None);
    // A name that cannot resolve must not become a policy denial — the
    // connect site surfaces the real DNS error instead.
    assert_eq!(
        resolve_and_classify_host("invalid.invalid.this-tld-does-not-exist"),
        None
    );
}

/// IPv4-mapped metadata must also be caught on the resolution path (not
/// just the literal path) — `::ffff:169.254.169.254` as a resolved
/// address is blocked via the shared `classify_ip`. Exercised offline by
/// passing the literal v6 string (no DNS query).
#[test]
fn resolve_and_classify_blocks_ipv4_mapped_metadata_offline() {
    match resolve_and_classify_host("::ffff:169.254.169.254") {
        Some(PolicyDecision::Deny(_)) => {}
        other => panic!("expected Deny for mapped IMDS, got {other:?}"),
    }
}

/// The `CRATONVM_RESOLVE_OUTBOUND_HOST` flag parses presence and the
/// `0`/`false`/`off`/`no` disablers identically to the private-nets flag.
/// We can't flip the cached process-global atomic per-test, so assert the
/// parsing predicate directly via the same logic the cache uses.
#[test]
fn resolve_outbound_flag_parsing_semantics() {
    // Mirror the disabler set the cache uses; presence (non-disabler) = on.
    let is_on = |v: &str| {
        let v = v.trim().to_ascii_lowercase();
        !(v.is_empty() || v == "0" || v == "false" || v == "off" || v == "no")
    };
    for off in ["", " ", "0", "false", "FALSE", "Off", "no", "NO"] {
        assert!(!is_on(off), "expected disabled for {off:?}");
    }
    for on in ["1", "true", "yes", "on", "anything"] {
        assert!(is_on(on), "expected enabled for {on:?}");
    }
}

/// An IPv4-mapped destination must be dialled as plain IPv4, because on
/// Windows an AF_INET6 socket cannot reach one (`IPV6_V6ONLY` defaults to
/// 1 → WSAEADDRNOTAVAIL). The port must survive the fold.
#[test]
fn normalize_connect_addr_folds_v4_mapped() {
    let mapped: SocketAddr = "[::ffff:127.0.0.1]:8080".parse().unwrap();
    let got = normalize_connect_addr(mapped);
    assert!(matches!(got, SocketAddr::V4(_)), "expected V4, got {got}");
    assert_eq!(got.ip().to_string(), "127.0.0.1");
    assert_eq!(got.port(), 8080);

    // A non-loopback mapped address folds the same way.
    let mapped: SocketAddr = "[::ffff:10.1.2.3]:1".parse().unwrap();
    assert_eq!(normalize_connect_addr(mapped).ip().to_string(), "10.1.2.3");
}

/// Everything that is NOT `::ffff:a.b.c.d` must pass through untouched.
/// `::1` is the one that matters: `Ipv6Addr::to_ipv4` (which this must not
/// use) also matches the loopback and the deprecated IPv4-compatible form,
/// so it would silently rewrite `::1` to `0.0.0.1` and dial a stranger.
#[test]
fn normalize_connect_addr_leaves_everything_else_alone() {
    for literal in [
        "[::1]:80",
        "[fe80::1]:80",
        "[2001:db8::1]:80",
        "[::]:80",
        // Deprecated IPv4-COMPATIBLE form (`::a.b.c.d`), not the mapped
        // one — `hotspot_ip_string` does not fold it either, so neither
        // does this, and the two stay consistent.
        "[::127.0.0.1]:80",
        "127.0.0.1:80",
    ] {
        let addr: SocketAddr = literal.parse().unwrap();
        assert_eq!(
            normalize_connect_addr(addr),
            addr,
            "{literal} must pass through unchanged"
        );
    }
}

/// NARROWING (2026-09-17): the SSRF guard used to deny the entire
/// `169.254.0.0/16` / `fe80::/10` link-local ranges, which also caught
/// ordinary same-link auto-discovery that has nothing to do with cloud
/// metadata — confirmed breaking H2's `TestAutoServer` and Tomcat/Tribes
/// cluster membership. See
/// `link-local-outbound-policy-blocks-legitimate-local-services-RETIRED-20260917.md`.
/// Same-link addresses outside the specific known metadata list must now
/// be Allowed; the known metadata addresses stay Denied.
#[test]
fn link_local_narrowing_allows_non_metadata_same_link_addresses() {
    for allowed in [
        "169.254.1.1",
        "169.254.255.254",
        "fe80::1",
        "[fe80::abcd:1234]:80",
    ] {
        assert!(
            matches!(default_policy(allowed), PolicyDecision::Allow),
            "expected same-link (non-metadata) address to be allowed: {allowed}"
        );
    }
    for denied in ["169.254.169.254", "169.254.170.2", "fd00:ec2::254"] {
        assert!(
            matches!(default_policy(denied), PolicyDecision::Deny(_)),
            "expected known metadata address to stay denied: {denied}"
        );
    }
}
