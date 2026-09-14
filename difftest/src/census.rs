// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reading the launcher's JDK-only census artefacts
//! (`docs/feature-designs/jdk-only-mode.md` §9).
//!
//! A [`Mode::collects_census`](crate::runner::Mode::collects_census) child is
//! launched with three extra flags and writes three JSON files; this module
//! turns them into the [`StrictCensus`] that lands on the ledger row:
//!
//! | File | Flag | Contributes |
//! |---|---|---|
//! | JDK-only report | `--jdk-only-report` | `mode`, `jdk_feature`, `violations[]`, `counts{}` |
//! | class-origin census | `--dump-class-origins` | counts by `ClassOrigin::as_str()` |
//! | native registry (schema 2) | `--dump-native-registry` | invocations by `NativeKind::as_str()` |
//!
//! ## Parsing discipline
//!
//! These files are produced by a *different crate on a different schedule*
//! (agents B/C/D land them this wave). The parsers here are therefore
//! **shape-tolerant and never fatal**: a missing file, an unexpected wrapper
//! key, or an extra field yields a partial census rather than an error, because
//! an unreadable census must not turn into a fake divergence. What they do
//! *not* do is guess: an absent counter stays absent rather than defaulting to
//! zero, so "we did not measure it" never reads as "it was clean".
//!
//! The one thing that is pinned is the **tag vocabulary**: keys are the
//! contract's stable `as_str()` spellings (`boot-image`, `compatibility-stub`,
//! `generated-lambda`, …; `intrinsic`, `bridge`, `synthetic-stub`).

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::ledger::{StrictCensus, ViolationTally};
use crate::runner::DumpPaths;

/// `NativeKind::as_str()` tags, and the `counts` keys the JDK-only report uses
/// for the same three kinds (contract §9's `bridge_invocations`, …).
const NATIVE_KIND_REPORT_KEYS: &[(&str, &str)] = &[
    ("intrinsic", "intrinsic_invocations"),
    ("bridge", "bridge_invocations"),
    ("synthetic-stub", "synthetic_stub_invocations"),
];

/// `ClassOrigin::as_str()` tags reachable from the JDK-only report's coarser
/// `counts` block, used only when the class-origin census file is absent.
const CLASS_ORIGIN_REPORT_KEYS: &[(&str, &str)] = &[
    ("boot-image", "boot_image_classes"),
    ("application-class-path", "application_classes"),
    ("compatibility-stub", "compatibility_classes"),
];

/// Collect the census for one finished run.
///
/// `fallback_profile` / `fallback_feature` are what the harness already knows
/// from the [`Mode`](crate::runner::Mode) and the HotSpot oracle; the report's
/// own values win when present, since the VM is the authority on which policy
/// it actually booted under.
pub fn collect(
    dumps: &DumpPaths,
    fallback_profile: &str,
    fallback_feature: Option<u32>,
) -> StrictCensus {
    let report = read_json(&dumps.jdk_only_report);
    let mut census = StrictCensus::profile_only(fallback_profile, fallback_feature);

    if let Some(report) = report.as_ref() {
        if let Some(mode) = report.get("mode").and_then(Value::as_str) {
            census.jdk_profile = mode.to_string();
        }
        if let Some(feature) = report.get("jdk_feature").and_then(Value::as_u64) {
            census.jdk_feature = Some(feature as u32);
        }
        census.violations = parse_violations(report);
    }

    census.class_origins = read_json(&dumps.class_origins)
        .map(|v| parse_class_origins(&v))
        .filter(|m| !m.is_empty())
        .or_else(|| {
            report
                .as_ref()
                .map(|r| counts_by_key(r, CLASS_ORIGIN_REPORT_KEYS))
        })
        .unwrap_or_default();

    census.native_invocations = read_json(&dumps.native_registry)
        .map(|v| parse_native_invocations(&v))
        .filter(|m| !m.is_empty())
        .or_else(|| {
            report
                .as_ref()
                .map(|r| counts_by_key(r, NATIVE_KIND_REPORT_KEYS))
        })
        .unwrap_or_default();

    census
}

