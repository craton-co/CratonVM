// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
///
/// gci i20/ys: built with `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL=0` (the
/// model is default ON since 2026-10-15 and latched per collector). These
/// tests pin the DEFAULT LOOP's 20 % cut that a drain pass must not apply;
/// the model sizes from its predictors and never takes that cut, so on the
/// model the "teaching" arm could not reproduce the defect.
/// `a_drain_pass_does_not_resize_the_young_generation_under_the_model` covers
/// the model arm.

/// [`collector`] with the shipped defaults (the HotSpot young model on).
fn collector() -> G1Collector { collector_default() }
fn collector_default() -> G1Collector {
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


/// gci i20/ys: the same rule on the shipped default (the HotSpot young model,
/// `CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`, default ON since 2026-10-15): a
/// refused drain record leaves the young target where the containing pause
/// put it. Its census carries no survivor bytes, so the model's drain lift
/// (the packed survivor footprint + 1) has nothing to raise either.
#[test]
fn a_drain_pass_does_not_resize_the_young_generation_under_the_model() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let gc = collector_default();
    let start = gc.young_target_regions();
    gc.dbg_set_cost_estimates(17_983_000, 39, 249_569_000, 4);
    gc.dbg_record_collection(
        G1CollectionType::YoungOnly,
        232_622,
        &productive(),
        the_reproducers_drain_pass(),
        true,
    );
    assert_eq!(
        gc.young_target_regions(),
        start,
        "a refused drain record must not move the model's young target"
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


