use super::*;

const MB: usize = 1024 * 1024;

/// g1ep7: the phase drains the cleanup's candidates in at most the count
/// target's pauses, and always has one.
#[test]
fn g1ep7_the_mixed_phase_runs_at_most_the_count_target_pauses() {
    assert_eq!(mixed_phase_pause_bound(106, 8), 8);
    assert_eq!(mixed_phase_pause_bound(123, 8), 8);
    assert_eq!(mixed_phase_pause_bound(9, 8), 5);
    assert_eq!(mixed_phase_pause_bound(8, 8), 8);
    assert_eq!(mixed_phase_pause_bound(3, 8), 3);
    assert_eq!(mixed_phase_pause_bound(1, 8), 1);
    assert_eq!(mixed_phase_pause_bound(0, 8), 1);
    assert_eq!(mixed_phase_pause_bound(50, 0), 1);
    for c in 1..400 {
        let bound = mixed_phase_pause_bound(c, 8) as usize;
        assert!(bound <= 8, "{c}");
        assert!(bound * min_old_regions(c, 8) >= c, "{c}: the phase can take every candidate");
    }
}

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
