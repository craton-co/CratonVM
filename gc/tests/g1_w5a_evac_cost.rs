// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W5-A — the pause-cost model rests on a constant that nothing writes.
//!
//! # The finding
//!
//! `old_cset_copy_budget_ns` prices a candidate old region at
//! `live_bytes * evac_ns_per_byte`. That product is the only thing standing
//! between `max_gc_pause_ms` and a mixed collection set of any size. Its term
//! has exactly one writer — `update_evac_cost` — and that writer is reached
//! only from `mixed_collection` and `mixed_collection_parallel`.
//!
//! A mixed collection needs a completed mark cycle first. So:
//!
//!  * on a young-only run, which is most runs, the term holds its constructor
//!    value `4` for the life of the process; and
//!  * on a run that DOES go mixed, it still holds `4` for the FIRST mixed
//!    pause — the pause with the largest candidate set, nothing retired yet,
//!    and therefore the pause the goal was written for.
//!
//! The second is the sharper statement and it is the one
//! `the_first_mixed_pause_of_a_run_is_priced_by_a_guess` pins.
//!
//! W4-B's probe (`CRATONVM_G1_EVAC_COST_FROM_YOUNG=1`) put the true figure at
//! least 7x higher, and it was still climbing when the run ended.
//!
//! # What these tests pin
//!
//! Both arms of the same record in one process. `CRATONVM_G1_EVAC_COST_REAL`
//! is latched in a `OnceLock` for the life of the process, so a fixture that
//! set an environment variable could only ever observe the arm its test binary
//! started in — which pins today's default rather than the rule. Every
//! assertion below is either a DIFFERENCE between the two arms given an
//! identical record, or a property of the estimator that holds for any input.
//!
//! Three of the seven are about the *shape* of the estimator rather than about
//! this wave's flag, and they are the ones that will still be worth running
//! when the flag is gone: the convergence test
//! (`a_slow_ema_cannot_reach_the_truth_inside_a_run`), the fail-safe test
//! (`a_pause_that_copied_almost_nothing_is_refused_not_clamped`), and the
//! population test (`the_young_term_never_writes_the_old_one`), which is the
//! one an over-correction — "just feed young pauses into `evac_ns_per_byte`",
//! which is precisely what W4-B's probe did — would fail.

use cratonvm_gc::g1::G1PausePhases;
use cratonvm_gc::gc::GcStats;
use cratonvm_gc::{G1CollectionType, G1Collector, G1CollectorConfig};

/// The reproducer's heap: `HumongousChurn 48 6000 512 -Xmx160m --nojit`, a
/// 200 ms goal. Same fixture as `g1_w4b_drain_controllers.rs` so the two
/// lanes' numbers are comparable.
fn collector() -> G1Collector {
    G1Collector::new(G1CollectorConfig {
        heap_size: 160 * 1024 * 1024,
        initial_heap_size: 160 * 1024 * 1024,
        region_size: 1024 * 1024,
        max_gc_pause_ms: 200,
        ..Default::default()
    })
}

/// A productive young pause: 25.5 MB copied, so `update_young_target`'s
/// anti-storm arm (which resets to the ceiling when a pause reclaimed nothing)
/// does not fire and mask the effect under test.
fn productive() -> GcStats {
    GcStats {
        objects_copied: 638_275,
        bytes_copied: 25_531_008,
        bytes_freed: 26_214_400,
    }
}

/// An ordinary young pause: 322 ms wall, of which a 249.6 ms closure — the
/// shape the reproducer's first real pause had. Copying is 25.5 MB
/// (`productive`), so the closure prices at 249_569_000 / 25_531_008 ~= 9
/// ns/byte while the WHOLE PAUSE prices at 322_239_000 / 25_531_008 ~= 12.
/// The two numbers differing is the second half of the finding.
fn an_ordinary_young_pause() -> G1PausePhases {
    G1PausePhases {
        closure_us: 249_569,
        fixup_us: 17_983,
        fixup_regions: 39,
        fixup_bytes: 31_457_280,
        total_regions: 160,
        free_regions: 100,
        surv_regions: 55,
        hum_regions: 5,
        ..Default::default()
    }
}

