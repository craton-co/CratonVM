use super::*;

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("test fixture parses")
}

/// The report exactly as spelled in contract §9.
const REPORT: &str = r#"{
  "schema_version": 1,
  "mode": "jdk-only",
  "jdk_feature": 25,
  "violations": [],
  "counts": {
    "boot_image_classes": 312, "application_classes": 18,
    "generated_classes": 4,   "compatibility_classes": 0,
    "bridge_invocations": 1082, "intrinsic_invocations": 4301,
    "synthetic_stub_invocations": 0
  }
}"#;

#[test]
fn class_origins_count_by_tag() {
    let v = json(
        r#"{"schema_version":1,"classes":[
            {"name":"java/lang/String","origin":"boot-image","real_bytes_found":true,"loader_id":0},
            {"name":"java/lang/Object","origin":"boot-image","real_bytes_found":true,"loader_id":0},
            {"name":"[I","origin":"vm-array","real_bytes_found":false,"loader_id":0},
            {"name":"Foo$$Lambda","origin":"generated-lambda","real_bytes_found":false,"loader_id":1}
        ]}"#,
    );
    let counts = parse_class_origins(&v);
    assert_eq!(counts["boot-image"], 2);
    assert_eq!(counts["vm-array"], 1);
    assert_eq!(counts["generated-lambda"], 1);
    assert!(!counts.contains_key("compatibility-stub"), "absent ≠ zero");
}

#[test]
fn class_origins_accept_a_bare_array() {
    let v = json(r#"[{"name":"A","origin":"application-class-path"}]"#);
    assert_eq!(parse_class_origins(&v)["application-class-path"], 1);
}

#[test]
fn native_invocations_sum_per_kind_from_schema_2() {
    let v = json(
        r#"{"schema_version":2,"counts":{"intrinsic":2,"bridge":1,"synthetic-stub":1},
            "natives":[
              {"class":"java/lang/System","name":"arraycopy","descriptor":"()V","kind":"intrinsic","invocations":10},
              {"class":"java/lang/Object","name":"hashCode","descriptor":"()I","kind":"intrinsic","invocations":5},
              {"class":"java/io/FileOutputStream","name":"write0","descriptor":"()V","kind":"bridge","invocations":3},
              {"class":"org/jboss/X","name":"y","descriptor":"()V","kind":"synthetic-stub","invocations":0}
            ]}"#,
    );
    let counts = parse_native_invocations(&v);
    assert_eq!(counts["intrinsic"], 15);
    assert_eq!(counts["bridge"], 3);
    assert_eq!(counts["synthetic-stub"], 0);
}

#[test]
fn schema_1_registry_yields_no_invocation_counts() {
    // Schema 1 counts *registrations*; reporting them as invocations would
    // be a different measurement wearing the same label.
    let v = json(
        r#"{"counts":{"intrinsic":2,"bridge":1,"synthetic-stub":0,"total":3},
            "natives":[{"class":"a","name":"b","descriptor":"()V","kind":"intrinsic"}]}"#,
    );
    assert!(parse_native_invocations(&v).is_empty());
}

#[test]
fn violations_tally_by_kind_with_a_sample() {
    let report = json(
        r#"{"violations":[
            {"kind":"missing-native","class":"java/foo/Bar","method":"baz","descriptor":"()V"},
            {"kind":"missing-native","class":"java/foo/Bar","method":"qux","descriptor":"()I"},
            {"kind":"compatibility-class-requested","class":"org/jboss/logging/Logger","reason":"enterprise prefix"}
        ]}"#,
    );
    let tallies = parse_violations(&report);
    assert_eq!(tallies.len(), 2);
    let missing = tallies.iter().find(|t| t.kind == "missing-native").unwrap();
    assert_eq!(missing.count, 2);
    assert_eq!(missing.sample.as_deref(), Some("java/foo/Bar.baz()V"));
    let compat = tallies
        .iter()
        .find(|t| t.kind == "compatibility-class-requested")
        .unwrap();
    assert_eq!(compat.sample.as_deref(), Some("org/jboss/logging/Logger"));
}

#[test]
fn externally_tagged_violations_are_not_dropped() {
    let report = json(
        r#"{"violations":[{"MissingNative":{"class":"a/B","method":"c","descriptor":"()V"}}]}"#,
    );
    let tallies = parse_violations(&report);
    assert_eq!(tallies.len(), 1);
    assert_eq!(tallies[0].kind, "MissingNative");
    assert_eq!(tallies[0].count, 1);
    assert_eq!(tallies[0].sample.as_deref(), Some("a/B.c()V"));
}

#[test]
fn unrecognised_violation_shape_tallies_as_unknown() {
    let report = json(r#"{"violations":[12345]}"#);
    let tallies = parse_violations(&report);
    assert_eq!(tallies[0].kind, "unknown");
    assert_eq!(tallies[0].count, 1);
}

#[test]
fn report_counts_are_the_fallback_when_the_dumps_are_absent() {
    let dir = std::env::temp_dir().join(format!("difftest_census_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dumps = DumpPaths::in_dir(&dir);
    std::fs::write(&dumps.jdk_only_report, REPORT).expect("write report");
    // class-origins / native-registry deliberately not written.

    let census = collect(&dumps, "compatible", Some(21));
    // The report is the authority on the profile it actually booted under.
    assert_eq!(census.jdk_profile, "jdk-only");
    assert_eq!(census.jdk_feature, Some(25));
    assert_eq!(census.class_origins["boot-image"], 312);
    assert_eq!(census.compatibility_classes(), 0);
    assert_eq!(census.native_invocations["intrinsic"], 4301);
    assert_eq!(census.synthetic_stub_invocations(), 0);
    assert!(!census.has_violations());

    dumps.cleanup();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_dumps_yield_the_fallback_profile_not_an_error() {
    let dumps = DumpPaths::in_dir(Path::new("no/such/dir"));
    let census = collect(&dumps, "jdk-only", Some(25));
    assert_eq!(census.jdk_profile, "jdk-only");
    assert_eq!(census.jdk_feature, Some(25));
    assert!(census.class_origins.is_empty());
    assert!(census.native_invocations.is_empty());
    assert!(!census.has_violations());
}

#[test]
fn malformed_dump_is_ignored_not_fatal() {
    let dir = std::env::temp_dir().join(format!("difftest_census_bad_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dumps = DumpPaths::in_dir(&dir);
    std::fs::write(&dumps.jdk_only_report, "{not json").expect("write");
    std::fs::write(&dumps.class_origins, "").expect("write");
    let census = collect(&dumps, "jdk-only", Some(25));
    assert_eq!(census.jdk_profile, "jdk-only");
    assert!(census.class_origins.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
