// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 22, lane sync (r10 obs item 1): every literal feeder of the two
//! name-keyed event tables names a real row.
//!
//! `metrics::record_osr_event` and `metrics::record_loop_xform_event` ignore an
//! unknown name by design (they run on the interpreter's back-edge and in the
//! planner, where a panic is worse than a lost count). Round 10 found the cost
//! of that design: `jit_bridge.rs` recorded `"osr_optimizing_artifact_reused"`
//! for a row that did not exist, and the count read zero forever. The obs
//! proposal asked for a `debug_assert!` in the recorders; that would break the
//! tested contract (`record_osr_event_increments_its_row_only` deliberately
//! feeds `"not_an_osr_event"`), so the check lives here instead, over the
//! source text: every `record_osr_event("..")`, every
//! `record_loop_xform_event("..")`, every `tally("..")` in the loop rewriter
//! (its local alias), and every arm of `OsrExitSite::metric` must be a row.
//!
//! Non-literal feeders (a `match` or `if` expression as the argument) are not
//! seen; the rows they name are covered by the `metric()` arm scan where they
//! are an enum's, and otherwise stay a review question.

use std::path::{Path, PathBuf};

use cratonvm_jit::metrics::{LOOP_XFORM_EVENTS, OSR_EVENTS};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every string literal that directly follows `needle` (which ends in `("`).
fn literals_after(text: &str, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(needle) {
        let after = &rest[at + needle.len()..];
        if let Some(end) = after.find('"') {
            found.push(after[..end].to_string());
        }
        rest = &rest[at + needle.len()..];
    }
    found
}

/// Deliberately unknown names fed by tests of the ignore-unknown contract.
const CONTRACT_PROBES: &[&str] = &["not_an_osr_event"];

#[test]
fn every_literal_event_feeder_names_a_row() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&manifest.join("src"), &mut files);
    rust_files(&manifest.join("../vm/src"), &mut files);
    assert!(
        files.len() > 100,
        "the scan saw only {} files: the paths are wrong and this test would pass vacuously",
        files.len()
    );

    let mut seen = 0usize;
    let mut bad = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let mut check = |names: Vec<String>, table: &[&str], which: &str| {
            for name in names {
                seen += 1;
                if !table.contains(&name.as_str()) && !CONTRACT_PROBES.contains(&name.as_str()) {
                    bad.push(format!("{}: {which} feeder \"{name}\" has no row", path.display()));
                }
            }
        };
        check(literals_after(&text, "record_osr_event(\""), &OSR_EVENTS, "OSR");
        check(literals_after(&text, "record_loop_xform_event(\""), &LOOP_XFORM_EVENTS, "loop-xform");
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name == "loop_rewrite.rs" {
            check(literals_after(&text, "tally(\""), &LOOP_XFORM_EVENTS, "loop-xform (tally)");
        }
        if file_name == "osr_exit.rs" {
            // `OsrExitSite::metric`'s arms: `=> "osr_..."`.
            let arms = literals_after(&text, "=> \"osr_")
                .into_iter()
                .map(|tail| format!("osr_{tail}"))
                .collect();
            check(arms, &OSR_EVENTS, "OSR (OsrExitSite::metric)");
        }
    }
    assert!(seen >= 10, "only {seen} literal feeders found: the scan is not seeing the call sites");
    assert!(bad.is_empty(), "event feeders with no table row (the count would read zero forever):\n{}", bad.join("\n"));
}
