use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// `read_settings` reads process-global environment variables, so the
/// tests that mutate them must not run concurrently with each other.
/// Save/clear/restore the proxy env vars around a closure so a failing
/// assertion cannot leak state into the next test.
fn with_proxy_env<R>(pairs: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
    const KEYS: [&str; 8] = [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
    ];
    // Clear all eight, then apply the caller's pairs, in ONE thread-scoped
    // override. This used to be up to sixteen `environ` mutations per call
    // (eight removes, the sets, then eight restores) under a lock held
    // against fellow env tests only — a process-wide data race with every
    // other test running in parallel. Nothing is written to `environ` now,
    // so there is nothing to restore and nothing to race.
    let mut edits: Vec<(&str, Option<&str>)> = KEYS.iter().map(|k| (*k, None)).collect();
    for (key, value) in pairs {
        match edits.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = Some(value),
            None => edits.push((key, Some(value))),
        }
    }
    cratonvm_types::flags::with_thread_overrides(&edits, f)
}

/// PERF-fix guard. `read_settings` runs once per `ProxySelector.select`,
/// and the env probe used to allocate a lowercased *and* an uppercased
/// `String` per key, then have the call site redundantly ask for both
/// spellings again via `or_else`. The keys are now `&'static str` pairs
/// and the duplicate arms are gone — these tests pin the semantics that
/// change had to preserve: both spellings are still honoured, and
/// lowercase still wins when both are set.
#[test]
fn env_proxy_lookup_accepts_lowercase_spelling() {
    let settings = with_proxy_env(&[("http_proxy", "http://lower.corp:8080")], || {
        read_settings(&MockNativeContext::new())
    });
    assert_eq!(
        settings.http,
        Some(("lower.corp".to_string(), 8080)),
        "the Unix-convention lowercase spelling must still be honoured"
    );
}

#[test]
fn env_proxy_lookup_accepts_uppercase_spelling() {
    let settings = with_proxy_env(&[("HTTPS_PROXY", "http://upper.corp:3128")], || {
        read_settings(&MockNativeContext::new())
    });
    assert_eq!(
        settings.https,
        Some(("upper.corp".to_string(), 3128)),
        "the Windows-convention uppercase spelling must still be honoured; \
         dropping the duplicate `or_else` arm must not have dropped this case"
    );
}

#[cfg(not(windows))]
#[test]
fn env_proxy_lookup_prefers_lowercase_when_both_are_set() {
    let settings = with_proxy_env(
        &[
            ("all_proxy", "socks://lower.corp:1080"),
            ("ALL_PROXY", "socks://upper.corp:1081"),
        ],
        || read_settings(&MockNativeContext::new()),
    );
    assert_eq!(
        settings.socks,
        Some(("lower.corp".to_string(), 1080)),
        "lowercase-wins precedence must survive the key-pair rewrite"
    );
}

/// The Windows arm asserts the SAME thing as the POSIX one above, and the
/// reason it no longer asserts case-folding is worth keeping.
///
/// It used to expect `upper.corp:1081` — "Windows stores environment keys
/// case-insensitively, so the final assignment is the single value visible
/// through either spelling". That was true while `with_proxy_env` mutated
/// the real `environ`, where `all_proxy` and `ALL_PROXY` ARE one key and the
/// second set overwrites the first.
///
/// `b6df44b0b` stopped mutating `environ` — correctly, it was a process-wide
/// data race — and routed these through `with_thread_overrides`, a map keyed
/// by exact string. Two spellings are now two ENTRIES on every platform, so
/// the case-folding this test named is a property of `environ` that the
/// override path deliberately does not model, and the test had been
/// asserting the old path's semantics against the new one ever since.
///
/// **What is still covered:** given both spellings present, `read_settings`
/// prefers the lowercase one, on every platform. **What is no longer
/// reachable through this door:** that Windows collapses the two into one
/// before `read_settings` ever sees them. That needs a real-`environ` test
/// or an override map that case-folds when `cfg(windows)`, and neither is
/// worth a process-wide race to get back.
#[cfg(windows)]
#[test]
fn env_proxy_lookup_prefers_lowercase_when_both_are_set_windows() {
    let settings = with_proxy_env(
        &[
            ("all_proxy", "socks://lower.corp:1080"),
            ("ALL_PROXY", "socks://upper.corp:1081"),
        ],
        || read_settings(&MockNativeContext::new()),
    );
    assert_eq!(
        settings.socks,
        Some(("lower.corp".to_string(), 1080)),
        "lowercase-wins precedence, through the thread-override path that \
         replaced `environ` mutation — see this test's doc comment"
    );
}

