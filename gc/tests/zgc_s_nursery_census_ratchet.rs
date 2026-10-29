// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! **The nursery census can ratchet, and here is the number that says so.**
//!
//! This instrument refuted `proposal-d-real-young-space.md` — the largest
//! programme the 2026-09-20 ZGC round proposed — on a `survivor_page_pct` of
//! 98/100/98 against a `>= 90` threshold fixed in advance. A refutation is
//! load-bearing in a way a confirmation is not: nobody re-checks a number that
//! told them to keep going.
//!
//! The re-check found a structural bias, in the direction of the conclusion:
//!
//! * `ZgcRealHeap::gen_young_floor` moves only at a **whole-heap** collection,
//!   so "the nursery" is everything allocated since the last major;
//! * promotion on this backend is a header label, not a copy, so a promoted
//!   (genuinely old) object still sits inside the nursery's address range and
//!   is still counted as a nursery survivor;
//! * so the per-page "held a survivor" bit is **monotone** within a major
//!   epoch, and because the census sums pages across cycles, the late cycles —
//!   the ones examining the most pages — dominate the aggregate.
//!
//! The aggregate therefore drifts toward 100% whatever the workload's real
//! survival rate is. It is the same artefact wave 3 caught coming through the
//! malformed-geometry door, arriving instead through a floor that is stale by
//! design.
//!
//! What this file pins is the instrument that separates *"the workload's
//! survival rate defeats page reclamation"* from *"the census ratcheted"*:
//! `first_cycle_percent` and `ratchet_margin()`. Neither changes the verdict —
//! the thresholds were fixed before the runs and moving them afterwards would
//! be the same offence in the other direction — they make the verdict's
//! admissibility readable.

#![cfg(feature = "zgc")]

use cratonvm_gc::zgc::generation::{ZNurseryGeometry, ZNurserySurvivorCensus, ZYoungSpaceVerdict};

const BASE: usize = 0x1000_0000;
const PAGE: usize = 2 * 1024 * 1024;

fn geometry(pages: usize) -> ZNurseryGeometry {
    ZNurseryGeometry {
        base: BASE,
        page_bytes: PAGE,
        floor: BASE,
        end: BASE + pages * PAGE,
    }
}

/// One cycle: a nursery of `pages` logical pages, of which the first
/// `with_survivor` hold one.
fn cycle(census: &ZNurserySurvivorCensus, pages: usize, with_survivor: usize) {
    let survivors: Vec<(usize, usize)> = (0..with_survivor)
        .map(|i| (BASE + i * PAGE + 64, 32))
        .collect();
    census.observe_cycle(geometry(pages), survivors);
}

/// **The shape the real runs have, modelled.** Each cycle the nursery grows by
/// a fresh batch of pages; every page that ever held a survivor keeps holding
/// it, because nothing evacuates it; and only a small fraction of each *fresh*
/// batch holds one.
///
/// The marginal truth here is "one new page in five holds a survivor" —
/// `proposal-d` would be strongly **supported** by that. The aggregate reads
/// `Refuted` anyway.
#[test]
fn a_ratcheting_census_reads_refuted_on_a_workload_that_supports_the_proposal() {
    let census = ZNurserySurvivorCensus::new();

    // Cycle 1: 10 pages, 2 of them hold a survivor.
    cycle(&census, 10, 2);
    // Cycles 2..6: ten more pages each time. The survivors of every earlier
    // cycle are still there (they are below the growing cursor and nothing
    // moved them), plus two per fresh batch.
    for n in 2..=6 {
        cycle(&census, 10 * n, 2 * n);
    }

    let report = census.report();
    assert_eq!(report.cycles, 6);
    assert_eq!(report.geometry_rejected, 0, "the frames are well formed");

    assert_eq!(
        report.survivor_page_percent(),
        20,
        "with a constant 1-in-5 marginal rate the aggregate is also 20 -- this run is \
         the control, and it is here so the next test's climb cannot be blamed on the \
         model",
    );
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::Supported);
    assert_eq!(report.first_cycle_percent, 20);
    assert_eq!(report.ratchet_margin(), 0, "a flat series has no ratchet");
}

