/// `URI.resolve` must ROOT a merged path against an authority-bearing
/// absolute base. Measured against HotSpot 25.0.3+9 with
/// `probes/UriLocaleSweep.java`; the first row is the defect --- it
/// answered `http://hostx`, folding a relative reference into the HOST.
#[test]
fn uri_resolve_roots_a_merged_path_against_an_authority() {
    use super::uri_resolve_ref;
    assert_eq!(
        uri_resolve_ref("http://host", "x").unwrap(),
        "http://host/x"
    );
    assert_eq!(
        uri_resolve_ref("http://host", "a/b").unwrap(),
        "http://host/a/b"
    );
    // an authority-bearing base WITH a path keeps working
    assert_eq!(
        uri_resolve_ref("http://host/", "x").unwrap(),
        "http://host/x"
    );
    assert_eq!(
        uri_resolve_ref("http://host/a/b", "c").unwrap(),
        "http://host/a/c"
    );
    assert_eq!(
        uri_resolve_ref("http://host/a/", "c").unwrap(),
        "http://host/a/c"
    );
    // an EMPTY child must NOT gain a slash --- OpaqueUriProbe S17
    assert_eq!(uri_resolve_ref("https://h", "").unwrap(), "https://h");
    // no authority, so nothing to fold into: left alone
    assert_eq!(uri_resolve_ref("rel/path", "x").unwrap(), "rel/x");
    // an absolute reference path is verbatim
    assert_eq!(
        uri_resolve_ref("http://host/a/b", "/c").unwrap(),
        "http://host/c"
    );
}

use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// An OPAQUE URI has no query — the `?` inside its scheme-specific part is
/// an ordinary character.
///
/// Every row is a MEASURED HotSpot 25.0.3+9 `getRawQuery()` from
/// `probes/OpaqueUriProbe.java`, so a future reader can re-run rather than
/// re-derive. `mailto:user@example.com?subject=hello` is the one that broke
/// `WebClientUtilsTests.opaqueUriUnchanged`.
#[test]
fn an_opaque_uri_has_no_query() {
    let q = |s: &str| {
        let u: Vec<u16> = s.encode_utf16().collect();
        uri_query_units(&u).map(|v| String::from_utf16_lossy(&v))
    };
    // MEASURED null on HotSpot — opaque.
    assert_eq!(q("mailto:user@example.com?subject=hello"), None);
    assert_eq!(q("mailto:a@b.com?s=1#frag"), None);
    assert_eq!(q("classpath:foo/bar.xml?a=b"), None);
    assert_eq!(q("http:comp.lang.java?q=1"), None);
    assert_eq!(q("a:b/c?d=e"), None);
    assert_eq!(q("mailto:?subject=hello"), None);
    // MEASURED non-null on HotSpot — hierarchical, including the relative
    // reference whose colon sits inside a path segment.
    assert_eq!(
        q("https://api.example.com/search?q=test&page=1"),
        Some("q=test&page=1".into())
    );
    assert_eq!(q("https://host/page?q=1#section"), Some("q=1".into()));
    assert_eq!(q("/api/search?q=test"), Some("q=test".into()));
    assert_eq!(q("file:/tmp/x?a=b"), Some("a=b".into()));
    assert_eq!(q("/redirect:account?q=1"), Some("q=1".into()));
    assert_eq!(q("relative/path?q=1#f"), Some("q=1".into()));
    // A `?` after the `#` belongs to the fragment, on both shapes.
    assert_eq!(q("https://host/page#frag?param=value"), None);
    assert_eq!(q("https://host/page"), None);
}

/// `isOpaque()` / `isAbsolute()` must ask the SCHEME rule, not whether a
/// colon appears anywhere. MEASURED rows from the same probe.
#[test]
fn a_colon_in_a_relative_path_is_not_a_scheme() {
    assert!(uri_text_is_opaque("mailto:user@example.com?subject=hello"));
    assert!(uri_text_is_opaque("urn:isbn:096139210x"));
    assert!(uri_text_is_opaque("a:b/c?d=e"));
    // H11 — HotSpot: isOpaque=false, isAbsolute=false. Both read `true`
    // before this change.
    assert!(!uri_text_is_opaque("/redirect:account?q=1"));
    assert!(uri_scheme_colon("/redirect:account?q=1").is_none());
    // H12 — a relative reference whose FIRST segment carries the colon IS
    // parsed as a scheme by the JDK (`redirect` is a legal scheme name).
    assert!(uri_text_is_opaque("redirect:account/x"));
    // Hierarchical and relative controls.
    assert!(!uri_text_is_opaque("https://host/page?q=1"));
    assert!(!uri_text_is_opaque("file:/tmp/x"));
    assert!(!uri_text_is_opaque("/api/search?q=test"));
    assert!(!uri_text_is_opaque(""));
    assert!(uri_scheme_colon("https://host/page").is_some());
    assert!(uri_scheme_colon("relative/path?q=1#f").is_none());
    // A scheme name may not start with a digit.
    assert!(uri_scheme_colon("1abc:x").is_none());
}

/// An opaque base has no path to merge, so RFC 3986 §5 resolution does not
/// apply and `URI.resolve` hands back the child unchanged — its literal
/// first line, `if (child.isOpaque() || base.isOpaque()) return child`.
#[test]
fn resolving_against_an_opaque_base_yields_the_reference() {
    let r = |b: &str, c: &str| uri_resolve_ref(b, c).unwrap();
    // R01/R06/R07 — MEASURED on HotSpot.
    assert_eq!(r("mailto:a@b", "c@d"), "c@d");
    assert_eq!(r("mailto:a@b", "https://h/p"), "https://h/p");
    assert_eq!(r("mailto:a@b", ""), "");
    // R08 — an OPAQUE reference is returned verbatim off a hierarchical
    // base, query and all.
    assert_eq!(r("https://h/a/b", "mailto:c@d?x=1"), "mailto:c@d?x=1");
    // The hierarchical path is untouched by the short-circuit.
    assert_eq!(r("https://h/a/b", "c"), "https://h/a/c");
    assert_eq!(r("https://h/a/b", "/d"), "https://h/d");
}

/// `java.net.URI.normalize` does not clamp `..` at the root.
///
/// Every row MEASURED on HotSpot 25 — `probes/OpaqueUriProbe.java`'s
/// N-block. RFC 3986's `remove_dot_segments` discards a leading `..`,
/// which is what this used to do.
#[test]
fn a_leading_dotdot_survives_normalization() {
    let n = |s: &str| uri_remove_dot_segments(s).unwrap();
    assert_eq!(n("/a/../../x"), "/../x", "N02");
    assert_eq!(n("a/../../b"), "../b", "N04");
    assert_eq!(n("a/b/../../../c"), "../c", "N10");
    assert_eq!(n("/../x"), "/../x", "N08");
    // And the ordinary cases are untouched.
    assert_eq!(n("/a/b/../c"), "/a/c", "N03");
    assert_eq!(n("/a/./b"), "/a/b", "N05");
    assert_eq!(n("/a/b/.."), "/a/", "N06");
    assert_eq!(n("/a/b/."), "/a/b/", "N07");
    assert_eq!(n("/a//b"), "/a/b", "N11");
    assert_eq!(n("./a/b"), "a/b", "N12");
    assert_eq!(n(""), "");
}

/// `URI.resolve` is RFC 2396. Every row is a MEASURED S-row.
#[test]
fn resolve_follows_the_jdks_rfc2396_arms() {
    let r = |b: &str, c: &str| uri_resolve_ref(b, c).unwrap();
    let base = "https://h/a/b?q=1";
    // An EMPTY reference keeps the base's DIRECTORY and drops its query —
    // there is no "return the base" shortcut in the JDK.
    assert_eq!(r(base, ""), "https://h/a/", "S01");
    // A LONE FRAGMENT is the one arm that keeps path AND query.
    assert_eq!(r(base, "#f"), "https://h/a/b?q=1#f", "S02");
    // A query-only reference does NOT qualify for that arm.
    assert_eq!(r(base, "?q=2"), "https://h/a/?q=2", "S03");
    assert_eq!(r(base, "?q=2#f"), "https://h/a/?q=2#f", "S04");
    assert_eq!(r(base, "."), "https://h/a/", "S05");
    assert_eq!(r(base, ".."), "https://h/", "S06");
    assert_eq!(r(base, "c"), "https://h/a/c", "S08");
    // An AUTHORITY, an ABSOLUTE PATH and a SCHEME are each taken verbatim.
    assert_eq!(r(base, "//other/p/../q"), "https://other/p/../q", "S10");
    assert_eq!(r(base, "https://x/p/../q"), "https://x/p/../q", "S11");
    assert_eq!(r(base, "/p/../q"), "https://h/p/../q", "S25");
    assert_eq!(r(base, "/p/./q"), "https://h/p/./q", "S26");
    // The empty-reference directory step, at every path shape.
    assert_eq!(r("https://h/a/", ""), "https://h/a/", "S14");
    assert_eq!(r("https://h/a", ""), "https://h/", "S15");
    assert_eq!(r("https://h/", ""), "https://h/", "S16");
    assert_eq!(r("https://h", ""), "https://h", "S17");
    assert_eq!(r("/base/path?q=1", ""), "/base/", "S22");
    assert_eq!(r("file:/a/b", ""), "file:/a/", "S30");
    // `..` survives the root here too.
    assert_eq!(r("https://h/a/b", "../../x"), "https://h/../x", "S27");
    // An opaque base or reference short-circuits (R01/S13/S24).
    assert_eq!(r("mailto:a@b", "c@d"), "c@d");
    assert_eq!(r("mailto:a@b", "#f"), "#f", "S24");
    assert_eq!(r(base, "mailto:c@d?x=1"), "mailto:c@d?x=1", "S13");
}

/// The VM these DatagramSocket-table tests stamp their rows with. Private
/// to them (real identities count from 1; mock contexts answer 0).
const DS_TEST_VM: usize = 0x7A80_0501;

/// Behavioural cover for [`gc_scan_ds_roots`] / [`gc_update_ds_refs`].
///
/// `native_roots`' `datagram_socket_side_tables_are_a_registered_root_source`
/// asserts the row is *present in the inventory*, which is the half that
/// catches deletion. It cannot catch the pair being WRONG — a `rekey` that
/// re-keyed only `ds_side_table`, or dropped entries whose key was absent
/// from the pointer map, would leave that test green and put the socket
/// state back exactly where it was before the fix: `ds_get` missing and
/// answering `ds_default()`, so a live connected socket reports `fd = -1`
/// and `send()` fails with "DatagramSocket: closed".
///
/// Addresses are fabricated and never dereferenced — the remap only reads
/// `as_ptr()`, the scan only copies the key — and the test removes what it
/// inserted, since both tables are process-global.
#[test]
fn gc_update_ds_refs_rekeys_both_tables_and_keeps_their_values() {
    // SAFETY: used purely as map keys / `as_ptr()` operands, never read.
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let (old, new) = (at(0x51_0000), at(0x52_0000));

    ds_set(DS_TEST_VM, old, |d| {
        d.fd = 7;
        d.timeout = 4321;
        d.connected = 1;
        d.port = 53498;
    });
    ds_set_peer(DS_TEST_VM, old, "127.0.0.1", 9999);

    // gc-common w17-a: the rows are weak. The scan roots nothing; the
    // owner-death sweep (`sweep_object_keyed_rows`) is what stops a dead
    // socket's entry colliding with the next object on its address.
    let mut roots = Vec::new();
    gc_scan_ds_roots(DS_TEST_VM, &mut roots);
    assert!(
        roots.is_empty(),
        "the datagram rows must not root their socket"
    );

    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(old.as_ptr() as usize, new.as_ptr() as usize);
    gc_update_ds_refs(DS_TEST_VM, &pointer_map);

    let moved = ds_get(new);
    assert_eq!(moved.fd, 7, "fd must follow the mirror, not reset to -1");
    assert_eq!(moved.timeout, 4321);
    assert_eq!(moved.connected, 1);
    assert_eq!(moved.port, 53498);
    assert_eq!(
        ds_peer(new),
        Some(("127.0.0.1".to_string(), 9999)),
        "ds_peer_table must be re-keyed too, not just ds_side_table"
    );
    assert_eq!(
        ds_get(old).fd,
        -1,
        "the from-space key must be gone, not duplicated"
    );
    assert_eq!(ds_peer(old), None);

    ds_side_table().lock().remove(&new);
    ds_peer_table().lock().remove(&new);
}

/// A non-moving collection publishes an empty pointer map, and must leave
/// both tables exactly as they were — an unconditional drain-and-reinsert
/// that mishandled the empty case would silently clear live socket state.
#[test]
fn gc_update_ds_refs_is_a_no_op_when_nothing_moved() {
    // SAFETY: key-only, as above.
    let at = |a: usize| unsafe { ObjectRef::from_raw(a as *mut u8) };
    let sock = at(0x53_0000);
    ds_set(DS_TEST_VM, sock, |d| d.fd = 11);
    ds_set_peer(DS_TEST_VM, sock, "10.0.0.1", 1234);

    gc_update_ds_refs(DS_TEST_VM, &cratonvm_types::PointerMap::default());

    assert_eq!(ds_get(sock).fd, 11);
    assert_eq!(ds_peer(sock), Some(("10.0.0.1".to_string(), 1234)));

    ds_side_table().lock().remove(&sock);
    ds_peer_table().lock().remove(&sock);
}

