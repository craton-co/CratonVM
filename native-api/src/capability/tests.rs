use super::*;

fn set(mode: CapabilityMode) -> CapabilitySet {
    CapabilitySet::new(VmId::from_raw(1), mode)
}

/// `p5-natreg`: `check_native_register` is `check(native_register(..))`
/// row for row -- counts, first sites, ungranted tallies, refusals, and
/// the truncation past `MAX_AUDIT_ENTRIES` -- in every mode, with repeats
/// and interleaved other capabilities. One call site per arm, so the two
/// sets record different `first_site`s only by line: compared with the
/// sites stripped of their line, plus a direct line check.
#[test]
fn check_native_register_records_what_check_records() {
    for mode in [
        CapabilityMode::Permissive,
        CapabilityMode::Audit,
        CapabilityMode::Enforce,
    ] {
        let mut a = set(mode);
        let mut b = set(mode);
        let grant = Capability::parse_grant("native-register:java/lang/")
            .expect("a native-register name-prefix grant parses");
        a.grant(grant.clone());
        b.grant(grant);
        let names: Vec<(String, String)> = (0..MAX_AUDIT_ENTRIES + 300)
            .map(|i| {
                let class = if i % 3 == 0 {
                    "java/lang/Thing"
                } else {
                    "org/app/Thing"
                };
                (
                    format!("{class}{}", i % (MAX_AUDIT_ENTRIES + 100)),
                    format!("m{}", i % 7),
                )
            })
            .collect();
        for (i, (c, m)) in names.iter().enumerate() {
            if i % 500 == 0 {
                let _ = a.check(Capability::raw_memory("x"));
                let _ = b.check(Capability::raw_memory("x"));
            }
            let ra = a
                .check(Capability::native_register(c, m))
                .map_err(|e| e.capability);
            let rb = b.check_native_register(c, m).map_err(|e| e.capability);
            assert_eq!(ra, rb, "{mode:?} {c}.{m}");
        }
        let strip = |r: CapabilityAuditReport| {
            let uses: Vec<_> = r
                .uses
                .into_iter()
                .map(|u| (u.capability, u.count, u.ungranted, u.first_site.file))
                .collect();
            (r.mode, uses, r.truncated)
        };
        assert_eq!(strip(a.audit_report()), strip(b.audit_report()), "{mode:?}");
    }
}

// -----------------------------------------------------------------------
// Path normalization + prefix scope matching
// -----------------------------------------------------------------------

#[test]
fn normalize_folds_separators_and_dots() {
    assert_eq!(normalize_path("/data/./sub//file"), "/data/sub/file");
    assert_eq!(normalize_path("/data\\sub\\file"), "/data/sub/file");
    assert_eq!(normalize_path("data/sub/"), "data/sub");
    assert_eq!(normalize_path("/"), "/");
}

#[test]
fn normalize_resolves_interior_parent_segments() {
    // The JDK opens `a/../b` as `b`; so must we, or a legitimate path is
    // rejected. See native-io's `has_escaping_parent_segment` for the same
    // argument on the confinement side.
    assert_eq!(normalize_path("/data/sub/../file"), "/data/file");
    assert_eq!(normalize_path("data/sub/../file"), "data/file");
}

#[test]
fn normalize_clamps_absolute_parent_at_root_and_keeps_relative_escape() {
    assert_eq!(normalize_path("/../etc/passwd"), "/etc/passwd");
    assert_eq!(normalize_path("/data/../../etc"), "/etc");
    // A relative path that climbs above its anchor keeps the marker, so no
    // prefix grant can contain it.
    assert_eq!(normalize_path("../etc/passwd"), "../etc/passwd");
    assert_eq!(normalize_path("data/../../etc"), "../etc");
}

#[test]
fn path_grant_admits_only_what_is_under_it() {
    let grant = Scope::path("/data");
    assert!(grant.admits(&Scope::path("/data")));
    assert!(grant.admits(&Scope::path("/data/file")));
    assert!(grant.admits(&Scope::path("/data/sub/file")));
    // Sibling with a shared textual prefix must NOT match.
    assert!(!grant.admits(&Scope::path("/database/file")));
    assert!(!grant.admits(&Scope::path("/etc/passwd")));
}

