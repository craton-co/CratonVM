use super::*;
use crate::reservation::GRANULE;
use std::time::{Duration, Instant};

#[test]
fn zgc_uncommit_policy_delays_and_resets_on_reuse() {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    arena.uncommit_policy.as_mut().unwrap().release_window = true;
    let t = Instant::now();
    assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", t), 0);
    assert_eq!(
        arena.decommit_span_at(0, 8 * GRANULE, "test", t + Duration::from_secs(299)),
        0
    );
    // A relocation destination/hand-out restarts the unused interval.
    assert!(arena.commit_for_relocation(4 * GRANULE, GRANULE));
    let freed = arena.decommit_span_at(0, 8 * GRANULE, "test", t + Duration::from_secs(300));
    if arena.data.commit_bits().is_some() {
        assert_eq!(freed, 7 * GRANULE);
        assert!(arena.data.is_committed_at(4 * GRANULE));
        assert_eq!(
            arena.decommit_span_at(
                4 * GRANULE,
                5 * GRANULE,
                "test",
                t + Duration::from_secs(599)
            ),
            0
        );
        assert_eq!(
            arena.decommit_span_at(
                4 * GRANULE,
                5 * GRANULE,
                "test",
                t + Duration::from_secs(600)
            ),
            GRANULE
        );
    }
}

/// zr z3/fp: the end-of-sweep stamp starts the clock, so the first
/// give-back a delay later releases; without it that pass only stamps.
#[test]
fn zr3fp_a_sweep_stamp_starts_the_uncommit_clock() {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    if arena.data.commit_bits().is_none() {
        return; // wholly-committed fallback store
    }
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    // Middle = the whole arena (nothing bumped), all committed.
    let stamped = arena.stamp_idle_unused();
    assert_eq!(stamped, 8);
    // Already stamped: a second pass keeps the older times.
    assert_eq!(arena.stamp_idle_unused(), 0);
    arena.uncommit_policy.as_mut().unwrap().release_window = true;
    let later = Instant::now() + Duration::from_secs(300);
    assert_eq!(
        arena.decommit_span_at(0, 8 * GRANULE, "test", later),
        8 * GRANULE
    );

    // Control: no stamp, and the same pass only starts the clock.
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    arena.uncommit_policy.as_mut().unwrap().release_window = true;
    assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", later), 0);
    // Off / zero delay: nothing to stamp.
    arena.set_uncommit_policy(0, true, Duration::ZERO);
    assert_eq!(arena.stamp_idle_unused(), 0);
    arena.set_uncommit_policy(0, false, Duration::from_secs(300));
    assert_eq!(arena.stamp_idle_unused(), 0);
}

/// zr z3/fp: a release the `-Xms` floor refused keeps its stamp, so the
/// granule goes back as soon as the floor allows; the old rule restarted
/// its clock.
#[test]
fn zr3fp_a_floor_refused_release_keeps_its_idle_stamp() {
    for keep in [true, false] {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        if arena.data.commit_bits().is_none() {
            return;
        }
        arena.set_uncommit_policy(7 * GRANULE, true, Duration::from_secs(300));
        arena.set_keep_refused_idle_stamps(keep);
        arena.uncommit_policy.as_mut().unwrap().release_window = true;
        let t = Instant::now();
        assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", t), 0);
        // One granule above the floor: granule 0 goes, 1..7 are refused.
        let t300 = t + Duration::from_secs(300);
        assert_eq!(
            arena.decommit_span_at(0, 8 * GRANULE, "test", t300),
            GRANULE
        );
        // Commitment above the floor comes back (a relocation destination).
        assert!(arena.commit_for_relocation(0, GRANULE));
        let t301 = t + Duration::from_secs(301);
        let freed = arena.decommit_span_at(0, 8 * GRANULE, "test", t301);
        if keep {
            assert_eq!(freed, GRANULE, "granule 1 has been idle 301 s");
        } else {
            assert_eq!(freed, 0, "the old rule restarted granule 1's clock");
        }
    }
}