#[test]
fn env_no_proxy_patterns_are_lowercased_from_either_spelling() {
    let settings = with_proxy_env(&[("NO_PROXY", "Example.COM, .Internal ")], || {
        read_settings(&MockNativeContext::new())
    });
    assert_eq!(
        settings.no_proxy_patterns,
        vec!["example.com".to_string(), ".internal".to_string()],
        "NO_PROXY entries are comma-separated, trimmed and lowercased"
    );
}

#[test]
fn absent_proxy_env_leaves_settings_empty() {
    let settings = with_proxy_env(&[], || read_settings(&MockNativeContext::new()));
    assert_eq!(settings.http, None);
    assert_eq!(settings.https, None);
    assert_eq!(settings.socks, None);
    assert!(settings.no_proxy_patterns.is_empty());
}

#[test]
fn parse_proxy_url_handles_scheme_user_path() {
    assert_eq!(
        parse_proxy_url("http://proxy.corp:8080/", 80),
        Some(("proxy.corp".to_string(), 8080))
    );
    assert_eq!(
        parse_proxy_url("user:pwd@proxy.corp:3128", 80),
        Some(("proxy.corp".to_string(), 3128))
    );
    assert_eq!(
        parse_proxy_url("proxy.corp", 8080),
        Some(("proxy.corp".to_string(), 8080))
    );
    assert_eq!(
        parse_proxy_url("[2001:db8::1]:8080", 80),
        Some(("2001:db8::1".to_string(), 8080))
    );
    assert_eq!(parse_proxy_url("", 80), None);
}

#[test]
fn pattern_match_literal() {
    assert!(pattern_matches("example.com", "example.com", 80));
    assert!(pattern_matches("Example.COM", "example.com", 80));
    assert!(!pattern_matches("example.com", "foo.example.com", 80));
    assert!(!pattern_matches("example.com", "other.com", 80));
}

#[test]
fn pattern_match_leading_dot() {
    assert!(pattern_matches(".example.com", "foo.example.com", 80));
    assert!(pattern_matches(".example.com", "example.com", 80));
    assert!(!pattern_matches(".example.com", "other.com", 80));
}

#[test]
fn pattern_match_star_dot() {
    assert!(pattern_matches("*.example.com", "foo.example.com", 80));
    assert!(pattern_matches("*.example.com", "example.com", 80));
    assert!(!pattern_matches("*.example.com", "other.com", 80));
}

#[test]
fn pattern_match_global_star() {
    assert!(pattern_matches("*", "anything.example.com", 80));
}

#[test]
fn pattern_match_with_port() {
    assert!(pattern_matches("example.com:443", "example.com", 443));
    assert!(!pattern_matches("example.com:443", "example.com", 80));
}

#[test]
fn pattern_match_cidr_v4() {
    assert!(pattern_matches("10.0.0.0/8", "10.1.2.3", 80));
    assert!(!pattern_matches("10.0.0.0/8", "11.1.2.3", 80));
    assert!(pattern_matches("10.0.0.0/24", "10.0.0.55", 80));
    assert!(!pattern_matches("10.0.0.0/24", "10.0.1.55", 80));
    assert!(pattern_matches("0.0.0.0/0", "203.0.113.1", 80));
}

#[test]
fn pattern_match_cidr_v6() {
    assert!(pattern_matches("2001:db8::/32", "2001:db8:cafe::1", 80));
    assert!(!pattern_matches("2001:db8::/32", "2001:db9::1", 80));
    assert!(pattern_matches("::/0", "2001:db8::1", 80));
}

#[test]
fn pattern_match_ip_literal() {
    assert!(pattern_matches("127.0.0.1", "127.0.0.1", 80));
    assert!(!pattern_matches("127.0.0.1", "127.0.0.2", 80));
    assert!(pattern_matches("::1", "::1", 80));
}