#[test]
fn re1_http_parse_url_plain() {
    let (https, host, port, path, userinfo) = http_parse_url("http://example.com/foo").unwrap();
    assert!(!https);
    assert_eq!(host, "example.com");
    assert_eq!(port, 80);
    assert_eq!(path, "/foo");
    assert_eq!(userinfo, None);
}

#[test]
fn re1_http_parse_url_with_port() {
    let (https, host, port, path, userinfo) =
        http_parse_url("https://example.com:8443/api?x=1").unwrap();
    assert!(https);
    assert_eq!(host, "example.com");
    assert_eq!(port, 8443);
    assert_eq!(path, "/api?x=1");
    assert_eq!(userinfo, None);
}

#[test]
fn re1_http_parse_url_with_userinfo() {
    // user-info must be stripped from the connect target / Host header and
    // returned separately (ResourceTests.useUserInfoToSetBasicAuth).
    let (https, host, port, path, userinfo) =
        http_parse_url("http://alice:secret@localhost:8080/resource").unwrap();
    assert!(!https);
    assert_eq!(host, "localhost");
    assert_eq!(port, 8080);
    assert_eq!(path, "/resource");
    assert_eq!(userinfo.as_deref(), Some("alice:secret"));
}

#[test]
fn re1_http_parse_url_query_only_and_fragment_do_not_extend_authority() {
    let (https, host, port, path, userinfo) =
        http_parse_url("http://alice:secret@localhost:8080?trace=false&message=false").unwrap();
    assert!(!https);
    assert_eq!(host, "localhost");
    assert_eq!(port, 8080);
    assert_eq!(path, "/?trace=false&message=false");
    assert_eq!(userinfo.as_deref(), Some("alice:secret"));

    let (_, host, port, path, _) =
        http_parse_url("https://example.com:8443#client-only").unwrap();
    assert_eq!(host, "example.com");
    assert_eq!(port, 8443);
    assert_eq!(path, "/");
}

#[test]
fn re1_field5_full_url_discriminator() {
    // Full URLs (scheme-carrying) are accepted…
    assert!(field5_is_full_url("http://localhost:8080/x"));
    assert!(field5_is_full_url("file:/tmp/x.txt"));
    assert!(field5_is_full_url("jar:file:/a.jar!/e"));
    // …while authorities are rejected: bare host:port (digits after ':')
    // and user-info-carrying authorities ('@' before any '/').
    assert!(!field5_is_full_url("localhost:8080"));
    assert!(!field5_is_full_url("alice:secret@localhost:8080"));
    assert!(!field5_is_full_url("alice@localhost:8080"));
}

#[test]
fn re4_url_components_matches_java_net_url_grammar() {
    let (proto, host, port, path, query, reff) =
        url_components("https://example.com:8080/path?key=value#frag");
    assert_eq!(proto, "https");
    assert_eq!(host, "example.com");
    assert_eq!(port, 8080);
    assert_eq!(path, "/path");
    assert_eq!(query.as_deref(), Some("key=value"));
    assert_eq!(reff.as_deref(), Some("frag"));

    // No authority, no port, no query, no fragment.
    let (proto, host, port, path, query, reff) = url_components("file:/tmp/x.txt");
    assert_eq!(proto, "file");
    assert_eq!(host, "");
    assert_eq!(port, -1);
    assert_eq!(path, "/tmp/x.txt");
    assert_eq!(query, None);
    assert_eq!(reff, None);

    // user-info is stripped from the host; IPv6 literals keep their
    // brackets and only the post-`]` colon is a port.
    let (_, host, port, ..) = url_components("http://alice:secret@localhost:8080/x");
    assert_eq!(host, "localhost");
    assert_eq!(port, 8080);
    let (_, host, port, ..) = url_components("http://[::1]:9090/x");
    assert_eq!(host, "[::1]");
    assert_eq!(port, 9090);
    let (_, host, port, ..) = url_components("http://[::1]/x");
    assert_eq!(host, "[::1]");
    assert_eq!(port, -1);

    // A spec with no scheme yields an empty protocol — the signal
    // `URL.<init>` uses to raise MalformedURLException.
    assert_eq!(url_components("not a url").0, "");
}

#[test]
fn re4_url_scheme_token_rejects_full_urls_and_authorities() {
    assert!(url_is_scheme_token("https"));
    assert!(url_is_scheme_token("jar"));
    assert!(!url_is_scheme_token("file:/tmp/x"));
    assert!(!url_is_scheme_token("localhost:8080"));
    assert!(!url_is_scheme_token(""));
    assert!(!url_is_scheme_token("8080"));
}

#[test]
fn uri_split_keeps_opaque_question_mark_inside_ssp() {
    let (scheme, authority, path, query, fragment) =
        uri_split("mailto:user@example.com?subject=hello#frag");

    assert_eq!(scheme.as_deref(), Some("mailto"));
    assert_eq!(authority, None);
    assert_eq!(path, "user@example.com?subject=hello");
    assert_eq!(query, None);
    assert_eq!(fragment.as_deref(), Some("frag"));
}

/// Helper for the authority family: `(userInfo, host, port)` as three
/// `Option`-free strings so a row reads like the oracle's own output.
fn auth_row(authority: &str) -> (String, String, i32) {
    let (u, h, p) = uri_parse_authority(authority);
    (
        u.unwrap_or_else(|| "null".into()),
        h.unwrap_or_else(|| "null".into()),
        p,
    )
}

const DEMOTED: (&str, &str, i32) = ("null", "null", -1);

fn assert_demoted(authority: &str) {
    let (u, h, p) = auth_row(authority);
    assert_eq!(
        (u.as_str(), h.as_str(), p),
        DEMOTED,
        "authority {authority:?} must demote to registry-based: HotSpot answers \
         null/null/-1 for getUserInfo/getHost/getPort together"
    );
}

/// The blocker behind `RJdkBridge1`'s check 197 (`RJdkBridge1.java:1181`).
///
/// MEASURED on Temurin 25.0.3+9-LTS, 2026-08-17: `java.net.URI`'s port
/// production is `*DIGIT`. Rust's `str::parse::<i32>` takes a leading `+`
/// or `-`, and the old body ended in `p.parse::<i32>().ok()`, so
/// `http://h:-5/p` answered `-5` where HotSpot answers `-1`.
///
/// The half that a port-only clamp would have missed is on the next line
/// of the vector: a failed server-based parse does not just drop the port,
/// it demotes the whole authority to registry-based, so `getHost()` is
/// `null` too.
#[test]
fn uri_authority_port_is_digits_only_and_a_bad_port_demotes_the_whole_authority() {
    for bad in [
        "h:-5",          // leading '-'
        "h:+80",         // leading '+', which java.net.URL DOES take
        "h:-0",          // sign, even on a value that would parse
        "h:8x",          // trailing non-digit
        "h:x80",         // leading non-digit
        "h:x",           // no digits at all
        "h:80x80",       // digits either side of a non-digit
        "h:99999999999", // all digits, overflows int
        "h:2147483648",  // one past Integer.MAX_VALUE
        "h:4294967296",  // and past the unsigned range too
        "h:80:90",       // the port text runs to the END of the authority
        "h::80",         // so a second colon is a non-digit, not a split
        "u@h:x",         // userInfo is discarded WITH the host and port
        "u:pw@h:8x",
        "u@1.2.3.4:8x",
    ] {
        assert_demoted(bad);
    }
}

/// The boundary rows that keep the demotion from over-firing. Leading
/// zeros are not part of the value and do not overflow anything.
#[test]
fn uri_authority_keeps_every_port_java_actually_accepts() {
    for (authority, expect_host, expect_port) in [
        ("h:80", "h", 80),
        ("h:0", "h", 0),
        ("h:007", "h", 7),
        ("h:080", "h", 80),
        ("h:00000000080", "h", 80),
        ("h:0000000000000", "h", 0),
        ("h:2147483647", "h", 2147483647),
        ("h:", "h", -1),
        ("h", "h", -1),
        ("[::1]:80", "[::1]", 80),
        ("[::1]:", "[::1]", -1),
        ("[::1]:0", "[::1]", 0),
        ("[::1]", "[::1]", -1),
        ("1.2.3.4:80", "1.2.3.4", 80),
    ] {
        let (u, h, p) = auth_row(authority);
        assert_eq!(
            (u.as_str(), h.as_str(), p),
            ("null", expect_host, expect_port),
            "authority {authority:?}"
        );
    }
}

/// An EMPTY host demotes exactly like a bad port — `parseHostname`'s
/// `if (l < 0) failExpecting("hostname", start)`. MEASURED:
/// `new URI("http://:80/p").getPort()` is `-1` on HotSpot, not `80`, and
/// `new URI("http://u@:80/p").getUserInfo()` is `null`, not `u`.
#[test]
fn uri_authority_with_no_host_demotes_even_though_the_port_is_well_formed() {
    assert_demoted(":80");
    assert_demoted("u@:80");
    assert_demoted("");
    assert_demoted(".a");
    assert_demoted("a..");
}

/// `parseHostname`'s grammar, which is NOT "anything that is not a port".
/// MEASURED, HotSpot answers a null host for every row here.
#[test]
fn uri_authority_hostname_grammar_rejects_what_java_rejects() {
    for bad in [
        "a_b",       // '_' is legal in a reg-name, not in a domain label
        "a..b",      // empty label
        "-h",        // label must START alphanumeric
        "h-",        // and must not END with '-'
        "a.9b",      // multi-label: the LAST label must start with a letter
        "1.2.3",     // three octets is not an IPv4 address, and "3" is a
        "1.2.3.4.5", //  digit-leading last label, so neither production fits
        "1.2.3.4x",
        "256.1.1.1",
        "192.196.0.5555",
        "h$x",
        "h,x",
        "h;x",
        "h=x",
        "h&x",
        "h!x",
        "h~x",
        "h*x",
        "h'x",
        "h(x)",
        "h%20x",
        "h%41x", // an escape is legal in a reg-name, not in a host
        "a@b@c", // the FIRST '@' delimits, so "b@c" must parse as a host
        "u@h@",
        "u@h:80@x",
    ] {
        assert_demoted(bad);
    }
}

/// …and the hostnames it must keep. `9h` and `12` are single labels, where
/// the "starts with a letter" rule does not apply (`l > start` is false).
#[test]
fn uri_authority_hostname_grammar_keeps_what_java_keeps() {
    for (authority, expect_host) in [
        ("h.", "h."),
        ("a.b.", "a.b."),
        ("9h", "9h"),
        ("h9", "h9"),
        ("12", "12"),
        ("a.b9", "a.b9"),
        ("a-b", "a-b"),
        ("a-b.c-d", "a-b.c-d"),
        ("xn--d1acufc.xn--p1ai", "xn--d1acufc.xn--p1ai"),
        ("A.B", "A.B"),
        ("1.2.3.4", "1.2.3.4"),
        ("255.255.255.255", "255.255.255.255"),
        ("01.2.3.4", "01.2.3.4"), // leading zeros are not significant digits
    ] {
        let (_, h, p) = auth_row(authority);
        assert_eq!(
            (h.as_str(), p),
            (expect_host, -1),
            "authority {authority:?}"
        );
    }
}

/// User-info survives only when the whole server-based parse survives.
#[test]
fn uri_authority_user_info_is_taken_at_the_first_at_sign() {
    assert_eq!(
        auth_row("u:pw@h:80"),
        ("u:pw".to_string(), "h".to_string(), 80)
    );
    assert_eq!(auth_row("u@h"), ("u".to_string(), "h".to_string(), -1));
    // An empty user-info is not an absent one: HotSpot's
    // `new URI("http://@h/p").getUserInfo()` is "".
    assert_eq!(auth_row("@h"), (String::new(), "h".to_string(), -1));
    // Escapes are legal in a user info; `getUserInfo` decodes them.
    assert_eq!(
        auth_row("a%40b@h"),
        ("a%40b".to_string(), "h".to_string(), -1)
    );
    assert_eq!(
        auth_row("u@[::1]:80"),
        ("u".to_string(), "[::1]".to_string(), 80)
    );
}