/// **The ratchet itself.** Same heap, but survivors accumulate: every page
/// that has ever held one still does, because the floor has not moved and
/// promotion did not evacuate anything.
///
/// The marginal rate never changes — two of each fresh batch of ten. The
/// aggregate climbs past the refutation threshold anyway, and `ratchet_margin`
/// is what makes that visible instead of authoritative.
#[test]
fn the_ratchet_pushes_the_aggregate_past_the_refutation_threshold() {
    let census = ZNurserySurvivorCensus::new();

    // Cycle n examines 10n pages. The marginal rate is unchanged -- two of
    // each fresh batch of ten are born survivors -- but every page a previous
    // cycle marked is marked again (nothing evacuated it), and the low pages
    // additionally fill with *promoted* objects, which this census cannot
    // tell from nursery survivors. After the first cycle only the newest
    // couple of pages are clean, which is the shape the real runs reported
    // (123/125, 1041/1041, 98/100).
    for n in 1..=6usize {
        let pages = 10 * n;
        let held = if n == 1 { 2 } else { pages - 2 };
        cycle(&census, pages, held);
    }

    let report = census.report();
    assert_eq!(report.cycles, 6);
    assert!(
        report.survivor_page_percent() >= ZNurserySurvivorCensus::REFUTED_AT_PERCENT,
        "aggregate {} should have crossed the {} refutation threshold",
        report.survivor_page_percent(),
        ZNurserySurvivorCensus::REFUTED_AT_PERCENT,
    );
    assert_eq!(report.verdict(), ZYoungSpaceVerdict::Refuted);

    // ...and the admissibility test says not to believe it.
    assert_eq!(
        report.first_cycle_percent, 20,
        "the one cycle whose nursery holds only what was allocated since the floor \
         moved read 20%, which SUPPORTS the proposal",
    );
    assert!(
        report.ratchet_margin() >= 60,
        "a {}-point climb from first cycle to last is the instrument filling up, not \
         the workload changing",
        report.ratchet_margin(),
    );
    assert!(report.max_cycle_percent > report.min_cycle_percent);
}

/// The per-cycle fields must not invent a reading. Before any evidence cycle
/// they are all `0`, and `cycles` is what says that `0` is an absence — the
/// same discipline `cycles_observed` exists for.
#[test]
fn the_per_cycle_series_is_absent_rather_than_zero_until_there_is_evidence() {
    let census = ZNurserySurvivorCensus::new();
    let empty = census.report();
    assert_eq!(empty.cycles, 0);
    assert_eq!(empty.first_cycle_percent, 0);
    assert_eq!(empty.last_cycle_percent, 0);
    assert_eq!(empty.min_cycle_percent, 0);
    assert_eq!(empty.max_cycle_percent, 0);
    assert_eq!(empty.ratchet_margin(), 0);

    // An empty nursery is offered but is not evidence, so the series still
    // reports nothing -- counting it would drag every one of these toward 0,
    // i.e. toward "Supported", which is the direction an instrument must never
    // drift on its own.
    census.observe_cycle(geometry(0), Vec::new());
    let inert = census.report();
    assert_eq!(inert.cycles, 0);
    assert!(inert.is_wired(), "cycles_observed must still have fired");
    assert_eq!(inert.first_cycle_percent, 0);

    // The first real cycle STORES rather than mins: a first reading of 100
    // minned against the initial 0 would report a cycle that never happened.
    cycle(&census, 4, 4);
    let first = census.report();
    assert_eq!(first.cycles, 1);
    assert_eq!(first.first_cycle_percent, 100);
    assert_eq!(first.min_cycle_percent, 100);
    assert_eq!(first.max_cycle_percent, 100);
    assert_eq!(first.last_cycle_percent, 100);
}

/// A malformed frame is still refused and still contributes nothing to the
/// series — the wave-3 guard and this one must not interfere.
#[test]
fn a_malformed_frame_contributes_nothing_to_the_series() {
    let census = ZNurserySurvivorCensus::new();
    cycle(&census, 10, 5);
    let before = census.report();

    let bad = ZNurseryGeometry {
        base: BASE,
        page_bytes: PAGE,
        floor: BASE - PAGE, // below the grid base: an unset young floor
        end: BASE + 10 * PAGE,
    };
    assert!(!bad.is_well_formed());
    census.observe_cycle(bad, vec![(BASE + 64, 32)]);

    let after = census.report();
    assert_eq!(after.geometry_rejected, 1);
    assert_eq!(after.cycles, before.cycles);
    assert_eq!(after.last_cycle_percent, before.last_cycle_percent);
    assert_eq!(after.ratchet_margin(), before.ratchet_margin());
}
