use super::*;
use std::time::{Duration, Instant};

/// A heap of 100 regions with no young regions: whatever is not Free is
/// tenured, so both measures read the same story.
fn p(collections: u64, free_regions: usize, old_bytes: usize) -> FullCycleProgress {
    FullCycleProgress {
        collections,
        free_regions,
        old_bytes,
        tenured_regions: 100 - free_regions,
        tenured_bytes: old_bytes,
    }
}


#[test]
fn r22_memory_the_budget_renews_only_on_reclaiming_collections() {
    let t0 = Instant::now();
    let hard = t0 + FORCE_FULL_CYCLE_HARD_CAP;
    let mut deadline = t0 + FORCE_FULL_CYCLE_BUDGET;
    let mut seen = p(10, 4, 50 << 20);
    // Before the deadline: go on, the marks are not read.
    assert!(!force_full_cycle_out_of_time(t0, &mut deadline, hard, &mut seen, || panic!("read"), true));
    assert_eq!(seen, p(10, 4, 50 << 20));
    // Past it, a collection that freed a region: renewed from `now`.
    let late = t0 + FORCE_FULL_CYCLE_BUDGET + Duration::from_millis(1);
    assert!(!force_full_cycle_out_of_time(late, &mut deadline, hard, &mut seen, || p(11, 5, 50 << 20), true));
    assert_eq!(seen, p(11, 5, 50 << 20));
    assert!(deadline > late);
    // Past the renewed deadline: a collection that shrank Old renews too.
    let later = deadline + Duration::from_millis(1);
    assert!(!force_full_cycle_out_of_time(later, &mut deadline, hard, &mut seen, || p(12, 5, 49 << 20), true));
    // The review's case: collections complete but reclaim nothing (other threads'
    // futile pauses on a genuinely full heap) -- give up at the fixed budget.
    let later = deadline + Duration::from_millis(1);
    assert!(force_full_cycle_out_of_time(later, &mut deadline, hard, &mut seen, || p(40, 5, 49 << 20), true));
    assert!(force_full_cycle_out_of_time(later, &mut deadline, hard, &mut seen, || p(41, 4, 52 << 20), true));
    // No collection at all: give up, whatever the region numbers say.
    let mut d = t0 + FORCE_FULL_CYCLE_BUDGET;
    let mut s = p(10, 4, 50 << 20);
    assert!(force_full_cycle_out_of_time(late, &mut d, hard, &mut s, || p(10, 9, 1), true));
    // The switch off: the fixed budget, and the marks are not read.
    let mut d = t0 + FORCE_FULL_CYCLE_BUDGET;
    let mut s = p(10, 4, 50 << 20);
    assert!(force_full_cycle_out_of_time(late, &mut d, hard, &mut s, || panic!("read"), false));
    // The hard cap holds even while collections reclaim.
    let mut d = t0 + FORCE_FULL_CYCLE_BUDGET;
    let mut s = p(10, 4, 50 << 20);
    assert!(force_full_cycle_out_of_time(hard, &mut d, hard, &mut s, || p(13, 9, 1), true));    }