// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-2 lane A — the unified source walk on the PARALLEL arm with every
//! wave-2 lever armed.
//!
//! This is the arm that matters in production: parallel evacuation is the
//! default path, and it is the arm on which the five divergences the audit
//! found would actually have fired. Armed here:
//!
//! * `CRATONVM_G1_CARD_CURSOR=2` — the density-adaptive cursor, so BOTH of its
//!   per-region decisions are exercised (the holders' regions go from sparse to
//!   dense as the repeated pauses dirty them);
//! * `CRATONVM_G1_CARD_CLEAN=1` — card cleaning, which is what makes a wrong
//!   per-object screen lose an edge on the SECOND pause rather than never;
//! * `CRATONVM_G1_RSET_PRUNE_ON_SCAN=1` — scan-time pruning, so an entry
//!   dropped in error shows up as a source region the next pause never walks.
//!
//! Every assertion is the same one `g1_w2a_source_walk_defaults` makes.

mod g1_w2a_walk_common;

/// The levers, applied through `with_process_overrides` rather than
/// `std::env::set_var`.
///
/// `CRATONVM_G1_CARD_CLEAN` is a DECLARED flag: `flags()` serves it from a
/// snapshot latched on first read, so a `set_var` takes effect only if it wins
/// the race to initialise that snapshot — which depends on what else the binary
/// touched first, not on this code. `types/tests/flag_env_mutation_guard.rs` is
/// the gate that says so, and this test tripped it.
///
/// The process form, not the thread-local one, because a young pause runs its
/// evacuation on worker threads this test did not create, and a thread-local
/// override would not reach them.
///
/// One test per binary is still the rule here, and still for the original
/// reason: the `OnceLock` gates (`lane_a_card_cursor` and friends) latch once
/// per process, so two arms cannot coexist however the values are delivered.
const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_CARD_CURSOR", Some("2")),
    ("CRATONVM_G1_CARD_CLEAN", Some("1")),
    ("CRATONVM_G1_RSET_PRUNE_ON_SCAN", Some("1")),
];

#[test]
fn the_parallel_arm_with_every_lever_armed_finds_every_remembered_set_edge() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());
    });
}
