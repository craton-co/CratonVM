// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E, wave 2 (2026-09-20) — the Phase-2 card screen's byte skip-rate is
//! observable from a shipped binary, as a TREND.
//!
//! # Why this is a test and not a log line someone eyeballed
//!
//! The four card-screen counters have existed since F-05 and were rendered in
//! exactly one place: `[GC] g1 card-clean:` in `print_gc_summary`. That
//! function is reached only from `vm-cli`'s normal-return teardown, and
//! `JUnitCore` ends in `System.exit`, which never unwinds Rust frames — so on
//! every JUnit workload in the suites the rate was never printed at all. A
//! `tracing::debug!` would not have helped either: `release_max_level_info`
//! compiles it out and no `RUST_LOG` value recovers it from a shipped binary.
//!
//! That made one specific experiment unrunnable, and it is the experiment that
//! decides `CRATONVM_G1_CARD_CLEAN`'s default. G1's card table is
//! ADDITIVE-ONLY: it saturates by construction, so the screen's skip-rate
//! decays over a long run and cleaning is the only thing that pushes back.
//! "Does the rate decay, and does cleaning hold it up" is a question about the
//! SHAPE of the decay, which a single cumulative total averages away. See §5 of
//! `docs/internal/g1-2026-09-20/lane-a-card-clean-remeasure.md`.
//!
//! # The trap this file had to be rewritten to avoid
//!
//! The first draft aged objects at the default `promotion_age: 15`, ran eight
//! young pauses, and asserted the rates were internally consistent. It passed.
//! It also recorded `rset_regions_offered = 0` on every pause — nothing
//! tenured, so there were no Old regions, so Phase 2 was never offered a
//! remembered-set source, so **the card screen never ran at all**. The test was
//! green while exercising none of the code it is named for, which is precisely
//! the failure this lane keeps finding in the collector's own instruments (the
//! `[GC-STAT]` `try_read` census; the tenuring histogram) reproduced in a test.
//!
//! `fixture_with_old_to_young_edges` therefore asserts its own preconditions —
//! Old regions exist, and the screen was offered something — BEFORE anything is
//! asserted about the numbers.

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::{G1Collector, G1CollectorConfig, GarbageCollector};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

const CHILD: usize = 0;
const ARRAY: usize = 1;
const FIELDS: usize = 3;
const HOLDERS: usize = 64;

fn heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        initial_heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        // Tenure fast, and it is load-bearing — see the module doc. At the
        // default 15 nothing promotes inside a short fixture, no Old region
        // exists, and Phase 2 is never offered a source region to screen.
        promotion_age: 2,
        ..Default::default()
    })
}

/// Build a set of Old holders each pointing at a fresh young object, run
/// `pauses` young collections over it, and return the collector.
///
/// The old→young edge is the whole point: it is what the write barrier records
/// into the remembered set, and a remembered-set source is the only thing
/// Phase 2's card screen is ever asked about.
fn fixture_with_old_to_young_edges(gc: &G1Collector, pauses: usize) {
    let root = gc.alloc_object(ClassId::new(42), HOLDERS);
    let mut holders = Vec::with_capacity(HOLDERS);
    for h in 0..HOLDERS {
        let holder = gc.alloc_object(ClassId::new(41), FIELDS);
        let arr = gc.alloc_array(ClassId::new(43), ArrayElementType::Reference, 96);
        gc.set_field(holder, ARRAY, Value::Object(Some(arr)));
        gc.set_field(root, h, Value::Object(Some(holder)));
        holders.push(holder);
    }
    let mut roots = vec![root];

    // Age the holders until they tenure.
    for _ in 0..6 {
        gc.young_collection(&mut roots, &NoMonitors);
    }
    let root = roots[0];
    assert!(
        gc.count_regions(cratonvm_gc::region::RegionType::Old) > 0,
        "no holder tenured, so no region can be a remembered-set source and \
         the card screen is never asked anything — check promotion_age against \
         the collector default"
    );
    for (h, holder) in holders.iter_mut().enumerate() {
        let Value::Object(Some(cur)) = gc.get_field(root, h) else {
            panic!("holder {h} was lost while ageing");
        };
        *holder = cur;
    }

    // Now hang fresh young objects off every Old holder, once per pause, so the
    // screen is offered source regions on every pause rather than only the
    // first — a trend needs more than one sample.
    let mut roots = vec![root];
    for pause in 0..pauses {
        for (h, &holder) in holders.iter().enumerate() {
            let child = gc.alloc_object(ClassId::new(41), FIELDS);
            gc.set_field(holder, CHILD, Value::Object(Some(child)));
            let Value::Object(Some(arr)) = gc.get_field(holder, ARRAY) else {
                panic!("holder {h} lost its array");
            };
            let e = gc.alloc_object(ClassId::new(41), FIELDS);
            gc.set_array_element(arr, (h + pause) % 96, Value::Object(Some(e)))
                .expect("array store");
        }
        // Garbage, so the pause has a real Eden to reclaim.
        for _ in 0..2000 {
            let junk = gc.alloc_object(ClassId::new(44), 2);
            gc.set_field(junk, 0, Value::Int(0));
        }
        gc.young_collection(&mut roots, &NoMonitors);
    }
}