#[test]
fn path_traversal_cannot_escape_a_granted_prefix() {
    let grant = Scope::path("/data");
    // The whole point: `..` must be resolved BEFORE the prefix test.
    assert!(!grant.admits(&Scope::path("/data/../etc/passwd")));
    assert!(!grant.admits(&Scope::path("/data/sub/../../etc/passwd")));
    assert!(!grant.admits(&Scope::path("/data/../../etc/passwd")));
    // Backslash spelling of the same escape (Windows-style input on any
    // host) must be rejected identically.
    assert!(!grant.admits(&Scope::path("/data\\..\\etc\\passwd")));
    // A `..` that merely cancels an interior component stays inside.
    assert!(grant.admits(&Scope::path("/data/sub/../file")));
}

#[test]
fn relative_grant_root_rejects_absolute_and_escaping_requests() {
    let grant = Scope::path("work");
    assert!(grant.admits(&Scope::path("work/out.txt")));
    assert!(!grant.admits(&Scope::path("/work/out.txt")));
    assert!(!grant.admits(&Scope::path("../work/out.txt")));
}

#[cfg(windows)]
#[test]
fn windows_paths_compare_case_insensitively() {
    // NTFS is case-insensitive; a case-sensitive prefix test would let
    // `C:\DATA\x` slip past a `c:\data` grant.
    let grant = Scope::path(r"C:\data");
    assert!(grant.admits(&Scope::path(r"c:\DATA\file")));
    assert!(!grant.admits(&Scope::path(r"C:\data\..\windows\system32")));
}

// -----------------------------------------------------------------------
// Host / port scope matching
// -----------------------------------------------------------------------

#[test]
fn host_glob_matches_suffix_domain_and_subdomains() {
    let grant = Capability::parse_grant("network:*.example.com:443").unwrap();
    assert!(grant.admits(&Capability::network("example.com:443")));
    assert!(grant.admits(&Capability::network("api.example.com:443")));
    assert!(grant.admits(&Capability::network("a.b.example.com:443")));
    // Not a suffix match — `notexample.com` must not be admitted.
    assert!(!grant.admits(&Capability::network("notexample.com:443")));
    // Right host, wrong port.
    assert!(!grant.admits(&Capability::network("api.example.com:8443")));
}

#[test]
fn host_wildcard_and_port_range() {
    let any_host = Capability::parse_grant("network:*:443").unwrap();
    assert!(any_host.admits(&Capability::network("anything.invalid:443")));
    assert!(!any_host.admits(&Capability::network("anything.invalid:80")));

    let range = Capability::parse_grant("network:localhost:8000-8100").unwrap();
    assert!(range.admits(&Capability::network("localhost:8000")));
    assert!(range.admits(&Capability::network("localhost:8100")));
    assert!(!range.admits(&Capability::network("localhost:8101")));
}

#[test]
fn host_case_is_ignored_and_ipv6_literals_parse() {
    let grant = Capability::parse_grant("network:API.Example.COM:443").unwrap();
    assert!(grant.admits(&Capability::network("api.example.com:443")));

    assert_eq!(
        Scope::endpoint_str("[::1]:8080"),
        Scope::Endpoint {
            host: "::1".to_string(),
            port: PortSpec::Exact(8080)
        }
    );
}

#[test]
fn a_request_that_cannot_name_its_port_is_refused_by_a_narrow_grant() {
    // Fail closed: an address the call site could not parse becomes
    // `PortSpec::Any`, and a `:443` grant must not admit it.
    let grant = Capability::parse_grant("network:example.com:443").unwrap();
    let vague = Capability::network("example.com");
    assert!(!grant.admits(&vague));
    // Only an unbounded grant admits it.
    assert!(Capability::parse_grant("network:example.com:*")
        .unwrap()
        .admits(&vague));
}

#[test]
fn name_grants_match_exactly_or_by_prefix() {
    let exact = Capability::parse_grant("library-load:libssl.so.3").unwrap();
    assert!(exact.admits(&Capability::library_load("libssl.so.3")));
    assert!(!exact.admits(&Capability::library_load("libssl.so.1")));

    let prefix = Capability::parse_grant("library-load:libssl*").unwrap();
    assert!(prefix.admits(&Capability::library_load("libssl.so.3")));
    assert!(!prefix.admits(&Capability::library_load("libcrypto.so.3")));
}

#[test]
fn scopes_of_different_shapes_never_admit_each_other() {
    let path = Scope::path("/data");
    let endpoint = Scope::endpoint("example.com", 443);
    let name = Scope::name("libssl");
    assert!(!path.admits(&endpoint));
    assert!(!endpoint.admits(&path));
    assert!(!path.admits(&name));
    // A `Name` grant may cover a `Path` request (documented, for
    // `System.load("/usr/lib/libssl.so")` under `library-load:*ssl*`-style
    // grants) but never the reverse.
    assert!(Scope::name("*").admits(&Scope::path("/usr/lib/libssl.so")));
    assert!(!path.admits(&Scope::name("/data/x")));
}

