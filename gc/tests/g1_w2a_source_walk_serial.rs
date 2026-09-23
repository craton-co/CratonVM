// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the unified source walk on the SERIAL arm, same levers.
//!
//! `CRATONVM_G1_PARALLEL_EVAC=0` routes the pause through
//! `G1Collector::scan_source_region_for_cset_refs` instead of
//! `SharedEvac::seed_source_region`. Since wave 2 those are two forty-line
//! wrappers over one body, and this binary is the half of the evidence that
//! says so: it runs the workload `g1_w2a_source_walk_parallel` runs, with the
//! same levers, and asserts the same expectation.
//!
//! An arm that finds a different graph is the defect class this whole
//! unification exists to close — `CRATONVM_G1_PARALLEL_EVAC` is supposed to
//! change how FAST a pause runs, not what it FINDS.

mod g1_w2a_walk_common;

/// See the twin in `g1_w2a_source_walk_parallel`.
///
/// The levers go in through `with_process_overrides`, not `set_var`. Two of
/// these four are DECLARED flags, which `flags()` serves from a snapshot
/// latched on first read — so a `set_var` only takes effect if it happens to
/// win the race to initialise that snapshot, which is a property of what else
/// the binary touched first rather than of this function.
/// `types/tests/flag_env_mutation_guard.rs` is the gate that says so, and this
/// test tripped it. `with_process_overrides` is the supported route and it
/// covers the collector threads the pause spawns, which the thread-local form
/// would not.
const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_PARALLEL_EVAC", Some("0")),
    ("CRATONVM_G1_CARD_CURSOR", Some("2")),
    ("CRATONVM_G1_CARD_CLEAN", Some("1")),
    ("CRATONVM_G1_RSET_PRUNE_ON_SCAN", Some("1")),
];

#[test]
fn the_serial_arm_finds_exactly_what_the_parallel_arm_finds() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());
    });
}
