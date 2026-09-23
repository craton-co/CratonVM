// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E, wave 2 (2026-09-20) — `max_gc_pause_ms` now prices the fix-up walk
//! the collection set CAUSES, and the young half it is already committed to.
//!
//! `lane-e-the-pause-goal-still-does-not-price-the-fixup-walk-it-causes.md`
//! states the two gaps precisely:
//!
//! 1. `fixup_ns_ema` is a constant with respect to the choice being made.
//!    `select_old_regions_for_mixed_gc` adds regions while
//!    `Σ copy < goal − fixup(PREVIOUS cset)`, but `phase4_regions_to_walk`
//!    narrows the walk to the changed regions plus the remembered-set sources —
//!    so every old region added to the CSet adds its to-space destination AND
//!    pulls its rset sources into the walk. The model under-estimates in the
//!    direction that overruns.
//! 2. The young half of the collection set is not priced at all. Every Eden and
//!    Survivor region is in the CSet unconditionally, so the copy the budget is
//!    spending the goal on is the *second* copy of the pause.
//!
//! What these tests can and cannot prove: they prove the BUDGET is now a
//! function of both terms and that the selector spends it marginally. They
//! cannot prove `max_gc_pause_ms` is met — that is a workload measurement, and
//! `docs/internal/g1-2026-09-20/w2e-pause-goal-marginal-cost-model.md` records
//! it.

use cratonvm_gc::{G1Collector, G1CollectorConfig};

fn heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 64 * 1024,
        initial_heap_size: 64 * 64 * 1024,
        region_size: 64 * 1024,
        // 10 ms, so the budget is 10_000_000 ns and the arithmetic below is
        // readable without a calculator.
        max_gc_pause_ms: 10,
        ..Default::default()
    })
}

#[test]
fn the_budget_charges_the_young_half_before_the_old_one() {
    let gc = heap();
    // A 10 ms goal. The fix-up walk costs 2 ms; the young half costs 5 ms.
    gc.dbg_set_cost_estimates(2_000_000, 40, 5_000_000, 4);
    let (without_model, with_model, _) = gc.dbg_old_cset_budget();

    assert_eq!(
        without_model, 8_000_000,
        "pre-wave-2: goal minus the fix-up walk, and nothing else"
    );
    assert_eq!(
        with_model, 3_000_000,
        "with the model: and minus the young half the pause is already \
         committed to copying"
    );
    assert!(
        with_model < without_model,
        "the young charge can only ever make the old collection set smaller, \
         which is the safe direction for a pause goal"
    );
}

#[test]
fn a_goal_already_spent_floors_at_zero_rather_than_wrapping() {
    let gc = heap();
    // The young half alone costs more than the whole goal. This is not
    // hypothetical: it is the shape the finding describes on a large young
    // generation, and an unsigned subtraction that wrapped here would hand the
    // selector a budget of ~1.8e19 ns and take every candidate in the heap.
    gc.dbg_set_cost_estimates(6_000_000, 40, 9_000_000, 4);
    let (_, with_model, _) = gc.dbg_old_cset_budget();
    assert_eq!(with_model, 0);

    // And a zero budget must still make forward progress: the selector takes
    // one region unconditionally, or a mixed phase that once overran its goal
    // could never retire another candidate for the life of the process.
    //
    // Planted candidates, not an empty table: `len() <= 1` is satisfied by
    // `vec![]`, so on a heap with no Old candidates this assertion would hold
    // for a selector that had been deleted.
    let candidates = plant_old_candidates(&gc);
    assert!(candidates >= 8);
    let selected = gc.select_old_regions_for_mixed_gc_within(true);
    assert_eq!(
        selected.len(),
        1,
        "a spent budget must select exactly one region from {candidates}          candidates: one for forward progress, and not a second"
    );
}

#[test]
fn the_per_region_fixup_price_is_the_mean_over_walked_regions() {
    let gc = heap();
    gc.dbg_set_cost_estimates(2_000_000, 40, 0, 4);
    let (_, _, per_region) = gc.dbg_old_cset_budget();
    assert_eq!(per_region, 50_000, "2 ms over 40 regions is 50 us each");
}

#[test]
fn an_unmeasured_fixup_walk_is_charged_at_nothing_rather_than_at_infinity() {
    // The degradation path matters more than the happy one. Before either EMA
    // is warm there is no per-region price, and the marginal model must fall
    // back to EXACTLY the fixed-budget model rather than to something new: an
    // unmeasured cost charged at infinity would refuse every candidate and
    // silently end the mixed phase.
    let gc = heap();
    gc.dbg_set_cost_estimates(0, 0, 0, 4);
    let (_, _, per_region) = gc.dbg_old_cset_budget();
    assert_eq!(per_region, 0);

    // A non-zero total with a zero denominator is the other half of the same
    // trap: the first pause after a run of pauses that walked nothing.
    gc.dbg_set_cost_estimates(2_000_000, 0, 0, 4);
    let (_, _, per_region) = gc.dbg_old_cset_budget();
    assert_eq!(per_region, 0, "a zero denominator must not divide");
}

