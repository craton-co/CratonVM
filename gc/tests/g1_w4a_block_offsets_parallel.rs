// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-4 lane A — the block-offset jump, on the arm every young pause takes.
//!
//! # What this binary is evidence of
//!
//! `CRATONVM_G1_BLOCK_OFFSETS=1` lets the remembered-set source walk ENTER a
//! dirty card at an object start the card table recorded, instead of sizing
//! every object from the region's base to reach it. It is the only lever in
//! lane A that changes WHICH objects a source walk visits, so it is the only
//! one whose failure mode is a lost remembered-set edge rather than a slower
//! pause — and a lost remembered-set edge is a use-after-free.
//!
//! The workload is `g1_w2a_walk_common`, unchanged, and it is the right one for
//! exactly one reason: **every live object except the root is reachable ONLY
//! through a remembered-set edge out of a tenured holder.** An object the jump
//! skips is therefore not slow to find, it is lost, and the read-back at the
//! end of `run()` faults or returns the wrong id. A fixture whose objects were
//! also reachable from a root would pass with the jump landing anywhere.
//!
//! Three holder shapes are in that graph (flat, reference array, and a chained
//! flat holder whose referent is itself a holder), so the jump is exercised
//! against all three of the walk's dispatch arms and against a closure that has
//! to be fed transitively afterwards.
//!
//! # Why the arms are separate binaries
//!
//! `CRATONVM_G1_BLOCK_OFFSETS`, `CRATONVM_G1_PARALLEL_EVAC` and
//! `CRATONVM_G1_CARD_CURSOR` all latch in a `OnceLock` on first read, so one
//! configuration is one process and `cargo test` gives each `tests/*.rs` file
//! its own binary. This file is the PARALLEL arm — the default evacuator, and
//! the one on which `CRATONVM_G1_PARALLEL_EVAC_SCREEN` leaves the per-object
//! header screens disarmed, which is precisely the arm on which a mis-entry
//! would have the least standing between it and a wild read. The jump's own
//! entry screen runs regardless of that flag, and this binary is where that
//! matters.
//!
//! Its twin is `g1_w4a_block_offsets_serial`.

mod g1_w2a_walk_common;

/// Levers, through `with_process_overrides` and never `std::env::set_var`.
///
/// `flags()` serves declared flags from a snapshot latched on first read, so a
/// `set_var` takes effect only if it happens to win the race to initialise that
/// snapshot — a property of what else the binary touched first rather than of
/// the test. `types/tests/flag_env_mutation_guard.rs` is the gate that says so,
/// and `with_process_overrides` also covers the collector threads the pause
/// spawns, which a thread-local override would not.
///
/// CARD CLEANING IS ON HERE ON PURPOSE. Without it a G1 card goes clean in
/// exactly one place — `G1Region::reset` — so a tenured holder region's cards
/// only ever accumulate, every card is dirty within a few pauses, and a walk
/// that can only jump over CLEAN runs has nothing to jump over. Cleaning is
/// what produces the clean runs, so an arm without it would be a test of the
/// jump's code path rather than of the jump.
const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    ("CRATONVM_G1_CARD_CLEAN", Some("1")),
    // THE LAYOUT PIN. Not a tuning knob — see "WHAT THIS WORKLOAD DOES NOT
    // PROMISE: A JUMP COUNT" in `g1_w2a_walk_common`, which carries the
    // measurements.
    //
    // The short version: the jump count this fixture produces is a property of
    // which Old region each promoted holder lands in, and that is decided by
    // which evacuation WORKER copied it — work stealing, i.e. the OS
    // scheduler. At the default worker count the count is multi-modal under
    // load ({2, 6, 8}) where it reads a flat 8 on an idle host, and the low
    // mode reaches zero. Pinned to one worker it is 6, in 30 runs out of 30,
    // under a synthetic load average of ~49.
    //
    // `=1` rather than `CRATONVM_G1_PARALLEL_EVAC=0`: this keeps the PARALLEL
    // evacuator — its TLAB pool, its shared-destination bump, its disarmed
    // per-object screens — and removes only the interleaving between workers.
    // `g1_w4a_block_offsets_serial` pins the other one, and that pair is the
    // serial/parallel contrast those two files exist for; collapsing both onto
    // the serial arm would delete it.
    //
    // What this does NOT weaken: `entries_refused`, `unvouched` and
    // `violations` are properties of the producer rule rather than of the
    // layout. They read 0 on every arm, every worker count and every heap size
    // measured, and they are what this file is about.
    //
    // LATENT rather than measured: 25 runs out of 25 passed at load ~46 on
    // 2026-09-21, because this file needs only `jumps > 0` and the low mode is
    // 2. It is pinned anyway — 2 is one step from 0, `HOLDERS = 12000` on this
    // same fixture produces `jumps=0` even on an idle host, and the two files
    // that DID fail failed for exactly this reason. No failure of this binary
    // was observed; do not read the pin as evidence of one.
    ("CRATONVM_G1_WORKERS", Some("1")),
];

#[test]
fn the_jumping_walk_finds_exactly_what_the_linear_walk_finds() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());
        assert_engaged();
    });
}

/// The lever's own engagement count, asserted rather than assumed.
///
/// `g1_w2a_walk_common::run()` owns its collector and does not hand it back, so
/// the reading is the process-wide one. Without this the file passes
/// identically with the jump never taken — which is the failure
/// `orchestrator-wave-1-measurements.md` §7.3 names as the common cause of this
/// round's three retracted measurements: the experiment did not check that it
/// was exercising the thing it named.
fn assert_engaged() {
    let (jumps, bytes, refused) = cratonvm_gc::g1::block_offset_process_census();
    assert!(
        jumps > 0,
        "the block-offset jump never fired, so this run says nothing about it          (bytes_jumped={bytes} refused={refused})"
    );
    assert_eq!(
        refused, 0,
        "a candidate entry failed its header screen on a healthy heap -- the          block-offset table named an address that is not an object"
    );
}
