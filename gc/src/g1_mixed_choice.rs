// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gci i10/rh -- HotSpot 25 G1's mixed-collection choice, as pure functions.
//!
//! Findings page:
//! `docs/internal/gc/gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-FIXED-20261016.md`
//! (items 1, 2, 3 and 10). Every function here is the arithmetic of one
//! HotSpot rule, free of the region table, so that each rule is unit-tested
//! once and the three copies of the old-CSet selector in `g1.rs`
//! (`select_old_regions_for_mixed_gc_within`, the serial `mixed_collection`
//! and `mixed_collection_parallel`) can call ONE implementation instead of
//! drifting apart (item 3). gci i11/gm: they do -- all three now go through
//! `G1Collector::choose_old_cset`, whose take is [`old_cset_take`].
//!
//! Nothing in this module reads a switch or a process global. The callers in
//! `g1.rs` decide whether the HotSpot rule or the legacy rule applies
//! (`CRATONVM_G1_MIXED_HOTSPOT_CHOICE`, default ON since 2026-10-17; read once per collector in
//! `G1Collector::new`, so no process global is added).
//!
//! The HotSpot sources each function mirrors are named on the function. They
//! are cited from `src/hotspot/share/gc/g1/` of JDK 25.

/// HotSpot's live estimate of an Old region after marking:
/// `G1HeapRegion::live_bytes() == used() - garbage_bytes()`, where
/// `garbage_bytes` is fixed when marking finishes (`used_at_remark -
/// marked_bytes`) and does NOT change while the region keeps receiving
/// promoted objects afterwards.
///
/// This collector stores the marked bytes (`G1Region::live_bytes`) and nothing
/// else, so every byte promoted into a marked region after cleanup is read as
/// GARBAGE by the mixed selectors, the waste floor and the copy-cost estimate
/// (finding 1). Given the cursor at cleanup, the HotSpot estimate is
/// `cursor - (cursor_at_cleanup - live_at_cleanup)`.
///
/// * `cursor` -- the region's allocation cursor now.
/// * `live_at_cleanup` -- the marked live bytes cleanup stored.
/// * `cursor_at_cleanup` -- the cursor when cleanup stored them.
///
/// Saturating everywhere: a cursor never falls below its cleanup value while
/// the region keeps its contents (a reset clears both), and a clamped
/// `live_at_cleanup > cursor_at_cleanup` (cleanup clamps it) yields zero
/// garbage, i.e. the estimate is the whole cursor -- the "retain" direction.
#[inline]
pub fn live_estimate(cursor: usize, live_at_cleanup: usize, cursor_at_cleanup: usize) -> usize {
    let garbage = cursor_at_cleanup.saturating_sub(live_at_cleanup);
    cursor.saturating_sub(garbage)
}

/// HotSpot's reclaimable bytes of a candidate: its garbage as fixed at the end
/// of marking (`G1HeapRegion::garbage_bytes()`), independent of later
/// promotion into the region. The legacy reading here was `cursor -
/// live_bytes`, which counts every promoted byte as reclaimable.
#[inline]
pub fn reclaimable_bytes(live_at_cleanup: usize, cursor_at_cleanup: usize) -> usize {
    cursor_at_cleanup.saturating_sub(live_at_cleanup)
}

/// HotSpot `G1Policy::calc_min_old_cset_length`: the fewest old regions one
/// mixed pause takes, `ceil(candidates / G1MixedGCCountTarget)`, so that the
/// candidate list built at the end of marking is drained in about
/// `G1MixedGCCountTarget` mixed pauses. HotSpot takes these regions even when
/// the pause-time prediction says they do not fit (it logs them as
/// "expensive").
///
/// A `count_target` of 0 is treated as 1 (HotSpot's flag range starts at 0
/// and the division would trap; every region then goes in one pause, which
/// is what "no spreading" means).
#[inline]
pub fn min_old_regions(candidates_at_cleanup: usize, count_target: u8) -> usize {
    candidates_at_cleanup.div_ceil((count_target as usize).max(1))
}

