//! zt t2/al (`zgc-zt1sp-a-refused-commit-on-a-free-list-fit-drops-the-block-FIXED-20261019.md`):
//! a free-list fit whose commit is refused puts its span back on its
//! list (ZGC's `CRATONVM_ZGC_REFUSED_FIT_RELISTS`, default ON); with the
//! switch off (the kill control) the span is in no list afterwards.
use super::*;
use std::time::Duration;

/// A spent low end whose only free block is a 4 KiB span, with a ZGC-style
/// uncommit policy and every commit refused from here on.
fn full_with_one_span_refusing(relist: bool) -> Arena {
    let mut arena = Arena::new(64 * 1024);
    let cap = arena.capacity();
    assert!(arena.alloc(cap, 8).is_some());
    arena.add_free_block(8192, 4096);
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    arena.set_refused_fit_relists(relist);
    arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = true;
    arena
}

#[test]
fn a_refused_low_fit_puts_its_span_back() {
    let mut arena = full_with_one_span_refusing(true);
    assert!(arena.alloc(64, 8).is_none(), "every commit is refused");
    assert_eq!(arena.free_list_bytes(), 4096, "no byte left the free list");
    assert!(arena.refused_fits_relisted() >= 1, "the relist engaged");
    // The bytes are still allocatable once the OS commits again.
    arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = false;
    let base = arena.base_ptr() as usize;
    let p = arena.alloc(64, 8).expect("served once commits succeed") as usize - base;
    assert!((8192..8192 + 4096).contains(&p), "from the relisted span");
}

/// The kill control: off, the refused fit's bytes are lost.
#[test]
fn off_a_refused_low_fit_drops_its_span() {
    let mut arena = full_with_one_span_refusing(false);
    assert!(arena.alloc(64, 8).is_none());
    assert!(arena.free_list_bytes() < 4096, "the old drop");
    assert_eq!(arena.refused_fits_relisted(), 0);
}

/// The high end's last resort (`alloc`'s small-object arm through
/// `high_fit`) re-lists on the high list.
#[test]
fn a_refused_high_fit_puts_its_span_back_on_the_high_list() {
    for relist in [true, false] {
        let mut arena = Arena::new(8192);
        let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
        arena.add_free_block(big, 4096);
        arena.alloc(2048, 8).unwrap();
        arena.alloc(2048, 8).unwrap();
        assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        arena.set_refused_fit_relists(relist);
        arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = true;
        assert!(arena.alloc(64, 8).is_none());
        let bytes = arena.high_free_shape().1;
        if relist {
            assert_eq!(bytes, 4096, "re-listed on the high end");
        } else {
            assert_eq!(bytes, 4096 - 64, "off: the old drop");
        }
    }
}

/// No policy (every collector but ZGC): the setter is a no-op and nothing
/// is counted.
#[test]
fn an_arena_without_a_policy_never_relists() {
    let mut arena = Arena::new(64 * 1024);
    arena.set_refused_fit_relists(true);
    assert!(arena.uncommit_policy.is_none());
    assert_eq!(arena.refused_fits_relisted(), 0);
}

/// zt t6/rv3 (`zgc-zt3x2-*` item 2): a refused commit leaves the span's
/// idle stamps alone, so the re-listed span is still stamped for the
/// uncommitter. The positive control is the same call with commits
/// allowed, which clears the stamp. The refusal sits where the OS's
/// answer sits, so a clearing moved back above the commit fails here.
#[test]
fn a_refused_commit_keeps_the_idle_stamps() {
    let mut arena = Arena::new(64 * 1024);
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    let stamp = std::time::Instant::now();
    let g = 8192 / crate::reservation::GRANULE;
    {
        let policy = arena.uncommit_policy.as_mut().unwrap();
        assert!(!policy.idle_since.is_empty(), "stamping is armed");
        policy.idle_since[g] = Some(stamp);
        policy.refuse_commits_for_test = true;
    }
    assert!(!arena.commit_range_for_use(8192, 4096), "refused");
    assert_eq!(
        arena.uncommit_policy.as_ref().unwrap().idle_since[g],
        Some(stamp),
        "a refused commit must not clear the stamp"
    );
    arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = false;
    assert!(arena.commit_range_for_use(8192, 4096), "committed");
    assert_eq!(
        arena.uncommit_policy.as_ref().unwrap().idle_since[g],
        None,
        "positive control: a granted commit clears it"
    );
}

/// The choice survives a later policy change, as the stamp choice does.
#[test]
fn the_relist_choice_survives_a_policy_change() {
    let mut arena = Arena::new(64 * 1024);
    arena.set_uncommit_policy(0, true, Duration::from_secs(300));
    arena.set_refused_fit_relists(true);
    arena.set_uncommit_policy(4096, false, Duration::ZERO);
    assert!(arena.uncommit_policy.as_ref().unwrap().refused_fit_relists);
}
