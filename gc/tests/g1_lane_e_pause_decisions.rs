// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane E (2026-09-20) — G1-7: every pause states its own decision, and the
//! adaptive controllers that make those decisions are readable between pauses.
//!
//! The audit's G1-7 row is about fail-safes being invisible. The half that was
//! still missing is the other kind of silent decision: the four adaptive
//! controllers (`update_young_target`, `update_tenuring_threshold`,
//! `old_cset_copy_budget_ns`, `update_evac_cost`) each change what a pause
//! collects, and their state was printed only at SHUTDOWN — and for the
//! tenuring histogram, structurally never, because the only thing that
//! snapshotted it ran behind a flag while the thing that FILLED it did not.
//!
//! These tests assert the readouts, not the policies: a diagnostic that is
//! always zero reads as an answer, which is worse than no diagnostic at all.

use cratonvm_gc::collector::MonitorCleanup;
use cratonvm_gc::{G1Collector, G1CollectorConfig, GarbageCollector};
use cratonvm_types::ClassId;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn small_heap() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 64 * 1024,
        initial_heap_size: 64 * 64 * 1024,
        region_size: 64 * 1024,
        ..Default::default()
    })
}

#[test]
fn a_pause_that_copies_survivors_leaves_a_readable_age_histogram() {
    let gc = small_heap();

    // A root chain that must survive the pause, so the evacuator copies it into
    // survivor space and calls `note_survivor_age` for it.
    let mut roots: Vec<_> = (0..64)
        .map(|_| gc.alloc_object(ClassId::new(1), 2))
        .collect();

    let (threshold_before, hist_before) = gc.tenuring_state();
    assert!(
        hist_before.iter().all(|&b| b == 0),
        "nothing has been collected yet, so the histogram must be empty"
    );
    assert!(threshold_before >= 1);

    gc.young_collection(&mut roots, &NoMonitors);

    let (threshold, hist) = gc.tenuring_state();
    // The histogram is snapshotted by `update_tenuring_threshold`, which now
    // runs on EVERY pause rather than only when `CRATONVM_G1_ADAPTIVE_TENURING`
    // is on. Before that change this read was structurally zero in the off arm
    // and the live counters grew without bound for the life of the process.
    assert!(
        hist.iter().sum::<usize>() > 0,
        "a pause that copied {} roots to survivor space must report their ages: {hist:?}",
        roots.len()
    );
    // Age 0 is never populated: `note_survivor_age` is called with the age the
    // object carries AFTER the copy incremented it, so the first survival is
    // age 1.
    assert_eq!(hist[0], 0, "bucket 0 must stay empty by construction");
    assert!(hist[1] > 0, "first-survival bytes belong in bucket 1");
    assert!(
        (1..=15).contains(&threshold),
        "the tenuring threshold must stay in [1, promotion_age]: {threshold}"
    );
}

#[test]
fn the_histogram_is_consumed_by_each_pause_rather_than_accumulating() {
    let gc = small_heap();
    let mut roots: Vec<_> = (0..32)
        .map(|_| gc.alloc_object(ClassId::new(1), 2))
        .collect();

    gc.young_collection(&mut roots, &NoMonitors);
    let (_, after_first) = gc.tenuring_state();
    let first_total: usize = after_first.iter().sum();
    assert!(first_total > 0);

    // A second pause over the SAME live set copies the same bytes again. If the
    // histogram were not consumed, the total would roughly double; it must not.
    gc.young_collection(&mut roots, &NoMonitors);
    let (_, after_second) = gc.tenuring_state();
    let second_total: usize = after_second.iter().sum();
    assert!(
        second_total <= first_total * 3 / 2,
        "the histogram is accumulating across pauses: {first_total} -> {second_total}"
    );
    // And the ages moved on, which is the signal the threshold is derived from.
    assert!(
        after_second[2] > 0 || after_second[1] > 0,
        "the second pause must report ages: {after_second:?}"
    );
}