/// HotSpot `G1Policy::calc_max_old_cset_length`: the most old regions one
/// mixed pause takes, `ceil(num_regions * G1OldCSetRegionThresholdPercent /
/// 100)`. ROUNDED UP ("to be conservative", HotSpot's comment); the three
/// selectors here round DOWN and then floor at one, which differs whenever
/// `num_regions * percent` is not a multiple of 100 (64 regions at 10%: 6
/// here, 7 on HotSpot).
#[inline]
pub fn max_old_regions(num_regions: usize, threshold_percent: u8) -> usize {
    num_regions
        .saturating_mul(threshold_percent as usize)
        .div_ceil(100)
        .max(1)
}

/// How many of `costs` (per-candidate predicted copy cost in ns, already in
/// preference order) one mixed pause takes -- HotSpot
/// `G1CollectionSet::select_candidates_from_marking`, without its optional
/// (incremental) part:
///
/// 1. never more than `max_old`;
/// 2. the first `min_old` are taken whatever they cost;
/// 3. after that a region is taken only while the running cost including it
///    stays within `budget_ns`; the first one that does not fit ends the
///    selection.
///
/// At least one candidate is taken when any exists (the legacy selectors'
/// forward-progress rule; HotSpot's `min_old` is at least one whenever a
/// candidate exists, so the two agree).
///
/// Returns `(taken, budget_capped)`: `budget_capped` is true iff the budget,
/// not `max_old` or the end of the list, ended the selection -- the census
/// `mixed_cset_capped_budget` counts exactly that.
pub fn old_cset_take(
    costs: &[u64],
    min_old: usize,
    max_old: usize,
    budget_ns: u64,
) -> (usize, bool) {
    let limit = costs.len().min(max_old.max(1));
    let floor = min_old.max(1).min(limit);
    let mut spent: u64 = 0;
    for (i, &cost) in costs.iter().take(limit).enumerate() {
        let next = spent.saturating_add(cost);
        if i >= floor && next > budget_ns {
            return (i, true);
        }
        spent = next;
    }
    (limit, false)
}

/// HotSpot `G1Policy::next_gc_should_be_mixed`'s waste test: the mixed phase
/// goes on only while the candidates still hold MORE than
/// `G1HeapWastePercent` of the heap capacity in reclaimable bytes
/// (`reclaimable_percent <= threshold` ends it, so the comparison is strict).
/// HotSpot divides by the CURRENT capacity (`G1CollectedHeap::capacity()`,
/// committed), not by the maximum heap.
///
/// The legacy floor here is `reclaimable * 100 >= heap_size * waste`:
/// non-strict, and against `-Xmx` rather than the committed heap (finding 10).
#[inline]
pub fn waste_allows_mixed(reclaimable: usize, capacity: usize, waste_percent: u8) -> bool {
    (reclaimable as u128) * 100 > (capacity as u128) * (waste_percent as u128)
}

/// HotSpot's end-of-phase rule as a whole (`next_gc_should_be_mixed`): there
/// is at least one candidate left, and the waste test passes. HotSpot has no
/// pause COUNT that ends the phase; `G1MixedGCCountTarget` only sets the
/// per-pause minimum ([`min_old_regions`]). The legacy phase here ends after
/// exactly `mixed_gc_count_target` mixed pauses whatever is left (finding 2).
#[inline]
pub fn mixed_phase_continues(
    candidates_left: usize,
    reclaimable: usize,
    capacity: usize,
    waste_percent: u8,
) -> bool {
    candidates_left > 0 && waste_allows_mixed(reclaimable, capacity, waste_percent)
}

