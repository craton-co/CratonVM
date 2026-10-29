// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-5 lane B — the audit as a SAMPLE, which is the form it can ship in.
//!
//! # Why a sample is not a weaker audit
//!
//! `CRATONVM_G1_BLOCK_OFFSET_AUDIT=1` walks every span a jump skipped, which
//! costs exactly the work the jump removed. That is the right price for a soak
//! or for this test suite and the wrong one for production, so the lever is a
//! PERIOD: `N` audits one jump in every `N`.
//!
//! The fault it looks for is a card PRODUCER, and a producer is systematic. A
//! producer that names the wrong object's start does it on every edge it
//! records, not once — so sampling does not lower the probability of catching
//! it, it delays the catch by `N` jumps. Against a workload that takes
//! hundreds of thousands of jumps per run, `N = 1024` surfaces a systematic
//! violation inside a single pause, for 1/1024 of the saving given back.
//!
//! And a violation disarms the jump process-wide, exactly as an unvouched
//! producer address does. That is what makes the sampled arm a safety
//! mechanism rather than a diagnostic: it is the only thing in a SHIPPING
//! binary that can see a producer naming a real object start belonging to the
//! wrong object, which is the one hole the `GC_FLAG_HEADER` vouch cannot
//! reach.
//!
//! # What this binary asserts, and why the ratio is the point
//!
//! A sampling period is precisely the kind of lever that can silently resolve
//! to "never" — the failure `orchestrator-wave-1-measurements.md` §7.1
//! retracted a whole measurement for. So the assertion is not "the audit found
//! nothing"; that would also be true of an audit that never ran. It is that
//! `jumps / audit_spans` comes out near the declared period, which can only be
//! true if the sampler is both firing and sampling.

mod g1_w2a_walk_common;

/// Period 4 rather than 1024: this fixture takes tens of jumps, not hundreds
/// of thousands, so a production-sized period would sample zero of them and
/// the ratio assertion below would have nothing to check. The MECHANISM is
/// period-independent; what a bigger period changes is only how long a
/// violation waits, and that is arithmetic rather than behaviour.
const PERIOD: u64 = 4;

const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    ("CRATONVM_G1_BLOCK_OFFSET_AUDIT", Some("4")),
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
    // MEASURED FLAKY, and the worst of the family by a wide margin, because it
    // needs a whole sampling period's worth of jumps rather than one:
    // 11 failures in 25 runs at load ~46 on an 8-core host, 2026-09-21,
    // against 0 in 60 runs on the same host idle. Nobody had reported it.
    ("CRATONVM_G1_WORKERS", Some("1")),
];

#[test]
fn the_sampled_audit_fires_at_its_declared_period_and_finds_nothing() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();

        assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());

        let (jumps, _bytes, refused) = cratonvm_gc::g1::block_offset_process_census();
        let (spans, audited, violations) = cratonvm_gc::g1::block_offset_audit_census();
        let (tails, no_entry) = cratonvm_gc::g1::block_offset_no_jump_census();
        assert_eq!(refused, 0);
        // Layout-independent, so it is asserted before the jump count and is
        // the one that still means something if the workload's shape moves.
        assert_eq!(
            no_entry, 0,
            "the walk found {no_entry} clean-card gaps the table could not name a target for"
        );
        // THIS BINARY IS THE TIGHTEST CONSUMER of the fixture's jump count --
        // it needs a whole sampling period's worth, not merely one -- and
        // before `ARM` pinned `CRATONVM_G1_WORKERS=1` it was also the FLAKIEST
        // thing in this file family: 11 failures in 25 runs under a synthetic
        // load average of ~49 on an 8-core host (2026-09-21), against 0 in 60
        // runs on the same host idle. The fixture's low mode is `jumps=2` and
        // `PERIOD` is 4. See "WHAT THIS WORKLOAD DOES NOT PROMISE: A JUMP
        // COUNT" in `g1_w2a_walk_common`.
        assert!(
            jumps >= PERIOD,
            "the fixture took {jumps} jumps, fewer than one sampling period — \
             the ratio below cannot say anything (end_of_region_breaks={tails} \
             gaps_with_no_entry={no_entry}; check that `ARM`'s CRATONVM_G1_WORKERS \
             pin is still in force before suspecting the table)"
        );

        // The sampler fired…
        assert!(
            spans > 0,
            "the sampled audit never fired across {jumps} jumps"
        );
        // …and it sampled rather than auditing everything. Both bounds, because
        // a broken sampler fails in both directions: one that never resets
        // audits every jump, and one that never fires audits none.
        assert!(
            spans <= jumps,
            "more audited spans ({spans}) than jumps ({jumps})"
        );
        assert!(
            spans * PERIOD <= jumps + PERIOD,
            "audited {spans} of {jumps} jumps, which is denser than one in {PERIOD}"
        );
        assert!(
            spans * PERIOD * 2 >= jumps,
            "audited {spans} of {jumps} jumps, which is sparser than one in {PERIOD}"
        );

        assert!(audited > 0, "the audited spans contained no objects");
        assert_eq!(
            violations, 0,
            "an object inside a skipped span holds a cross-region reference"
        );
        let (_, unvouched, disarmed) = cratonvm_gc::g1_cards::block_offset_enforcement_census();
        assert_eq!(unvouched, 0);
        assert!(!disarmed, "a clean run must not disarm the jump");

        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();
    });
}