/// A second handshake recorded against the same carrier must DROP the
/// cached session handle, so the next accessor mints a session for the new
/// handshake instead of handing back the previous one, and so the old
/// global root is released rather than pinning that object for the life of
/// the VM.
///
/// The mock context has no moving collector, so `add_global_root` is a
/// no-op returning 0 there; the handle slot is therefore driven directly.
/// What this test can see — and what a live run cannot easily be made to
/// show — is the INVALIDATION, which is the half that leaks if it is
/// missing.
#[test]
fn re_recording_a_carrier_handshake_drops_the_cached_session_handle() {
    let mut ctx = MockNativeContext::new();
    let carrier = ctx.alloc_object(ClassId::new(0), 4);
    let key = mint_native_obj_key(&ctx, carrier);

    record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.3",
        "TLS_AES_256_GCM_SHA384",
        &[],
        "example.test",
        443,
    );
    let first = https_carrier_session(&ctx, carrier).expect("the handshake was recorded");
    assert_eq!(first.protocol, "TLSv1.3");
    assert_eq!(first.cipher, "TLS_AES_256_GCM_SHA384");
    assert_eq!(
        first.session_root, 0,
        "a freshly recorded handshake has no session object yet"
    );

    // Stand in for "an accessor minted and rooted the session".
    https_carrier_sessions()
        .lock()
        .get_mut(&key)
        .expect("entry")
        .session_root = 7;
    assert_eq!(
        https_carrier_session(&ctx, carrier).unwrap().session_root,
        7
    );

    record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.2",
        "TLS_RSA_WITH_AES_128",
        &[],
        "example.test",
        443,
    );
    let second = https_carrier_session(&ctx, carrier).expect("still recorded");
    assert_eq!(second.protocol, "TLSv1.2");
    assert_eq!(
        second.session_root, 0,
        "a new handshake must not hand out the previous handshake's SSLSession"
    );
}

/// SOURCE WITNESS — the real-JDK carriers must be initialised BY NAME.
///
/// `URL.openConnection()` hands back an instance of a real JDK class
/// (`HttpsURLConnectionImpl`, or the abstract `java/net/HttpURLConnection`),
/// and this file's `HUC_*` constants are a synthetic slot map that does not
/// match the JDK's layout. Four indexed writes landed on `url`, `doInput`,
/// `connectTimeout` and `requests`; the coercion guard caught two of them
/// and the other two were silent, because an `Int(1)` into `connectTimeout`
/// is a perfectly well-typed 1-millisecond timeout.
///
/// This cannot be asserted behaviourally here: the mock context has no
/// real class layout, so an indexed write and a by-name write are the same
/// operation to it — which is exactly why the bug survived a full unit
/// suite. The witness is against the source, deliberately, and it is
/// scoped to the carrier branch rather than the whole file: the `jrt:`
/// carrier a few lines above IS a CratonVM synthetic with 16 slots of our
/// own, and indexed writes are correct there.
#[test]
fn the_real_jdk_carriers_are_initialised_by_field_name() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("net_phase_e.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("net_phase_e.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

    let anchor = lines
        .iter()
        .position(|l| l.contains(r#""sun/net/www/protocol/https/HttpsURLConnectionImpl""#))
        .expect("the https carrier class must still be named here");
    let alloc = lines[anchor..]
        .iter()
        .position(|l| l.contains("try_alloc_concurrent_synthetic(ctx, carrier"))
        .map(|i| anchor + i)
        .expect("the carrier allocation must still follow the class choice");
    // The initialisation runs until the closure hands the object back.
    let end = lines[alloc..]
        .iter()
        .position(|l| l.contains("Ok(Some(Value::Object(Some(conn))))"))
        .map(|i| alloc + i)
        .expect("the carrier must still be returned");

    let indexed: Vec<&&str> = lines[alloc..end]
        .iter()
        .filter(|l| l.contains("ctx.set_field(conn,"))
        .collect();
    assert!(
        indexed.is_empty(),
        "a real-JDK carrier is being written by SLOT INDEX: {indexed:?}. Those \
         constants are this file's synthetic map and do not match the JDK's field \
         layout — MEASURED, slot 1 is URLConnection.doInput and slot 9 is \
         URLConnection.requests. Use ctx.set_field_by_name."
    );
    // `url` is the one field only the mint site can supply; the rest of
    // the declared initial state moved into
    // `http_url_connection::huc_write_declared_field_defaults`, shared
    // with `huc_init`, so that a subclass carrier and a minted one cannot
    // disagree about what a fresh connection looks like. The assertion is
    // the same claim as before — nothing here is written by SLOT — plus
    // the requirement that the shared list is actually called.
    assert!(
        lines[alloc..end]
            .iter()
            .any(|l| l.contains(r#"ctx.set_field_by_name(conn, "url""#)),
        "the carrier's own `url` must still be written by name"
    );
    assert!(
        lines[alloc..end]
            .iter()
            .any(|l| l.contains("huc_write_declared_field_defaults")),
        "the carrier must still be given the field values its constructor \
         would have written — `URLConnection`/`HttpURLConnection` declare four \
         `-1` sentinels that a zeroed carrier reads as ALREADY SET"
    );
}

/// G51-1 N1 — the carrier must CARRY the dialled endpoint, because the
/// object that needs it is minted long after the request returned.
///
/// `https_session_object` runs at the first ACCESSOR call. By then
/// `perform` has returned, its `Url1` is gone, and — as G51-1 §2 measured
/// on `RSslLiveSession`'s own `distinct` family — a second connection may
/// already have been opened. Anything that recovers the endpoint at mint
/// time rather than carrying it from the handshake reports one
/// connection's peer for another's session. This test is what stops the
/// two fields being quietly dropped as unused.
#[test]
fn a_recorded_handshake_carries_the_endpoint_the_url_named() {
    let mut ctx = MockNativeContext::new();
    let carrier = ctx.alloc_object(ClassId::new(0), 4);

    record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.3",
        "TLS_AES_256_GCM_SHA384",
        &[],
        "127.0.0.1",
        45123,
    );

    let s = https_carrier_session(&ctx, carrier).expect("the handshake was recorded");
    assert_eq!(
        s.peer_host, "127.0.0.1",
        "the host is the one the URL NAMED. G51-1 §1 measured HotSpot answering the \
         IP literal for a request to https://127.0.0.1/, against a leaf subject of \
         CN=localhost and an SNI of localhost — so neither the certificate nor SNI \
         may be used to derive it"
    );
    assert_eq!(s.peer_port, 45123);
}

/// SOURCE WITNESS — `https_session_object` must write the endpoint it
/// carries into the side table the readers consult.
///
/// This cannot be asserted behaviourally from here: `https_session_object`
/// allocates a real `javax/net/ssl/SSLSession` through
/// `try_alloc_concurrent_synthetic` and publishes it under a global root,
/// none of which the mock context models. What CAN be checked is that the
/// call is present and that it sits after the session is re-read from its
/// pin — a `record_session_peer_endpoint` on a stale `ObjectRef` would key
/// the table on a vacated from-space address and answer nothing, which is
/// indistinguishable from the bug this closes.
#[test]
fn the_minted_session_records_its_endpoint() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("net_phase_e.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("net_phase_e.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

    let fn_start = lines
        .iter()
        .position(|l| l.contains("fn https_session_object("))
        .expect("https_session_object must still exist");
    let fn_end = lines
        .iter()
        .enumerate()
        .skip(fn_start)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i)
        .expect("https_session_object must be terminated");
    let body = &lines[fn_start..fn_end];

    let write = body
        .iter()
        .position(|l| l.contains("record_session_peer_endpoint("))
        .expect(
            "https_session_object must record the dialled endpoint; without it \
             getPeerHost()/getPeerPort() answer null/-1 on every HTTPS client \
             session — slot 2 is HTTPS_CLIENT_SESSION_MARKER, chosen so that the \
             socket-registry fallback misses BY DESIGN, so the side table is the \
             only place the answer can come from",
        );
    let reread = body[..write]
        .iter()
        .rposition(|l| l.contains("read_native_pin(session_pin, session0)"))
        .expect("the session must be re-read from its pin before it is used");
    assert!(
        body[reread + 1..write]
            .iter()
            .all(|l| !l.contains("ctx.create_string(") && !l.contains("try_alloc")),
        "an allocation sits between the session's re-read at body line {reread} and \
         record_session_peer_endpoint at body line {write} — a moving young GC there \
         leaves the table keyed on a vacated address"
    );
}

/// Recycling a carrier removes its entry outright, so the six accessors go
/// back to answering `IllegalStateException: connection not yet open` —
/// HotSpot's own answer after a drained body — and the cached session stops
/// being held.
#[test]
fn forgetting_a_recycled_carrier_removes_the_entry_entirely() {
    let mut ctx = MockNativeContext::new();
    let carrier = ctx.alloc_object(ClassId::new(0), 4);
    record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.3",
        "TLS_AES_128_GCM_SHA256",
        &[],
        "example.test",
        443,
    );
    assert!(https_carrier_session(&ctx, carrier).is_some());

    forget_https_carrier_session(&mut ctx, carrier);
    assert!(
        https_carrier_session(&ctx, carrier).is_none(),
        "a recycled carrier must have no recorded handshake left — a stale entry is how \
         a connection that HotSpot reports as closed keeps answering"
    );
    // Idempotent: draining twice, or draining a connection that never
    // handshaked, must not panic and must not release a handle twice.
    forget_https_carrier_session(&mut ctx, carrier);
}

/// `https_session_object` must consult the cache BEFORE it allocates.
///
/// The order is the whole fix: an allocation-first body that only *then*
/// looked at the table would still mint one `SSLSession` per accessor call
/// and only cache the last one, which is exactly the shape that made
/// `RSslLiveSession`'s `client.sslSession.sameObjectTwice` answer `false`.
/// No behavioural test can see that without a live TLS peer, so the
/// ordering is asserted against the source — the same witness shape
/// `http_url_connection.rs` already uses for `huc_verify_hostname`'s
/// capture-before-early-return rule.
#[test]
fn the_session_cache_lookup_precedes_the_allocation() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("net_phase_e.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("net_phase_e.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let fn_start = lines
        .iter()
        .position(|l| l.starts_with("fn https_session_object("))
        .expect("https_session_object must still exist");
    let fn_end = lines
        .iter()
        .enumerate()
        .skip(fn_start)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i)
        .expect("https_session_object must be terminated");
    let body = &lines[fn_start..fn_end];

    let lookup = body
        .iter()
        .position(|l| l.contains("https_cached_session_object("))
        .expect(
            "https_session_object must consult the per-carrier cache; without it every \
             accessor mints a new SSLSession and getSSLSession() is not idempotent",
        );
    let alloc = body
        .iter()
        .position(|l| l.contains("alloc_session_carrier("))
        .expect("https_session_object must still be the minter");
    assert!(
        lookup < alloc,
        "the cache lookup is at body line {lookup}, BELOW the allocation at {alloc} — \
         a session would be minted on every call and the cache would only ever record \
         the last one"
    );
    assert!(
        body.iter().any(|l| l.contains("add_global_root(")),
        "the minted session must be held by a GLOBAL root: a raw ObjectRef in \
         https_carrier_sessions would dangle after a moving collection"
    );
}

#[test]
fn uri_scheme_specific_part_excludes_fragment() {
    assert_eq!(
        uri_raw_scheme_specific_part("mailto:foo@bar.com#baz"),
        "foo@bar.com"
    );
    assert_eq!(
        uri_raw_scheme_specific_part("mailto:user@example.com?subject=hello"),
        "user@example.com?subject=hello"
    );
    assert_eq!(
        uri_raw_scheme_specific_part("https://example.com/foo?bar#baz"),
        "//example.com/foo?bar"
    );
}

#[test]
fn re1_http_build_request_adds_content_length_for_post() {
    let req = http_build_request("POST", "h", 80, "/", &[], b"hello", 80);
    let s = String::from_utf8_lossy(&req);
    assert!(s.contains("POST / HTTP/1.1"));
    assert!(s.contains("Host: h"));
    assert!(s.contains("Content-Length: 5"));
    assert!(s.ends_with("hello"));
}

/// nio20 wave 3, L1: an unknown-length body goes out chunked, and a
/// caller-framed `Transfer-Encoding` suppresses the `Content-Length`.
#[test]
fn r20_http_build_request_chunked_body_has_no_content_length() {
    let body = http_chunk_encode(b"hello");
    assert_eq!(body, b"5\r\nhello\r\n0\r\n\r\n");
    assert_eq!(http_chunk_encode(b""), b"0\r\n\r\n");
    let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
    let req = http_build_request("POST", "h", 80, "/", &headers, &body, 80);
    let s = String::from_utf8_lossy(&req);
    assert!(s.contains("Transfer-Encoding: chunked\r\n"), "{s}");
    assert!(!s.to_ascii_lowercase().contains("content-length"), "{s}");
    assert!(s.ends_with("5\r\nhello\r\n0\r\n\r\n"), "{s}");
}

#[test]
fn re5_http_request_timeout_bounds_delayed_response_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    });

    let error = match http_perform_request_with_timeout(
        "GET",
        &format!("http://127.0.0.1:{port}/slow"),
        &[],
        &[],
        Duration::from_millis(10),
        0,
        None,
        &mut http_no_server_trust_gate,
    ) {
        Ok(_) => panic!("a delayed response head must exceed the request deadline"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("timed out"));
    server.join().unwrap();
}

/// A one-connection rustls HTTPS server on loopback serving the throwaway
/// `t27_certs/server.*` identity. Answers `true` when it received request
/// bytes (and then answers `200 ok`), `false` when the client went away
/// after the handshake without sending any. Every socket wait is bounded.
fn w14a_one_shot_tls_server() -> (u16, std::thread::JoinHandle<bool>) {
    w15a_one_shot_tls_server(
        include_str!("../t27_certs/server.crt"),
        include_str!("../t27_certs/server.key"),
    )
}

