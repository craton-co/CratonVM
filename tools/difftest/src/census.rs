// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Reading the launcher's Real-JDK census artefacts
//! (`docs/book/src/user-guide/real-jdk-mode.md` §9).
//!
//! A [`Mode::collects_census`](crate::runner::Mode::collects_census) child is
//! launched with three extra flags and writes three JSON files; this module
//! turns them into the [`StrictCensus`] that lands on the ledger row:
//!
//! | File | Flag | Contributes |
//! |---|---|---|
//! | Real-JDK report | `--real-jdk-report` | `mode`, `jdk_feature`, `violations[]`, `counts{}` |
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

/// `NativeKind::as_str()` tags, and the `counts` keys the Real-JDK report uses
/// for the same three kinds (contract §9's `bridge_invocations`, …).
const NATIVE_KIND_REPORT_KEYS: &[(&str, &str)] = &[
    ("intrinsic", "intrinsic_invocations"),
    ("bridge", "bridge_invocations"),
    ("synthetic-stub", "synthetic_stub_invocations"),
];

/// `ClassOrigin::as_str()` tags reachable from the Real-JDK report's coarser
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
/// Real-JDK report's `*_invocations` counters.
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
// Violations (`--real-jdk-report`, contract §3's `JdkOnlyViolation`)
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
mod tests;
