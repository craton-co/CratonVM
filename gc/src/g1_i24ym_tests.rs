// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i24/ym -- the HotSpot young model's shrink after the 75 ms pause of
//! vi20-ys G `ys20_g_mtc50_def_r1` (`MtChurnProbe 4 60 48`, G1 `-Xmx256m
//! -XX:MaxGCPauseMillis=50`), replayed through the model's own pure
//! predictor functions on that run's `[GC-STAT]` inputs.
//!
//! What the replay pins (report `docs/internal/gc-round-gci-20261011/i24-ym-report.md`):
//!  * the model's arithmetic reproduces the measured shrink (153 after pause
//!    1, 68 after pause 2; the run printed 153 / 69, the one-region gap is
//!    the rset bytes the record does not print);
//!  * the term that shrinks young is HotSpot's own `survivor_base_time_ms`
//!    (`predict_survivor_regions_evac_time`): 19.8 MB of fresh survivors at a
//!    learned 99.9 % survival and ~1.1 ns per byte = 22 ms of a 50 ms goal.
//!    Without it the same record keeps the ceiling;
//!  * with the OFF arm's pause-2 copy price (same binary, PLAB off) the same
//!    model answers 94, not 68: most of the shrink is the price input.
//!
//! A child module of `g1` (see the `#[path]` declaration in `g1.rs`), so the
//! private items are reachable through `super`.
//!
//! Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i24_ym_`.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: usize = 1024 * 1024;
/// `-Xmx256m` at 1 MiB regions: `young_bounds=12/153` in the run.
const MIN: usize = 12;
const MAX: usize = 153;
const GOAL_US: u64 = 50_000;

/// The model's predictors, as `G1Collector` holds them (`yt_*` cells).
#[derive(Default)]
struct Model {
    ps: AtomicU64,
    base: AtomicU64,
    surv_per_region: AtomicU64,
    surv_permille: AtomicU64,
}

/// One young-only record, the fields `teach_young_cost_model` reads.
struct Pause {
    pause_us: u64,
    copied: u64,
    work_us: u64,
    eden_copied: u64,
    cset_regions: u32,
    surv_regions_in: usize,
    surv_bytes_in: u64,
}

impl Model {
    /// `teach_young_cost_model`'s arithmetic (rset bytes 0: not printed).
    fn teach(&self, p: &Pause) {
        yt_ema_into(&self.ps, p.work_us.saturating_mul(1_000_000) / p.copied);
        let ps = self.ps.load(Ordering::Relaxed);
        yt_ema_into(&self.base, young_pause_base_us(p.pause_us, p.copied, ps));
        let (eden_bytes, surv_bytes) = split_young_copied(p.copied, p.eden_copied);
        let eden_regions = young_eden_cset_regions(p.cset_regions, p.surv_regions_in);
        yt_ema_into(&self.surv_per_region, eden_bytes / eden_regions);
        if p.surv_bytes_in > 0 {
            yt_ema_into(
                &self.surv_permille,
                survivor_survival_permille(surv_bytes, p.surv_bytes_in),
            );
        }
    }

    /// `store_predicted_young_target`'s arithmetic (no mixed reserve), with
    /// the survivor term optionally dropped.
    fn target(&self, surv_bytes_last: u64, with_survivor_term: bool) -> usize {
        let ps = self.ps.load(Ordering::Relaxed);
        let region_ns = young_region_cost_ns(self.surv_per_region.load(Ordering::Relaxed), ps);
        let surv_term = if with_survivor_term {
            survivor_copy_us(
                surv_bytes_last,
                self.surv_permille.load(Ordering::Relaxed),
                ps,
            )
        } else {
            0
        };
        hotspot_young_target(
            MIN,
            MAX,
            survivor_equiv_regions(surv_bytes_last, MIB),
            GOAL_US,
            self.base.load(Ordering::Relaxed).saturating_add(surv_term),
            region_ns,
        )
        .min(MAX)
    }
}

/// `ys20_g_mtc50_def_r1` pause 1: 21.6 MB copied, all of it Eden-sourced.
fn pause_1() -> Pause {
    Pause {
        pause_us: 39_542,
        copied: 21_638_200,
        work_us: 22_633 + 1_295,
        eden_copied: 21_638_200,
        cset_regions: 112,
        surv_regions_in: 0,
        surv_bytes_in: 0,
    }
}

/// Pause 2 (tenuring threshold 1): the 21.6 MB of age-1 survivors promoted
/// (`old_gen_bytes=21637960`), 19.8 MB of Eden copied into Survivor.
/// `work_us` / `pause_us` are the def run's unless overridden.
fn pause_2(work_us: u64, pause_us: u64) -> Pause {
    Pause {
        pause_us,
        copied: 41_409_336,
        work_us,
        eden_copied: 41_409_336 - 21_637_960,
        cset_regions: 124,
        surv_regions_in: 21,
        surv_bytes_in: 21_638_200,
    }
}

const SURV_AFTER_PAUSE_2: u64 = 41_409_336 - 21_637_960;

#[test]
fn gci_i24_ym_the_model_reproduces_the_mtc50_shrink_from_the_survivor_term() {
    let m = Model::default();
    m.teach(&pause_1());
    assert_eq!(m.target(21_638_200, true), 153, "pause 1 keeps the ceiling");

    m.teach(&pause_2(48_372 + 361, 75_509));
    assert_eq!(m.ps.load(Ordering::Relaxed), 1_113, "ps per byte");
    assert_eq!(m.base.load(Ordering::Relaxed), 17_355, "base us");
    assert_eq!(m.surv_permille.load(Ordering::Relaxed), 999, "survivor survival");
    assert_eq!(
        survivor_copy_us(SURV_AFTER_PAUSE_2, 999, 1_113),
        21_983,
        "HotSpot's survivor evacuation term: 22 ms of the 50 ms goal"
    );
    assert_eq!(m.target(SURV_AFTER_PAUSE_2, true), 68, "the measured shrink (printed 69)");
    assert_eq!(
        m.target(SURV_AFTER_PAUSE_2, false),
        153,
        "without the survivor term the same record keeps the ceiling"
    );
}

#[test]
fn gci_i24_ym_the_off_arms_copy_price_alone_moves_the_target_to_94() {
    // Same records, but pause 2 at `ys20_g_mtc50_off_r1`'s closure + rset
    // (27679 + 1064 us) and pause (45222 us): the model, unchanged, answers 94.
    let m = Model::default();
    m.teach(&pause_1());
    m.teach(&pause_2(27_679 + 1_064, 45_222));
    assert_eq!(m.ps.load(Ordering::Relaxed), 1_053);
    assert_eq!(m.target(SURV_AFTER_PAUSE_2, true), 94);
}