/// [`w14a_one_shot_tls_server`] serving the given identity (gc-common
/// w15-a: the wrong-name case serves `server1.*`, issued to `foo.test`
/// only, to a client that dials `127.0.0.1`).
fn w15a_one_shot_tls_server(
    cert_pem: &str,
    key_pem: &str,
) -> (u16, std::thread::JoinHandle<bool>) {
    let cfg =
        crate::t27_tls::build_server_config_single_cert(cert_pem, key_pem, &[], false, None)
            .expect("the test server identity must load");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        let conn = rustls::ServerConnection::new(cfg).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        let mut got: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tls.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    got.extend_from_slice(&buf[..n]);
                    if got.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        if got.is_empty() {
            return false;
        }
        let _ = tls
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let _ = tls.flush();
        tls.conn.send_close_notify();
        let _ = tls.flush();
        true
    });
    (port, server)
}

/// The client config `HttpClient` builds over an `SSLContext` with custom
/// `TrustManager`s: the pass-through verifier, which accepts any chain.
fn w14a_trust_manager_shaped_client_config() -> Arc<ClientConfig> {
    crate::t27_tls::build_client_config_with_revocation(
        rustls::RootCertStore::empty(),
        &["http/1.1"],
        None,
        None,
        true,
    )
    .expect("the pass-through client config must build")
}

/// gc-common w14-a (common-w13a-httpclient-custom-trustmanager-is-never-
/// consulted): a rejecting server-trust gate fails the exchange after the
/// handshake and BEFORE any request byte reaches the server, with the
/// `TLS handshake: ` message `re5_do_request` maps to
/// `SSLHandshakeException`. Before the gate existed nothing consulted the
/// application's `TrustManager`s on this path, so the request went out.
#[test]
fn w14a_rejecting_server_trust_gate_fails_the_https_exchange() {
    let (port, server) = w14a_one_shot_tls_server();
    let mut seen_chain_len = 0usize;
    let mut reject = |_host: &str, _port: u16, chain: &[Vec<u8>]| -> std::io::Result<()> {
        seen_chain_len = chain.len();
        Err(std::io::Error::other(
            "TLS handshake: TrustManager rejected the peer certificate chain: test",
        ))
    };
    let result = http_perform_request_with_timeout(
        "POST",
        &format!("https://127.0.0.1:{port}/secret"),
        &[(
            "Authorization".to_string(),
            "Bearer do-not-leak".to_string(),
        )],
        b"credentials",
        Duration::from_secs(10),
        0,
        Some(w14a_trust_manager_shaped_client_config()),
        &mut reject,
    );
    let error = match result {
        Ok(resp) => panic!(
            "a rejecting TrustManager must fail the request, got status {}",
            resp.status
        ),
        Err(e) => e,
    };
    assert!(
        error.to_string().starts_with("TLS handshake: "),
        "the rejection must carry the SSLHandshakeException prefix: {error}"
    );
    assert!(
        seen_chain_len >= 1,
        "the gate must be handed the server's certificate chain"
    );
    assert!(
        !server.join().unwrap(),
        "no request byte may reach a server the TrustManager rejected"
    );
}

/// The accepting counterpart: the gate runs once for the one hop, sees the
/// server's chain, and the request completes as before.
#[test]
fn w14a_accepting_server_trust_gate_lets_the_https_exchange_complete() {
    let (port, server) = w14a_one_shot_tls_server();
    let mut calls = 0usize;
    let mut accept = |_host: &str, _port: u16, chain: &[Vec<u8>]| -> std::io::Result<()> {
        calls += 1;
        assert!(!chain.is_empty(), "the gate must see the server's chain");
        Ok(())
    };
    let resp = http_perform_request_with_timeout(
        "GET",
        &format!("https://127.0.0.1:{port}/ok"),
        &[],
        &[],
        Duration::from_secs(10),
        0,
        Some(w14a_trust_manager_shaped_client_config()),
        &mut accept,
    )
    .expect("an accepting TrustManager must let the request through");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");
    assert_eq!(calls, 1, "one hop, one trust decision");
    assert!(
        server.join().unwrap(),
        "the server must have received the request"
    );
}

/// A `ctx_trust_managers_table` key no test attaches managers to. The real
/// gate over it consults no Java manager (the list is empty) and applies
/// endpoint identification as JSSE's own default manager does, which is
/// exactly the half a plain `X509TrustManager` adds after it accepts.
const W15A_UNATTACHED_TRUST_KEY: u64 = 0x0A15_7E57_0000_0A15;

/// One `HttpClient`-shaped exchange against a loopback server serving
/// `cert_pem`/`key_pem`, dialled as `127.0.0.1`, through the REAL
/// `re5_server_trust_gate`. Returns the exchange result, whether the gate
/// kept a Java-side failure, and whether the server received request bytes.
fn w15a_exchange_through_the_real_gate(
    cert_pem: &str,
    key_pem: &str,
    verify_hostname: bool,
) -> (std::io::Result<HttpResponse>, bool, bool) {
    let (port, server) = w15a_one_shot_tls_server(cert_pem, key_pem);
    let mut ctx = MockNativeContext::new();
    let mut failure: Option<MethodCallFailed> = None;
    // No `SSLContext`: no engine is minted, so the managers (none here)
    // are consulted exactly as before gc-common w16-a.
    let mut gate = |host: &str, port: u16, chain: &[Vec<u8>]| -> std::io::Result<()> {
        re5_server_trust_gate(
            &mut ctx,
            Some(W15A_UNATTACHED_TRUST_KEY),
            None,
            host,
            port,
            verify_hostname,
            chain,
            &mut failure,
        )
    };
    let result = http_perform_request_with_timeout(
        "GET",
        &format!("https://127.0.0.1:{port}/w15a"),
        &[(
            "Authorization".to_string(),
            "Bearer do-not-leak".to_string(),
        )],
        &[],
        Duration::from_secs(10),
        0,
        Some(w14a_trust_manager_shaped_client_config()),
        &mut gate,
    );
    let kept_failure = failure.is_some();
    let server_got_request = server.join().unwrap();
    (result, kept_failure, server_got_request)
}

/// gc-common w15-a
/// (`common-w14a-httpclient-custom-trustmanager-skips-endpoint-identification`):
/// a certificate issued to another name fails the request, with the
/// `TLS handshake: ` prefix `re5_do_request` maps to `SSLHandshakeException`
/// and before any request byte (here, a bearer token) reaches the server.
/// `server1.*` names `foo.test` only; the client dialled `127.0.0.1`.
/// Before the fix the gate compared nothing against the host, so this
/// request succeeded.
#[test]
fn w15a_certificate_for_another_name_fails_the_https_exchange() {
    let (result, kept_failure, server_got_request) = w15a_exchange_through_the_real_gate(
        include_str!("../t27_certs/server1.crt"),
        include_str!("../t27_certs/server1.key"),
        true,
    );
    let error = match result {
        Ok(resp) => panic!(
            "a certificate for foo.test must not be accepted for 127.0.0.1, got status {}",
            resp.status
        ),
        Err(e) => e,
    };
    let message = error.to_string();
    assert!(
        message.starts_with("TLS handshake: endpoint identification"),
        "the rejection must name endpoint identification and carry the \
         SSLHandshakeException prefix: {message}"
    );
    assert!(
        kept_failure,
        "the gate must keep the exception for re5_do_request"
    );
    assert!(
        !server_got_request,
        "no request byte may reach a server whose certificate names another host"
    );
}

/// The right-name counterpart: `server.*` carries `IP:127.0.0.1`, so the
/// same exchange completes.
#[test]
fn w15a_certificate_for_the_dialled_name_passes_the_https_exchange() {
    let (result, kept_failure, server_got_request) = w15a_exchange_through_the_real_gate(
        include_str!("../t27_certs/server.crt"),
        include_str!("../t27_certs/server.key"),
        true,
    );
    let resp = result.expect("a certificate naming 127.0.0.1 must be accepted for it");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");
    assert!(!kept_failure);
    assert!(
        server_got_request,
        "the server must have received the request"
    );
}

/// `jdk.internal.httpclient.disableHostnameVerification` switches the check
/// off, as on HotSpot: the wrong-name certificate is accepted.
#[test]
fn w15a_disabled_hostname_verification_accepts_another_name() {
    let (result, kept_failure, server_got_request) = w15a_exchange_through_the_real_gate(
        include_str!("../t27_certs/server1.crt"),
        include_str!("../t27_certs/server1.key"),
        false,
    );
    let resp = result.expect("with hostname verification disabled the name is not checked");
    assert_eq!(resp.status, 200);
    assert!(!kept_failure);
    assert!(server_got_request);
}

/// `Utils.hostnameVerificationDisabledValue`: absent = on; present and
/// empty, or `Boolean.parseBoolean`-true, = off; anything else = on.
#[test]
fn w15a_disable_hostname_verification_property_values() {
    assert!(!re5_hostname_verification_disabled_value(None));
    assert!(re5_hostname_verification_disabled_value(Some("")));
    assert!(re5_hostname_verification_disabled_value(Some("true")));
    assert!(re5_hostname_verification_disabled_value(Some("TRUE")));
    assert!(!re5_hostname_verification_disabled_value(Some("false")));
    assert!(!re5_hostname_verification_disabled_value(Some("yes")));
    assert!(!re5_hostname_verification_disabled_value(Some("1")));
}

#[test]
fn re1_socket_side_table_is_identity_keyed() {
    let mut ctx = MockNativeContext::new();
    let sock = match ctx.new_object("java/net/Socket").unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected socket object, got {other:?}"),
    };

    sock_set(&ctx, sock, |s| {
        s.port = 9999;
        s.stream_id = 123;
    });

    let side = sock_get(&ctx, sock);
    assert_eq!(side.port, 9999);
    assert_eq!(side.stream_id, 123);
    // gc-common w28-a: filed under the socket's weak lock key.
    assert_eq!(
        existing_native_obj_key(&ctx, sock).map(|k| k.key),
        crate::existing_weak_lock_key(&ctx, sock)
    );
    assert!(existing_native_obj_key(&ctx, sock).is_some());
}

#[test]
fn re2_tcp_listener_binds_and_accepts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let th = std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 5];
            let _ = s.read(&mut buf);
            assert_eq!(&buf, b"hello");
        }
    });
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"hello").unwrap();
    let _ = th.join();
}

#[test]
fn re3_resolve_localhost() {
    let ip = resolve_host("localhost").expect("localhost must resolve");
    assert!(ip.is_loopback());
}

#[test]
fn re3_parse_literal_v4() {
    let ip = resolve_host("10.0.0.1").unwrap();
    assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
}

// Regression (BUG-TC0622 Gap A): an IPv4-mapped IPv6 literal must fold to
// its IPv4 dotted-quad so the `InetAddress` mirror is an `Inet4Address`
// (4-byte getAddress) — otherwise Tomcat's RemoteIpFilter can't match a
// dual-stack loopback peer against `127.0.0.0/8`. Genuine IPv6 must render in
// HotSpot's full uncompressed eight-group form (NOT Rust's RFC-5952 `::`),
// and plain IPv4 / non-IP hosts pass through byte-identical.
#[test]
fn hotspot_ip_string_matches_jdk_text_form() {
    // v4-mapped fold → Inet4Address dotted-quad.
    assert_eq!(hotspot_ip_string("::ffff:127.0.0.1"), "127.0.0.1");
    assert_eq!(hotspot_ip_string("::ffff:10.1.2.3"), "10.1.2.3");
    // Genuine IPv6: HotSpot's Inet6Address.numericToTextFormat — eight
    // minimal-hex groups joined by ':', no zero-compression.
    assert_eq!(hotspot_ip_string("::1"), "0:0:0:0:0:0:0:1");
    assert_eq!(hotspot_ip_string("fe80::1"), "fe80:0:0:0:0:0:0:1");
    assert_eq!(hotspot_ip_string("2001:db8::1"), "2001:db8:0:0:0:0:0:1");
    // Plain IPv4 and non-IP hosts: untouched.
    assert_eq!(hotspot_ip_string("127.0.0.1"), "127.0.0.1");
    assert_eq!(hotspot_ip_string("example.com"), "example.com");
}

/// The v4-mapped fold belongs to the ENTRY POINT, not to the address.
///
/// `getByName` / `InetAddress.getByAddress(byte[])` hand back an
/// `Inet4Address` for `::ffff:a.b.c.d` and HotSpot agrees — that is
/// [`hotspot_ip_string`], asserted above. But
/// `Inet6Address.getByAddress(String, byte[], int)` is specified to KEEP
/// all sixteen bytes, and CratonVM does not intercept it: the real JDK
/// bytecode builds the object, and the only CratonVM code that touches it
/// afterwards is `inet_addr_resolve`'s `holder6` branch, which reads the
/// 16 real octets back out. Folding there produced an object that answered
/// `instanceof Inet6Address` while `getAddress()` returned four bytes, and
/// `io.netty.util.NetUtil.toAddressString` indexes 16
/// (`ArrayIndexOutOfBoundsException: Index 4 out of bounds for length 4`,
/// `NetUtilTest.testIpv4MappedIp6GetByName` — HotSpot 14/14, CratonVM
/// 13/14 until this split).
#[test]
fn ipv6_uncompressed_text_never_folds_a_v4_mapped_address() {
    let mapped: std::net::Ipv6Addr = "::ffff:192.168.0.1".parse().unwrap();
    // The reader form keeps the v6 shape …
    assert_eq!(ipv6_uncompressed_text(&mapped), "0:0:0:0:0:ffff:c0a8:1");
    // … and re-parses to sixteen octets, which is the property netty needs.
    assert_eq!(
        ipv6_uncompressed_text(&mapped)
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets()
            .len(),
        16
    );
    // … while the entry-point form still folds, byte-identical to HotSpot.
    assert_eq!(hotspot_ip_string("::ffff:192.168.0.1"), "192.168.0.1");
    // A genuine IPv6 address renders the same through both.
    let real: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
    assert_eq!(ipv6_uncompressed_text(&real), "2001:db8:0:0:0:0:0:1");
    assert_eq!(
        ipv6_uncompressed_text(&real),
        hotspot_ip_string("2001:db8::1")
    );
}

