use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn test_parse_empty() {
    let p = Policy::parse("").unwrap();
    assert!(p.is_empty());
}

#[test]
fn test_parse_single_grant() {
    let src = r#"
        grant codeBase "file:/home/app/-" {
            permission java.io.FilePermission "/tmp/*", "read,write";
            permission java.net.SocketPermission "*:80", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 1);
    let g = &p.grants[0];
    assert_eq!(g.code_base.as_deref(), Some("file:/home/app/-"));
    assert_eq!(g.permissions.len(), 2);
    let p0 = &g.permissions[0];
    assert_eq!(p0.class_name, "java.io.FilePermission");
    assert_eq!(p0.target.as_deref(), Some("/tmp/*"));
    assert_eq!(p0.actions.as_deref(), Some("read,write"));
    let p1 = &g.permissions[1];
    assert_eq!(p1.class_name, "java.net.SocketPermission");
    assert_eq!(p1.target.as_deref(), Some("*:80"));
    assert_eq!(p1.actions.as_deref(), Some("connect"));
}

#[test]
fn test_parse_multiple_grants() {
    let src = r#"
        grant {
            permission java.security.AllPermission;
        };
        grant codeBase "file:/plugins/-" {
            permission java.io.FilePermission "/var/log/-", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 2);
    assert!(p.grants[0].code_base.is_none());
    assert_eq!(
        p.grants[0].permissions[0].class_name,
        "java.security.AllPermission"
    );
    assert_eq!(p.grants[1].code_base.as_deref(), Some("file:/plugins/-"));
}

#[test]
fn test_parse_with_comments() {
    let src = r#"
        // top-level comment
        grant {
            /* block
               comment */
            permission java.lang.RuntimePermission "createClassLoader";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 1);
    assert_eq!(
        p.grants[0].permissions[0].target.as_deref(),
        Some("createClassLoader")
    );
}