/// gci i17/mx -- is one mixed pause PRODUCTIVE for the futile-copy breaker
/// (`G1Collector::note_mixed_productivity`)?
///
/// `bytes_freed` is what the pause handed back (the collection set's used
/// bytes plus any eager humongous reclaim); `bytes_copied` is what it moved.
/// The breaker's legacy bar is `freed >= legacy_ratio_pct % of copied`
/// (150 %), set from W6-M's futile loop (`freed == copied`, the whole
/// distribution at or under 100 %). That bar contradicts the candidate rule
/// the same collector applies: an Old region under
/// `G1MixedGCLiveThresholdPercent` (85) live is a HotSpot candidate, and a
/// pause that evacuates regions 67-85 % live frees `1 / live` of what it
/// copies -- 118-150 % -- so it read "futile" although it reclaims exactly
/// what HotSpot's mixed pause reclaims. Four of them in a row trip the
/// breaker, and since a refusal ends the mixed phase the breaker is then
/// polled once per marking cycle, so one mixed phase in 32 CYCLES survives:
/// the old generation of `GciGo11G1NamesProbe mixed` crept 71 -> 92 MB at
/// -Xmx96m with no mixed pause after the first phase (i15a: exactly four mixed
/// pauses, GC(15)-(18), then none for ~30 cycles).
///
/// So a pause is also productive when it freed MORE than the live threshold
/// says a candidate-only pause must: `freed * live_threshold_pct > copied *
/// 100` (strict, so `freed == copied` -- the futile loop's exact signature --
/// stays futile at any threshold up to 100). The legacy bar still counts, so
/// the rule only widens what is productive; a threshold of 0 (no region is
/// ever a candidate) adds nothing. HotSpot itself has no such breaker.
///
/// Callers handle `bytes_copied == 0` (not a sample) before asking.
#[inline]
pub fn mixed_pause_productive(
    bytes_copied: usize,
    bytes_freed: usize,
    legacy_ratio_pct: u64,
    live_threshold_pct: u8,
) -> bool {
    let copied = bytes_copied as u128;
    let freed = bytes_freed as u128;
    if freed * 100 >= copied * legacy_ratio_pct as u128 {
        return true;
    }
    let threshold = live_threshold_pct.min(100) as u128;
    threshold > 0 && freed * threshold > copied * 100
}

/// gci i17/mx -- the live-threshold half of [`mixed_pause_productive`] as a
/// percentage (`freed / copied` must EXCEED it), for the census line:
/// `floor(10000 / threshold)`, 117 at the default 85. 0 when the threshold is
/// 0 (that half is off).
#[inline]
pub fn live_threshold_ratio_pct(live_threshold_pct: u8) -> u64 {
    let threshold = live_threshold_pct.min(100) as u64;
    if threshold == 0 {
        0
    } else {
        10_000 / threshold
    }
}

/// gci i18/mx -- is a (non-drain) mixed-driver pause a sample of old-to-old
/// copying for the futile breaker? Not when it failed evacuation: the regions
/// it kept are left out of its `bytes_freed` (`free_or_keep_cset` counts only
/// the regions it resets), so it reads futile by construction, like the drain
/// pass that follows it. Not when it was a compaction (a full compaction or an
/// allocation-failure plan): its old half is chosen by the Free pool or the
/// target window, not by the mixed screens the breaker judges -- the last-ditch
/// `92M->92M` compaction of the 96m probe counted as a futile mixed pause.
#[inline]
pub fn is_futile_breaker_sample(evacuation_failed: bool, compaction: bool) -> bool {
    !evacuation_failed && !compaction
}

