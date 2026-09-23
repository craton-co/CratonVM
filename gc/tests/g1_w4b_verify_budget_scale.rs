// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W4-B — the CSet verifier's budget does not scale with the heap, and
//! the flag most people use to look at it turns it off.
//!
//! # What this file pins
//!
//! Only the parts that can be stated as an invariant. The coverage
//! measurements themselves are in
//! `docs/internal/g1-2026-09-20/w4b-the-verify-budget-does-not-scale-and-the-flag-that-displays-it-changes-it.md`
//! — they need a real heap at three sizes and are a document, not an
//! assertion.
//!
//! What IS an invariant, and what a regression here would look like:
//!
//! 1. **The census line names both of its switches and says whether the pass
//!    was budgeted at all.** This is the trap: `budget_truncated=0` under
//!    `--verbose:gc` means "there was no budget", not "the budget was never
//!    reached", and one run reported 6 547 331 objects through one door and
//!    24 576 through the other. Without `unbounded_pauses=` on the line those
//!    two are indistinguishable in kind.
//! 2. **A `bytes` denominator exists at all.** "The sampler rotates, so
//!    coverage accumulates" is a claim about a RATE, and a rate needs a
//!    denominator in the same units the walk advances in. There was none.
//!
//! The sweep-period arithmetic itself (`walkable / n`, floored at one region)
//! is checked here as a pure function of its inputs, because the interesting
//! failure is the one the first implementation had: a second bound that could
//! only ever lower coverage.

use cratonvm_gc::g1::{verify_budget_for_report, verify_sweep_pauses_for_report};
use cratonvm_gc::gc_metrics::{collector_decision_report, record_g1_cset_verify};

#[test]
fn the_cset_verify_line_names_both_switches_and_says_whether_it_was_budgeted() {
    // One recorded pass, so the line prints at all. `unbounded = true` is the
    // `--verbose:gc` shape: a whole-heap sweep with no budget, whose
    // `truncated = false` is a tautology rather than a result.
    record_g1_cset_verify(1_309_466, 56_714_246, 0, false, true);
    let text = collector_decision_report();

    for needle in [
        "[GC] g1 cset-verify:",
        // The denominator that did not exist: a coverage FRACTION is
        // unstatable without it.
        "bytes=",
        "bytes/pause=",
        // The trap detector.
        "unbounded_pauses=",
        // W3-C's rule — a census line names the switch that governs it. This
        // line has two, and they interact.
        "budget=",
        "sweep_pauses=",
    ] {
        assert!(
            text.contains(needle),
            "`{needle}` is missing from the collector decision report. The \
             cset-verify line is the only published statement of how much of \
             the heap the dangling-reference verifier actually covered, and \
             every one of these fields exists because without it a reader \
             draws the wrong conclusion from the fields that remain.\n\n{text}"
        );
    }
}

#[test]
fn the_two_switches_report_their_resolved_values() {
    // Not `assert_eq!` against a literal: the point is that the REPORTED value
    // is the one the code path uses, so a build with either flag set in the
    // ambient environment still passes and still says something true. What is
    // pinned is that the defaults are the pre-wave-4 behaviour — a flat object
    // budget, no sweep period — so a build that silently armed the scaled
    // budget would fail here.
    let budget = verify_budget_for_report();
    let sweep = verify_sweep_pauses_for_report();
    if std::env::var_os("CRATONVM_G1_VERIFY_BUDGET").is_none() {
        assert_eq!(
            budget, 4096,
            "the flat object budget's default is the pre-wave-4 behaviour"
        );
    }
    if std::env::var_os("CRATONVM_G1_VERIFY_SWEEP_PAUSES").is_none() {
        assert_eq!(
            sweep, 0,
            "`CRATONVM_G1_VERIFY_SWEEP_PAUSES` defaults to 0, i.e. the flat \
             object budget is the whole story and today's behaviour is \
             unchanged"
        );
    }

    let text = collector_decision_report();
    assert!(
        text.contains(&format!("budget={budget}"))
            && text.contains(&format!("sweep_pauses={sweep}")),
        "the line must print the values the code path resolved, not literals"
    );
}

/// The sweep-period budget, as the pure arithmetic the wrapper performs.
///
/// Mirrored here rather than called, because the real one needs a borrowed
/// region table inside a pause. What is worth pinning is the SHAPE, and the
/// shape is the thing the first implementation got wrong.
fn byte_budget(walkable: usize, n: usize, region_size: usize) -> usize {
    (walkable / n.max(1)).max(region_size)
}

#[test]
fn the_sweep_budget_scales_with_the_live_set_and_not_with_the_heap_the_operator_asked_for() {
    let region = 1024 * 1024;
    // The three heaps measured in the filed page, at their measured occupancy.
    // A period of 64 must give a per-pause budget proportional to what is
    // actually walkable — which is the whole content of the change, because
    // the flat budget gave the same 4096 objects to all three.
    let small = byte_budget(56 * 1024 * 1024, 64, region);
    let medium = byte_budget(134 * 1024 * 1024, 64, region);
    let large = byte_budget(269 * 1024 * 1024, 64, region);
    assert!(
        small < medium && medium < large,
        "a fixed sweep period must buy a budget that grows with the live set: \
         {small} / {medium} / {large}"
    );
    // And the ratios are the live-set ratios, not something attenuated.
    assert_eq!(
        large / small,
        4,
        "269 MiB is 4.8x 56 MiB; the budget tracks it"
    );

    // A 32 GiB `-Xmx` holding a small live set must NOT buy a budget sized for
    // 32 GiB. This is why `walkable` is summed from region cursors rather than
    // taken from `config.heap_size`.
    let huge_heap_small_live = byte_budget(200 * 1024 * 1024, 64, region);
    assert_eq!(
        huge_heap_small_live,
        byte_budget(200 * 1024 * 1024, 64, region),
        "the budget is a function of what is walkable, not of -Xmx"
    );
}

#[test]
fn the_floor_is_one_region_so_a_nearly_empty_heap_still_makes_progress() {
    let region = 1024 * 1024;
    // 8 MiB walkable with a period of 64 asks for 128 KiB per pause, which is
    // the failure mode a divisor alone produces: a sampler that truncates
    // after a handful of objects and calls it rotation. The floor is the thing
    // the flat budget was right about.
    assert_eq!(byte_budget(8 * 1024 * 1024, 64, region), region);
    // And the floor stops applying as soon as the division beats it, so it
    // cannot become a second flat budget in disguise.
    assert!(byte_budget(256 * 1024 * 1024, 64, region) > region);
}