#[test]
fn parse_uri_handles_basic_shapes() {
    let u = parse_uri_min("http://example.com/path").unwrap();
    assert_eq!(u.scheme, "http");
    assert_eq!(u.host, "example.com");
    assert_eq!(u.port, 80);

    let u = parse_uri_min("https://example.com:8443/").unwrap();
    assert_eq!(u.scheme, "https");
    assert_eq!(u.port, 8443);

    let u = parse_uri_min("http://user:pwd@host.example/").unwrap();
    assert_eq!(u.host, "host.example");

    let u = parse_uri_min("http://[2001:db8::1]:8080/").unwrap();
    assert_eq!(u.host, "2001:db8::1");
    assert_eq!(u.port, 8080);

    assert!(parse_uri_min("not a uri").is_none());
}

#[test]
fn cidr_v4_basic_arithmetic() {
    assert!(v4_cidr(
        Ipv4Addr::new(10, 0, 0, 0),
        8,
        Ipv4Addr::new(10, 1, 2, 3)
    ));
    assert!(!v4_cidr(
        Ipv4Addr::new(10, 0, 0, 0),
        8,
        Ipv4Addr::new(11, 1, 2, 3)
    ));
    assert!(v4_cidr(
        Ipv4Addr::new(192, 168, 1, 0),
        24,
        Ipv4Addr::new(192, 168, 1, 5)
    ));
    assert!(!v4_cidr(
        Ipv4Addr::new(192, 168, 1, 0),
        24,
        Ipv4Addr::new(192, 168, 2, 5)
    ));
    assert!(v4_cidr(Ipv4Addr::UNSPECIFIED, 0, Ipv4Addr::new(1, 2, 3, 4)));
}

#[test]
fn cidr_v6_basic_arithmetic() {
    let net: Ipv6Addr = "2001:db8::".parse().unwrap();
    let inside: Ipv6Addr = "2001:db8:cafe::1".parse().unwrap();
    let outside: Ipv6Addr = "2001:db9:cafe::1".parse().unwrap();
    assert!(v6_cidr(net, 32, inside));
    assert!(!v6_cidr(net, 32, outside));
    assert!(v6_cidr(Ipv6Addr::UNSPECIFIED, 0, inside));
}

#[test]
fn record_failure_keeps_bounded_log() {
    // Drain whatever was there.
    failures().lock().clear();
    for i in 0..200 {
        record_failure(format!("http://h{i}/"), format!("h{i}"), 80);
    }
    let g = failures().lock();
    assert_eq!(g.len(), MAX_FAILURE_LOG);
    // First entry is from the *last* MAX_FAILURE_LOG events.
    assert!(g.first().unwrap().host.starts_with("h"));
}

#[test]
fn anchor_strings_compiles() {
    _anchor_strings();
}

/// gc-common w19-d: the address, its holder, the proxy and the list are
/// built with every earlier part rooted across the next allocation. The
/// mock never moves an object at an allocation, so this checks the
/// assembled shape and that no root is left behind.
#[test]
fn w19d_the_proxy_list_holds_the_proxy_and_its_address() {
    let mut ctx = MockNativeContext::new();
    let isa = alloc_inet_socket_address(&mut ctx, "proxy.corp", 3128).unwrap();
    let holder = match ctx.get_field(isa, ISA_HOST) {
        Value::Object(Some(h)) => h,
        other => panic!("expected the holder, got {other:?}"),
    };
    match ctx.get_field(holder, 0) {
        Value::Object(Some(s)) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("proxy.corp"))
        }
        other => panic!("expected the host String, got {other:?}"),
    }
    assert_eq!(ctx.get_field(holder, 2), Value::Int(3128));
    assert_eq!(ctx.get_field(isa, ISA_PORT), Value::Int(3128));

    let proxy = alloc_proxy(&mut ctx, PROXY_TYPE_HTTP, Some(isa)).unwrap();
    assert_eq!(
        ctx.get_field(proxy, PROXY_TYPE),
        Value::Int(PROXY_TYPE_HTTP)
    );
    assert_eq!(
        ctx.get_field(proxy, PROXY_ADDRESS),
        Value::Object(Some(isa))
    );

    let list = alloc_proxy_list(&mut ctx, &[proxy]).unwrap();
    assert_eq!(ctx.get_field(list, 0), Value::Int(1));
    match ctx.get_field(list, 1) {
        Value::Object(Some(backing)) => {
            assert_eq!(
                ctx.get_array_element(backing, 0),
                Value::Object(Some(proxy))
            )
        }
        other => panic!("expected the backing array, got {other:?}"),
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