#[test]
fn an_any_request_is_admitted_only_by_an_any_grant() {
    // A gate that cannot say what it is touching must not slide under a
    // narrow grant.
    let narrow = Capability::FileRead(Scope::path("/data"));
    let vague = Capability::FileRead(Scope::Any);
    assert!(!narrow.admits(&vague));
    assert!(Capability::FileRead(Scope::Any).admits(&vague));
}

// -----------------------------------------------------------------------
// Modes
// -----------------------------------------------------------------------

#[test]
fn permissive_allows_everything_and_still_counts() {
    let caps = set(CapabilityMode::Permissive);
    assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());
    assert!(caps.check(Capability::process_spawn("/bin/sh")).is_ok());
    assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());

    let report = caps.audit_report();
    assert_eq!(report.mode, CapabilityMode::Permissive);
    assert_eq!(report.uses.len(), 2, "{report}");
    assert_eq!(report.total_checks(), 3);
}

#[test]
fn audit_allows_everything_but_prices_the_enforce_flip() {
    let mut caps = set(CapabilityMode::Audit);
    caps.grant(Capability::parse_grant("file-read:/data").unwrap());

    // Granted — allowed, and NOT counted against the flip.
    assert!(caps.check(Capability::file_read("/data/a")).is_ok());
    // Ungranted — still allowed (this is Audit), but priced.
    assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());
    assert!(caps.check(Capability::file_read("/etc/shadow")).is_ok());

    let report = caps.audit_report();
    assert_eq!(report.total_checks(), 3);
    assert_eq!(
        report.total_ungranted(),
        2,
        "an Enforce flip would refuse exactly the two /etc/shadow reads:\n{report}"
    );
    let shadow = report
        .uses
        .iter()
        .find(|u| matches!(u.capability.scope(), Scope::Path(p) if p == "/etc/shadow"))
        .expect("recorded");
    assert_eq!(shadow.count, 2);
    assert_eq!(shadow.ungranted, 2);
}

#[test]
fn enforce_denies_what_is_not_granted_and_allows_what_is() {
    let mut caps = set(CapabilityMode::Enforce);
    caps.grant(Capability::parse_grant("file-read:/data").unwrap())
        .grant(Capability::parse_grant("network:*.example.com:443").unwrap());

    assert!(caps.check(Capability::file_read("/data/a")).is_ok());
    assert!(caps
        .check(Capability::network("api.example.com:443"))
        .is_ok());
    assert!(caps.check(Capability::file_read("/etc/shadow")).is_err());
    assert!(caps.check(Capability::file_write("/data/a")).is_err());
    assert!(caps.check(Capability::process_spawn("/bin/sh")).is_err());
}

#[test]
fn enforce_with_no_grants_denies_everything() {
    let caps = set(CapabilityMode::Enforce);
    for kind in CapabilityKind::ALL {
        let req = Capability::of(kind, Scope::name("anything"));
        assert!(
            caps.check(req).is_err(),
            "{kind:?} must be denied by an empty Enforce set"
        );
    }
}

#[test]
fn mode_parsing_covers_the_operator_spellings() {
    assert_eq!(
        CapabilityMode::parse("PERMISSIVE"),
        Some(CapabilityMode::Permissive)
    );
    assert_eq!(CapabilityMode::parse("audit"), Some(CapabilityMode::Audit));
    assert_eq!(
        CapabilityMode::parse(" Enforce "),
        Some(CapabilityMode::Enforce)
    );
    assert_eq!(CapabilityMode::parse("deny"), Some(CapabilityMode::Enforce));
    assert_eq!(CapabilityMode::parse("nonsense"), None);
    // The default must be permissive — a default-deny flip is a separate,
    // documented migration (docs/security/native-capabilities.md).
    assert_eq!(CapabilityMode::default(), CapabilityMode::Permissive);
}

// -----------------------------------------------------------------------
// Per-VM isolation
// -----------------------------------------------------------------------