#[test]
fn re3_get_by_address_uses_hotspot_ipv6_text_and_concrete_layout() {
    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(ArrayElementType::Byte, 16);
    for (i, byte) in [
        0xfeu8, 0x80, 0, 0, 0, 0, 0, 0, 0x67, 0xb0, 0x09, 0x9e, 0x5a, 0x9b, 0x28, 0x7e,
    ]
    .iter()
    .enumerate()
    {
        ctx.set_array_element(bytes, i, Value::Int((*byte as i8) as i32));
    }

    let result = native_inet_get_by_address(&mut ctx, &[Value::Object(Some(bytes))])
        .expect("a sixteen-byte address must be accepted");
    let address = match result {
        Some(Value::Object(Some(address))) => address,
        other => panic!("expected InetAddress, got {other:?}"),
    };

    // Control, from the real JDK 25 on these exact 16 bytes:
    //
    //   getHostAddress = fe80:0:0:0:67b0:99e:5a9b:287e
    //   toString       = /fe80:0:0:0:67b0:99e:5a9b:287e
    //
    // Two separate facts, and this test asserted them as one. The TEXT is
    // HotSpot's uncompressed eight-group form (not Rust's RFC-5952
    // `fe80::67b0:99e:5a9b:287e`). The HOSTNAME is ABSENT — note the
    // leading `/` with nothing before it. `getByAddress(byte[])` is handed
    // octets and no name, so the mirror must not invent one; see
    // `alloc_inet_address_unnamed`, whose doc names this exact factory.
    //
    // The original assertion demanded `host == ip`, which is the shape the
    // unnamed-mirror work was undone from: it renders
    // `fe80:.../fe80:...` instead of `/fe80:...`. It could only ever have
    // passed against the bug.
    assert_eq!(
        inet_addr_resolve(&ctx, address),
        Some((String::new(), "fe80:0:0:0:67b0:99e:5a9b:287e".to_string(),)),
        "getByAddress must preserve HotSpot's uncompressed IPv6 text AND \
         leave the mirror unnamed"
    );

    // The user-visible half of the contract, through the real natives
    // rather than the side table, because that is where the two facts above
    // are actually combined.
    let mut registry = NativeMethodRegistry::new();
    register_re3_inet_address(&mut registry);

    let to_string = registry
        .find("java/net/InetAddress", "toString", "()Ljava/lang/String;")
        .expect("InetAddress.toString native is registered");
    let rendered = match to_string(&mut ctx, &[Value::Object(Some(address))]).unwrap() {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        other => panic!("expected String from toString, got {other:?}"),
    };
    assert_eq!(
        rendered.as_deref(),
        Some("/fe80:0:0:0:67b0:99e:5a9b:287e"),
        "HotSpot renders an unnamed InetAddress with a bare leading slash"
    );

    // `getHostName()` is where the numeric text legitimately stands in for
    // the missing name — HotSpot reaches the same answer by attempting a
    // reverse lookup and falling back to `getHostAddress()`. That fallback
    // lives in `inet_addr_host_name_value`, NOT in the stored pair, which
    // is the distinction the old assertion collapsed.
    let host_name = match inet_addr_host_name_value(&mut ctx, address) {
        Value::Object(Some(s)) => ctx.read_string(s),
        other => panic!("expected String from getHostName, got {other:?}"),
    };
    assert_eq!(
        host_name.as_deref(),
        Some("fe80:0:0:0:67b0:99e:5a9b:287e"),
        "an unnamed mirror answers getHostName() with its numeric text"
    );

    // The paired half, so the absent name above reads as a DECISION and not
    // as a mirror that cannot carry one: the named factory still records
    // it. Without this, deleting the host name everywhere would pass.
    // HotSpot on the same bytes:
    //   getByAddress("example.invalid", bytes) -> example.invalid/fe80:0:0:0:…
    let named =
        alloc_inet_address(&mut ctx, "example.invalid", "fe80:0:0:0:67b0:99e:5a9b:287e")
            .unwrap();
    assert_eq!(
        inet_addr_resolve(&ctx, named),
        Some((
            "example.invalid".to_string(),
            "fe80:0:0:0:67b0:99e:5a9b:287e".to_string(),
        )),
        "a supplied host name is kept"
    );
}

#[test]
fn re4_http_decode_chunked() {
    let raw = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
    let body = http_decode_chunked(raw).unwrap();
    assert_eq!(body, b"helloworld");
}

// FIX(net-phase-e #1): a crafted chunk size must never panic — neither on
// numeric overflow of the hex parse nor on the `n + 2` / `&data[..n]`
// arithmetic. All malformed/oversize sizes must return a protocol error.
#[test]
fn re4_http_decode_chunked_overflow_is_error_not_panic() {
    // Hex value that overflows usize (would have panicked `n + 2` / slice).
    let huge = b"ffffffffffffffff\r\nx\r\n0\r\n\r\n";
    assert!(http_decode_chunked(huge).is_err());

    // In-range-but-absurd size beyond MAX_CHUNK cap -> protocol error,
    // not an out-of-range slice panic.
    let big = b"7fffffff\r\nx\r\n0\r\n\r\n"; // ~2 GiB declared, tiny body
    assert!(http_decode_chunked(big).is_err());

    // Non-hex / empty chunk size -> protocol error.
    assert!(http_decode_chunked(b"zz\r\nx\r\n0\r\n\r\n").is_err());
    assert!(http_decode_chunked(b"\r\nx\r\n0\r\n\r\n").is_err());

    // A valid small chunk just over the available data is "truncated".
    assert!(http_decode_chunked(b"5\r\nhi\r\n").is_err());
}

#[test]
fn re4_http_read_response_parses_status_and_body() {
    let raw = b"HTTP/1.1 204 No Content\r\nServer: t\r\nContent-Length: 0\r\n\r\n";
    let resp = http_read_response(&raw[..], false).unwrap();
    assert_eq!(resp.status, 204);
    assert_eq!(resp.body, Vec::<u8>::new());
    assert!(resp.headers.iter().any(|(k, _)| k == "Server"));
}

/// Serves exactly the given header block, then fails every further read
/// with `WouldBlock` -- the shape of a keep-alive HEAD response: the
/// headers legitimately declare the Content-Length the corresponding GET
/// would have, but no body bytes ever arrive, and a blocking socket's
/// `SO_RCVTIMEO` eventually surfaces EAGAIN ("Resource temporarily
/// unavailable (os error 11)").
struct HeadersThenEagain<'a>(&'a [u8]);

impl Read for HeadersThenEagain<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.0.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Resource temporarily unavailable (os error 11)",
            ));
        }
        let n = self.0.len().min(buf.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

#[test]
fn re5_http_read_response_head_skips_declared_body() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Encoding: gzip\r\n\r\n";
    // HEAD: must return immediately with an empty body, never touching
    // the socket again.
    let resp = http_read_response(HeadersThenEagain(&raw[..]), true).unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k == "Content-Length" && v == "500"));
    // Non-HEAD control: the same wire bytes make the reader wait for the
    // declared 500 body bytes and hit the read timeout (the pre-fix
    // failure mode for HEAD).
    match http_read_response(HeadersThenEagain(&raw[..]), false) {
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock),
        Ok(_) => panic!("non-HEAD read must wait for the declared body"),
    }
}

#[test]
fn re5_handler_tag_classifies_synthetic_vs_real_handlers() {
    let mut ctx = MockNativeContext::new();
    // Synthetic tagged handler -> its tag.
    let bh =
        try_alloc_concurrent_synthetic(&mut ctx, "java/net/http/HttpResponse$BodyHandler", 1)
            .unwrap();
    let tag = ctx.create_string("string");
    ctx.set_field(bh, 0, Value::Object(Some(tag)));
    assert_eq!(
        re5_handler_tag(&ctx, Some(Value::Object(Some(bh)))).as_deref(),
        Some("string")
    );
    // Null / absent handler degrades to the InputStream default.
    assert_eq!(re5_handler_tag(&ctx, None).as_deref(), Some("inputstream"));
    assert_eq!(
        re5_handler_tag(&ctx, Some(Value::Object(None))).as_deref(),
        Some("inputstream")
    );
    // A real user handler class -> None: drive the real protocol.
    let real = try_alloc_concurrent_synthetic(
        &mut ctx,
        "org/springframework/http/client/JdkClientHttpRequest$DecompressingBodyHandler",
        1,
    )
    .unwrap();
    assert_eq!(re5_handler_tag(&ctx, Some(Value::Object(Some(real)))), None);
}

/// Virtual-call hook that records `onNext`/`onComplete` deliveries by
/// bumping counters in the receiver's own fields (slot 0 / slot 1).
fn re5_recording_subscriber_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    let slot = match method_name {
        "onNext" => 0,
        "onComplete" => 1,
        _ => return None,
    };
    let cur = ctx.get_field(receiver, slot).as_int().unwrap_or(0);
    ctx.set_field(receiver, slot, Value::Int(cur + 1));
    Some(Ok(None))
}

fn re5_test_replay_subscription(
    ctx: &mut MockNativeContext,
    subscriber: ObjectRef,
) -> ObjectRef {
    let subscription =
        try_alloc_concurrent_synthetic(ctx, RE5_REPLAY_SUBSCRIPTION, RE5_SUB_NUM_FIELDS)
            .unwrap();
    ctx.set_field(
        subscription,
        RE5_SUB_SUBSCRIBER,
        Value::Object(Some(subscriber)),
    );
    ctx.set_field(subscription, RE5_SUB_BODY, Value::Object(None));
    ctx.set_field(subscription, RE5_SUB_STATE, Value::Int(0));
    subscription
}

#[test]
fn re5_replay_subscription_delivers_completion_exactly_once() {
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(re5_recording_subscriber_hook);
    let subscriber =
        try_alloc_concurrent_synthetic(&mut ctx, "test/RecordingSubscriber", 2).unwrap();
    let subscription = re5_test_replay_subscription(&mut ctx, subscriber);

    // Zero / negative demand: nothing delivered.
    for n in [0i64, -3] {
        re5_replay_subscription_request(
            &mut ctx,
            &[Value::Object(Some(subscription)), Value::Long(n)],
        )
        .unwrap();
    }
    assert_eq!(ctx.get_field(subscriber, 1), Value::Int(0));

    // First positive demand: exactly one onComplete (empty body -> no
    // onNext), and the parked references are dropped.
    re5_replay_subscription_request(
        &mut ctx,
        &[Value::Object(Some(subscription)), Value::Long(1)],
    )
    .unwrap();
    assert_eq!(ctx.get_field(subscriber, 0), Value::Int(0));
    assert_eq!(ctx.get_field(subscriber, 1), Value::Int(1));
    assert!(matches!(
        ctx.get_field(subscription, RE5_SUB_SUBSCRIBER),
        Value::Object(None)
    ));

    // One-shot: repeat demand delivers nothing further.
    re5_replay_subscription_request(
        &mut ctx,
        &[Value::Object(Some(subscription)), Value::Long(9)],
    )
    .unwrap();
    assert_eq!(ctx.get_field(subscriber, 1), Value::Int(1));
}

#[test]
fn re5_replay_subscription_cancel_before_demand_suppresses_delivery() {
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(re5_recording_subscriber_hook);
    let subscriber =
        try_alloc_concurrent_synthetic(&mut ctx, "test/RecordingSubscriber", 2).unwrap();
    let subscription = re5_test_replay_subscription(&mut ctx, subscriber);

    re5_replay_subscription_cancel(&mut ctx, &[Value::Object(Some(subscription))]).unwrap();
    re5_replay_subscription_request(
        &mut ctx,
        &[Value::Object(Some(subscription)), Value::Long(1)],
    )
    .unwrap();
    assert_eq!(ctx.get_field(subscriber, 0), Value::Int(0));
    assert_eq!(ctx.get_field(subscriber, 1), Value::Int(0));
    assert!(matches!(
        ctx.get_field(subscription, RE5_SUB_SUBSCRIBER),
        Value::Object(None)
    ));
}