#[test]
fn the_young_sizing_controller_is_readable_and_stays_inside_its_bounds() {
    let gc = small_heap();
    let total_regions = gc.num_regions();

    // The documented floor and ceiling: 5% and 60% of the region count, each at
    // least one region. They are constants in `g1.rs` rather than public items,
    // so this asserts the property rather than importing the numbers.
    let target_before = gc.young_target_regions();
    assert!(
        target_before >= 1 && target_before <= total_regions,
        "young target {target_before} outside [1, {total_regions}]"
    );
    assert_eq!(
        gc.young_region_count(),
        0,
        "nothing has been allocated yet, so no young region exists"
    );

    let mut roots: Vec<_> = (0..16)
        .map(|_| gc.alloc_object(ClassId::new(1), 4))
        .collect();
    assert!(
        gc.young_region_count() >= 1,
        "allocation must have produced at least one Eden region"
    );

    gc.young_collection(&mut roots, &NoMonitors);

    let target_after = gc.young_target_regions();
    assert!(
        target_after >= 1 && target_after <= total_regions,
        "young target {target_after} left its bounds after a pause"
    );
    // A pause well inside the goal must not TIGHTEN the target — that is the
    // anti-storm rule `update_young_target` documents, and the one a reader of
    // the summary line needs to be able to check.
    //
    // LANE W5-A (2026-09-21) — asserted against what the pause MEASURED rather
    // than against the assumption that a sixteen-object pause on a 4 MiB heap
    // is fast. It is not always: this test was seen to fail once, on a loaded
    // host, with `38 -> 30` — which is exactly `cur * 4 / 5`, the tightening
    // arm, and therefore a genuine `pause_us > goal_us` rather than a defect in
    // the controller. A 200 ms default goal is a long time, and it is not a
    // time this test can guarantee under `cargo test`'s parallel threads, a
    // cold first-touch commit of the heap, and whatever else the host is doing.
    //
    // The RULE is what is worth pinning and it does not depend on the clock:
    // tighten only on an overrun, never on a pause inside the goal. So read the
    // pause the collector actually recorded and assert the rule against it. A
    // test that fails on host load teaches its readers to re-run rather than to
    // read, and this round has already had to widen three such bounds.
    let goal_us = 200 * 1_000;
    let recorded = gc.pause_history_snapshot();
    let overran = recorded
        .last()
        .map(|r| r.pause_us > goal_us)
        .unwrap_or(false);
    if overran {
        assert!(
            target_after < target_before,
            "the pause overran the goal, so the target had to tighten: \
             {target_before} -> {target_after}"
        );
    } else {
        assert!(
            target_after >= target_before,
            "a sub-goal pause tightened the young target: {target_before} -> {target_after}"
        );
    }
}

#[test]
fn the_summary_reports_the_policies_without_a_collection_having_happened() {
    // `print_gc_summary` returns early once it reaches `pause_summary()` on a
    // run with no collection. The policy lines are deliberately BEFORE that
    // early return, so a run that never collected still says what it was
    // configured to do. The assertion here is that it neither panics nor
    // deadlocks — it is called at shutdown, potentially from a teardown path
    // holding nothing, and the young-sizing line it now emits reads four
    // atomics that a pause writes.
    let gc = small_heap();
    gc.print_gc_summary();

    gc.enable_gc_logging();
    let mut roots: Vec<_> = (0..8)
        .map(|_| gc.alloc_object(ClassId::new(1), 1))
        .collect();
    gc.young_collection(&mut roots, &NoMonitors);
    gc.disable_gc_logging();
    gc.print_gc_summary();

    assert_eq!(gc.collection_count(), 1);
    let summary = gc
        .pause_summary()
        .expect("a collection has run, so a summary must exist");
    assert_eq!(summary.young.count, 1);
    assert_eq!(summary.dropped, 0);
}