#[test]
fn two_vms_with_conflicting_grants_do_not_interfere() {
    let mut a = CapabilitySet::new(VmId::from_raw(0xA), CapabilityMode::Enforce);
    a.grant(Capability::parse_grant("file-read:/a").unwrap());
    let mut b = CapabilitySet::new(VmId::from_raw(0xB), CapabilityMode::Enforce);
    b.grant(Capability::parse_grant("file-read:/b").unwrap());

    assert!(a.check(Capability::file_read("/a/x")).is_ok());
    assert!(a.check(Capability::file_read("/b/x")).is_err());
    assert!(b.check(Capability::file_read("/b/x")).is_ok());
    assert!(b.check(Capability::file_read("/a/x")).is_err());

    // Audit logs are separate too — VM A never sees VM B's traffic.
    assert_eq!(a.audit_report().vm, VmId::from_raw(0xA));
    assert_eq!(b.audit_report().vm, VmId::from_raw(0xB));
    assert_eq!(a.audit_report().total_checks(), 2);
    assert_eq!(b.audit_report().total_checks(), 2);
}

#[test]
fn modes_are_per_vm_too() {
    // The specific cross-VM interference the C2 review names: a policy
    // change in one VM must not alter another's behaviour.
    let strict = CapabilitySet::new(VmId::from_raw(1), CapabilityMode::Enforce);
    let lax = CapabilitySet::new(VmId::from_raw(2), CapabilityMode::Permissive);
    let req = || Capability::process_spawn("/bin/sh");
    assert!(strict.check(req()).is_err());
    assert!(lax.check(req()).is_ok());
}

#[test]
fn installation_is_keyed_by_vm_and_is_reversible() {
    // Use VmIds that no other test installs, so this stays independent of
    // test execution order.
    let vm_x = VmId::from_raw(0x5EC_0001);
    let vm_y = VmId::from_raw(0x5EC_0002);

    let mut x = CapabilitySet::new(vm_x, CapabilityMode::Enforce);
    x.grant(Capability::parse_grant("file-read:/x").unwrap());
    let y = CapabilitySet::new(vm_y, CapabilityMode::Permissive);

    assert!(capabilities_for(vm_x).is_none());
    assert!(install_capabilities(Arc::new(x)).is_none());
    assert!(install_capabilities(Arc::new(y)).is_none());

    let looked_up = capabilities_for(vm_x).expect("installed");
    assert_eq!(looked_up.mode(), CapabilityMode::Enforce);
    assert!(looked_up.check(Capability::file_read("/x/a")).is_ok());
    assert!(looked_up.check(Capability::file_read("/y/a")).is_err());

    // The other VM's policy is untouched by the first VM's enforcement.
    let other = capabilities_for(vm_y).expect("installed");
    assert_eq!(other.mode(), CapabilityMode::Permissive);
    assert!(other.check(Capability::file_read("/x/a")).is_ok());

    // Replacement returns the displaced set; teardown removes the entry.
    let replaced = install_capabilities(Arc::new(CapabilitySet::new(
        vm_x,
        CapabilityMode::Permissive,
    )));
    assert!(replaced.is_some());
    assert!(uninstall_capabilities(vm_x).is_some());
    assert!(uninstall_capabilities(vm_y).is_some());
    assert!(capabilities_for(vm_x).is_none());
    assert!(uninstall_capabilities(vm_x).is_none());
}

/// JIT round 16 wave 11 (lane ffmcost): the length mirror answers `true`
/// while a policy is installed. (Only the positive direction is asserted:
/// a parallel test may hold its own entry, so `false` after the uninstall
/// is not this test's to promise.)
#[test]
fn any_capabilities_installed_sees_an_installed_policy() {
    let vm = VmId::from_raw(0x5EC_0F11);
    assert!(
        install_capabilities(Arc::new(CapabilitySet::new(vm, CapabilityMode::Permissive)))
            .is_none()
    );
    assert!(any_capabilities_installed());
    assert!(uninstall_capabilities(vm).is_some());
}

// -----------------------------------------------------------------------
// Denial error content
// -----------------------------------------------------------------------

