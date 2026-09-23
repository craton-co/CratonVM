// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W4-B — a drain pass was teaching four adaptive controllers as if it
//! were a young pause.
//!
//! # What W3-C left here
//!
//! `w3c-instrumentation-audit.md` §3 split the evacuation-failure drain pass
//! out of the pause PERCENTILES (`G1PausePhases::drain_pass`) and routed the
//! rest to this lane verbatim:
//!
//! > **NOT fixed, and outside this lane:** a drain pass also feeds
//! > `fixup_ns_ema` and `young_copy_ns_ema` — the inputs to lane E's
//! > pause-cost model and the young sizing controller — as if it were a young
//! > pause. Left alone deliberately: those are sizing decisions, not
//! > reporting, and changing what the controller learns from is a behaviour
//! > change that needs its own measurement.
//!
//! The sweep found two more beyond the two named: `update_young_target` (the
//! sharpest of the four, because it is unsmoothed — one record over the goal
//! takes 20 % off the young target on the spot) and `gc_overhead_ppm`, whose
//! `pause / (pause + mutator)` quantity is ~1 000 000 ppm BY CONSTRUCTION for
//! a record that ends inside the pause which closed the previous interval.
//!
//! `evac_ns_per_byte` was checked and is NOT reached: `update_evac_cost` has
//! exactly two callers, both mixed drivers, and the drain is neither.
//!
//! # What these tests pin
//!
//! Both arms of the same record, in one process. The flag
//! (`CRATONVM_G1_DRAIN_NOT_A_PAUSE`) is latched in a `OnceLock`, so a fixture
//! that set an environment variable could only ever observe the build it was
//! compiled against — which would pin today's behaviour rather than the rule.
//! `dbg_record_collection` takes the rule as a parameter for exactly that
//! reason, and every assertion below is a DIFFERENCE between the two arms
//! given identical input, not an absolute number.
//!
//! The three things a drain pass is still allowed to teach are pinned too, in
//! `a_drain_pass_still_feeds_the_three_inputs_that_are_not_estimators`. A
//! change that silences a drain pass everywhere would pass the first four
//! tests and fail that one, which is the point of its existing.

use cratonvm_gc::g1::{
    drain_controller_samples_refused, reset_drain_controller_samples_refused, G1PausePhases,
};
use cratonvm_gc::gc::GcStats;
use cratonvm_gc::{G1CollectionType, G1Collector, G1CollectorConfig};
use std::sync::Mutex;

/// Serialises against the process-global `DRAIN_CONTROLLER_SAMPLES_REFUSED`.
/// `#[test]` functions in one binary run on parallel threads and would reset
/// each other's baseline — the trap `g1_w3c_drain_pass_is_not_a_pause.rs`
/// documents and paid for.
static SERIAL: Mutex<()> = Mutex::new(());

/// A collector whose pause goal is 200 ms, matching the reproducer
/// (`HumongousChurn 48 6000 512 -Xmx160m --nojit`), so the numbers in the
/// assertions below are the numbers in `w4b-a-drain-pass-is-not-a-pause.md`.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 160 * 1024 * 1024,
        initial_heap_size: 160 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 200,
        ..Default::default()
    })
}

/// A productive record: `update_young_target`'s anti-storm arm resets the
/// target to its ceiling when a pause reclaimed NOTHING, which would mask the
/// effect under test.
fn productive() -> GcStats {
    GcStats {
        objects_copied: 638_275,
        bytes_copied: 25_531_008,
        bytes_freed: 26_214_400,
    }
}

/// The drain pass the reproducer actually took, as a `G1PausePhases`: 232.6 ms
/// against a 200 ms goal, a 163.2 ms closure, and a 33.7 ms whole-heap fix-up
/// over 60 regions and 61.8 MB — against the containing pause's 39 regions and
/// 31.5 MB, in a record that copied a quarter as much.
fn the_reproducers_drain_pass() -> G1PausePhases {
    let mut p = G1PausePhases {
        drain_pass: true,
        ..Default::default()
    };
    p.closure_us = 163_234;
    p.fixup_us = 33_741;
    p.fixup_regions = 60;
    p.fixup_bytes = 61_818_624;
    p.total_regions = 160;
    p.free_regions = 100;
    p.surv_regions = 55;
    p.hum_regions = 5;
    p
}

/// The same record with the flag flipped, on two collectors that have been
/// brought to the identical starting state.
///
/// Returns `(refusing_arm, teaching_arm)` — the state after the drain record,
/// as `(young_target, fixup_ns_ema, fixup_regions_ema, young_copy_ns_ema)`.
fn both_arms() -> ((usize, u64, u64, u64), (usize, u64, u64, u64)) {
    let warm = |gc: &G1Collector| {
        // Warm the estimates to the values the reproducer's FIRST (real) young
        // pause left them at, so the drain record is folded into a live EMA
        // rather than into a zero — `prev == 0` takes the observation whole,
        // which would flatter the effect.
        gc.dbg_set_cost_estimates(17_983_000, 39, 249_569_000, 4);
    };
    let read = |gc: &G1Collector| {
        let (f, fr, y, _) = gc.dbg_controller_estimates();
        (gc.young_target_regions(), f, fr, y)
    };

    let refusing = collector();
    warm(&refusing);
    let teaching = collector();
    warm(&teaching);
    // Same starting young target on both, whatever the ergonomic picked.
    assert_eq!(
        refusing.young_target_regions(),
        teaching.young_target_regions(),
        "the two arms must start from the same target or the comparison is vacuous"
    );

    let stats = productive();
    refusing.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &stats,
        the_reproducers_drain_pass(),
        true,
    );
    teaching.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &stats,
        the_reproducers_drain_pass(),
        false,
    );
    (read(&refusing), read(&teaching))
}

