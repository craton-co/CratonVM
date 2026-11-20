use super::*;

fn outcome(stdout: &str, ret: &str) -> Outcome {
    Outcome {
        stdout: stdout.to_string(),
        return_value: ret.to_string(),
    }
}

#[test]
fn report_records_match_and_divergence() {
    let mut report = DivergenceReport::new();
    report.record("A", "m", "()I", &outcome("ok", "1"), &outcome("ok", "1"));
    report.record("B", "m", "()I", &outcome("foo", "1"), &outcome("bar", "1"));
    assert_eq!(report.total_tested, 2);
    assert_eq!(report.total_matched, 1);
    assert_eq!(report.divergences.len(), 1);
    assert_eq!(report.divergences[0].class_name, "B");
}

#[test]
fn report_default_is_empty() {
    let report = DivergenceReport::new();
    assert_eq!(report.total_tested, 0);
    assert!(report.divergences.is_empty());
}

#[test]
fn classification_serializes_kebab_case() {
    let json = serde_json::to_string(&Classification::JitOnly).unwrap();
    assert_eq!(json, "\"jit-only\"");
}

#[test]
fn ledger_round_trips_through_json() {
    let mut ledger = Ledger::new(
        "linux-x86_64".into(),
        "2026-06-21T00:00:00Z".into(),
        "25".into(),
    );
    ledger.entries.push(LedgerEntry {
        id: "div-0001".into(),
        class: "ArithEdge".into(),
        repro_path: "regression/div-0001/ArithEdge.java".into(),
        classification: Classification::Universal,
        cratonvm: Observation::empty(),
        hotspot: Observation::empty(),
        status: LedgerStatus::Known,
        first_seen: "2026-06-21T00:00:00Z".into(),
        linked_doc: None,
        jdk_profile: PROFILE_COMPATIBLE.into(),
        jdk_feature: None,
        class_origins: BTreeMap::new(),
        native_invocations: BTreeMap::new(),
        jdk_only_violations: Vec::new(),
    });
    let json = serde_json::to_string_pretty(&ledger).unwrap();
    let back: Ledger = serde_json::from_str(&json).unwrap();
    assert_eq!(back.schema_version, LEDGER_SCHEMA_VERSION);
    assert_eq!(back.entries.len(), 1);
    assert_eq!(back.entries[0].status, LedgerStatus::Known);
    // `linked_doc: None` is skipped on the wire.
    assert!(!json.contains("linked_doc"));
    // Empty schema-2 census fields are skipped too, so a row that measured
    // nothing looks exactly like a schema-1 row plus its profile.
    assert!(!json.contains("class_origins"));
    assert!(!json.contains("native_invocations"));
    assert!(!json.contains("jdk_only_violations"));
    assert!(!json.contains("jdk_feature"));
    assert!(json.contains("\"jdk_profile\": \"compatible\""));
}

// -- schema 2 -----------------------------------------------------------

/// A schema-1 file, byte-for-byte the shape committed before this wave.
const SCHEMA_1: &str = r#"{
  "schema_version": 1,
  "host": "windows-x86_64",
  "captured_at": "epoch:1785436557",
  "jdk": "openjdk version \"25.0.3\"",
  "entries": [
    {
      "id": "div-0001",
      "class": "ExceptionId",
      "repro_path": "difftest/seeds/ExceptionId.java",
      "classification": "universal",
      "cratonvm": {"stdout":"a","stderr":"","exit_code":0,"exception":null,"timed_out":false,"wall_ms":0},
      "hotspot": {"stdout":"b","stderr":"","exit_code":0,"exception":null,"timed_out":false,"wall_ms":0},
      "status": "known",
      "first_seen": "2026-06-21"
    }
  ]
}"#;

#[test]
fn schema_1_loads_unchanged_and_keys_as_compatible() {
    let mut ledger: Ledger = serde_json::from_str(SCHEMA_1).expect("schema 1 still parses");
    assert_eq!(ledger.schema_version, 1);
    let e = &ledger.entries[0];
    assert_eq!(e.jdk_profile, PROFILE_COMPATIBLE);
    assert_eq!(e.key(), ("ExceptionId", "compatible"));
    assert!(e.jdk_feature.is_none());
    assert!(e.class_origins.is_empty());
    assert!(e.jdk_only_violations.is_empty());
    // Nothing about the schema-1 payload is reinterpreted.
    assert_eq!(e.status, LedgerStatus::Known);
    assert_eq!(e.cratonvm.stdout, "a");

    assert!(ledger.migrate(), "schema 1 → 2 is a change");
    assert_eq!(ledger.schema_version, LEDGER_SCHEMA_VERSION);
    assert!(!ledger.migrate(), "migrating twice is a no-op");
}