/// LANE W5-A, the finding itself — a run of ordinary young pauses leaves the
/// mixed copy budget's per-byte term exactly where the constructor put it.
///
/// This is the test that would have caught the defect in wave 1. It asserts a
/// NEGATIVE, which is unusual and is the point: the bug is not that the number
/// is wrong, it is that nothing ever computes it, and a number that is never
/// computed looks exactly like a number that settled.
#[test]
fn the_shipped_term_is_never_written_on_a_young_only_run() {
    let gc = collector();
    let stats = productive();
    for _ in 0..40 {
        gc.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            false, // pre-wave-5 arm
        );
    }
    let (old, old_n, young, young_n, _, price) = gc.evac_cost_state();
    assert_eq!(
        old, 4,
        "forty young pauses and the term the mixed copy budget is built on is \
         still its constructor value"
    );
    assert_eq!(old_n, 0, "and not one of them was a sample");
    assert_eq!(young, 0, "the young term does not exist in this arm either");
    assert_eq!(young_n, 0);
    assert_eq!(
        price, 4,
        "so the price a mixed CSet charges per live byte is a compile-time guess"
    );
}

/// The same forty pauses under the wave-5 model produce a measured price, and
/// a sample count that says so.
#[test]
fn the_wave_five_model_measures_the_term_from_the_pauses_that_happen() {
    let gc = collector();
    let stats = productive();
    for _ in 0..40 {
        gc.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            true,
        );
    }
    let (old, old_n, young, young_n, refused, _) = gc.evac_cost_state();
    // `evac_cost_state().5` reports the price THIS PROCESS will use, which
    // reads the latched flag and is therefore the off arm inside a test binary.
    // The arm under test comes from the parameterised accessor, for the same
    // `OnceLock` reason every `_within` in `g1.rs` exists.
    let price = gc.dbg_old_evac_ns_per_byte(true);
    assert_eq!(
        young_n, 40,
        "every young pause is a sample of young copying"
    );
    assert_eq!(refused, 0, "all forty cleared the minimum-bytes gate");
    // 249_569_000 ns of closure over 25_531_008 bytes.
    assert_eq!(
        young, 9,
        "the measured young copy cost, in ns per live byte"
    );
    assert_eq!(
        old_n, 0,
        "no mixed pause has happened, so the OLD term has no sample"
    );
    assert_eq!(
        old, 4,
        "and the raw field is untouched — the two terms are separate"
    );
    assert_eq!(
        price, young,
        "but the price a mixed CSet would charge is the young measurement, not \
         the constructor guess: this is the bootstrap, and it is the whole \
         value of the change on the first mixed pause of a run"
    );
    assert!(
        price > 2 * 4,
        "and it is more than twice the shipped constant, so a budget built on \
         4 believed it could afford more than twice the copying it can"
    );
}

/// The sharper half of the finding: the defect is not confined to young-only
/// runs. A workload that reaches a mixed phase still prices its FIRST mixed
/// collection set — the largest one — with the constructor value.
#[test]
fn the_first_mixed_pause_of_a_run_is_priced_by_a_guess() {
    let stats = productive();

    // Pre-wave-5. Young pauses happen; the term cannot move; the first mixed
    // pause selects its collection set at 4 ns/byte.
    let legacy = collector();
    for _ in 0..8 {
        legacy.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            false,
        );
    }
    assert_eq!(
        legacy.dbg_old_evac_ns_per_byte(false),
        4,
        "the price at the moment the first mixed CSet is chosen"
    );

    // Wave-5. The same eight pauses have calibrated the young term, and the
    // first mixed selection is priced by a measurement instead.
    let real = collector();
    for _ in 0..8 {
        real.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            true,
        );
    }
    let bootstrapped = real.dbg_old_evac_ns_per_byte(true);
    assert!(
        bootstrapped > legacy.dbg_old_evac_ns_per_byte(false),
        "the first mixed pause of the run is priced higher, so its collection \
         set is smaller, so its pause goal is closer to a bound: {bootstrapped} \
         against 4"
    );

    // And the moment a mixed pause takes a real sample, the bootstrap stops.
    // 400 ms of closure over 25.5 MB — old copying reading more expensive per
    // live byte than young copying, which is the direction the populations are
    // expected to differ in.
    real.dbg_fold_evac_cost(false, 400_000_000, 25_531_008);
    let measured = real.dbg_old_evac_ns_per_byte(true);
    assert_eq!(
        measured, 15,
        "once an old evacuation has been measured, the old measurement is used"
    );
    assert_ne!(
        measured, bootstrapped,
        "the young term is a bootstrap, not a permanent blend: it is replaced, \
         not averaged in, the instant the right population has a sample"
    );
}

