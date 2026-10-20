// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-5 lane B — the block-offset table's precondition, ENFORCED rather than
//! audited.
//!
//! # What W4-A left open
//!
//! `docs/internal/g1-2026-09-20/w4a-block-offset-table.md` §8.1 kept the table
//! default-OFF for a correctness reason, not a performance one. The jump is
//! sound iff **an object holding a cross-region reference always has a dirty
//! START card**, and that rule was held up by an audit of four producers plus
//! one tripwire on one of them. A fifth producer that named a SLOT's address
//! would pass every test in the tree, and would not reliably show up as an
//! entry refusal either: a slot address that happens to sit at a real object
//! boundary further along the card decodes as a perfectly plausible header, so
//! the walk jumps to it, steps over the object that needed scanning, and
//! reports `entries_refused=0`.
//!
//! # What this binary is evidence of
//!
//! The three claims W5-B makes, in the order they matter.
//!
//! 1. **On a real G1 heap the rule holds, and the enforcement costs no jumps.**
//!    The workload below is the one every arm of the unified source walk is
//!    checked against — every live object but the root is reachable only
//!    through a remembered-set edge, so an object the jump skips is LOST and
//!    the graph assertion fails. With the lever armed it must report
//!    `jumps > 0`, `entries_refused == 0`, and now also `unvouched == 0` and
//!    `disarmed == false`. That last pair is the new statement: every entry
//!    the table handed the walk was, at the moment it was installed, an
//!    address whose sixteen bytes carried `GC_FLAG_HEADER` — the bit every
//!    allocator sets and nothing clears.
//!
//! 2. **The AUDIT arm engages and finds nothing.** `CRATONVM_G1_BLOCK_OFFSET_AUDIT`
//!    walks the bytes each jump skipped and asks of every object starting in
//!    them whether it holds a reference into another region. A hit is the one
//!    shape the vouch cannot see — a producer naming a real object start that
//!    belongs to the WRONG object. `audited > 0` beside `violations == 0` is
//!    the pair that means something; `violations == 0` alone would also be
//!    true of an audit that never ran.
//!
//! 3. It is a sibling binary, `g1_w5b_disarm_falls_back`, that says what
//!    happens when the enforcement fires.
//!
//! Note the flag discipline the round requires: `with_process_overrides`,
//! never `std::env::set_var`.

mod g1_w2a_walk_common;

/// The lever, the audit, and the two things that make there be clean card runs
/// to jump over at all. Card cleaning is on for the same reason the W4-A
/// binaries turn it on: G1's card table is additive, so without cleaning every
/// card a holder has ever taken a store into stays dirty and there is nothing
/// to skip.
const ARM: &[(&str, Option<&str>)] = &[
    ("CRATONVM_G1_BLOCK_OFFSETS", Some("1")),
    ("CRATONVM_G1_BLOCK_OFFSET_AUDIT", Some("1")),
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
    // MEASURED FLAKY, and this is the binary the flake was reported against:
    // 1 failure in 25 runs at load ~46 on an 8-core host, 2026-09-21, against
    // 0 in 100 runs on the same host idle.
    ("CRATONVM_G1_WORKERS", Some("1")),
];

#[test]
fn the_jump_engages_and_every_entry_it_used_was_vouched() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();

        assert_eq!(g1_w2a_walk_common::run(), g1_w2a_walk_common::expected());

        let (jumps, bytes, refused) = cratonvm_gc::g1::block_offset_process_census();
        let (tails, no_entry) = cratonvm_gc::g1::block_offset_no_jump_census();

        // THE TABLE'S OWN PROPERTY, ASSERTED FIRST AND WITHOUT A LOWER BOUND
        // ON ANYTHING. Every gap the cursor found, the table answered for:
        // `no_entry` counts the times the walk had a dirty card AHEAD of it
        // and the table could not name a target. It is not layout-dependent —
        // it read 0 on every arm, every worker count and every heap size
        // measured on 2026-09-21 — so it is the assertion that still means
        // something if the workload's shape ever moves again.
        assert_eq!(
            no_entry, 0,
            "the walk found {no_entry} clean-card gaps the block-offset table \
             could not name a target for. The table is not answering for cards it \
             recorded, which is a defect in the table rather than in this fixture \
             — unlike `jumps=0`, which can just mean there was nothing to jump over"
        );
        assert_eq!(refused, 0, "a candidate entry failed its header screen");

        // (1) The lever engaged. A test that passes identically with its lever
        // inert is the failure `orchestrator-wave-1-measurements.md` §7.3
        // attributes three retracted measurements of this round to, and it is
        // as available to a test as to a benchmark.
        //
        // This is the layout-dependent one, which is why `ARM` pins
        // `CRATONVM_G1_WORKERS=1`; the message names the two counters that say
        // WHICH way it went, because "the jump never fired" alone was the same
        // output for "the workload produced no gap" and "the table had no
        // answer" and cost a day of hunting on 2026-09-21.
        assert!(
            jumps > 0,
            "the block-offset jump never fired (bytes_jumped={bytes} refused={refused} \
             end_of_region_breaks={tails} gaps_with_no_entry={no_entry}). \
             `gaps_with_no_entry=0` beside this says the walk never had a clean-card \
             gap to jump over at all — the workload stopped producing the \
             precondition, so look at the heap's shape (and at whether `ARM`'s \
             CRATONVM_G1_WORKERS pin is still in force) before looking at the table"
        );

        // (2) And every entry behind those jumps was vouched. This is the
        // W5-B statement; nothing before this wave could make it.
        let (vouches, unvouched, disarmed) =
            cratonvm_gc::g1_cards::block_offset_enforcement_census();
        assert!(
            vouches > 0,
            "the vouch never ran, so `unvouched=0` is not evidence of anything"
        );
        assert_eq!(
            unvouched, 0,
            "a card producer named an address with no GC_FLAG_HEADER on a real G1 heap — \
             either the producer rule is broken or some G1 allocation path publishes a \
             header without the bit, and BOTH are reasons the table may not ship on"
        );
        assert!(
            !disarmed,
            "the block-offset jump disarmed itself during a clean run"
        );

        // (3) The audit ran, over real skipped bytes, and found the producer
        // rule holding end to end.
        let (spans, audited, violations) = cratonvm_gc::g1::block_offset_audit_census();
        assert!(spans > 0, "the audit walked no jump spans");
        assert!(
            audited > 0,
            "the audit arm walked no objects, so `violations=0` says nothing \
             (jumps={jumps} bytes_jumped={bytes})"
        );
        assert_eq!(
            violations, 0,
            "an object inside a span the jump skipped holds a reference into another \
             region — its start card should have been dirty and the walk should have \
             stopped at it"
        );

        cratonvm_gc::g1_cards::reset_block_offset_enforcement_for_tests();
    });
}