/// Read and parse a JSON file, returning `None` for "absent or unreadable".
fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Pull `{tag: counts[report_key]}` out of a report's `counts` object. Absent
/// keys are omitted, never zero-filled.
fn counts_by_key(report: &Value, keys: &[(&str, &str)]) -> BTreeMap<String, u64> {
    let Some(counts) = report.get("counts") else {
        return BTreeMap::new();
    };
    keys.iter()
        .filter_map(|(tag, key)| {
            counts
                .get(*key)
                .and_then(Value::as_u64)
                .map(|n| (tag.to_string(), n))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Class origins (`--dump-class-origins`, contract §5's `ClassOriginEntry`)
// ---------------------------------------------------------------------------

/// Count `ClassOriginEntry` rows by their `origin` tag.
///
/// Accepts either a bare array of rows or an object wrapping one (the wrapper
/// key is agent B's choice; any array-of-objects value carrying an `origin`
/// string is taken as the census).
pub fn parse_class_origins(value: &Value) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    let Some(rows) = find_row_array(value, "origin") else {
        return out;
    };
    for row in rows {
        if let Some(origin) = row.get("origin").and_then(Value::as_str) {
            *out.entry(origin.to_string()).or_insert(0) += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Native registry (schema 2, contract §4's `NativeCensusEntry`)
// ---------------------------------------------------------------------------

/// Sum per-slot `invocations` by `NativeKind::as_str()` tag.
///
/// This needs the **schema-2** census: schema 1 has no `invocations` field, and
/// its `counts` block counts *registrations*, which is a different question. A
/// schema-1 file therefore yields an empty map and [`collect`] falls back to the
/// JDK-only report's `*_invocations` counters.
pub fn parse_native_invocations(value: &Value) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let Some(rows) = find_row_array(value, "kind") else {
        return out;
    };
    let mut saw_invocations = false;
    for row in rows {
        let Some(kind) = row.get("kind").and_then(Value::as_str) else {
            continue;
        };
        if let Some(n) = row.get("invocations").and_then(Value::as_u64) {
            saw_invocations = true;
            *out.entry(kind.to_string()).or_insert(0) += n;
        }
    }
    if !saw_invocations {
        out.clear();
    }
    out
}

/// Find the first array-of-objects (at the root or one level down) whose rows
/// carry `marker`. Keeps the parsers independent of the wrapper key each dump
/// happens to use.
fn find_row_array<'a>(value: &'a Value, marker: &str) -> Option<&'a Vec<Value>> {
    let is_census = |v: &Value| {
        v.as_array()
            .map(|rows| rows.iter().any(|r| r.get(marker).is_some()))
            .unwrap_or(false)
    };
    if is_census(value) {
        return value.as_array();
    }
    value
        .as_object()?
        .values()
        .find(|v| is_census(v))
        .and_then(Value::as_array)
}

// ---------------------------------------------------------------------------
// Violations (`--jdk-only-report`, contract §3's `JdkOnlyViolation`)
// ---------------------------------------------------------------------------

/// Tally the report's `violations[]` by `JdkOnlyViolation::kind()`.
///
/// Each element is expected to carry a `kind` string. An externally-tagged
/// enum encoding (`{"MissingNative": {...}}`) is also accepted, since
/// `to_json()`'s exact framing is agent F's call; anything else tallies as
/// `"unknown"` rather than being dropped, so a violation is never lost.
pub fn parse_violations(report: &Value) -> Vec<ViolationTally> {
    let Some(rows) = report.get("violations").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut by_kind: BTreeMap<String, ViolationTally> = BTreeMap::new();
    for row in rows {
        let kind = violation_kind(row);
        let sample = violation_sample(row);
        let tally = by_kind
            .entry(kind.clone())
            .or_insert_with(|| ViolationTally {
                kind,
                count: 0,
                sample: None,
            });
        tally.count += 1;
        if tally.sample.is_none() {
            tally.sample = sample;
        }
    }
    by_kind.into_values().collect()
}

fn violation_kind(row: &Value) -> String {
    if let Some(k) = row.get("kind").and_then(Value::as_str) {
        return k.to_string();
    }
    // Externally-tagged single-key object, e.g. {"MissingNative": {...}}.
    if let Some(obj) = row.as_object() {
        if obj.len() == 1 {
            if let Some(k) = obj.keys().next() {
                return k.clone();
            }
        }
    }
    if let Some(s) = row.as_str() {
        return s.to_string();
    }
    "unknown".to_string()
}

/// A one-line human sample: the report's own `summary`, else the
/// class/method/descriptor triple the contract guarantees on most variants.
fn violation_sample(row: &Value) -> Option<String> {
    if let Some(s) = row.get("summary").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let body = match row.as_object() {
        Some(obj) if obj.len() == 1 && row.get("class").is_none() => obj.values().next()?,
        _ => row,
    };
    let class = body.get("class").and_then(Value::as_str)?;
    match (
        body.get("method").and_then(Value::as_str),
        body.get("descriptor").and_then(Value::as_str),
    ) {
        (Some(m), Some(d)) => Some(format!("{class}.{m}{d}")),
        (Some(m), None) => Some(format!("{class}.{m}")),
        _ => Some(class.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
}