#[test]
fn test_parse_permission_without_target() {
    let src = r#"
        grant {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    let perm = &p.grants[0].permissions[0];
    assert_eq!(perm.class_name, "java.security.AllPermission");
    assert!(perm.target.is_none());
    assert!(perm.actions.is_none());
}

#[test]
fn test_parse_escape_sequences() {
    let src = r#"
        grant {
            permission java.io.FilePermission "C:\\Users\\x", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(
        p.grants[0].permissions[0].target.as_deref(),
        Some(r"C:\Users\x")
    );
}

#[test]
fn test_parse_error_missing_brace() {
    let src = r#"
        grant {
            permission java.security.AllPermission
        };
    "#;
    let err = Policy::parse(src).unwrap_err();
    assert!(matches!(err, PolicyError::Parse { .. }));
}

#[test]
fn test_parse_error_bad_directive() {
    let err = Policy::parse("wibble {};").unwrap_err();
    match err {
        PolicyError::Parse { message, .. } => assert!(message.contains("grant")),
        _ => panic!("expected Parse error"),
    }
}

#[test]
fn test_parse_signed_by() {
    let src = r#"
        grant signedBy "myAlias" {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants[0].signed_by.as_deref(), Some("myAlias"));
}

// -- implies() tests --

#[test]
fn test_implies_all_permission() {
    let src = r#"grant { permission java.security.AllPermission; };"#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.io.FilePermission", "/etc/passwd", "read", None));
    assert!(p.implies("java.net.SocketPermission", "*:80", "connect", None));
}

#[test]
fn test_implies_specific_permission() {
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/*", "read,write";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.io.FilePermission", "/tmp/x", "read", None));
    assert!(p.implies("java.io.FilePermission", "/tmp/y", "write", None));
    assert!(!p.implies("java.io.FilePermission", "/etc/x", "read", None));
    assert!(!p.implies("java.io.FilePermission", "/tmp/x", "execute", None));
}

#[test]
fn test_implies_recursive_wildcard() {
    let src = r#"
        grant {
            permission java.io.FilePermission "/var/log/-", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.io.FilePermission",
        "/var/log/app/info.txt",
        "read",
        None
    ));
    assert!(p.implies("java.io.FilePermission", "/var/log/x", "read", None));
    assert!(!p.implies("java.io.FilePermission", "/etc/x", "read", None));
}

#[test]
fn test_implies_code_base_filter() {
    let src = r#"
        grant codeBase "file:/home/app/-" {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // Matches when codeBase is within /home/app.
    assert!(p.implies(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        Some("file:/home/app/lib/x.jar")
    ));
    // Does not match when codeBase is elsewhere.
    assert!(!p.implies(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        Some("file:/opt/other/y.jar")
    ));
    // Does not match when no codeBase is in scope.
    assert!(!p.implies("java.io.FilePermission", "/tmp/x", "read", None));
}

#[test]
fn test_implies_socket_wildcard_port() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "*:80", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "example.com:80",
        "connect",
        None
    ));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "example.com:443",
        "connect",
        None
    ));
}

#[test]
fn test_implies_actions_case_insensitive_subset() {
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/*", "read,write,execute";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.io.FilePermission", "/tmp/a", "READ", None));
    assert!(p.implies("java.io.FilePermission", "/tmp/a", "read,write", None));
    assert!(!p.implies("java.io.FilePermission", "/tmp/a", "delete", None));
}

#[test]
fn test_implies_slash_class_form() {
    let src = r#"grant { permission java.io.FilePermission "/x", "read"; };"#;
    let p = Policy::parse(src).unwrap();
    // Both forms resolve to the same permission.
    assert!(p.implies("java/io/FilePermission", "/x", "read", None));
    assert!(p.implies("java.io.FilePermission", "/x", "read", None));
}

#[test]
fn test_from_file_roundtrip() {
    use std::io::Write;
    let dir = std::env::temp_dir().join("cratonvm-policy-test");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("test.policy");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "grant codeBase \"file:/home/app/-\" {{\n    permission java.io.FilePermission \"/tmp/*\", \"read,write\";\n    permission java.net.SocketPermission \"*:80\", \"connect\";\n}};"
        )
        .unwrap();
    }
    let p = Policy::from_file(&path).unwrap();
    assert_eq!(p.grants.len(), 1);
    assert_eq!(p.grants[0].permissions.len(), 2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_code_base_single_wildcard() {
    // `/*` (single segment) vs `/-` (recursive).
    let src = r#"
        grant codeBase "file:/apps/*" {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // Direct child only.
    assert!(p.implies(
        "java.io.FilePermission",
        "/x",
        "read",
        Some("file:/apps/foo")
    ));
    // Not recursive.
    assert!(!p.implies(
        "java.io.FilePermission",
        "/x",
        "read",
        Some("file:/apps/foo/bar")
    ));
}

#[test]
fn test_empty_actions() {
    let src = r#"
        grant {
            permission java.lang.RuntimePermission "getProtectionDomain";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.lang.RuntimePermission",
        "getProtectionDomain",
        "",
        None
    ));
}

#[test]
fn test_keystore_directive_ignored() {
    let src = r#"
        keystore "file:keystore.jks", "jks";
        grant {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 1);
}

// -----------------------------------------------------------------------
// T11 · Extended SocketPermission target grammar.
// -----------------------------------------------------------------------

#[test]
fn t11_socket_permission_exact_host_port() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "api.example.com:443", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "api.example.com:443",
        "connect",
        None
    ));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "api.example.com:80",
        "connect",
        None
    ));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "other.example.com:443",
        "connect",
        None
    ));
}

#[test]
fn t11_socket_permission_port_range() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "*:8080-8090", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    for port in 8080..=8090 {
        assert!(
            p.implies(
                "java.net.SocketPermission",
                &format!("host.example:{port}"),
                "connect",
                None,
            ),
            "port {port} should be allowed"
        );
    }
    assert!(!p.implies(
        "java.net.SocketPermission",
        "host.example:8079",
        "connect",
        None
    ));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "host.example:8091",
        "connect",
        None
    ));
}

#[test]
fn t11_socket_permission_domain_wildcard() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "*.example.com:443", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "api.example.com:443",
        "connect",
        None
    ));
    assert!(p.implies(
        "java.net.SocketPermission",
        "deep.nested.example.com:443",
        "connect",
        None
    ));
    assert!(p.implies(
        "java.net.SocketPermission",
        "example.com:443",
        "connect",
        None
    ));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "otherexample.com:443",
        "connect",
        None
    ));
}

#[test]
fn t11_socket_permission_cidr_matches() {
    // /8 CIDR — entire first-octet 10.* block.
    let src = r#"
        grant {
            permission java.net.SocketPermission "10.0.0.0/8:80", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "10.1.2.3:80", "connect", None));
    assert!(p.implies(
        "java.net.SocketPermission",
        "10.255.255.255:80",
        "connect",
        None
    ));
    assert!(!p.implies("java.net.SocketPermission", "11.0.0.1:80", "connect", None));
    // Port mismatch still denies.
    assert!(!p.implies("java.net.SocketPermission", "10.1.2.3:443", "connect", None));
}

#[test]
fn t11_socket_permission_cidr_private_16() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "192.168.0.0/16", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // /16 + no port → any port OK.
    assert!(p.implies(
        "java.net.SocketPermission",
        "192.168.1.10:22",
        "connect",
        None
    ));
    assert!(p.implies(
        "java.net.SocketPermission",
        "192.168.255.1:8443",
        "connect",
        None
    ));
    assert!(!p.implies("java.net.SocketPermission", "10.0.0.1:22", "connect", None));
}

#[test]
fn t11_socket_permission_localhost_alias() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "localhost:8080", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "localhost:8080",
        "connect",
        None
    ));
    assert!(p.implies(
        "java.net.SocketPermission",
        "127.0.0.1:8080",
        "connect",
        None
    ));
    assert!(p.implies("java.net.SocketPermission", "::1:8080", "connect", None));
    assert!(!p.implies(
        "java.net.SocketPermission",
        "example.com:8080",
        "connect",
        None
    ));
}

#[test]
fn t11_socket_permission_still_supports_wildcard_port() {
    // Regression: `"*:<port>"` wildcard must keep working.
    let src = r#"
        grant {
            permission java.net.SocketPermission "*:80", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "anything.example.com:80",
        "connect",
        None
    ));
}

#[test]
fn t11_signed_by_blocks_unmatched_signer() {
    // When a grant has a signedBy clause, classes without that signer
    // digest must NOT pick up its permissions.
    let src = r#"
        grant signedBy "abc123" {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // Unsigned class: no digests → grant filtered out.
    assert!(!p.implies_full(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        None,
        &[] as &[&str],
    ));
    // Wrong signer digest → filtered out.
    assert!(!p.implies_full(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        None,
        &["deadbeef".to_string()],
    ));
    // Correct signer digest → grant applies.
    assert!(p.implies_full(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        None,
        &["abc123".to_string()],
    ));
}

#[test]
fn t11_signed_by_case_insensitive() {
    let src = r#"
        grant signedBy "ABCDEF" {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies_full(
        "java.io.FilePermission",
        "/x",
        "read",
        None,
        &["abcdef".to_string()],
    ));
}

#[test]
fn t11_port_range_open_ended() {
    // JDK accepts `-8080` (all ports up to 8080) and `8080-` (8080+).
    let src = r#"
        grant {
            permission java.net.SocketPermission "host:-1024", "connect";
            permission java.net.SocketPermission "host:1025-", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "host:80", "connect", None));
    assert!(p.implies("java.net.SocketPermission", "host:1024", "connect", None));
    assert!(p.implies("java.net.SocketPermission", "host:65535", "connect", None));
}

// -----------------------------------------------------------------------
// T17.Γ · X.509 Subject DN + IPv6 CIDR.
// -----------------------------------------------------------------------

/// Build a PKCS#7 signer blob whose signer cert has the given CN.
fn make_pkcs7_with_cn(cn: &str) -> Vec<u8> {
    use super::super::x509::builder::*;
    // 2.5.4.3 is id-at-commonName.
    let cn_oid = [0x55, 0x04, 0x03];
    let subject = name(&[rdn(&atv(&cn_oid, &printable_string(cn)))]);
    let cert = certificate(&subject);
    pkcs7_signed_data(&cert)
}

fn dn_of(pkcs7: &[u8]) -> String {
    x509::parse_signer_dn(pkcs7).expect("pkcs7 must parse")
}

#[test]
fn t17_g_signed_by_dn_matches_cert_subject() {
    // grant signedBy "CN=Acme Corp" must match a cert whose subject
    // renders as exactly "CN=Acme Corp".
    let src = r#"
        grant signedBy "CN=Acme Corp" {
            permission java.io.FilePermission "/opt/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    let pkcs7 = make_pkcs7_with_cn("Acme Corp");
    let dn = dn_of(&pkcs7);
    assert_eq!(dn, "CN=Acme Corp");
    assert!(p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
}

#[test]
fn t17_g_signed_by_dn_substring_match() {
    // JDK's signedBy is a loose substring match.
    let src = r#"
        grant signedBy "Acme" {
            permission java.io.FilePermission "/opt/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    let pkcs7 = make_pkcs7_with_cn("Acme Corp");
    let dn = dn_of(&pkcs7);
    assert!(p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
}

#[test]
fn t17_g_signed_by_no_match_denied() {
    // signedBy "CN=Evil" against a cert whose DN is "CN=Acme" →
    // grant does not apply.
    let src = r#"
        grant signedBy "CN=Evil" {
            permission java.io.FilePermission "/opt/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    let pkcs7 = make_pkcs7_with_cn("Acme");
    let dn = dn_of(&pkcs7);
    assert!(!p.implies_full("java.io.FilePermission", "/opt/a", "read", None, &[dn],));
}

#[test]
fn t17_g_signed_by_malformed_pkcs7_returns_err() {
    // Random bytes must not panic — the parser rejects them.
    let garbage: Vec<u8> = (0..256).map(|i| (i * 7 + 13) as u8).collect();
    let err = x509::parse_signer_dn(&garbage).unwrap_err();
    assert!(matches!(
        err,
        x509::X509Error::NotPkcs7
            | x509::X509Error::MalformedAsn1
            | x509::X509Error::NoSignerCert
    ));
}

#[test]
fn t17_g_ipv6_cidr_matches_loopback() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "::1/128", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "[::1]:443", "connect", None));
    // Non-loopback must not match a /128.
    assert!(!p.implies(
        "java.net.SocketPermission",
        "[2001:db8::1]:443",
        "connect",
        None
    ));
}

#[test]
fn t17_g_ipv6_cidr_fe80_matches_link_local() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "fe80::/10", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "[fe80::1]:80", "connect", None));
    // Another link-local address in the block.
    assert!(p.implies(
        "java.net.SocketPermission",
        "[febf:ffff::]:80",
        "connect",
        None
    ));
    // 2001:db8::/32 is global scope, must not match.
    assert!(!p.implies(
        "java.net.SocketPermission",
        "[2001:db8::1]:80",
        "connect",
        None
    ));
}

#[test]
fn t17_g_ipv6_port_range_bracketed() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "[::1]:8080-8090", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "[::1]:8085", "connect", None));
    assert!(!p.implies("java.net.SocketPermission", "[::1]:9000", "connect", None));
}

#[test]
fn t17_g_ipv4_still_works() {
    // Regression: Delta's IPv4 CIDR path must stay intact.
    let src = r#"
        grant {
            permission java.net.SocketPermission "10.0.0.0/8", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies("java.net.SocketPermission", "10.1.2.3:80", "connect", None));
    assert!(!p.implies("java.net.SocketPermission", "11.0.0.1:80", "connect", None));
}

#[test]
fn t17_g_ipv6_any_prefix_matches_everything() {
    // ::/0 is the IPv6 "match anywhere" CIDR.
    let src = r#"
        grant {
            permission java.net.SocketPermission "::/0", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.implies(
        "java.net.SocketPermission",
        "[2001:db8::cafe]:8080",
        "connect",
        None
    ));
    // v4-mapped form also ok.
    assert!(p.implies(
        "java.net.SocketPermission",
        "[::ffff:10.0.0.1]:80",
        "connect",
        None
    ));
}

#[test]
fn t17_g_ipv6_malformed_forms_do_not_match() {
    let src = r#"
        grant {
            permission java.net.SocketPermission "::1/128", "connect";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // Triple-colon and double-compression are invalid per RFC 4291;
    // std::net::Ipv6Addr rejects them, so matching returns false.
    assert!(!p.implies("java.net.SocketPermission", "[:::1]:443", "connect", None));
    assert!(!p.implies("java.net.SocketPermission", "[::1::]:443", "connect", None));
}

#[test]
fn t17_g_signed_by_digest_mode_preserved() {
    // 64 hex chars → SHA-256 digest mode.  DN strings in the slice
    // must NOT match a 64-hex alias.
    let digest = "a".repeat(64);
    let src_template = format!(
        r#"grant signedBy "{digest}" {{ permission java.io.FilePermission "/x", "read"; }};"#
    );
    let p = Policy::parse(&src_template).unwrap();
    // DN present but no matching digest → denied.
    assert!(!p.implies_full(
        "java.io.FilePermission",
        "/x",
        "read",
        None,
        &["CN=Acme Corp".to_string()],
    ));
    // Correct digest → allowed.
    assert!(p.implies_full(
        "java.io.FilePermission",
        "/x",
        "read",
        None,
        &[digest.clone()],
    ));
}

// -----------------------------------------------------------------------
// WP6.8 · Property substitution + principal enforcement.
// -----------------------------------------------------------------------

#[test]
fn wp68_substitution_resolves_known_property() {
    set_test_property("policy.test.home", Some("/opt/myapp"));
    let src = r#"
        grant codeBase "file:${policy.test.home}/lib/-" {
            permission java.io.FilePermission "/tmp/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // Substituted code base must be the resolved path.
    assert_eq!(p.grants.len(), 1);
    assert!(!p.grants[0].disabled);
    assert_eq!(
        p.grants[0].code_base.as_deref(),
        Some("file:/opt/myapp/lib/-")
    );
    // And it must actually match a request from that code base.
    assert!(p.implies(
        "java.io.FilePermission",
        "/tmp/x",
        "read",
        Some("file:/opt/myapp/lib/x.jar"),
    ));
    set_test_property("policy.test.home", None);
}

#[test]
fn wp68_substitution_failure_disables_grant() {
    // Make sure no test_property is set for this name.
    set_test_property("policy.unset.var.zzz", None);
    let src = r#"
        grant codeBase "file:${policy.unset.var.zzz}/-" {
            permission java.security.AllPermission;
        };
        grant {
            permission java.io.FilePermission "/safe/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 2);
    assert!(
        p.grants[0].disabled,
        "grant with unset ${{...}} must be disabled"
    );
    assert!(!p.grants[1].disabled, "second grant must be unaffected");
    // The disabled grant cannot be used to satisfy a permission.
    assert!(!p.implies(
        "java.security.AllPermission",
        "",
        "",
        Some("file:/anything/-"),
    ));
    // The unaffected grant still works.
    assert!(p.implies("java.io.FilePermission", "/safe/x", "read", None));
}

#[test]
fn wp68_substitution_path_separator_shortcut() {
    let src = r#"
        grant codeBase "file:${/}usr${/}share${/}-" {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    let cb = p.grants[0].code_base.as_deref().unwrap();
    // Platform-correct separator was inserted.
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let expected = format!("file:{sep}usr{sep}share{sep}-");
    assert_eq!(cb, expected);
}

#[test]
fn wp68_substitution_dollar_escape_preserves_literal() {
    // `\$` must produce a literal `$` and NOT trigger
    // substitution of `${USER}`, even if that env var is set.
    let src = r#"
        grant {
            permission java.io.FilePermission "/tmp/file_\${USER}_x", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(
        p.grants[0].permissions[0].target.as_deref(),
        Some("/tmp/file_${USER}_x")
    );
}

#[test]
fn wp68_substitution_unset_var_in_permission_disables_grant() {
    // Substitution failure inside a permission target also
    // disables the surrounding grant.
    set_test_property("wp68.unset.var", None);
    let src = r#"
        grant {
            permission java.io.FilePermission "${wp68.unset.var}", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 1);
    assert!(p.grants[0].disabled);
    // Negative implication: the grant is dead, so even an exact
    // literal target must not match.
    assert!(!p.implies("java.io.FilePermission", "${wp68.unset.var}", "read", None,));
}

#[test]
fn wp68_principal_clause_now_captured() {
    // Old code dropped `principal ...` on the floor; this test
    // pins the new behaviour where the clause becomes a Subject
    // filter.
    let src = r#"
        grant principal javax.security.auth.x500.X500Principal "CN=Alice" {
            permission java.io.FilePermission "/secret/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 1);
    assert_eq!(p.grants[0].principals.len(), 1);
    assert_eq!(
        p.grants[0].principals[0].class_name,
        "javax.security.auth.x500.X500Principal"
    );
    assert_eq!(p.grants[0].principals[0].name, "CN=Alice");
}

#[test]
fn wp68_principal_filter_blocks_unauthenticated_caller() {
    // A grant with a `principal` clause must NOT match when the
    // caller has no Subject.  This is the security-critical
    // behaviour — the previous parser silently turned this grant
    // into an unrestricted wildcard.
    let src = r#"
        grant principal javax.security.auth.x500.X500Principal "CN=Alice" {
            permission java.io.FilePermission "/secret/*", "read";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    // No subject → grant must not apply.
    assert!(!p.implies_full_with_principals(
        "java.io.FilePermission",
        "/secret/x",
        "read",
        None,
        &[] as &[&str],
        &[],
    ));
    // Wrong principal → grant must not apply.
    assert!(!p.implies_full_with_principals(
        "java.io.FilePermission",
        "/secret/x",
        "read",
        None,
        &[] as &[&str],
        &[(
            "javax.security.auth.x500.X500Principal".to_string(),
            "CN=Bob".to_string(),
        )],
    ));
    // Matching principal → grant applies.
    assert!(p.implies_full_with_principals(
        "java.io.FilePermission",
        "/secret/x",
        "read",
        None,
        &[] as &[&str],
        &[(
            "javax.security.auth.x500.X500Principal".to_string(),
            "CN=Alice".to_string(),
        )],
    ));
}

#[test]
fn wp68_principal_class_and_name_wildcards() {
    let src = r#"
        grant principal * "CN=Anyone" {
            permission java.lang.RuntimePermission "doX";
        };
        grant principal javax.security.auth.x500.X500Principal * {
            permission java.lang.RuntimePermission "doY";
        };
        grant principal * * {
            permission java.lang.RuntimePermission "doZ";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants.len(), 3);

    // First grant: any class with name "CN=Anyone".
    let alice = ("com.acme.MyPrincipal".to_string(), "CN=Anyone".to_string());
    assert!(p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doX",
        "",
        None,
        &[] as &[&str],
        &[alice.clone()],
    ));

    // Second grant: X500Principal with any name.
    let bob = (
        "javax.security.auth.x500.X500Principal".to_string(),
        "CN=Bob".to_string(),
    );
    assert!(p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doY",
        "",
        None,
        &[] as &[&str],
        &[bob.clone()],
    ));

    // Third grant: matches everyone (still requires a subject).
    let any = ("X".to_string(), "Y".to_string());
    assert!(p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doZ",
        "",
        None,
        &[] as &[&str],
        &[any.clone()],
    ));
    // But still rejects unauthenticated callers (every grant
    // here has at least one principal clause).
    assert!(!p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doZ",
        "",
        None,
        &[] as &[&str],
        &[],
    ));
}

#[test]
fn wp68_multiple_principals_require_all_match() {
    // JDK semantics: multi-principal grants are conjunctive.
    let src = r#"
        grant principal A "alpha", principal B "beta" {
            permission java.lang.RuntimePermission "doIt";
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert_eq!(p.grants[0].principals.len(), 2);
    // Only one principal → not enough.
    assert!(!p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doIt",
        "",
        None,
        &[] as &[&str],
        &[("A".to_string(), "alpha".to_string())],
    ));
    // Both principals → matches.
    assert!(p.implies_full_with_principals(
        "java.lang.RuntimePermission",
        "doIt",
        "",
        None,
        &[] as &[&str],
        &[
            ("A".to_string(), "alpha".to_string()),
            ("B".to_string(), "beta".to_string()),
        ],
    ));
}

#[test]
fn wp68_disabled_grant_not_used_via_signedby_path_either() {
    // Belt-and-braces: a disabled grant must not produce a match
    // when consulted via the cert-digest path.
    set_test_property("wp68.unset.signed", None);
    let src = r#"
        grant signedBy "abc" codeBase "${wp68.unset.signed}" {
            permission java.security.AllPermission;
        };
    "#;
    let p = Policy::parse(src).unwrap();
    assert!(p.grants[0].disabled);
    assert!(!p.implies_full(
        "java.io.FilePermission",
        "/x",
        "read",
        Some("any"),
        &["abc".to_string()],
    ));
}