#[test]
fn the_skip_rate_readout_exists_before_any_pause_and_is_not_a_lie() {
    let gc = heap();
    let (scanned, skipped, all, first, now) = gc.card_screen_skip_rates();
    assert_eq!(
        (scanned, skipped, all, first, now),
        (0, 0, 0, 0, 0),
        "with no pause taken, every figure must be zero — a rate computed from \
         an empty denominator must not be invented"
    );
}

#[test]
fn a_run_of_pauses_leaves_a_readable_cumulative_census() {
    let gc = heap();
    fixture_with_old_to_young_edges(&gc, 8);

    // THE TRIPWIRE, before anything is asserted about the numbers.
    //
    // `rset_regions_offered == 0` means Phase 2 had nothing to do, and every
    // figure below would then be a legitimate zero that reads exactly like a
    // working instrument. This assertion is what makes the file fail loudly
    // instead of passing while measuring nothing.
    let history = gc.pause_history_snapshot();
    let offered: u64 = history
        .iter()
        .map(|r| r.phases.rset_regions_offered as u64)
        .sum();
    assert!(
        history.len() >= 8,
        "the workload must have taken the pauses it asked for, got {}",
        history.len()
    );
    assert!(
        offered > 0,
        "Phase 2 was never offered a remembered-set source region across {} \
         pauses, so the card screen never ran and nothing below this line is a \
         measurement of it",
        history.len()
    );

    let (scanned, skipped, all_ppm, first_ppm, now_ppm) = gc.card_screen_skip_rates();
    assert!(
        scanned + skipped > 0,
        "the screen was offered {offered} source regions but accounted for no \
         bytes at all — the census is not wired to the walk"
    );

    // The cumulative rate must be DERIVED from the cumulative bytes rather than
    // accumulated beside them: two accumulators drift, and a drifting rate is
    // worse than none because it still looks like a measurement.
    let expected = skipped * 1_000_000 / (scanned + skipped);
    assert_eq!(all_ppm, expected);

    for (name, ppm) in [("all", all_ppm), ("first", first_ppm), ("now", now_ppm)] {
        assert!(
            ppm <= 1_000_000,
            "{name} rate of {ppm} ppm is above 100%, which is not a rate"
        );
    }
    // The trend's endpoints must both exist once the screen has run. `first` is
    // frozen after the run's first 64 instrumented pauses and this run is
    // shorter, so on this fixture the two are measuring overlapping pauses —
    // what is asserted is that neither is structurally zero while the other is
    // not, which is the shape a mis-wired endpoint would take.
    assert!(
        (first_ppm > 0) == (now_ppm > 0),
        "one trend endpoint is zero while the other is not (first={first_ppm} \
         now={now_ppm}) — they are computed from the same pauses on a run this \
         short, so exactly one of them is mis-wired"
    );
}

#[test]
fn a_pause_that_offered_the_screen_nothing_does_not_move_the_rate() {
    // The anti-dilution rule. `rset_regions_offered == 0` means Phase 2 had
    // nothing to do — a different fact from "the screen ran and refused every
    // region" — and folding it in as a 0 % sample would drag the trend toward
    // zero for a reason that has nothing to do with card saturation. On a long
    // run of a workload with little old-to-young pointing, that alone would
    // manufacture the decay this instrument exists to detect.
    let gc = heap();
    let mut roots: Vec<ObjectRef> = Vec::new();
    for _ in 0..8 {
        gc.young_collection(&mut roots, &NoMonitors);
    }
    let offered: u64 = gc
        .pause_history_snapshot()
        .iter()
        .map(|r| r.phases.rset_regions_offered as u64)
        .sum();
    assert_eq!(
        offered, 0,
        "this fixture is supposed to offer the screen nothing; it offered \
         {offered}, so it is not testing the dilution rule"
    );
    let (_, _, _, _, now) = gc.card_screen_skip_rates();
    assert_eq!(
        now, 0,
        "eight pauses that offered the screen nothing must leave the rolling \
         rate untouched at its initial zero, not averaged down to one"
    );
}
