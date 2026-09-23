// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JIT crate's diagnostic counters that no other report reads.
//!
//! # Why this module exists
//!
//! Round 10 took a census of the workspace's atomic-backed instruments and
//! found 112 with no reader outside the file that defines them
//! (`scripts/check-orphan-instruments.sh`, and the page that filed it,
//! `docs/internal/retired/r10-diagread-orphan-instrument-debt-register-20260921-RETIRED-20260922.md`).
//! Each one writes or holds a number that nothing in a shipped run ever looks
//! at, and the reason that matters is one sentence: **zero is
//! indistinguishable from "this never happened"**. A wrong answer has a test; a
//! number with no reader produces a correct-looking silence forever.
//!
//! The dispositions the register offered were "wire it" or "delete it". The
//! ones reachable from here are wired, and this is where. What is NOT here was
//! deleted instead, or was never an orphan (an instrument whose only caller is
//! in its own file is over-broad visibility, not a missing reader, and those
//! were narrowed to `pub(crate)`).
//!
//! # Why one opt-in report rather than a line each in the method-stats dump
//!
//! The register's own objection to bulk-wiring was that "a 112-line diagnostic
//! wall that nobody reads is the same defect with more characters". That
//! objection is about the DEFAULT run, so this report is not on by default and
//! is not mixed into any existing one: it is its own switch, it names the
//! subsystem beside every number, and it prints every row including the zeros —
//! because a zero that a reader can reach is the whole point, and a report that
//! hides its zeros cannot distinguish "not wired" from "did not happen".
//!
//! # Adding to it
//!
//! Push `(name, value)` with a name a grep will find. Prefer the instrument's
//! own function name so `rg <name>` lands on both the definition and this read.
//! Do not call a `reset_*` from here: a reset is not a reading, and it destroys
//! whatever window a concurrent observer had open.

/// Append this crate's otherwise-unread instruments to `out`.
///
/// Takes a sink rather than returning a `Vec` so the aggregate report
/// (`cratonvm_vm::runtime::instrument_census`) builds one allocation for the
/// whole workspace, and so this function can never itself become a
/// zero-argument counter getter with no reader.
pub fn instrument_census(out: &mut Vec<(&'static str, String)>) {
    out.push((
        "jit.runtime_not_compilable_size",
        crate::compile_gate::runtime_not_compilable_size().to_string(),
    ));
    let (coarsened, refused) = crate::ea_ir_bridge::lock_coarsening_census();
    out.push((
        "jit.lock_coarsening_census",
        format!("coarsened={coarsened} refused={refused}"),
    ));
    out.push((
        "jit.admission_funnel",
        render_rows(&crate::ir_evidence::admission_funnel()),
    ));
    out.push((
        "jit.inputs_generation",
        crate::ir_evidence::inputs_generation().to_string(),
    ));
    out.push((
        "jit.transform_census",
        render_rows(&crate::ir_evidence::transform_census()),
    ));
    out.push((
        "jit.ir_buffer_overflow_retries",
        crate::ir_lower::ir_buffer_overflow_retries().to_string(),
    ));
    out.push((
        "jit.ir_osr_entry_skipped_census",
        crate::ir_lower::ir_osr_entry_skipped_census().to_string(),
    ));
    out.push((
        "jit.read_unlocated_tile_roots",
        crate::ir_lower::mir_totals::read_unlocated_tile_roots().to_string(),
    ));
    out.push((
        "jit.ir_incomparable_input_placements",
        crate::ir_schedule::ir_incomparable_input_placements().to_string(),
    ));
    let (rpo_blocks, rpo_fallbacks) = crate::ir_schedule::rpo_layout_counts();
    out.push((
        "jit.rpo_layout_counts",
        format!("blocks={rpo_blocks} fallbacks={rpo_fallbacks}"),
    ));
    out.push((
        "jit.compile_id_exhaustions",
        crate::compile_id_exhaustions().to_string(),
    ));
    out.push((
        "jit.late_lambda_adapters_withdrawn",
        crate::late_lambda_adapters_withdrawn().to_string(),
    ));
    let (aastore_a, aastore_b, aastore_c) =
        crate::x64::bytecode_walk::aastore_barrier_gate_census();
    out.push((
        "jit.aastore_barrier_gate_census",
        format!("{aastore_a}/{aastore_b}/{aastore_c}"),
    ));
}

/// `name=count` pairs, space separated, or `-` when the table is empty.
///
/// `-` rather than an empty string on purpose: an empty value in a
/// `key=value` report reads as a formatting bug, and these tables are
/// legitimately empty on a run that never reached the subsystem.
fn render_rows(rows: &[(&'static str, u64)]) -> String {
    if rows.is_empty() {
        return "-".to_string();
    }
    rows.iter()
        .map(|(label, n)| format!("{label}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}