/// zt t23/hunt4 (`-XX:+AlwaysPreTouch`): turning the bit on touches what
/// is committed now (the startup commit), a commit that makes a granule
/// resident touches that whole granule, an already-resident range touches
/// nothing more, the bit survives a policy change, and no byte changes.
/// Off: nothing is touched.
#[test]
fn zt23hunt4_pre_touch_touches_the_startup_commit_and_later_commits() {
    let mut arena = Arena::new(8 * GRANULE);
    if arena.data.commit_bits().is_none() {
        // The wholly-committed fallback store: nothing commits lazily.
        return;
    }
    arena.commit_initial_prefix(2 * GRANULE);
    arena.set_uncommit_policy(2 * GRANULE, true, Duration::from_secs(300));
    // SAFETY: byte 0 is inside the committed prefix.
    unsafe { *arena.data.as_mut_ptr() = 0x5A };
    assert_eq!(arena.set_pre_touch(false), 0);
    assert!(arena.commit_range_for_use(2 * GRANULE, 8));
    assert_eq!(arena.pre_touched_bytes(), 0, "off: no touch");
    // Granules 0..3 are committed now: one coalesced span.
    let committed = arena.committed_bytes();
    assert_eq!(committed, 3 * GRANULE);
    assert_eq!(arena.set_pre_touch(true), committed);
    assert_eq!(arena.pre_touched_bytes(), committed);
    // SAFETY: byte 0 is committed; the touch wrote back what it read.
    assert_eq!(unsafe { *arena.data.as_ptr() }, 0x5A);
    assert!(arena.commit_range_for_use(0, GRANULE));
    assert_eq!(arena.pre_touched_bytes(), committed, "resident: no touch");
    // A policy change keeps the bit; a new granule is touched whole.
    arena.set_uncommit_policy(GRANULE, true, Duration::from_secs(300));
    assert!(arena.commit_range_for_use(5 * GRANULE + 16, 8));
    assert_eq!(arena.pre_touched_bytes(), committed + GRANULE);
}

#[test]
fn zgc_cursor_retraction_defers_unmapping_until_explicit_give_back() {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    arena.set_uncommit_policy(0, true, Duration::ZERO);
    arena.alloc(4 * GRANULE, 8).unwrap();
    let before = arena.committed_bytes();
    arena.retract_cursor_to(0);
    assert_eq!(arena.committed_bytes(), before);
    let released = arena.decommit_unused_above(0);
    if arena.commit_bits().is_some() {
        assert_eq!(released, before);
    }
}

#[test]
fn zgc_startup_commit_covers_both_allocation_ends_without_growing() {
    let mut arena = Arena::new(32 * GRANULE);
    let before = (arena.cursor, arena.high_cursor, arena.capacity());
    assert_eq!(arena.commit_initial_zgc(8 * GRANULE), 8 * GRANULE);
    assert_eq!((arena.cursor, arena.high_cursor, arena.capacity()), before);
    assert!(arena.data.is_committed_at(0));
    assert!(arena.data.is_committed_at(31 * GRANULE));
    if arena.commit_bits().is_some() {
        assert_eq!(arena.committed_bytes(), 8 * GRANULE);
    }
}

#[test]
fn zgc_uncommit_policy_covers_retraction_and_disabled_mode() {
    let mut arena = Arena::new(8 * GRANULE);
    arena.commit_initial_prefix(8 * GRANULE);
    arena.set_uncommit_policy(2 * GRANULE + 1, true, Duration::ZERO);
    arena.decommit_unused_above(0);
    assert!(arena.committed_bytes() >= 3 * GRANULE);
    let _ = arena.commit_initial_prefix(8 * GRANULE);
    arena.set_uncommit_policy(0, false, Duration::ZERO);
    let before = arena.committed_bytes();
    assert_eq!(arena.decommit_unused_above(0), 0);
    assert_eq!(arena.committed_bytes(), before);
}