#[test]
fn the_marginal_model_never_selects_more_than_the_fixed_one() {
    // The invariant that makes this change safe to land opt-in: every term the
    // model adds is a subtraction from the budget or an addition to a
    // candidate's price, so arming it can only shrink the collection set. A
    // build where the model selected MORE would be a bug in the arithmetic, not
    // a policy difference.
    let gc = heap();
    let candidates = plant_old_candidates(&gc);

    // THE TRIPWIRE. `select_old_regions_for_mixed_gc` filters on
    // `region_type == Old && live_bytes > 0 && live_bytes < 85% of a region`.
    // `live_bytes` is written by the mark cycle, so a fixture that merely
    // allocates and ages leaves every Old region at `live_bytes = 0`, the
    // candidate list empty, and BOTH arms returning `vec![]` — on which
    // `marginal.len() <= fixed.len()` is vacuously true and this test asserts
    // nothing at all. That is why the regions are planted explicitly.
    assert!(
        candidates >= 8,
        "planted only {candidates} mixed-GC candidates; with fewer than a          handful the budget never binds and the comparison below is vacuous"
    );

    // Warm the estimators to a state where the budget BINDS: a 10 ms goal, a
    // 2 ms fix-up walk over 40 regions (so 50 us per walked region) and a 3 ms
    // young half. The fixed model then has 8 ms and the marginal one 5 ms, and
    // each candidate costs the marginal model an extra 50 us per walk region it
    // adds.
    gc.dbg_set_cost_estimates(2_000_000, 40, 3_000_000, 4);

    let fixed = gc.select_old_regions_for_mixed_gc_within(false);
    let marginal = gc.select_old_regions_for_mixed_gc_within(true);

    assert!(
        !fixed.is_empty(),
        "the fixed model selected nothing from {candidates} candidates, so the          comparison below is vacuous"
    );
    assert!(
        marginal.len() <= fixed.len(),
        "the marginal model selected {} regions where the fixed one selected {}          — every term it adds is a cost, so this cannot happen",
        marginal.len(),
        fixed.len()
    );
    // Forward progress is unconditional in both arms: a mixed phase that once
    // overran its goal must still be able to retire a candidate.
    assert!(!marginal.is_empty());
}

#[test]
fn the_marginal_model_actually_binds_where_the_fixed_one_does_not() {
    // The previous test proves an inequality that a no-op change would also
    // satisfy. This one proves the model DOES something: with the same
    // candidates and a budget the fixed model spends on many regions, the
    // marginal charge must cost it some.
    let gc = heap();
    let candidates = plant_old_candidates(&gc);
    assert!(candidates >= 8);

    // A fix-up walk that is expensive PER REGION (5 ms over 10 regions = 500 us
    // each) against a 10 ms goal, so the marginal charge is the dominant term.
    gc.dbg_set_cost_estimates(5_000_000, 10, 0, 1);
    let fixed = gc.select_old_regions_for_mixed_gc_within(false);
    let marginal = gc.select_old_regions_for_mixed_gc_within(true);
    eprintln!(
        "marginal cost model: fixed={} regions, marginal={} regions, from          {candidates} candidates",
        fixed.len(),
        marginal.len()
    );
    assert!(
        marginal.len() < fixed.len(),
        "with a 500 us-per-region fix-up charge against a 5 ms residual budget,          the marginal model must take FEWER regions than the fixed one; it took          {} against {}. If these are equal the marginal term is not reaching          the selector.",
        marginal.len(),
        fixed.len()
    );
}

/// Plant Old regions that `select_old_regions_for_mixed_gc` will accept as
/// mixed-GC candidates, and return how many.
///
/// Written against the region table directly rather than grown through the
/// allocator because the filter requires `live_bytes > 0`, which only a
/// completed concurrent mark cycle produces — and a fixture that ran one would
/// be testing the marker, not the budget.
fn plant_old_candidates(gc: &G1Collector) -> usize {
    use cratonvm_gc::region::RegionType;
    let region_size = 64 * 1024usize;
    let mut planted = 0usize;
    gc.with_regions_mut(|regions| {
        // Leave the low regions alone; the allocator may already own them.
        for (i, r) in regions.iter_mut().enumerate().skip(8).take(16) {
            r.set_region_type(RegionType::Old);
            // Well under the 85 % `mixed_gc_live_threshold_percent` cut, and
            // non-zero so the region has "liveness data from the last mark".
            r.live_bytes = region_size / 10;
            r.gc_efficiency = i as f64;
            // Remembered-set sources, which is what the marginal model charges
            // for: each candidate pulls its own sources into the fix-up walk,
            // and a source named twice is charged once.
            for src in 0..6usize {
                r.rset.add_reference(src);
            }
            planted += 1;
        }
    });
    planted
}

struct NoMonitors;
impl cratonvm_gc::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}