#[test]
fn re5_body_publishers_of_byte_array_preserves_binary_static_arg_slot_zero() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    let native = registry
        .find(
            "java/net/http/HttpRequest$BodyPublishers",
            "ofByteArray",
            "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        )
        .expect("ofByteArray native is registered");

    let mut ctx = MockNativeContext::new();
    let bytes = ctx.new_array(ArrayElementType::Byte, 3);
    ctx.set_array_element(bytes, 0, Value::Int(0x1f));
    ctx.set_array_element(bytes, 1, Value::Int(0x8b));
    ctx.set_array_element(bytes, 2, Value::Int(0xff));

    let publisher = match native(&mut ctx, &[Value::Object(Some(bytes))]).unwrap() {
        Some(Value::Object(Some(publisher))) => publisher,
        other => panic!("expected BodyPublisher object, got {other:?}"),
    };
    assert!(matches!(
        ctx.get_field(publisher, 0),
        Value::Object(Some(body)) if body == bytes
    ));
    let body_field = ctx.get_field(publisher, 0);
    assert_eq!(
        re5_request_body_bytes(&mut ctx, body_field, Re5Caller::Send).unwrap(),
        vec![0x1f, 0x8b, 0xff]
    );
}

fn re5_test_byte_buffer(ctx: &mut MockNativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().copied().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    let bb = try_alloc_concurrent_synthetic(ctx, "java/nio/HeapByteBuffer", 5).unwrap();
    ctx.set_field(bb, 0, Value::Object(Some(arr)));
    ctx.set_field(bb, 1, Value::Int(0));
    ctx.set_field(bb, 2, Value::Int(bytes.len() as i32));
    ctx.set_field(bb, 3, Value::Int(bytes.len() as i32));
    ctx.set_field(bb, 4, Value::Int(-1));
    bb
}

fn re5_scripted_publisher_subscribe(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "request" && descriptor == "(J)V" {
        return Some(Ok(None));
    }
    if method_name != "subscribe" || descriptor != "(Ljava/util/concurrent/Flow$Subscriber;)V" {
        return None;
    }
    let subscriber = match args.first().copied() {
        Some(Value::Object(Some(s))) => s,
        _ => return Some(Ok(None)),
    };
    // This helper answers `Option<MethodCallResult>`, so a refused
    // allocation is reported the same way the `on_subscribe` failure below
    // is: `Some(Err(..))`. A `?` here would mean "no such method".
    let subscription = match try_alloc_concurrent_synthetic(
        ctx,
        "java/util/concurrent/Flow$Subscription",
        2,
    ) {
        Ok(o) => o,
        Err(e) => return Some(Err(e)),
    };
    if let Err(e) = re5_body_collector_on_subscribe(
        ctx,
        &[
            Value::Object(Some(subscriber)),
            Value::Object(Some(subscription)),
        ],
    ) {
        return Some(Err(e));
    }
    let bb = re5_test_byte_buffer(ctx, b"streamed-body");
    if let Err(e) = re5_body_collector_on_next(
        ctx,
        &[Value::Object(Some(subscriber)), Value::Object(Some(bb))],
    ) {
        return Some(Err(e));
    }
    Some(re5_body_collector_on_complete(
        ctx,
        &[Value::Object(Some(subscriber))],
    ))
}

#[test]
fn re5_body_publishers_from_publisher_keeps_flow_publisher() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    let native = registry
        .find(
            "java/net/http/HttpRequest$BodyPublishers",
            "fromPublisher",
            "(Ljava/util/concurrent/Flow$Publisher;)Ljava/net/http/HttpRequest$BodyPublisher;",
        )
        .expect("fromPublisher native is registered");

    let mut ctx = MockNativeContext::new();
    let publisher =
        try_alloc_concurrent_synthetic(&mut ctx, "test/SynchronousPublisher", 0).unwrap();
    let body_publisher = match native(&mut ctx, &[Value::Object(Some(publisher))]).unwrap() {
        Some(Value::Object(Some(body_publisher))) => body_publisher,
        other => panic!("expected BodyPublisher object, got {other:?}"),
    };

    assert!(matches!(
        ctx.get_field(body_publisher, 0),
        Value::Object(Some(o)) if o == publisher
    ));
}

#[test]
fn re5_request_body_bytes_drives_from_publisher_bytebuffers() {
    let mut ctx = MockNativeContext::new();
    ctx.set_invoke_virtual_hook(re5_scripted_publisher_subscribe);
    let publisher =
        try_alloc_concurrent_synthetic(&mut ctx, "test/SynchronousPublisher", 0).unwrap();

    let body =
        re5_request_body_bytes(&mut ctx, Value::Object(Some(publisher)), Re5Caller::Send)
            .unwrap();

    assert_eq!(body, b"streamed-body");
}

/// Interpreter round i3 wave 83, lane L6: a re5 region reads the state of
/// the JDK door's HotSpot thread -- `send` parks, `sendAsync`'s inline
/// fallback and `contentLength` are VM waits (RUNNABLE), and the async
/// task waits for a publisher in the VM -- and every one is still exactly
/// one GC-blocking region.
#[test]
fn re5_regions_take_the_callers_state() {
    for (caller, exchange_in_vm, publisher_in_vm) in [
        (Re5Caller::Send, false, false),
        (Re5Caller::SendAsyncInline, true, true),
        (Re5Caller::AsyncTask, false, true),
        (Re5Caller::ContentLength, true, true),
    ] {
        let mut ctx = MockNativeContext::new();
        re5_begin_exchange_region(&mut ctx, caller);
        ctx.end_blocking_region();
        assert_eq!(ctx.blocking_region_counts(), (1, 1), "{caller:?} exchange");
        assert_eq!(
            ctx.vm_wait_region_count(),
            usize::from(exchange_in_vm),
            "{caller:?} exchange"
        );

        let mut ctx = MockNativeContext::new();
        re5_begin_publisher_wait_region(&mut ctx, caller);
        ctx.end_blocking_region();
        assert_eq!(ctx.blocking_region_counts(), (1, 1), "{caller:?} publisher");
        assert_eq!(
            ctx.vm_wait_region_count(),
            usize::from(publisher_in_vm),
            "{caller:?} publisher"
        );
    }
}

#[test]
fn re7_udp_roundtrip() {
    use std::net::UdpSocket;
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.send_to(b"ping", server_addr).unwrap();
    let mut buf = [0u8; 4];
    let (n, _) = server.recv_from(&mut buf).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf, b"ping");
}

#[test]
fn re8_enumerates_at_least_loopback() {
    let ips = re8_enumerate_local_ips();
    assert!(ips.iter().any(|i| i.is_loopback()));
}

#[test]
fn re9_iae_helper_reachable() {
    assert!(matches!(
        iae(""),
        cratonvm_types::error::MethodCallFailed::InternalError(_)
    ));
}

#[test]
fn re10_parse_minimal_http_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"GET /hi HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    });
    let (stream, _) = listener.accept().unwrap();
    let req = parse_http_request(stream).unwrap();
    assert_eq!(req.method, "GET");
    assert_eq!(req.uri, "/hi");
    assert!(req.headers.iter().any(|(k, v)| k == "Host" && v == "x"));
}

/// Writes `payload` in ONE write to a fresh loopback connection and returns
/// the server side, holding the client open until the returned handle is
/// dropped (so a parse sees the bytes, not an early EOF).
fn re10_pipelined(payload: &'static [u8]) -> (TcpStream, std::thread::JoinHandle<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(payload).unwrap();
        c
    });
    let (stream, _) = listener.accept().unwrap();
    (stream, client)
}

/// The one-shot parser's zero-length-body rule would swallow a pipelined
/// second request into the first request's body. The keep-alive parse must
/// hand it back instead, and the second parse must find it complete.
#[test]
fn re10_keepalive_parse_carries_a_pipelined_request_over() {
    let (mut stream, client) =
        re10_pipelined(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\nHost: x\r\n\r\n");
    let first = parse_http_request_core(&mut stream, Vec::new(), false, true).unwrap();
    assert_eq!(first.uri, "/a");
    assert!(first.body.is_empty());
    assert!(first.persistent);
    let second = parse_http_request_core(&mut stream, first.leftover, true, true).unwrap();
    assert_eq!(second.uri, "/b");
    assert!(second.leftover.is_empty());
    drop(client.join());
}

#[test]
fn re10_keepalive_parse_carries_over_after_a_content_length_body() {
    let (mut stream, client) = re10_pipelined(
        b"POST /a HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET /b HTTP/1.1\r\n\r\n",
    );
    let first = parse_http_request_core(&mut stream, Vec::new(), false, true).unwrap();
    assert_eq!(first.body, b"abc");
    let second = parse_http_request_core(&mut stream, first.leftover, true, true).unwrap();
    assert_eq!((second.method.as_str(), second.uri.as_str()), ("GET", "/b"));
    drop(client.join());
}

#[test]
fn re10_keepalive_parse_carries_over_after_a_chunked_body() {
    let (mut stream, client) = re10_pipelined(
        b"POST /a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /b HTTP/1.1\r\n\r\n",
    );
    let first = parse_http_request_core(&mut stream, Vec::new(), false, true).unwrap();
    assert_eq!(first.body, b"abc");
    let second = parse_http_request_core(&mut stream, first.leftover, true, true).unwrap();
    assert_eq!(second.uri, "/b");
    drop(client.join());
}

#[test]
fn re10_connection_close_and_http10_requests_are_not_persistent() {
    let (mut stream, client) =
        re10_pipelined(b"GET /a HTTP/1.1\r\nConnection: keep-alive, close\r\n\r\n");
    assert!(
        !parse_http_request_core(&mut stream, Vec::new(), false, true)
            .unwrap()
            .persistent
    );
    drop(client.join());
    let (mut stream, client) = re10_pipelined(b"GET /a HTTP/1.0\r\n\r\n");
    assert!(
        !parse_http_request_core(&mut stream, Vec::new(), false, true)
            .unwrap()
            .persistent
    );
    drop(client.join());
}

#[test]
fn re10_chunked_request_len_requires_the_final_empty_line() {
    assert_eq!(http_chunked_request_len(b"3\r\nabc\r\n0\r\n"), None);
    // "3\r\n" + "abc\r\n" + "0\r\n" + "\r\n" = 3 + 5 + 3 + 2
    assert_eq!(http_chunked_request_len(b"3\r\nabc\r\n0\r\n\r\n"), Some(13));
    // "0\r\n" + "X-T: 1\r\n" + "\r\n" = 3 + 8 + 2; "NEXT" is the next request
    assert_eq!(
        http_chunked_request_len(b"0\r\nX-T: 1\r\n\r\nNEXT"),
        Some(13)
    );
    assert_eq!(http_chunked_request_len(b"zz\r\n"), None);
}

// VULN-FIX [nb-net-phase-e] regression: an oversized advertised body must be
// rejected with 413 and NOT allocated. We set a tiny per-process unlikely
// value by exploiting the default cap (8 MiB) being far above 10 bytes while
// we claim a body larger than the cap via Content-Length.
#[test]
fn re10_oversized_content_length_rejected_with_413() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Advertise a body well beyond the 8 MiB default cap; do NOT actually
    // send it (the server must refuse before reading the body).
    let huge = http_max_request_body() + 1;
    let handle = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            format!("POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: {huge}\r\n\r\n").as_bytes(),
        )
        .unwrap();
        // Read back whatever the server responds with.
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    // parse must reject (None) without reading the huge body.
    assert!(parse_http_request(stream).is_none());
    let resp = handle.join().unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 413"),
        "expected 413 response, got: {text}"
    );
}

// VULN-FIX [nb-net-phase-e] regression: conflicting duplicate Content-Length
// headers (request-smuggling vector) must be rejected with 400 rather than
// last-wins.
#[test]
fn re10_conflicting_content_length_rejected_with_400() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 5\r\n\r\nabc",
        )
        .unwrap();
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    assert!(parse_http_request(stream).is_none());
    let resp = handle.join().unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 400"),
        "expected 400 response, got: {text}"
    );
}

// VULN-FIX [nb-net-phase-e] regression: a small, well-formed body within the
// cap is still accepted unchanged.
#[test]
fn re10_small_body_within_cap_accepted() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
    });
    let (stream, _) = listener.accept().unwrap();
    let req = parse_http_request(stream).unwrap();
    assert_eq!(req.body, b"hello");
}

// VULN-FIX [nb-net-phase-e] regression: a chunked request body (no
// Content-Length) must be fully decoded, not silently ignored. Without the
// fix the parser framed solely by Content-Length (defaulting to 0), read
// zero body bytes, and left the chunked payload unread → keep-alive desync.
#[test]
fn re10_chunked_request_body_is_fully_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
              5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n",
        )
        .unwrap();
    });
    let (stream, _) = listener.accept().unwrap();
    let req = parse_http_request(stream).unwrap();
    assert_eq!(req.body, b"helloworld");
}

// VULN-FIX [nb-net-phase-e] regression: a chunked body that arrives in
// multiple TCP reads (split across the header boundary and between chunks)
// must still be reassembled completely.
#[test]
fn re10_chunked_request_body_split_across_reads() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        // Header + first partial chunk, then the rest in a second write.
        c.write_all(
            b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc",
        )
        .unwrap();
        c.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        c.write_all(b"\r\n4\r\ndefg\r\n0\r\n\r\n").unwrap();
        c.flush().unwrap();
    });
    let (stream, _) = listener.accept().unwrap();
    let req = parse_http_request(stream).unwrap();
    assert_eq!(req.body, b"abcdefg");
}