/// The population rule. A young pause must never write the OLD term, because
/// the two are not two draws from one distribution — an old CSet region is
/// selected for being mostly garbage, so its live bytes are scattered through
/// a region the copy still steps over, while a young CSet is copied out of
/// contiguous bump-allocated Eden.
///
/// This is the test the tempting over-correction fails. "The term is never
/// written, so write it from young pauses" is exactly what W4-B's probe
/// (`CRATONVM_G1_EVAC_COST_FROM_YOUNG`) did, and it is why that probe's 27
/// ns/byte is not an estimate of anything the budget spends.
#[test]
fn the_young_term_never_writes_the_old_one() {
    let gc = collector();
    let stats = productive();
    let before = gc.evac_cost_state().0;
    for _ in 0..64 {
        gc.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            true,
        );
    }
    let (old, old_n, young, young_n, _, _) = gc.evac_cost_state();
    assert_eq!(
        old, before,
        "sixty-four young pauses moved the OLD term not at all"
    );
    assert_eq!(old_n, 0);
    assert!(
        young > 0 && young_n == 64,
        "while the YOUNG term took every one of them"
    );
}

/// The convergence rule, asserted against the shipped pre-wave-5 writer rather
/// than re-derived: a 1/8 EMA starting from a guess cannot reach a value seven
/// times higher inside the number of samples a real run supplies.
///
/// This is why W4-B's probe read 5, 5, 6, 23, 38 and settled around 27 while
/// still climbing at the last pause — and why 27 is a floor on how wrong 4 is
/// rather than an estimate of how wrong. A mixed PHASE is a handful of pauses;
/// there is no run short of a soak in which the old estimator arrives.
#[test]
fn a_slow_ema_cannot_reach_the_truth_inside_a_run() {
    // The truth: 30 ns per byte, offered the same way every time.
    const TRUTH_NS: u64 = 30 * 1_000_000;
    const BYTES: usize = 1_000_000;

    let slow = collector();
    for _ in 0..5 {
        slow.dbg_update_evac_cost_legacy(TRUTH_NS, BYTES);
    }
    let after_five = slow.evac_cost_state().0;
    assert_eq!(
        after_five, 15,
        "five samples of a 30 ns/byte truth and the 1/8 EMA has covered exactly \
         HALF the distance from the constructor value — and five pauses is the \
         whole of the run W4-B measured, which is why its probe read 5, 5, 6, \
         23, 38 and was still climbing at the last one"
    );
    // And it never arrives, at any number of samples. `(prev * 7 + observed)
    // / 8` in INTEGER arithmetic has a fixed point wherever
    // `observed - prev < 8`: the truncation eats the whole increment. Two
    // hundred identical samples of a 30 ns/byte truth stall at 23 — a
    // permanent 23 % under-estimate, in the under-pricing direction, on top of
    // however far the estimator got from its constructor value.
    //
    // This is a second defect in the same eleven lines and it is worth naming
    // separately from the cold start, because `fold_evac_cost` inherits the
    // same arithmetic: adopting the first sample whole means it STARTS at the
    // truth and drifts by at most the same 7 units, instead of approaching
    // from 4 and stopping short.
    for _ in 0..200 {
        slow.dbg_update_evac_cost_legacy(TRUTH_NS, BYTES);
    }
    assert_eq!(
        slow.evac_cost_state().0,
        23,
        "two hundred identical samples and the integer 1/8 EMA has a fixed \
         point 23 % below the truth it was handed every single time"
    );

    let fast = collector();
    fast.dbg_fold_evac_cost(false, TRUTH_NS, BYTES);
    assert_eq!(
        fast.evac_cost_state().0,
        30,
        "the first REAL sample is adopted whole, because replacing a guess and \
         tracking drift between measurements are different operations"
    );

    // And after the first, it is the same slow EMA as before: the change is to
    // the cold start, not to the smoothing.
    fast.dbg_fold_evac_cost(false, 300 * 1_000_000, BYTES);
    let after_outlier = fast.evac_cost_state().0;
    assert!(
        after_outlier < 70,
        "a 10x outlier moves a warm estimate by an eighth, not to itself: \
         {after_outlier}"
    );
}