#[test]
fn a_drain_pass_does_not_resize_the_young_generation() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let start = collector().young_target_regions();
    let (refusing, teaching) = both_arms();

    assert_eq!(
        refusing.0, start,
        "a drain pass must leave the young target where the containing pause \
         put it; it moved from {start} to {}",
        refusing.0
    );
    assert!(
        teaching.0 < refusing.0,
        "the null arm must reproduce the defect, or this test is not measuring \
         anything: target {} (teaching) vs {} (refusing), from {start}",
        teaching.0,
        refusing.0
    );
    // 232.6 ms against a 200 ms goal is an overrun, and the overrun arm is a
    // flat 20 % cut with no smoothing at all. That is the whole reason this is
    // the sharpest of the four.
    assert_eq!(
        teaching.0,
        (start * 4 / 5).max(1),
        "the defect's shape is a 20% cut applied by a record that is not a pause"
    );
}

#[test]
fn a_drain_pass_does_not_teach_the_fixup_cost_model() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (refusing, teaching) = both_arms();

    assert_eq!(
        (refusing.1, refusing.2),
        (17_983_000, 39),
        "`fixup_ns_ema` and its denominator must be exactly what the last real \
         pause left"
    );
    assert!(
        teaching.1 > refusing.1,
        "the null arm must reproduce the defect: fixup_ns_ema {} (teaching) vs \
         {} (refusing)",
        teaching.1,
        refusing.1
    );
    // The direction is the finding. A drain's fix-up is a WHOLE-HEAP walk by
    // default, so it biases the estimate of what an ordinary (narrowed) pause's
    // walk costs UPWARD — and `old_cset_copy_budget_ns` subtracts that estimate
    // from the pause goal, so a mixed collection set is made smaller by a cost
    // that no mixed pause will pay.
    assert!(
        teaching.2 > refusing.2,
        "the whole-heap walk must also inflate the region DENOMINATOR: {} vs {}",
        teaching.2,
        refusing.2
    );
}

#[test]
fn a_drain_pass_does_not_price_the_young_half_of_a_collection_set() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (refusing, teaching) = both_arms();

    assert_eq!(
        refusing.3, 249_569_000,
        "`young_copy_ns_ema` must be exactly what the last real young pause left"
    );
    assert_ne!(
        teaching.3, refusing.3,
        "the null arm must reproduce the defect"
    );
    // Deliberately NOT asserted as a direction. A drain's closure copies only
    // the self-forwarded residue, so its `closure_us` is not biased high or
    // low against a young pause's — it is drawn from a different distribution
    // altogether, which is worse for an estimator than a bias because no
    // correction can recover it. On this record it happens to pull DOWN.
}

#[test]
fn the_refusal_is_counted_so_a_null_result_can_be_told_from_a_null_workload() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset_drain_controller_samples_refused();
    let before = drain_controller_samples_refused();

    let gc = collector();
    let stats = productive();
    // Three records: a real young pause, a drain pass on the refusing arm, and
    // a drain pass on the teaching arm. Only the middle one is a refusal.
    let mut real = G1PausePhases {
        total_regions: 160,
        free_regions: 91,
        ..Default::default()
    };
    real.closure_us = 249_569;
    real.fixup_us = 17_983;
    real.fixup_regions = 39;
    gc.dbg_record_collection(G1CollectionType::YoungOnly, 322_239, &stats, real, true);
    gc.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &stats,
        the_reproducers_drain_pass(),
        true,
    );
    gc.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &stats,
        the_reproducers_drain_pass(),
        false,
    );

    assert_eq!(
        drain_controller_samples_refused() - before,
        1,
        "exactly one of the three records is a drain pass on the refusing arm. \
         This counter is the denominator §7.3 of \
         `orchestrator-wave-1-measurements.md` asks for: without it, a workload \
         that never reaches an evacuation failure is indistinguishable from a \
         flag that does nothing."
    );
}

#[test]
fn a_drain_pass_still_feeds_the_three_inputs_that_are_not_estimators() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // (1) The pause RING still holds the record — nothing is hidden, which is
    //     what lets `[GC-SUMMARY] drain_passes` report it separately.
    // (2) The region CENSUS is still taken, which is what `apply_heap_resize`
    //     reads. W3-C made the drain the fifth path to fill it; refusing the
    //     controllers must not undo that.
    // (3) `collection_count` still counts it, because the `[GC-STAT]` line and
    //     the process pause total are accounting, not estimation.
    let gc = collector();
    let before = gc.collection_count();
    let stats = productive();
    gc.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &stats,
        the_reproducers_drain_pass(),
        true,
    );

    assert_eq!(
        gc.collection_count() - before,
        1,
        "a refused controller sample is still a recorded interval"
    );
    let hist = gc.pause_history_snapshot();
    let drains: Vec<_> = hist.iter().filter(|r| r.phases.drain_pass).collect();
    assert_eq!(drains.len(), 1, "the record must still be in the ring");
    assert_eq!(
        drains[0].phases.total_regions, 160,
        "and must still carry its region census for the sizing policy to read"
    );
    let s = gc
        .pause_summary()
        .expect("a record was made, so there is a summary");
    assert_eq!(
        s.drain_passes, 1,
        "and must still be reported as a drain pass"
    );
    assert_eq!(
        s.young.count, 0,
        "and must still be excluded from the young percentiles (W3-C)"
    );
}