#[test]
fn a_newer_schema_is_a_hard_error_not_a_partial_read() {
    let dir = std::env::temp_dir().join(format!("difftest_ledger_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("future.json");
    std::fs::write(
        &path,
        SCHEMA_1.replace("\"schema_version\": 1", "\"schema_version\": 99"),
    )
    .expect("write");
    let err = Ledger::load(&path).expect_err("a newer schema must fail loudly");
    assert!(err.to_string().contains("99"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn find_is_keyed_by_class_and_profile() {
    let mut ledger = Ledger::new("h".into(), "t".into(), "25".into());
    for profile in [PROFILE_COMPATIBLE, PROFILE_JDK_ONLY] {
        ledger.entries.push(LedgerEntry {
            id: format!("div-{profile}"),
            class: "Same".into(),
            repro_path: "seeds/Same.java".into(),
            classification: Classification::Universal,
            cratonvm: Observation::empty(),
            hotspot: Observation::empty(),
            status: LedgerStatus::Known,
            first_seen: "t".into(),
            linked_doc: None,
            jdk_profile: profile.into(),
            jdk_feature: Some(25),
            class_origins: BTreeMap::new(),
            native_invocations: BTreeMap::new(),
            jdk_only_violations: Vec::new(),
        });
    }
    assert_eq!(
        ledger.find("Same", PROFILE_JDK_ONLY).map(|e| e.id.as_str()),
        Some("div-jdk-only")
    );
    assert_eq!(
        ledger
            .find("Same", PROFILE_COMPATIBLE)
            .map(|e| e.id.as_str()),
        Some("div-compatible")
    );
    assert!(ledger.find("Same", "no-such-profile").is_none());
    assert!(ledger.find("Other", PROFILE_COMPATIBLE).is_none());
}

#[test]
fn row_label_marks_only_strict_rows() {
    assert_eq!(row_label("X", PROFILE_COMPATIBLE), "X");
    assert_eq!(row_label("X", PROFILE_JDK_ONLY), "X@jdk-only");
}

#[test]
fn strict_census_counters_distinguish_absent_from_zero() {
    let mut c = StrictCensus::profile_only(PROFILE_JDK_ONLY, Some(25));
    assert!(c.is_jdk_only());
    // Nothing measured yet: the accessors read 0, but the maps are empty —
    // callers that need "did we measure it" read the map.
    assert_eq!(c.compatibility_classes(), 0);
    assert!(c.class_origins.is_empty());
    assert!(!c.has_violations());

    c.class_origins.insert("compatibility-stub".into(), 3);
    c.native_invocations.insert("synthetic-stub".into(), 0);
    assert_eq!(c.compatibility_classes(), 3);
    assert_eq!(c.synthetic_stub_invocations(), 0);
    assert!(c.native_invocations.contains_key("synthetic-stub"));

    c.record_violation("missing-native", Some("a/B.c()V".into()));
    c.record_violation("missing-native", Some("d/E.f()V".into()));
    c.record_violation("profile-mismatch", None);
    assert!(c.has_violations());
    assert_eq!(c.violation_count(), 3);
    assert_eq!(c.violations.len(), 2);
    let mn = c
        .violations
        .iter()
        .find(|v| v.kind == "missing-native")
        .unwrap();
    assert_eq!(mn.count, 2);
    assert_eq!(mn.sample.as_deref(), Some("a/B.c()V"), "first sample wins");
}

#[test]
fn classification_jdk_only_violation_serializes_kebab_case() {
    let json = serde_json::to_string(&Classification::JdkOnlyViolation).unwrap();
    assert_eq!(json, "\"jdk-only-violation\"");
}

#[test]
fn every_channel_has_a_unique_stable_label() {
    // The labels are what a divergence report prints, so they are a
    // compatibility surface: a reader (and a CI log grep) must be able to
    // tell `exception-type` from `exception-message`.
    let labels: Vec<&str> = Channel::all().iter().map(|c| c.label()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "duplicate channel label");
    assert_eq!(labels.len(), 8);
    // The serde spelling and the report spelling must not drift apart.
    for c in Channel::all() {
        let json = serde_json::to_string(c).unwrap();
        assert_eq!(json, format!("\"{}\"", c.label()), "{c:?}");
    }
}

#[test]
fn observation_empty_is_inert() {
    let o = Observation::empty();
    assert!(!o.timed_out);
    assert!(o.exception.is_none());
    assert_eq!(o.exit_code, None);
}