/// The counted fail-safe. A pause that copied almost nothing carries no
/// information about cost per byte — its ratio is one cache miss divided by a
/// small number — and under the first-sample-adopted-whole rule such a point
/// would otherwise BECOME the calibration.
///
/// It is refused and counted rather than clamped, and rather than asserted
/// against: a `debug_assert!` here would fire inside a GC pause on any
/// humongous workload, where almost nothing in a young CSet is copyable, and
/// would manufacture a worse bug than it reports.
#[test]
fn a_pause_that_copied_almost_nothing_is_refused_not_clamped() {
    let gc = collector();
    let before = gc.evac_cost_state();

    // 40 ms of closure over 4 KiB: ~10 000 ns/byte, which the clamp would cap
    // at 4096 and the whole-pause estimator would have adopted.
    gc.dbg_fold_evac_cost(true, 40_000_000, 4 * 1024);
    let after = gc.evac_cost_state();

    assert_eq!(after.2, before.2, "the young term did not move");
    assert_eq!(after.3, 0, "and it was not counted as a sample");
    assert_eq!(after.4, before.4 + 1, "it was counted as a REFUSAL");

    // A run whose every pause is refused reports young=0/0 with a large
    // refusal count, which is a complete and honest result: that workload
    // genuinely never copies enough in one pause to measure a per-byte cost.
    for _ in 0..9 {
        gc.dbg_fold_evac_cost(true, 40_000_000, 4 * 1024);
    }
    let (_, _, young, young_n, refused, price) = gc.evac_cost_state();
    assert_eq!((young, young_n), (0, 0));
    assert_eq!(refused, 10);
    assert_eq!(
        price, 4,
        "and with nothing measured, the price falls back to the named default \
         rather than to something invented"
    );
}

/// The wave-5 model must leave the OFF arm alone, byte for byte. The flag is
/// the round's standing requirement and the null arm of every measurement
/// taken over it.
#[test]
fn the_off_arm_is_the_previous_behaviour() {
    let stats = productive();
    let a = collector();
    let b = collector();
    for _ in 0..12 {
        a.dbg_record_collection_costed(
            G1CollectionType::YoungOnly,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            false,
        );
        b.dbg_record_collection_costed(
            G1CollectionType::Mixed,
            322_239,
            &stats,
            an_ordinary_young_pause(),
            true,
            false,
        );
    }
    // Neither collection type writes the term through the record path in the
    // off arm: the young arm is gated behind `CRATONVM_G1_EVAC_COST_FROM_YOUNG`
    // (default off) and the mixed arm takes its sample in the DRIVER, which a
    // synthetic record does not run.
    assert_eq!(a.evac_cost_state(), (4, 0, 0, 0, 0, 4));
    assert_eq!(b.evac_cost_state(), (4, 0, 0, 0, 0, 4));

    // The three sizing EMAs the budget subtracts are unaffected by this lane in
    // either arm — they have their own writers and their own W4-B rule. Pinned
    // here so a later edit to `fold_evac_cost` that reached sideways into them
    // is caught.
    let (fixup, fixup_regions, young_copy, _) = a.dbg_controller_estimates();
    assert_eq!(
        fixup, 17_983_000,
        "fixup_ns_ema is still written by every young pause"
    );
    assert_eq!(fixup_regions, 39);
    assert_eq!(young_copy, 249_569_000);
}