// VULN-FIX [nb-net-phase-e] regression: Transfer-Encoding AND Content-Length
// together is a request-smuggling vector (RFC 7230 §3.3.3) and must be
// rejected with 400 rather than framed by either header.
#[test]
fn re10_te_and_content_length_together_rejected_with_400() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
              Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        )
        .unwrap();
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    assert!(parse_http_request(stream).is_none());
    let resp = handle.join().unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 400"),
        "expected 400 response, got: {text}"
    );
}

// VULN-FIX [nb-net-phase-e] regression: an unknown / unsupported transfer
// coding (here gzip, which we cannot de-frame) must be rejected with 400
// instead of guessing at the body boundary.
#[test]
fn re10_unknown_transfer_encoding_rejected_with_400() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\n")
            .unwrap();
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    assert!(parse_http_request(stream).is_none());
    let resp = handle.join().unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 400"),
        "expected 400 response, got: {text}"
    );
}

// VULN-FIX [nb-net-phase-e] regression: `chunked` must be the FINAL coding
// to frame the body (RFC 7230 §3.3.1). A list ending in a non-chunked
// coding (e.g. `chunked, gzip`) is unsafe to de-frame → 400.
#[test]
fn re10_chunked_not_last_rejected_with_400() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked, gzip\r\n\r\n")
            .unwrap();
        let mut resp = Vec::new();
        let _ = c.read_to_end(&mut resp);
        resp
    });
    let (stream, _) = listener.accept().unwrap();
    assert!(parse_http_request(stream).is_none());
    let resp = handle.join().unwrap();
    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 400"),
        "expected 400 response, got: {text}"
    );
}

// VULN-FIX [nb-net-phase-e] regression: a legitimate `identity` coding
// followed by `chunked` (the only non-chunked coding we accept) still frames
// by chunked and decodes the body.
#[test]
fn re10_identity_then_chunked_is_accepted() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: identity, chunked\r\n\r\n\
              2\r\nhi\r\n0\r\n\r\n",
        )
        .unwrap();
    });
    let (stream, _) = listener.accept().unwrap();
    let req = parse_http_request(stream).unwrap();
    assert_eq!(req.body, b"hi");
}

/// MEASURED 2026-08-17 (probe `G16Ctx`, HotSpot 25.0.3+9-LTS): a freshly
/// `init`-ed `SSLContext`'s session contexts answer 20480 and 86400 on the
/// first call, before anything is configured. They used to answer 0 and 0 —
/// which is not "no answer", it is the answer a caller gets after SETTING
/// zero, so the initial state was reporting itself as a configured one.
#[test]
fn ssl_session_context_defaults_are_the_measured_hotspot_values() {
    let fresh = SscSide::default();
    assert_eq!(
        fresh.cache_size, 20480,
        "SSLSessionContext.getSessionCacheSize() on an unconfigured context"
    );
    assert_eq!(
        fresh.timeout_secs, 86400,
        "SSLSessionContext.getSessionTimeout() on an unconfigured context, in seconds"
    );
}

/// The nine inherited `java.net.ServerSocket` methods RE.6b takes over on a
/// `javax.net.ssl.SSLServerSocket` receiver. Each one was MEASURED throwing
/// `NullPointerException: ... the return value of
/// "java.net.ServerSocket.getImpl()" is null` before this registrar existed
/// (probe `G16Sweep`, 2026-08-17); `RSslLiveSession` died on the second of
/// them at its first statement.
#[test]
fn ssl_server_socket_inherited_option_surface_is_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_ssl_server_socket_options(&mut registry);
    for (method, descriptor) in [
        ("getSoTimeout", "()I"),
        ("setSoTimeout", "(I)V"),
        ("getReuseAddress", "()Z"),
        ("setReuseAddress", "(Z)V"),
        ("getReceiveBufferSize", "()I"),
        ("setReceiveBufferSize", "(I)V"),
        ("supportedOptions", "()Ljava/util/Set;"),
        ("getOption", "(Ljava/net/SocketOption;)Ljava/lang/Object;"),
        (
            "setOption",
            "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;",
        ),
    ] {
        assert!(
            registry
                .find("javax/net/ssl/SSLServerSocket", method, descriptor)
                .is_some(),
            "missing SSLServerSocket.{method}{descriptor} native"
        );
    }
}

/// The registrar must NOT claim any name `t27_tls::register_sslserversocket`
/// owns on the same class. `register_phase_e_networking` runs at
/// lib.rs:18688 and `register_t27_natives` at lib.rs:18731, so a name in
/// both files is silently taken over by t27's later body — a fix that
/// compiles, registers, and never runs. That failure shape has already
/// landed twice in this codebase (HANDOFF-20260814 §5); this test is the
/// tripwire for the third time.
#[test]
fn ssl_server_socket_option_registrar_leaves_the_t27_owned_names_alone() {
    let mut registry = NativeMethodRegistry::new();
    register_ssl_server_socket_options(&mut registry);
    for (method, descriptor) in [
        ("accept", "()Ljava/net/Socket;"),
        ("bind", "(Ljava/net/SocketAddress;)V"),
        ("close", "()V"),
        ("isClosed", "()Z"),
        ("getLocalPort", "()I"),
        ("getEnabledProtocols", "()[Ljava/lang/String;"),
        ("getSupportedProtocols", "()[Ljava/lang/String;"),
        ("setEnabledProtocols", "([Ljava/lang/String;)V"),
        ("getNeedClientAuth", "()Z"),
        ("setNeedClientAuth", "(Z)V"),
        ("getWantClientAuth", "()Z"),
        ("setWantClientAuth", "(Z)V"),
    ] {
        assert!(
            registry
                .find("javax/net/ssl/SSLServerSocket", method, descriptor)
                .is_none(),
            "SSLServerSocket.{method}{descriptor} is t27_tls's; registering it \
             here is dead code, because register_t27_natives runs later and wins"
        );
    }
}

/// `isBound` / `getInetAddress` / `getLocalSocketAddress` / `toString` are
/// deliberately absent, and must stay absent until the bind ADDRESS reaches
/// this surface. MEASURED: `ServerSocket.toString()` short-circuits to the
/// constant `"ServerSocket[unbound]"` while `isBound()` is false, which is
/// the only reason `toString()` currently agrees with HotSpot on a live SSL
/// server socket. Registering `isBound` alone flips that agreeing row into
/// `NullPointerException: ... "this.impl" is null`. They move together or
/// not at all — see the NOMINATION in
/// `G16-1-the-server-socket-impl-and-how-far-RSslLiveSession-got-20260817.md`.
#[test]
fn ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal() {
    let mut registry = NativeMethodRegistry::new();
    register_ssl_server_socket_options(&mut registry);
    let bound_rows = [
        ("isBound", "()Z"),
        ("getInetAddress", "()Ljava/net/InetAddress;"),
        ("getLocalSocketAddress", "()Ljava/net/SocketAddress;"),
        ("toString", "()Ljava/lang/String;"),
    ];
    let mut registered = 0usize;
    for (method, descriptor) in bound_rows {
        if registry
            .find("javax/net/ssl/SSLServerSocket", method, descriptor)
            .is_some()
        {
            registered += 1;
        }
    }
    assert!(
        registered == 0 || registered == bound_rows.len(),
        "{registered} of {} bind-address-dependent rows registered; isBound and \
         toString are one contract and cannot land separately",
        bound_rows.len()
    );
}

#[test]
fn re6_ssl_context_session_accessors_are_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_re6_ssl_context(&mut registry);
    for method in ["getClientSessionContext", "getServerSessionContext"] {
        assert!(
            registry
                .find(
                    "javax/net/ssl/SSLContext",
                    method,
                    "()Ljavax/net/ssl/SSLSessionContext;",
                )
                .is_some(),
            "missing {method} native"
        );
    }
}

#[test]
fn re5_real_jdk_http_client_builder_fluent_methods_are_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    for (method, descriptor) in [
        (
            "version",
            "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        ),
        ("priority", "(I)Ljava/net/http/HttpClient$Builder;"),
        (
            "executor",
            "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "cookieHandler",
            "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "proxy",
            "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "authenticator",
            "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "sslContext",
            "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
        ),
        (
            "sslParameters",
            "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
        ),
    ] {
        assert!(
            registry
                .find("java/net/http/HttpClient$Builder", method, descriptor)
                .is_some(),
            "missing {method}{descriptor}"
        );
    }
}

#[test]
fn re5_http_client_builder_retains_configured_object_values() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    let mut ctx = MockNativeContext::new();

    let new_builder = registry
        .find(
            "java/net/http/HttpClient",
            "newBuilder",
            "()Ljava/net/http/HttpClient$Builder;",
        )
        .expect("HttpClient.newBuilder native");
    let builder = match new_builder(&mut ctx, &[]).unwrap() {
        Some(Value::Object(Some(builder))) => builder,
        other => panic!("newBuilder returned {other:?}"),
    };
    let executor = try_alloc_concurrent_synthetic(&mut ctx, "test/Executor", 0).unwrap();
    let proxy = try_alloc_concurrent_synthetic(&mut ctx, "test/ProxySelector", 0).unwrap();

    for (method, descriptor, value) in [
        (
            "executor",
            "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
            executor,
        ),
        (
            "proxy",
            "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
            proxy,
        ),
    ] {
        let setter = registry
            .find("java/net/http/HttpClient$Builder", method, descriptor)
            .expect("builder setter native");
        setter(
            &mut ctx,
            &[Value::Object(Some(builder)), Value::Object(Some(value))],
        )
        .expect("builder setter should succeed");
    }

    let build = registry
        .find(
            "java/net/http/HttpClient$Builder",
            "build",
            "()Ljava/net/http/HttpClient;",
        )
        .expect("HttpClient.Builder.build native");
    let client = match build(&mut ctx, &[Value::Object(Some(builder))]).unwrap() {
        Some(Value::Object(Some(client))) => client,
        other => panic!("build returned {other:?}"),
    };
    assert_eq!(
        ctx.get_field(client, RE5_CLIENT_EXECUTOR),
        Value::Object(Some(executor))
    );
    assert_eq!(
        ctx.get_field(client, RE5_CLIENT_PROXY),
        Value::Object(Some(proxy))
    );
}

#[test]
fn re10_http_server_executor_bridges_are_concrete_receiver_registrations() {
    let mut registry = NativeMethodRegistry::new();
    register_re10_http_server(&mut registry);
    for class in [
        "com/sun/net/httpserver/HttpServer",
        "sun/net/httpserver/HttpServerImpl",
    ] {
        assert!(
            registry
                .find(class, "setExecutor", "(Ljava/util/concurrent/Executor;)V")
                .is_some(),
            "missing {class}.setExecutor bridge"
        );
        assert!(
            registry
                .find(class, "getExecutor", "()Ljava/util/concurrent/Executor;")
                .is_some(),
            "missing {class}.getExecutor bridge"
        );
    }
}

// -----------------------------------------------------------------------
// G29-1 — the fabricated HttpRequest and its missing accessors.
//
// `HttpRequest$Builder.build()` mints an object stamped with the ABSTRACT
// class `java/net/http/HttpRequest`, so the receiver's runtime class IS the
// class every accessor resolves against and a missing registration is an
// `AbstractMethodError`, not a fallback. These tests pin the surface (every
// declared method has a row) and the slot map that surface reads.
// -----------------------------------------------------------------------

/// The seven instance accessors `java.net.http.HttpRequest` declares
/// (`javap -p`, JDK 25.0.3+9-LTS). Four of the seven had no row until
/// G29-1, and each of those four threw `AbstractMethodError` (MEASURED).
#[test]
fn http_request_registers_all_seven_declared_accessors() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    for (name, descriptor) in [
        ("method", "()Ljava/lang/String;"),
        ("uri", "()Ljava/net/URI;"),
        ("timeout", "()Ljava/util/Optional;"),
        ("version", "()Ljava/util/Optional;"),
        ("bodyPublisher", "()Ljava/util/Optional;"),
        ("expectContinue", "()Z"),
        ("headers", "()Ljava/net/http/HttpHeaders;"),
    ] {
        assert!(
            registry
                .find("java/net/http/HttpRequest", name, descriptor)
                .is_some(),
            "java/net/http/HttpRequest.{name}{descriptor} has no native — it is \
             ABSTRACT and the receiver IS this class, so the call throws \
             AbstractMethodError"
        );
    }
}

