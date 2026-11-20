use super::*;
use crate::reservation::GRANULE;
use std::time::{Duration, Instant};

fn idle_arena(floor: usize, delay: Duration, accounting: bool) -> Option<Arena> {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    arena.commit_bits()?;
    arena.set_uncommit_policy(floor, true, delay);
    arena.set_readable_give_back(true);
    arena.set_reset_accounting(accounting);
    Some(arena)
}

/// zr z4/un: the uncommitter's pass stamps first, releases a delay later,
/// and reports when the next granule is due.
#[test]
fn zr4un_idle_pass_releases_after_the_delay_only() {
    let Some(mut arena) = idle_arena(0, Duration::from_secs(2), true) else {
        return;
    };
    let t = Instant::now();
    let first = arena.uncommit_idle_pass(t, 0..0, usize::MAX);
    assert_eq!(first.reset + first.decommitted, 0);
    assert_eq!(first.pending, 8);
    assert_eq!(first.next_due, Some(t + Duration::from_secs(2)));
    let later = arena.uncommit_idle_pass(t + Duration::from_secs(2), 0..0, usize::MAX);
    assert_eq!(later.reset + later.decommitted, 8 * GRANULE);
    assert_eq!(arena.net_committed_bytes(), 8 * GRANULE - later.reset);
    // Nothing left: a reset granule is not reset again.
    let again = arena.uncommit_idle_pass(t + Duration::from_secs(9), 0..0, usize::MAX);
    assert_eq!(again, ArenaIdlePass::default());
}

/// zt t10/rl3 (t10/x7 R2): `-XX:ZUncommitDelay=18446744073709551615`
/// (u64::MAX seconds, which HotSpot 25 accepts) made `since + delay`
/// overflow and panic the uncommitter. Such a delay is never due: the
/// pass counts every granule pending and releases nothing.
#[test]
fn zt10rl3_an_unrepresentable_uncommit_delay_is_never_due() {
    let Some(mut arena) = idle_arena(0, Duration::from_secs(u64::MAX), true) else {
        return;
    };
    let reset_before = arena.bytes_reset;
    let t = Instant::now();
    let first = arena.uncommit_idle_pass(t, 0..0, usize::MAX);
    assert_eq!(first.reset + first.decommitted, 0);
    assert_eq!(first.pending, 8);
    assert_eq!(first.next_due, None);
    let later = arena.uncommit_idle_pass(t + Duration::from_secs(3600), 0..0, usize::MAX);
    assert_eq!(later.reset + later.decommitted, 0);
    assert_eq!(later.pending, 8);
    assert_eq!(arena.bytes_reset, reset_before);
    assert_eq!(arena.net_committed_bytes(), 8 * GRANULE);
}

/// zr z4/un: the excluded span (the last slide's forwarding records) and
/// a handed-out granule are never touched; the budget bounds a pass.
#[test]
fn zr4un_idle_pass_skips_exclusion_hand_outs_and_honours_budget() {
    let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
        return;
    };
    let p = arena.alloc(GRANULE, 8).expect("hand-out") as usize;
    let base = arena.base_ptr() as usize;
    assert_eq!(p - base, 0, "the first low hand-out is granule 0");
    let t = Instant::now();
    let one = arena.uncommit_idle_pass(t, 2 * GRANULE..3 * GRANULE, 1);
    assert!(one.budget_hit);
    assert_eq!(one.reset + one.decommitted, GRANULE);
    let rest = arena.uncommit_idle_pass(t, 2 * GRANULE..3 * GRANULE, usize::MAX);
    assert!(!rest.budget_hit);
    // 8 granules - granule 0 (handed out) - granule 2 (excluded) - 1 done.
    assert_eq!(rest.reset + rest.decommitted, 5 * GRANULE);
    assert!(!arena.is_reset_idle(0));
    assert!(!arena.is_reset_idle(2));
}

/// zr z4/un: the pass refuses an arena in decommit mode (a decommitted
/// granule faults on touch, and mutators run beside the pass).
#[test]
fn zr4un_idle_pass_refuses_decommit_mode() {
    let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
        return;
    };
    arena.set_readable_give_back(false);
    let before = arena.committed_bytes();
    assert_eq!(
        arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX),
        ArenaIdlePass::default()
    );
    assert_eq!(arena.committed_bytes(), before);
}

/// zr z4/un: under reset accounting the `-Xms` floor holds against the NET
/// commitment; without it (the `=0` arm) the floor test saw the peak and
/// every granule was reset.
#[test]
fn zr4un_reset_accounting_keeps_the_floor_resident() {
    for accounting in [true, false] {
        let Some(mut arena) = idle_arena(3 * GRANULE, Duration::ZERO, accounting) else {
            return;
        };
        let pass = arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX);
        let reset = pass.reset + pass.decommitted;
        if accounting {
            assert_eq!(reset, 5 * GRANULE);
            assert_eq!(arena.net_committed_bytes(), 3 * GRANULE);
            assert_eq!(arena.reset_idle_bytes(), 5 * GRANULE);
        } else {
            assert_eq!(
                reset,
                8 * GRANULE,
                "the old test never saw the commitment fall"
            );
            assert_eq!(arena.net_committed_bytes(), arena.committed_bytes());
        }
    }
}

/// zr z4/un: committing a reset granule for use counts it again.
#[test]
fn zr4un_a_reused_reset_granule_counts_again() {
    let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
        return;
    };
    let _ = arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX);
    assert_eq!(arena.net_committed_bytes(), 0);
    let _ = arena.alloc(GRANULE + 8, 8).expect("bump");
    assert_eq!(arena.net_committed_bytes(), 2 * GRANULE);
    // The policy change at VM start keeps the ledger.
    arena.set_uncommit_policy(0, true, Duration::ZERO);
    assert_eq!(arena.net_committed_bytes(), 2 * GRANULE);
}