/// gci i18/mx (`CRATONVM_G1_MIXED_FUTILE_RESET_PER_CYCLE`, default ON since
/// 2026-10-15, `=0` kills) -- does
/// the breaker's streak restart at this poll? Only on the switch arm, and only
/// at the FIRST breaker poll of a mixed phase, i.e. on fresh mark data: the
/// streak judged the previous cycle's candidate set. Without it a trip
/// latches across cycles, because a refused poll ends the phase and the
/// breaker is then consulted once per marking cycle (one phase in about
/// `MIXED_FUTILE_PROBE_EVERY` CYCLES survives). A loop on stale mark data
/// still trips after `MIXED_FUTILE_STREAK_TRIP` pauses in every phase, and a
/// phase is bounded by its pause budget, so the loop stays bounded.
#[inline]
pub fn futile_streak_restarts(reset_per_cycle: bool, first_poll_of_phase: bool) -> bool {
    reset_per_cycle && first_poll_of_phase
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: usize = 1024 * 1024;

    /// gci i17/mx -- a pause over candidates 67-85 % live is productive; the
    /// futile loop's `freed == copied` is not, at any threshold.
    #[test]
    fn gci_i17mx_a_candidate_only_mixed_pause_is_productive() {
        // 80 % live regions: freed / copied = 1.25, under the legacy 150 %.
        assert!(mixed_pause_productive(800, 1000, 150, 85));
        // Just over 100 / 85 = 117.6 %: productive; just under: not.
        assert!(mixed_pause_productive(1000, 1177, 150, 85));
        assert!(!mixed_pause_productive(1000, 1176, 150, 85));
        // W6-M's signature, exactly: futile whatever the threshold.
        for t in [0u8, 50, 85, 99, 100, 200] {
            assert!(!mixed_pause_productive(4096, 4096, 150, t), "threshold {t}");
            assert!(!mixed_pause_productive(20 * MB, 20 * MB, 150, t), "threshold {t}");
        }
        // The legacy bar still decides above it.
        assert!(mixed_pause_productive(1000, 1500, 150, 0));
        assert!(mixed_pause_productive(1024, 4096, 150, 85));
        // A low threshold never makes the rule stricter than the legacy bar.
        assert!(mixed_pause_productive(1000, 1600, 150, 50));
        assert!(!mixed_pause_productive(1000, 1400, 150, 50));
        // No overflow at the extremes.
        assert!(!mixed_pause_productive(usize::MAX, usize::MAX, 150, 85));
        assert!(mixed_pause_productive(1, usize::MAX, 150, 85));
    }

    #[test]
    fn gci_i18mx_only_a_clean_non_compaction_pause_is_a_breaker_sample() {
        assert!(is_futile_breaker_sample(false, false));
        assert!(!is_futile_breaker_sample(true, false), "evacuation failure");
        assert!(!is_futile_breaker_sample(false, true), "compaction");
        assert!(!is_futile_breaker_sample(true, true));
    }

    #[test]
    fn gci_i18mx_the_streak_restarts_only_on_the_switch_at_a_phase_start() {
        assert!(futile_streak_restarts(true, true));
        assert!(!futile_streak_restarts(true, false), "mid-phase polls keep the streak");
        assert!(!futile_streak_restarts(false, true), "default arm: latches as before");
        assert!(!futile_streak_restarts(false, false));
    }

    #[test]
    fn gci_i17mx_the_live_threshold_ratio_for_the_census_line() {
        assert_eq!(live_threshold_ratio_pct(85), 117);
        assert_eq!(live_threshold_ratio_pct(100), 100);
        assert_eq!(live_threshold_ratio_pct(200), 100);
        assert_eq!(live_threshold_ratio_pct(50), 200);
        assert_eq!(live_threshold_ratio_pct(0), 0);
    }

    #[test]
    fn gci_i10rh_live_estimate_counts_bytes_promoted_after_cleanup_as_live() {
        // Cleanup saw 600 KiB used, 150 KiB of it marked live: 450 KiB garbage.
        let (live, at) = (150 * 1024, 600 * 1024);
        // Nothing promoted since: the estimate is the marked bytes.
        assert_eq!(live_estimate(at, live, at), live);
        // 300 KiB promoted since: HotSpot counts them live, garbage stays 450.
        assert_eq!(live_estimate(at + 300 * 1024, live, at), live + 300 * 1024);
        assert_eq!(reclaimable_bytes(live, at), 450 * 1024);
    }

    #[test]
    fn gci_i10rh_live_estimate_saturates_in_the_retain_direction() {
        // A clamped verdict (live above the cursor) means no garbage at all.
        assert_eq!(live_estimate(100, 200, 100), 100);
        assert_eq!(reclaimable_bytes(200, 100), 0);
        // A region reset since (cursor below the cleanup cursor) cannot report
        // more live bytes than it holds.
        assert_eq!(live_estimate(10, 0, MB), 0);
    }

    #[test]
    fn gci_i10rh_min_old_is_the_count_target_ceiling() {
        assert_eq!(min_old_regions(0, 8), 0);
        assert_eq!(min_old_regions(1, 8), 1);
        assert_eq!(min_old_regions(8, 8), 1);
        assert_eq!(min_old_regions(9, 8), 2);
        assert_eq!(min_old_regions(40, 8), 5);
        // count_target 0 behaves as 1: everything in one pause.
        assert_eq!(min_old_regions(40, 0), 40);
    }

    #[test]
    fn gci_i10rh_max_old_rounds_up_like_hotspot() {
        assert_eq!(max_old_regions(64, 10), 7);
        assert_eq!(max_old_regions(256, 10), 26);
        assert_eq!(max_old_regions(200, 10), 20);
        assert_eq!(max_old_regions(4, 10), 1);
        assert_eq!(max_old_regions(0, 10), 1);
    }

    #[test]
    fn gci_i10rh_the_minimum_is_taken_over_budget_then_the_budget_binds() {
        let costs = [100u64, 100, 100, 100, 100];
        // min 3, budget 150: the three go in regardless, the fourth does not fit.
        assert_eq!(old_cset_take(&costs, 3, 10, 150), (3, true));
        // min 1, budget 250: two fit, the third would make 300.
        assert_eq!(old_cset_take(&costs, 1, 10, 250), (2, true));
        // A budget of zero still takes the minimum (forward progress).
        assert_eq!(old_cset_take(&costs, 0, 10, 0), (1, true));
        // max_old caps before the budget does.
        assert_eq!(old_cset_take(&costs, 1, 2, u64::MAX), (2, false));
        // The whole list fits.
        assert_eq!(old_cset_take(&costs, 1, 10, u64::MAX), (5, false));
        // min above max_old is held to max_old.
        assert_eq!(old_cset_take(&costs, 9, 4, 0), (4, false));
        // Nothing to take.
        assert_eq!(old_cset_take(&[], 3, 10, 0), (0, false));
    }

    #[test]
    fn gci_i10rh_legacy_rule_is_old_cset_take_with_a_minimum_of_one() {
        // The legacy loop: `if selected > 0 && spent + cost > budget { break }`.
        let legacy = |costs: &[u64], max_old: usize, budget: u64| {
            let mut spent = 0u64;
            let mut n = 0usize;
            for &c in costs.iter().take(max_old) {
                if n > 0 && spent.saturating_add(c) > budget {
                    break;
                }
                spent = spent.saturating_add(c);
                n += 1;
            }
            n
        };
        let costs = [50u64, 400, 10, 10, 900, 1, 1];
        for max_old in 1..=8 {
            for budget in [0u64, 49, 50, 60, 449, 450, 460, 470, 10_000] {
                assert_eq!(
                    old_cset_take(&costs, 1, max_old, budget).0,
                    legacy(&costs[..], max_old, budget),
                    "max_old={max_old} budget={budget}"
                );
            }
        }
    }

    #[test]
    fn gci_i10rh_the_waste_floor_is_strict_and_against_capacity() {
        // 5% of 100 MiB is 5 MiB: exactly at the threshold ends the phase.
        assert!(!waste_allows_mixed(5 * MB, 100 * MB, 5));
        assert!(waste_allows_mixed(5 * MB + 1, 100 * MB, 5));
        // Against a 64 MiB committed heap the same 4 MiB passes; against a
        // 128 MiB maximum it would not.
        assert!(waste_allows_mixed(4 * MB, 64 * MB, 5));
        assert!(!waste_allows_mixed(4 * MB, 128 * MB, 5));
        // waste 0: any garbage at all continues.
        assert!(waste_allows_mixed(1, 100 * MB, 0));
        assert!(!waste_allows_mixed(0, 100 * MB, 0));
    }

    #[test]
    fn gci_i10rh_the_phase_ends_on_candidates_or_waste_not_on_a_count() {
        assert!(mixed_phase_continues(3, 10 * MB, 100 * MB, 5));
        assert!(!mixed_phase_continues(0, 10 * MB, 100 * MB, 5));
        assert!(!mixed_phase_continues(3, 5 * MB, 100 * MB, 5));
    }

    /// gci i11/gm finding 3 -- the edges `G1Collector::choose_old_cset` relies
    /// on when it hands this function the prices of the first `max_old`
    /// candidates: a minimum above the list takes the whole list without a
    /// budget cap, an empty list takes nothing, and an uncapped full
    /// compaction (`usize::MAX`, `u64::MAX`) takes everything.
    #[test]
    fn gci_i11gm_the_take_edges_the_one_selector_relies_on() {
        assert_eq!(old_cset_take(&[5, 5, 5], 9, 3, 0), (3, false));
        assert_eq!(old_cset_take(&[], 1, 3, 0), (0, false));
        assert_eq!(old_cset_take(&[u64::MAX, 7, 7], 1, usize::MAX, u64::MAX), (3, false));
        // The legacy rule: one region whatever it costs, then the budget.
        assert_eq!(old_cset_take(&[10, 1], 1, 2, 5), (1, true));
    }
}
