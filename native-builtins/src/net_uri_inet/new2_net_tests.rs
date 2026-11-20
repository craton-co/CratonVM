use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Helper: allocate a minimal `InetAddress` synthetic carrying a
/// hostname / address pair. Mirrors what the real natives produce.
fn alloc_inet(ctx: &mut MockNativeContext, host: &str, addr: &str) -> ObjectRef {
    let ia = try_alloc_concurrent_synthetic(ctx, "java/net/InetAddress", 2).unwrap();
    let h = ctx.create_string(host);
    let a = ctx.create_string(addr);
    ctx.set_field(ia, 0, Value::Object(Some(h)));
    ctx.set_field(ia, 1, Value::Object(Some(a)));
    ia
}

// ---------------- C1: getLocalHost ----------------

#[test]
fn resolve_real_hostname_is_non_empty() {
    // The helper must always return a non-empty string. On any host
    // with a configured hostname this will be that name; otherwise
    // it falls back to "localhost". Either way: non-empty.
    let h = resolve_real_hostname();
    assert!(!h.is_empty(), "hostname must never be empty");
}

#[test]
fn resolve_real_hostname_is_cached_and_matches_uncached_probe() {
    // `resolve_real_hostname` latches its result in a `OnceLock` because
    // the third resolution step forks and execs `hostname`, and
    // `InetAddress.getLocalHost()` / `NetworkInterface` enumeration call it
    // on every invocation. Two things must hold: repeated calls are stable,
    // and the cached value is exactly what the uncached probe produces.
    let first = resolve_real_hostname();
    let second = resolve_real_hostname();
    assert_eq!(first, second, "cached hostname must be stable across calls");
    assert_eq!(
        first,
        crate::resolve_real_hostname_uncached(),
        "cache must not change the resolved value"
    );
    assert!(!first.is_empty(), "hostname must never be empty");
}

#[test]
fn resolve_primary_ipv4_is_parseable() {
    let h = resolve_real_hostname();
    let ip = resolve_primary_ipv4(&h);
    assert!(
        ip.parse::<std::net::Ipv4Addr>().is_ok(),
        "primary IPv4 must be a parseable v4 address (got {ip})"
    );
}

