// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1's promoted-bytes counter on the PARALLEL evacuator -- the default arm.
//!
//! `CRATONVM_G1_PARALLEL_EVAC` is on unless set to `0`, so this is the
//! evacuator a shipping G1 run uses, and it is the one where the counter can be
//! wrong in an interesting way: `SharedEvac::evacuate` CAS-installs its
//! forwarding word and a losing worker abandons its copy. See
//! `g1_promoted_bytes_common` for the whole argument.
//!
//! The lever is passed explicitly rather than relied on as a default, so the
//! binary still exercises the arm it is named for if the default ever flips.

mod g1_promoted_bytes_common;

/// The levers go in through `with_process_overrides`, not `set_var`:
/// `CRATONVM_G1_PARALLEL_EVAC` is a DECLARED flag and `flags()` serves it from
/// a snapshot latched on first read, so a `set_var` takes effect only if it
/// wins the race to initialise that snapshot.
/// `types/tests/flag_env_mutation_guard.rs` is the gate that says so. The
/// PROCESS form, not the thread-local one: the evacuation runs on worker
/// threads this test did not create.
const ARM: &[(&str, Option<&str>)] = &[("CRATONVM_G1_PARALLEL_EVAC", Some("1"))];

#[test]
fn the_parallel_evacuator_counts_the_bytes_it_promotes() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        let measured = g1_promoted_bytes_common::run();
        assert_the_parallel_evacuator_actually_ran();
        g1_promoted_bytes_common::assert_promotion_is_counted("parallel evac", measured);
    });
}

/// The tripwire without which this binary is the serial test wearing a
/// parallel name.
///
/// `EvacWorkerCensus` is filled by `parallel_evacuate` and by nothing else, so
/// an empty census -- or one in which no worker copied a byte -- means the
/// pause took the serial driver and the CAS arm this file exists to cover was
/// never executed. That is the failure mode `g1_lane_d_parallel_seed` was
/// written after being caught in: every assertion passes, about the wrong
/// code.
fn assert_the_parallel_evacuator_actually_ran() {
    let rows = cratonvm_gc::g1::g1_evac_worker_census();
    let copied: u64 = rows.iter().map(|r| r.bytes_copied + r.seed_bytes).sum();
    assert!(
        copied > 0,
        "CRATONVM_G1_PARALLEL_EVAC=1 is armed and the worker census reports no          bytes copied by any worker ({} rows). The pause took the SERIAL          evacuator, so the CAS-winner arm this binary is named for never ran.",
        rows.len(),
    );
}

#[test]
fn the_dispatcher_reports_what_the_parallel_evacuator_counted() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        g1_promoted_bytes_common::assert_the_dispatcher_delegates("parallel evac");
    });
}