#[test]
fn denial_carries_kind_scope_vm_and_call_site() {
    let caps = CapabilitySet::new(VmId::from_raw(0x1234), CapabilityMode::Enforce);
    // Deliberately one line, with `line!()` immediately after: `check` is
    // `#[track_caller]`, so the recorded site must be the gate's line.
    //
    // `#[rustfmt::skip]` is LOAD-BEARING, not style. `cargo fmt` reflowed
    // this call across three lines in `3de6b9c64`, `line!() - 1` then named
    // the `.expect_err` line instead of the `.check` line, and the test went
    // off by exactly one — red for every lane on the first gate it runs.
    // The comment above already said "deliberately one line" and a comment
    // is not a compile-time link; this attribute is.
    #[rustfmt::skip]
    let denied = caps.check(Capability::network("metadata.internal:80")).expect_err("denied");
    let expected_line = line!() - 1;

    assert_eq!(denied.capability, CapabilityKind::Network);
    assert_eq!(
        denied.scope,
        Scope::Endpoint {
            host: "metadata.internal".to_string(),
            port: PortSpec::Exact(80)
        }
    );
    assert_eq!(denied.vm, VmId::from_raw(0x1234));
    assert!(
        denied.site.file.ends_with("capability/tests.rs"),
        "site should name the gate's file, got {}",
        denied.site.file
    );
    assert_eq!(
        denied.site.line, expected_line,
        "#[track_caller] must report the gate line, not capability.rs internals"
    );

    // The message names every field and hands back a paste-able grant.
    let rendered = denied.to_string();
    assert!(rendered.contains("network"), "{rendered}");
    assert!(rendered.contains("metadata.internal:80"), "{rendered}");
    assert!(rendered.contains("vm#0x1234"), "{rendered}");
    assert_eq!(denied.suggested_grant(), "network:metadata.internal:80");
    // And that suggestion actually admits the request it came from.
    let grant = Capability::parse_grant(&denied.suggested_grant()).unwrap();
    assert!(grant.admits(&Capability::network("metadata.internal:80")));
}

