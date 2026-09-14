// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: pin the inventory of single-pass optimizing passes (Finding #68).
//!
//! Contract: `optimizing-passes-still-exist-in-both-tiers-20260912.md`.
//!
//! The single-pass tier (`jit/src/x64/*`) and the IR tier (`jit/src/ir.rs` ->
//! `ir_optimize.rs` -> `ir_lower.rs`) historically carried duplicated copies of
//! optimizing passes.
//!
//! This test pins the single-pass optimizing pass inventory and verifies that
//! the single-pass backend default remains compatible while supporting pure
//! baseline compilation via `BackendRequest::baseline_mode` or
//! `CRATONVM_JIT_BASELINE_FAST`.

use std::path::Path;

/// The inventory of single-pass optimizing/speculative pass source files in `jit/src/x64/`.
const SINGLE_PASS_OPTIMIZING_PASS_SOURCES: &[&str] = &[
    "bce.rs",
    "escape_analysis.rs",
    "inlining.rs",
    "licm.rs",
    "licm_int.rs",
    "loop_unroll_admission.rs",
    "null_check_elim.rs",
];

#[test]
fn single_pass_optimizing_pass_sources_do_not_grow() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let x64_dir = manifest_dir.join("src").join("x64");

    let mut present_passes = Vec::new();
    for entry in std::fs::read_dir(&x64_dir).expect("read x64 dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if SINGLE_PASS_OPTIMIZING_PASS_SOURCES.contains(&name.as_str()) {
            present_passes.push(name);
        }
    }
    present_passes.sort();

    let mut expected = SINGLE_PASS_OPTIMIZING_PASS_SOURCES
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    expected.sort();

    assert_eq!(
        present_passes,
        expected,
        "Single-pass optimizing pass inventory drifted: expected {expected:?}, got {present_passes:?}"
    );
}

#[test]
fn backend_request_default_mode_is_compatible_not_baseline() {
    let req = cratonvm_jit::x64::BackendRequest::default();
    assert!(
        !req.baseline_mode,
        "BackendRequest::default() must remain compatible (not baseline) by default"
    );
}

#[test]
fn backend_request_baseline_mode_field_is_configurable() {
    let mut req = cratonvm_jit::x64::BackendRequest::default();
    req.baseline_mode = true;
    assert!(req.baseline_mode);
}