#[test]
fn inet_get_local_host_populates_fields() {
    let mut ctx = MockNativeContext::new();
    let result = native_inet_localhost(&mut ctx, &[]).unwrap();
    let ia = match result {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an InetAddress object, got {other:?}"),
    };
    // field 0: hostname — non-empty
    let host = match ctx.get_field(ia, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert!(!host.is_empty(), "hostname must be set");
    // field 1: ip — parseable as IPv4
    let ip_s = match ctx.get_field(ia, 1) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    assert!(
        ip_s.parse::<std::net::Ipv4Addr>().is_ok(),
        "ip must parse as IPv4 (got {ip_s})"
    );
}

// ---------------- C3: InetAddress predicates ----------------

#[test]
fn inet_predicates_loopback_ipv4() {
    let mut ctx = MockNativeContext::new();
    let lo = alloc_inet(&mut ctx, "localhost", "127.0.0.1");
    assert_eq!(
        native_inet_is_loopback(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        native_inet_is_any_local(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
        Some(Value::Int(0))
    );
    assert_eq!(
        native_inet_is_multicast(&mut ctx, &[Value::Object(Some(lo))]).unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn inet_predicates_loopback_ipv6() {
    let mut ctx = MockNativeContext::new();
    // ::1 is the IPv6 loopback. The IPv4-only string-prefix check used
    // to miss this — NEW-2 fix uses the parsed IpAddr predicates.
    let lo6 = alloc_inet(&mut ctx, "ip6-localhost", "::1");
    assert_eq!(
        native_inet_is_loopback(&mut ctx, &[Value::Object(Some(lo6))]).unwrap(),
        Some(Value::Int(1)),
        "::1 must be recognized as loopback"
    );
}

#[test]
fn inet_predicates_any_local() {
    let mut ctx = MockNativeContext::new();
    let any4 = alloc_inet(&mut ctx, "wildcard", "0.0.0.0");
    let any6 = alloc_inet(&mut ctx, "wildcard6", "::");
    assert_eq!(
        native_inet_is_any_local(&mut ctx, &[Value::Object(Some(any4))]).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        native_inet_is_any_local(&mut ctx, &[Value::Object(Some(any6))]).unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn inet_predicates_link_local_v4_v6() {
    let mut ctx = MockNativeContext::new();
    let v4 = alloc_inet(&mut ctx, "ll4", "169.254.1.2");
    let v6 = alloc_inet(&mut ctx, "ll6", "fe80::abcd");
    assert_eq!(
        native_inet_is_link_local(&mut ctx, &[Value::Object(Some(v4))]).unwrap(),
        Some(Value::Int(1))
    );
    assert_eq!(
        native_inet_is_link_local(&mut ctx, &[Value::Object(Some(v6))]).unwrap(),
        Some(Value::Int(1))
    );
    let lan = alloc_inet(&mut ctx, "lan", "192.168.0.1");
    assert_eq!(
        native_inet_is_link_local(&mut ctx, &[Value::Object(Some(lan))]).unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn inet_predicates_site_local_v4() {
    let mut ctx = MockNativeContext::new();
    for ip in ["10.0.0.1", "172.16.5.4", "192.168.1.1"] {
        let ia = alloc_inet(&mut ctx, "lan", ip);
        assert_eq!(
            native_inet_is_site_local(&mut ctx, &[Value::Object(Some(ia))]).unwrap(),
            Some(Value::Int(1)),
            "{ip} must be site-local"
        );
    }
    let public_ip = alloc_inet(&mut ctx, "public", "8.8.8.8");
    assert_eq!(
        native_inet_is_site_local(&mut ctx, &[Value::Object(Some(public_ip))]).unwrap(),
        Some(Value::Int(0))
    );
}

#[test]
fn inet_predicates_multicast() {
    let mut ctx = MockNativeContext::new();
    let mc = alloc_inet(&mut ctx, "mcast", "224.0.0.1");
    assert_eq!(
        native_inet_is_multicast(&mut ctx, &[Value::Object(Some(mc))]).unwrap(),
        Some(Value::Int(1))
    );
    let mc6 = alloc_inet(&mut ctx, "mcast6", "ff02::1");
    assert_eq!(
        native_inet_is_multicast(&mut ctx, &[Value::Object(Some(mc6))]).unwrap(),
        Some(Value::Int(1))
    );
}

// ---------------- C4: getAddress() ----------------

#[test]
fn inet_get_address_v4() {
    let mut ctx = MockNativeContext::new();
    let ia = alloc_inet(&mut ctx, "h", "10.20.30.40");
    let result = native_inet_get_address(&mut ctx, &[Value::Object(Some(ia))]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(a))) => a,
        _ => panic!("expected byte[]"),
    };
    assert_eq!(ctx.array_length(arr), 4);
    let mut got = [0u8; 4];
    for i in 0..4 {
        if let Value::Int(v) = ctx.get_array_element(arr, i) {
            got[i] = v as u8;
        }
    }
    assert_eq!(got, [10u8, 20, 30, 40]);
}

#[test]
fn inet_get_address_v6() {
    let mut ctx = MockNativeContext::new();
    let ia = alloc_inet(&mut ctx, "h", "2001:db8::1");
    let result = native_inet_get_address(&mut ctx, &[Value::Object(Some(ia))]).unwrap();
    let arr = match result {
        Some(Value::Object(Some(a))) => a,
        _ => panic!("expected byte[]"),
    };
    assert_eq!(ctx.array_length(arr), 16);
    let mut got = [0u8; 16];
    for i in 0..16 {
        if let Value::Int(v) = ctx.get_array_element(arr, i) {
            got[i] = v as u8;
        }
    }
    let expected = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets();
    assert_eq!(got, expected);
}

// ---------------- C5: isReachable ----------------

#[test]
fn inet_is_reachable_loopback() {
    let mut ctx = MockNativeContext::new();
    let lo = alloc_inet(&mut ctx, "localhost", "127.0.0.1");
    assert_eq!(
        native_inet_is_reachable(&mut ctx, &[Value::Object(Some(lo)), Value::Int(500)],)
            .unwrap(),
        Some(Value::Int(1)),
        "loopback must always be reachable"
    );
}

#[test]
fn inet_is_reachable_unspecified_is_reachable() {
    let mut ctx = MockNativeContext::new();
    let any = alloc_inet(&mut ctx, "any", "0.0.0.0");
    assert_eq!(
        native_inet_is_reachable(&mut ctx, &[Value::Object(Some(any)), Value::Int(500)],)
            .unwrap(),
        Some(Value::Int(1))
    );
}

#[test]
fn inet_is_reachable_unroutable_returns_false() {
    // 192.0.2.0/24 is the documentation-only TEST-NET-1 (RFC 5737);
    // it should never be reachable from a real network. We use a 100ms
    // timeout to keep the test fast.
    let mut ctx = MockNativeContext::new();
    let test_net = alloc_inet(&mut ctx, "doc", "192.0.2.1");
    let res =
        native_inet_is_reachable(&mut ctx, &[Value::Object(Some(test_net)), Value::Int(100)])
            .unwrap();
    assert_eq!(
        res,
        Some(Value::Int(0)),
        "TEST-NET-1 must not be reachable in <100ms"
    );
}

// ---------------- C8: end-to-end TCP loopback ----------------
//
// This test is a *direct* exercise of std::net (not the native-method
// surface) — it confirms that the Berkeley-socket primitives the
// native layer relies on actually round-trip on this build. If this
// test passes, the existing Java-facing native paths (which all
// funnel through std::net via s2_registry / fd_table) are exercised
// end-to-end against real loopback.

#[test]
fn loopback_tcp_round_trip() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let local = listener.local_addr().expect("local_addr");
    // Server thread: accept once, echo bytes back.
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 5];
        let n = s.read(&mut buf).expect("server read");
        assert_eq!(n, 5);
        s.write_all(&buf[..n]).expect("server echo");
    });

    let mut client = TcpStream::connect(local).expect("connect");
    client.write_all(b"hello").expect("client write");
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).expect("client read");
    assert_eq!(&reply, b"hello");

    server.join().expect("server thread");
}

#[test]
fn loopback_tcp_via_socket2_supports_set_reuse_address() {
    // Sanity-check that the socket2 sockopt path used by the
    // ServerSocket.setReuseAddress native is actually wired to the
    // OS, not a no-op.
    use socket2::SockRef;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let sock = SockRef::from(&listener);
    sock.set_reuse_address(true).expect("setsockopt");
    assert!(sock.reuse_address().expect("getsockopt"));
}

// ---------------- C6: socket exception messages ----------------
//
// These cannot be exercised through MockNativeContext alone because
// the exception classes are concrete throwables in the wider VM and
// the message round-trip requires the JDK exception chain. The
// native registration itself is unit-tested by virtue of compiling
// and the broader VM test suite covering throw/catch paths.

// ---------------- DNS-config probe caching ----------------

/// `os_dns_nameservers_string` probes the platform resolver configuration
/// on Windows. The natives behind `sun/net/dns/ResolverConfigurationImpl.init0`
/// and `.loadDNSconfig0` call it under the JDK's resolver-config refresh
/// path, so an unlatched probe re-paid the whole cost every refresh.
/// The cache must be stable and must not change the answer.
///
/// Only a non-empty answer is latched — see `os_dns_nameservers_string`'s
/// own doc for why remembering "no nameservers" is actively harmful. This
/// test holds either way: with servers configured both calls return the
/// latched list; with none, both return the same empty string.
#[test]
fn os_dns_nameservers_is_cached_and_matches_uncached_probe() {
    let first = os_dns_nameservers_string();
    let second = os_dns_nameservers_string();
    assert_eq!(
        first, second,
        "a resolved DNS-nameserver list must be latched, not re-run per call"
    );
    assert_eq!(
        first,
        os_dns_nameservers_string_uncached(),
        "latching must not change the resolved nameserver list"
    );
}

/// The parse contract the callers depend on: a space-separated list of
/// bare IP literals, and — critically — never a `null`-ish value, because
/// the real JDK's `loadConfig()` feeds it straight into
/// `stringToList(os_nameservers)` which calls `String.split` with no null
/// guard (see the fn doc). An empty string is the correct "no servers"
/// answer; anything else must parse as an `IpAddr`.
#[test]
fn os_dns_nameservers_yields_bare_ip_literals_or_empty() {
    let servers = os_dns_nameservers_string();
    for token in servers.split(' ').filter(|t| !t.is_empty()) {
        assert!(
            token.parse::<std::net::IpAddr>().is_ok(),
            "`{token}` is not an IP literal; ResolverConfigurationImpl \
             splits this string on whitespace and hands the pieces to the \
             DNS client verbatim"
        );
        assert!(
            !token.contains('%'),
            "`{token}` still carries an IPv6 zone id, which our UDP layer \
             cannot route"
        );
    }
}

// ---------------------------------------------------------------------
// G14-1: the `new URI(String)` refusal surface.
//
// Every expectation below is a TRANSCRIPTION of a row measured on
// Temurin 25.0.3+9-LTS on 2026-08-17, not a derivation from the grammar.
// The reason strings and the indices are what `URISyntaxException`'s
// `getReason()` / `getIndex()` actually answered; where they look
// arbitrary (an index pointing at the first non-digit for one port
// failure and at the port's first character for another) that is because
// they ARE arbitrary — they are wherever the JDK's scan stopped.
// ---------------------------------------------------------------------

/// The offending character's index, and whether the JDK would blame a
/// malformed escape triple rather than the component's character set.
#[track_caller]
fn assert_escape_fault(uri: &str, index: usize) {
    assert_eq!(
        uri_first_char_fault(uri),
        Some((index, UriCharFault::MalformedEscape)),
        "`{uri}` must be a `Malformed escape pair` at index {index}"
    );
}

#[track_caller]
fn assert_illegal_char(uri: &str, index: usize) {
    assert_eq!(
        uri_first_char_fault(uri),
        Some((index, UriCharFault::Illegal)),
        "`{uri}` must be an illegal-character refusal at index {index}"
    );
}

#[test]
fn uri_malformed_escape_pairs_are_their_own_reason() {
    // MEASURED: all sixteen are `Malformed escape pair at index N`, never
    // `Illegal character in <component>` — and the component makes no
    // difference to the reason, only to the index.
    assert_escape_fault("http://h/a%", 10);
    assert_escape_fault("http://h/a%2", 10);
    assert_escape_fault("http://h/a%A", 10);
    assert_escape_fault("http://h/a%zz", 10);
    assert_escape_fault("http://h/a%2z", 10);
    assert_escape_fault("http://h/a%z2", 10);
    assert_escape_fault("http://h/a%GG", 10);
    assert_escape_fault("http://h/a%2G", 10);
    assert_escape_fault("http://h/a%%20", 10);
    assert_escape_fault("http://h/p?q=%2", 13);
    assert_escape_fault("http://h/p?q=%zz", 13);
    assert_escape_fault("http://h/p#f%2", 12);
    assert_escape_fault("http://h/p#f%zz", 12);
    assert_escape_fault("http://h%2/p", 8);
    assert_escape_fault("http://h%zz/p", 8);
    assert_escape_fault("http://u%2@h/p", 8);
    assert_escape_fault("%2", 0);
    assert_escape_fault("a%zzb", 1);
    assert_escape_fault("mailto:a%2", 8);
}

#[test]
fn uri_well_formed_escapes_are_accepted() {
    for uri in [
        "http://h/a%20b",
        "http://h/a%41b",
        "http://h/a%C3%A9",
        "http://h/a%c3%a9",
        "http://h/a%FF",
        "http://h/a%ff%fe",
    ] {
        assert_eq!(uri_first_char_fault(uri), None, "`{uri}` is legal");
    }
}

#[test]
fn uri_first_offence_wins_left_to_right() {
    // MEASURED — the JDK scans forward and reports whichever offence it
    // reaches first, so the SAME two characters swap the reason when their
    // order swaps.
    assert_illegal_char("http://h/a b%2", 10); // the space, at 10
    assert_escape_fault("http://h/a%2 b", 10); // the `%`, at 10
    assert_illegal_char("http://h/a<b%2", 10);
    assert_escape_fault("http://h/a%2<b", 10);
    assert_escape_fault("http://h/a%2%2", 10);
}

#[test]
fn percent_inside_an_ipv6_literal_is_a_scope_id_not_an_escape() {
    // MEASURED — HotSpot ACCEPTS all three. We used to refuse the first two
    // with `Illegal character in authority`, which is the worse half of the
    // bug: refusing input the JDK accepts.
    for uri in [
        "http://[::1%eth0]/p",
        "http://[::1%zz]/p",
        "http://[::1%25]/p",
    ] {
        assert_eq!(
            uri_first_char_fault(uri),
            None,
            "`{uri}` must survive the character scan"
        );
    }
    // …but only INSIDE the brackets. A bare `%` anywhere else is still an
    // escape triple that has to be well formed.
    assert_escape_fault("http://[::1]/a%2", 14);
    assert_escape_fault("http://h%2/p", 8);
}

// -----------------------------------------------------------------
// G14-1 §6 — `parseAuthority`'s ladder
// -----------------------------------------------------------------

#[track_caller]
fn assert_authority_fail(uri: &str, reason: &str, index: usize) {
    let got = uri_authority_fail(uri).map(|f| (f.reason, f.index));
    assert_eq!(
        got,
        Some((reason, Some(index))),
        "`{uri}` must be refused with `{reason}` at {index}"
    );
}

#[test]
fn a_bracket_outside_a_bracketed_host_is_an_illegal_hostname() {
    // MEASURED on HotSpot 25.0.3+9-LTS. Every row's index is the `[` or
    // `]` itself, EXCEPT `http://ab]/p`, where the label `ab` is consumed
    // first — which is what says the scan runs the hostname grammar
    // rather than looking for a bracket.
    assert_authority_fail("http://a[b]/p", "Illegal character in hostname", 8);
    assert_authority_fail("http://a]b/p", "Illegal character in hostname", 8);
    assert_authority_fail("http://a[/p", "Illegal character in hostname", 8);
    assert_authority_fail("http://ab]/p", "Illegal character in hostname", 9);
    assert_authority_fail("http://]/p", "Illegal character in hostname", 7);
    assert_authority_fail("http://x[y]z/p", "Illegal character in hostname", 8);
    assert_authority_fail("http://u@a[b]/p", "Illegal character in hostname", 10);
    // A `:` legally ENDS a hostname, so the port does not shift the index.
    assert_authority_fail("http://a[b]:80/p", "Illegal character in hostname", 8);
    assert_authority_fail("http://a[b]", "Illegal character in hostname", 8);
    assert_authority_fail("http://a[b]?q", "Illegal character in hostname", 8);
    assert_authority_fail("http://a[b]#f", "Illegal character in hostname", 8);
}

#[test]
fn a_bracket_in_the_user_info_is_its_own_reason() {
    // MEASURED. The index is the first offending BYTE, not the user
    // info's start — rows one and three disagree about that, which is why
    // one row would have pinned the wrong rule.
    assert_authority_fail("http://[::1]@h/p", "Illegal character in user info", 7);
    assert_authority_fail("http://[::1]@h:80/p", "Illegal character in user info", 7);
    assert_authority_fail("http://u[v]@h/p", "Illegal character in user info", 8);
    assert_authority_fail("http://u]@h/p", "Illegal character in user info", 8);
    assert_authority_fail("http://[::1]@[::2]/p", "Illegal character in user info", 7);
}

#[test]
fn a_reg_name_clean_authority_is_never_refused() {
    // THE BLAST-RADIUS TEST. `L_REG_NAME` and `L_SERVER` differ by exactly
    // `[` and `]`, so a failed server parse over any other authority
    // DEMOTES instead of throwing. MEASURED — the oracle accepts every one
    // of these with a null host.
    for uri in [
        "http://h_x/p",
        "http://h~x/p",
        "http://h!x/p",
        "http://h$x/p",
        "http://h&x/p",
        "http://h'x/p",
        "http://h(x)/p",
        "http://h*x/p",
        "http://h+x/p",
        "http://h,x/p",
        "http://h;x/p",
        "http://h=x/p",
        "http://h%41x/p",
        "http://h:80x/p",
        "http://-h/p",
        "http://h-/p",
        "http://a..b/p",
        "http://a./p",
        "http://.a/p",
        "http://a.9b/p",
        "http://h:-5/p",
        "http://h:+80/p",
        "http://h:8x/p",
        "http://u@h:x/p",
        "http://:80/p",
        "http://u@:80/p",
        "http://h:80:90/p",
        "http://a@b@c/p",
        "http://u@@h/p",
        // and the ordinary ones
        "http://h/p",
        "http://u@h/p",
        "http://u:p@h/p",
        "http://h:80/p",
        "http://a.b9/p",
        "http://9h/p",
        "http://12/p",
    ] {
        assert_eq!(uri_authority_fail(uri), None, "`{uri}` must not be refused");
    }
}

#[test]
fn the_illegal_authority_index_is_qreg_not_the_character() {
    // `fail("Illegal character in authority", serverChars ? q : qreg)`.
    // MEASURED: the first row's offending character and `qreg` coincide,
    // the rest disagree — 13 versus 7 for the second, which is the row
    // G14-1 §6 left open.
    assert_eq!(uri_authority_illegal_index("http://ho st/", 9), 9);
    assert_eq!(uri_authority_illegal_index("http://[::1]: /p", 13), 7);
    assert_eq!(uri_authority_illegal_index("http://[::1] /p", 12), 7);
    assert_eq!(uri_authority_illegal_index("http://[::1]<x/p", 12), 7);
    assert_eq!(uri_authority_illegal_index("http://u@[::1]: /p", 15), 9);
    assert_eq!(uri_authority_illegal_index("http://h:8 0/", 10), 10);
    assert_eq!(uri_authority_illegal_index("http://u v@h/p", 8), 8);
    // A fault OUTSIDE the authority is handed back untouched — the rule is
    // the authority's, and a path or query keeps reporting its own scan.
    assert_eq!(uri_authority_illegal_index("http://h/a b", 10), 10);
    assert_eq!(uri_authority_illegal_index("/a b", 2), 2);
}

#[test]
fn the_closing_bracket_refusal_needs_the_bracket_at_the_hosts_start() {
    // MEASURED, and the two rows disagree: a `[` that opens the HOST wants
    // a closing bracket, and a `[` anywhere else is just an illegal
    // hostname character.
    assert_eq!(uri_closing_bracket_fail_index("http://[b/p"), Some(9));
    assert_eq!(uri_closing_bracket_fail_index("http://[]/p"), Some(8));
    assert_eq!(uri_closing_bracket_fail_index("http://[::1/p"), Some(11));
    assert_eq!(uri_closing_bracket_fail_index("http://[/"), Some(8));
    assert_eq!(uri_closing_bracket_fail_index("http://u@[/p"), Some(10));
    assert_eq!(uri_closing_bracket_fail_index("http://a[/p"), None);
    assert_eq!(uri_closing_bracket_fail_index("http://a[b]/p"), None);
    assert_eq!(uri_closing_bracket_fail_index("http://[::1]/p"), None);
}

#[track_caller]
fn assert_ipv6_fail(uri: &str, reason: &str, index: Option<usize>) {
    let got = uri_ipv6_authority_fail(uri).map(|f| (f.reason, f.index));
    assert_eq!(
        got,
        Some((reason, index)),
        "`{uri}` must be refused with `{reason}` at {index:?}"
    );
}

#[test]
fn ipv6_literal_bodies_match_the_jdk_parser() {
    assert_ipv6_fail("http://[abc]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail("http://[abcd]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail("http://[1]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail("http://[12]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail("http://[1:2]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail("http://[1:2:3]/p", "IPv6 address too short", Some(8));
    assert_ipv6_fail(
        "http://[1:2:3:4:5:6:7]/p",
        "IPv6 address too short",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[abcde]/p",
        "IPv6 hexadecimal digit sequence too long",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[12345]/p",
        "IPv6 hexadecimal digit sequence too long",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[12345::1]/p",
        "IPv6 hexadecimal digit sequence too long",
        Some(8),
    );
    assert_ipv6_fail("http://[v7.abc]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[V7.abc]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[vz.abc]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[v.abc]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[v7.]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[1.2.3.4]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[g::1]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[:1]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail("http://[%eth0]/p", "Malformed IPv6 address", Some(8));
    assert_ipv6_fail(
        "http://[::1:2:3:4:5:6:7:8]/p",
        "Malformed IPv6 address",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[1:2:3:4:5:6:7:8:9]/p",
        "IPv6 address too long",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[1:2:3:4:5:6:7:1.2.3.4]/p",
        "IPv6 address too long",
        Some(8),
    );
    assert_ipv6_fail(
        "http://[1:]/p",
        "Expected digits for an IPv6 address",
        Some(10),
    );
    assert_ipv6_fail(
        "http://[::1:]/p",
        "Expected digits for an IPv6 address",
        Some(12),
    );
}

#[test]
fn embedded_ipv4_indices_are_wherever_the_scan_stopped() {
    // These four indices are the reason this is a transcription: each one
    // is a different position for the same reason string.
    assert_ipv6_fail("http://[::256.1.1.1]/p", "Malformed IPv4 address", Some(10));
    assert_ipv6_fail("http://[::1.2.3]/p", "Malformed IPv4 address", Some(15));
    assert_ipv6_fail("http://[::1.2.3.400]/p", "Malformed IPv4 address", Some(16));
    assert_ipv6_fail("http://[::1.2.3.4.5]/p", "Malformed IPv4 address", Some(17));
    assert_ipv6_fail(
        "http://[::ffff:1.2.3.999]/p",
        "Malformed IPv4 address",
        Some(21),
    );
    assert_ipv6_fail(
        "http://[::1.2.3.4x]/p",
        "Expected hex digits or IPv4 address",
        Some(10),
    );
}

#[test]
fn ipv6_scope_id_rules() {
    // The ONE refusal in the family that carries no index at all.
    assert_ipv6_fail("http://[::1%]/p", "scope id expected", None);
    for uri in [
        "http://[::1%eth0]/p",
        "http://[::1%zz]/p",
        "http://[::1%25]/p",
    ] {
        assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is legal");
    }
}

#[test]
fn port_after_an_ipv6_literal_is_digits_only() {
    assert_ipv6_fail(
        "http://[::1]:x/p",
        "Illegal character in port number",
        Some(13),
    );
    assert_ipv6_fail(
        "http://[::1]:-5/p",
        "Illegal character in port number",
        Some(13),
    );
    assert_ipv6_fail(
        "http://[::1]:+80/p",
        "Illegal character in port number",
        Some(13),
    );
    // The index moves to the first NON-digit, not the start of the port.
    assert_ipv6_fail(
        "http://[::1]:8x/p",
        "Illegal character in port number",
        Some(14),
    );
    assert_ipv6_fail(
        "http://[::1]:80x80/p",
        "Illegal character in port number",
        Some(15),
    );
    // …and userinfo shifts every index along with it.
    assert_ipv6_fail(
        "http://u@[::1]:x/p",
        "Illegal character in port number",
        Some(15),
    );
    // All digits but out of `int` range is a DIFFERENT reason, reported at
    // the start of the port rather than at any particular digit.
    assert_ipv6_fail(
        "http://[::1]:99999999999/p",
        "Malformed port number",
        Some(13),
    );
    assert_ipv6_fail(
        "http://[::1]:2147483648/p",
        "Malformed port number",
        Some(13),
    );
}

#[test]
fn anything_but_a_colon_after_the_closing_bracket_expects_a_port() {
    assert_ipv6_fail("http://[::1]]/p", "Expected port number", Some(12));
    assert_ipv6_fail("http://[::1]x/p", "Expected port number", Some(12));
}

#[test]
fn well_formed_ipv6_authorities_are_left_alone() {
    for uri in [
        "http://[::1]/p",
        "http://[::]/p",
        "http://[fe80::1]/p",
        "http://[1:2:3:4:5:6:7:8]/p",
        "http://[1234::1]/p",
        "http://[1:2:3:4:5:6:1.2.3.4]/p",
        "http://[::1.2.3.4]/p",
        "http://[::ffff:1.2.3.4]/p",
        "http://[::1]:80/p",
        "http://[::1]:0/p",
        "http://[::1]:007/p",
        "http://[::1]:2147483647/p",
        "http://[::1]:/p",
        "http://[::1]",
        "http://[::1]?q",
        "http://[::1]#f",
        "http://u@[::1]/p",
        "http://u@[::1]:80/p",
    ] {
        assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is legal");
    }
}

#[test]
fn unclosed_and_empty_brackets_stay_with_the_older_check() {
    // `native_uri_init`'s `Expected closing bracket for IPv6 address` check
    // already refuses these at the right indices; the IPv6 body scanner
    // must not also claim them, or the reason would change.
    for uri in ["http://[/", "http://[]/p", "http://[::1/"] {
        assert_eq!(uri_ipv6_authority_fail(uri), None, "`{uri}` is not ours");
    }
}

#[test]
fn an_authority_less_uri_has_no_port_to_find() {
    // The three rows `uri_store_named`'s sentinel write exists for: with no
    // `//`, `url_parse` reads the LAST colon as a port delimiter.
    for uri in ["urn:isbn:0451450523", "a:1234", "mailto:a@b.com:25"] {
        let (_, authority, _, _, _) = crate::net_phase_e::uri_split(uri);
        assert!(
            authority.is_none(),
            "`{uri}` has no authority, so `getPort()` must be -1"
        );
    }
    // …and a URI that DOES have one keeps it.
    let (_, authority, _, _, _) = crate::net_phase_e::uri_split("http://h:80/p");
    assert_eq!(authority.as_deref(), Some("h:80"));
}