#[test]
fn denial_converts_into_a_security_exception() {
    let caps = CapabilitySet::new(VmId::from_raw(7), CapabilityMode::Enforce);
    let denied = caps
        .check(Capability::process_spawn("/bin/sh"))
        .expect_err("denied");
    match RuntimeError::from(denied) {
        RuntimeError::SecurityException { message } => {
            assert!(message.contains("process-spawn"), "{message}");
        }
        other => panic!("expected SecurityException, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Grant-list parsing
// -----------------------------------------------------------------------

#[test]
fn grant_list_parses_and_reports_bad_entries() {
    let mut caps = set(CapabilityMode::Enforce);
    let bad = caps.grant_from_list(
        "file-read:/data; file-write:/tmp ;network:*.example.com:443;\
         process-spawn:*;bogus-kind:/x;file-read",
    );
    assert_eq!(bad, vec!["bogus-kind:/x", "file-read"]);
    assert_eq!(caps.grants().len(), 4);
    assert!(caps.check(Capability::file_read("/data/x")).is_ok());
    assert!(caps.check(Capability::file_write("/tmp/x")).is_ok());
    assert!(caps
        .check(Capability::process_spawn("/bin/anything"))
        .is_ok());
    assert!(caps.check(Capability::file_write("/data/x")).is_err());
}

#[test]
fn every_kind_round_trips_through_the_grant_syntax() {
    for kind in CapabilityKind::ALL {
        let text = format!("{}:*", kind.as_str());
        let parsed =
            Capability::parse_grant(&text).unwrap_or_else(|| panic!("{text} must parse"));
        assert_eq!(parsed.kind(), kind);
        assert_eq!(parsed.scope(), &Scope::Any);
        assert_eq!(CapabilityKind::parse(kind.as_str()), Some(kind));
    }
}

// -----------------------------------------------------------------------
// Audit report
// -----------------------------------------------------------------------

#[test]
fn audit_report_is_sorted_deterministic_and_derives_a_working_grant_set() {
    let caps = set(CapabilityMode::Audit);
    let allow = |r: Result<(), CapabilityDenied>| r.expect("audit mode allows");
    allow(caps.check(Capability::network("db.internal:5432")));
    allow(caps.check(Capability::file_read("/data/b")));
    allow(caps.check(Capability::file_read("/data/a")));
    allow(caps.check(Capability::file_read("/data/a")));
    allow(caps.check(Capability::library_load("libssl.so.3")));

    let report = caps.audit_report();
    assert!(!report.truncated);
    assert_eq!(report.uses.len(), 4);
    assert_eq!(report.total_checks(), 5);
    // Nothing is granted, so an Enforce flip would refuse every check.
    assert_eq!(report.total_ungranted(), 5);

    // Deterministic ordering: kind first (declaration order), then scope.
    let order: Vec<String> = report
        .uses
        .iter()
        .map(|u| u.capability.to_string())
        .collect();
    assert_eq!(
        order,
        vec![
            "file-read:/data/a",
            "file-read:/data/b",
            "network:db.internal:5432",
            "library-load:libssl.so.3",
        ]
    );
    assert_eq!(caps.audit_report(), report, "report must be stable");

    // The derived grant set must actually admit everything the run did —
    // this is the whole promise of the audit mode.
    let mut derived = CapabilitySet::new(VmId::from_raw(99), CapabilityMode::Enforce);
    assert!(derived
        .grant_from_list(&report.suggested_grants())
        .is_empty());
    for use_row in &report.uses {
        assert!(
            derived.check(use_row.capability.clone()).is_ok(),
            "derived grants must admit {}",
            use_row.capability
        );
    }
    // ...and nothing else.
    assert!(derived.check(Capability::file_read("/etc/shadow")).is_err());
}

#[test]
fn audit_records_the_first_call_site_not_the_last() {
    let caps = set(CapabilityMode::Permissive);
    // One line, and `#[rustfmt::skip]` to keep it that way — see
    // `denial_carries_kind_scope_vm_and_call_site` for what a reflow does
    // to `line!() - 1`.
    #[rustfmt::skip]
    caps.check(Capability::file_read("/data/a")).expect("permissive allows");
    let first = line!() - 1;
    caps.check(Capability::file_read("/data/a"))
        .expect("permissive allows");

    let report = caps.audit_report();
    assert_eq!(report.uses.len(), 1);
    assert_eq!(report.uses[0].count, 2);
    assert_eq!(report.uses[0].first_site.line, first);
}

#[test]
fn reset_audit_clears_records_but_keeps_policy() {
    let mut caps = set(CapabilityMode::Enforce);
    caps.grant(Capability::parse_grant("file-read:/data").unwrap());
    caps.check(Capability::file_read("/data/a"))
        .expect("granted");
    assert_eq!(caps.audit_report().total_checks(), 1);
    caps.reset_audit();
    assert_eq!(caps.audit_report().total_checks(), 0);
    assert!(caps.check(Capability::file_read("/data/a")).is_ok());
    assert!(caps.check(Capability::file_read("/etc/x")).is_err());
}

#[test]
fn audit_map_is_bounded_and_says_so() {
    let caps = set(CapabilityMode::Permissive);
    for i in 0..(MAX_AUDIT_ENTRIES + 32) {
        caps.check(Capability::file_read(&format!("/tmp/f{i}")))
            .expect("permissive allows");
    }
    let report = caps.audit_report();
    assert_eq!(report.uses.len(), MAX_AUDIT_ENTRIES);
    assert!(
        report.truncated,
        "a capped report must never claim to be complete"
    );
}

// -----------------------------------------------------------------------
// Native classification
// -----------------------------------------------------------------------

#[test]
fn classification_covers_the_sensitive_entry_points() {
    assert_eq!(
        classify_native("java/lang/ProcessBuilder", "start"),
        Some(CapabilityKind::ProcessSpawn)
    );
    assert_eq!(
        classify_native("java/lang/Runtime", "exec"),
        Some(CapabilityKind::ProcessSpawn)
    );
    assert_eq!(
        classify_native("java/lang/Runtime", "loadLibrary0"),
        Some(CapabilityKind::LibraryLoad)
    );
    assert_eq!(
        classify_native("java/lang/System", "load"),
        Some(CapabilityKind::LibraryLoad)
    );
    assert_eq!(
        classify_native("jdk/internal/misc/Unsafe", "putLong"),
        Some(CapabilityKind::RawMemory)
    );
    assert_eq!(
        classify_native("java/lang/foreign/Linker", "downcallHandle"),
        Some(CapabilityKind::ForeignDowncall)
    );
    assert_eq!(
        classify_native("java/lang/foreign/Linker", "upcallHandle"),
        Some(CapabilityKind::ForeignUpcall)
    );
    assert_eq!(
        classify_native("java/lang/foreign/Linker", "upcallStub"),
        Some(CapabilityKind::ForeignUpcall)
    );
    assert_eq!(
        classify_native("sun/nio/ch/Net", "connect0"),
        Some(CapabilityKind::Network)
    );
    assert_eq!(
        classify_native("java/io/File", "delete0"),
        Some(CapabilityKind::FileWrite)
    );
    // Runtime methods that are not capability-relevant stay unclassified,
    // so the dispatch gate does not fire on `Runtime.gc`.
    assert_eq!(classify_native("java/lang/Runtime", "gc"), None);
    assert_eq!(classify_native("java/util/HashMap", "put"), None);
    assert_eq!(classify_native("java/io/File", "getName"), None);
}