/// `HttpRequest$Builder` is an INTERFACE and the builder object is stamped
/// with it, so the same rule covers every method it declares. `HEAD()` is a
/// default method with real bytecode and is registered anyway — MEASURED,
/// that bytecode leaves `bodyPublisher()` present where HotSpot answers
/// empty.
#[test]
fn http_request_builder_registers_its_whole_declared_surface() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    let b = "Ljava/net/http/HttpRequest$Builder;";
    let publisher = "Ljava/net/http/HttpRequest$BodyPublisher;";
    for (name, descriptor) in [
        ("uri", format!("(Ljava/net/URI;){b}")),
        ("expectContinue", format!("(Z){b}")),
        (
            "version",
            format!("(Ljava/net/http/HttpClient$Version;){b}"),
        ),
        (
            "header",
            format!("(Ljava/lang/String;Ljava/lang/String;){b}"),
        ),
        ("headers", format!("([Ljava/lang/String;){b}")),
        ("timeout", format!("(Ljava/time/Duration;){b}")),
        (
            "setHeader",
            format!("(Ljava/lang/String;Ljava/lang/String;){b}"),
        ),
        ("GET", format!("(){b}")),
        ("HEAD", format!("(){b}")),
        ("POST", format!("({publisher}){b}")),
        ("PUT", format!("({publisher}){b}")),
        ("DELETE", format!("(){b}")),
        ("method", format!("(Ljava/lang/String;{publisher}){b}")),
        ("build", "()Ljava/net/http/HttpRequest;".to_string()),
        ("copy", format!("(){b}")),
    ] {
        assert!(
            registry
                .find("java/net/http/HttpRequest$Builder", name, &descriptor)
                .is_some(),
            "java/net/http/HttpRequest$Builder.{name}{descriptor} has no native"
        );
    }
}

/// `BodyPublisher` declares exactly one method and had ZERO registrations —
/// the same 0-of-1 ratio `PathMatcher.matches` had. `HttpHeaders` declares
/// four public readers and carried only `map`.
#[test]
fn body_publisher_and_http_headers_readers_are_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_re5_http_client(&mut registry);
    assert!(
        registry
            .find(
                "java/net/http/HttpRequest$BodyPublisher",
                "contentLength",
                "()J"
            )
            .is_some(),
        "BodyPublisher.contentLength()J has no native"
    );
    for (name, descriptor) in [
        ("map", "()Ljava/util/Map;"),
        ("firstValue", "(Ljava/lang/String;)Ljava/util/Optional;"),
        ("allValues", "(Ljava/lang/String;)Ljava/util/List;"),
        (
            "firstValueAsLong",
            "(Ljava/lang/String;)Ljava/util/OptionalLong;",
        ),
        ("toString", "()Ljava/lang/String;"),
    ] {
        assert!(
            registry
                .find("java/net/http/HttpHeaders", name, descriptor)
                .is_some(),
            "java/net/http/HttpHeaders.{name}{descriptor} has no native"
        );
    }
}

/// `build()` copies `0..RE5_REQUEST_NUM_FIELDS` slot-for-slot. It copied
/// `0..5` against a five-slot builder; the moment a slot was added past the
/// fifth, the copy would drop it and the accessor reading it would answer
/// the allocator's zero. Pin the map so the two ends cannot drift.
#[test]
fn request_slot_map_is_dense_distinct_and_bounded() {
    let slots = [
        RE5_REQUEST_METHOD,
        RE5_REQUEST_URI,
        RE5_REQUEST_BODY,
        RE5_REQUEST_HEADERS,
        RE5_REQUEST_TIMEOUT_FIELD,
        RE5_REQUEST_EXPECT_CONTINUE,
        RE5_REQUEST_VERSION,
        RE5_REQUEST_BODY_PUBLISHER,
    ];
    let mut sorted = slots.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        slots.len(),
        "two request slots share an index — one accessor is reading another's state"
    );
    assert_eq!(sorted, (0..RE5_REQUEST_NUM_FIELDS).collect::<Vec<_>>());
    assert_eq!(RE5_BP_NUM_FIELDS, 2);
    assert!(RE5_BP_PAYLOAD < RE5_BP_DECLARED_LENGTH);
}

/// `re5_new_request_builder` must initialise EVERY slot, not a prefix:
/// `build()` copies the whole range, so an uninitialised tail becomes the
/// built request's state.
#[test]
fn a_fresh_builder_initialises_every_slot_it_declares() {
    let mut ctx = MockNativeContext::new();
    let b = match re5_new_request_builder(&mut ctx, None).unwrap() {
        Some(Value::Object(Some(b))) => b,
        other => panic!("expected a builder object, got {other:?}"),
    };
    let method = match ctx.get_field(b, RE5_REQUEST_METHOD) {
        Value::Object(Some(s)) => s,
        other => panic!("method slot is not a String: {other:?}"),
    };
    assert_eq!(ctx.read_string(method), Some("GET".to_string()));
    assert_eq!(ctx.get_field(b, RE5_REQUEST_URI), Value::Object(None));
    assert_eq!(ctx.get_field(b, RE5_REQUEST_BODY), Value::Object(None));
    assert_eq!(ctx.get_field(b, RE5_REQUEST_HEADERS), Value::Object(None));
    assert_eq!(
        ctx.get_field(b, RE5_REQUEST_TIMEOUT_FIELD),
        Value::Object(None)
    );
    assert_eq!(
        ctx.get_field(b, RE5_REQUEST_EXPECT_CONTINUE),
        Value::Int(0),
        "expectContinue defaults to false on HotSpot (MEASURED)"
    );
    assert_eq!(ctx.get_field(b, RE5_REQUEST_VERSION), Value::Object(None));
    assert_eq!(
        ctx.get_field(b, RE5_REQUEST_BODY_PUBLISHER),
        Value::Object(None)
    );
}

/// MEASURED on HotSpot: two `header("Accept", …)` calls group into one name
/// with two values in insertion order, and a differently-cased name joins
/// the same group.
#[test]
fn header_lines_group_case_insensitively_in_first_seen_order() {
    let mut ctx = MockNativeContext::new();
    let holder = ctx.alloc_object(ClassId::new(0), 1);
    let arr = ctx.new_array(ArrayElementType::Reference, 4);
    for (i, line) in ["Accept: a", "X-Foo: b", "accept: c"].iter().enumerate() {
        let s = ctx.create_string(line);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }
    ctx.set_field(holder, 0, Value::Object(Some(arr)));
    let groups = re5_header_groups(&ctx, holder);
    assert_eq!(
        groups,
        vec![
            ("Accept".to_string(), vec!["a".to_string(), "c".to_string()]),
            ("X-Foo".to_string(), vec!["b".to_string()]),
        ]
    );
}

/// The `HttpHeaders` carrier that `HttpRequest.headers()` hands back: one
/// field, and field 0 is the request's own `"key: value"` array.
///
/// `re5_header_groups` reads that carrier, which is why the sibling test
/// `header_lines_group_case_insensitively_in_first_seen_order` builds a
/// one-field object. A builder cannot be passed to it directly: a request
/// keeps its lines at `RE5_REQUEST_HEADERS` (3) and field 0 is
/// `RE5_REQUEST_METHOD`, so the scan reads the method name, finds no array,
/// and returns no groups at all.
fn re5_headers_view(ctx: &mut MockNativeContext, request: ObjectRef) -> ObjectRef {
    // Allocate before reading the field: the read must not be carried
    // across an allocation.
    let view = ctx.alloc_object(ClassId::new(0), 1);
    let lines = ctx.get_field(request, RE5_REQUEST_HEADERS);
    ctx.set_field(view, 0, lines);
    view
}

/// The header array was a fixed 32 entries and `header()` stopped writing
/// when it filled up. MEASURED on HotSpot, a request with 40 headers reports
/// `map().size() == 40`, so the 33rd was being dropped in silence.
#[test]
fn appending_past_the_initial_capacity_grows_instead_of_dropping() {
    let mut ctx = MockNativeContext::new();
    let holder = ctx.alloc_object(ClassId::new(0), RE5_REQUEST_NUM_FIELDS);
    ctx.set_field(holder, RE5_REQUEST_HEADERS, Value::Object(None));
    for i in 0..40 {
        re5_builder_append_header(&mut ctx, holder, &format!("H{i}: v{i}"));
    }
    let view = re5_headers_view(&mut ctx, holder);
    let groups = re5_header_groups(&ctx, view);
    assert_eq!(
        groups.len(),
        40,
        "a header past the 32nd was dropped without a word"
    );
    assert_eq!(groups[39].0, "H39");
    assert_eq!(groups[39].1, vec!["v39".to_string()]);
}

/// MEASURED: `header("Accept","a").setHeader("Accept","z")` leaves
/// `{Accept=[z]}` — `setHeader` REPLACES, and must not disturb other names.
#[test]
fn set_header_replaces_every_value_for_that_name_only() {
    let mut ctx = MockNativeContext::new();
    let holder = ctx.alloc_object(ClassId::new(0), RE5_REQUEST_NUM_FIELDS);
    ctx.set_field(holder, RE5_REQUEST_HEADERS, Value::Object(None));
    re5_builder_append_header(&mut ctx, holder, "Accept: a");
    re5_builder_append_header(&mut ctx, holder, "X-Foo: keep");
    re5_builder_append_header(&mut ctx, holder, "accept: b");
    re5_builder_set_header(&mut ctx, holder, "Accept", "z");
    let view = re5_headers_view(&mut ctx, holder);
    let groups = re5_header_groups(&ctx, view);
    assert_eq!(
        groups,
        vec![
            ("X-Foo".to_string(), vec!["keep".to_string()]),
            ("Accept".to_string(), vec!["z".to_string()]),
        ]
    );
}

/// A publisher minted with a DECLARED length answers it verbatim, including
/// the `-1` that `fromPublisher(p)` means by "unknown until subscribed".
/// Deriving it from the payload instead would subscribe a Flow.Publisher
/// inside a `contentLength()` call — a side effect in a getter.
#[test]
fn a_declared_publisher_length_is_stored_verbatim() {
    let mut ctx = MockNativeContext::new();
    let unknown = match re5_new_body_publisher(&mut ctx, Value::Object(None), Some(-1)).unwrap()
    {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a publisher, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(unknown, RE5_BP_DECLARED_LENGTH),
        Value::Long(-1)
    );
    let derived = match re5_new_body_publisher(&mut ctx, Value::Object(None), None).unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected a publisher, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(derived, RE5_BP_DECLARED_LENGTH),
        Value::Object(None),
        "no declared length must stay absent so contentLength() derives it"
    );
}

/// `GET()`/`DELETE()`/`HEAD()` clear the publisher; the verb setters set
/// both the wire payload and the publisher object. MEASURED: a builder that
/// only ever ADDS reports `newBuilder(u).POST(p).GET().build()
/// .bodyPublisher()` as present, where HotSpot answers empty.
#[test]
fn clearing_the_publisher_clears_both_slots_it_owns() {
    let mut ctx = MockNativeContext::new();
    let holder = ctx.alloc_object(ClassId::new(0), RE5_REQUEST_NUM_FIELDS);
    let publisher =
        match re5_new_body_publisher(&mut ctx, Value::Object(None), Some(7)).unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected a publisher, got {other:?}"),
        };
    let payload = ctx.create_string("hi");
    ctx.set_field(publisher, RE5_BP_PAYLOAD, Value::Object(Some(payload)));
    re5_builder_set_publisher(&mut ctx, holder, Value::Object(Some(publisher)));
    assert_eq!(
        ctx.get_field(holder, RE5_REQUEST_BODY_PUBLISHER),
        Value::Object(Some(publisher))
    );
    assert_eq!(
        ctx.get_field(holder, RE5_REQUEST_BODY),
        Value::Object(Some(payload))
    );
    re5_builder_set_publisher(&mut ctx, holder, Value::Object(None));
    assert_eq!(
        ctx.get_field(holder, RE5_REQUEST_BODY_PUBLISHER),
        Value::Object(None)
    );
    assert_eq!(ctx.get_field(holder, RE5_REQUEST_BODY), Value::Object(None));
}

/// The refusals, MEASURED on HotSpot 25.0.3+9-LTS. `header(null, v)` and
/// `header(k, null)` carry DIFFERENT messages, and both differ from the
/// message-less NPE the publisher/version/timeout setters raise. The
/// difference is observable from Java, so it is modelled, not approximated.
#[test]
fn header_pair_refusals_carry_the_measured_messages() {
    let mut ctx = MockNativeContext::new();
    let name = ctx.create_string("k");
    let value = ctx.create_string("v");
    let empty = ctx.create_string("");
    let ok = re5_check_header_pair(
        &ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(Some(value)),
        ],
    );
    assert_eq!(ok.ok(), Some(("k".to_string(), "v".to_string())));

    let null_name = re5_check_header_pair(
        &ctx,
        &[
            Value::Object(None),
            Value::Object(None),
            Value::Object(Some(value)),
        ],
    );
    let text = format!("{:?}", null_name.err().unwrap());
    assert!(
        text.contains("name"),
        "header(null, v) must be NullPointerException: name — got {text}"
    );

    let null_value = re5_check_header_pair(
        &ctx,
        &[
            Value::Object(None),
            Value::Object(Some(name)),
            Value::Object(None),
        ],
    );
    let text = format!("{:?}", null_value.err().unwrap());
    assert!(
        text.contains("value"),
        "header(k, null) must be NullPointerException: value — got {text}"
    );

    let empty_name = re5_check_header_pair(
        &ctx,
        &[
            Value::Object(None),
            Value::Object(Some(empty)),
            Value::Object(Some(value)),
        ],
    );
    let text = format!("{:?}", empty_name.err().unwrap());
    assert!(
        text.contains("invalid header name"),
        "header(\"\", v) must be IllegalArgumentException: invalid header \
         name — got {text}"
    );
}
