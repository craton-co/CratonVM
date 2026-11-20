use super::*;
use std::collections::HashSet;

#[test]
fn a_dead_session_frees_its_blocks_and_a_live_one_keeps_them() {
    const VM: usize = 0x00F7_1401;
    forget_vm_ffm_singletons(VM);
    assert!(auto_arena_push(VM, 7, 100, 1, true));
    assert!(
        auto_arena_push(VM, 7, 100, 2, false),
        "same row, no new one"
    );
    assert!(
        auto_arena_push(VM, 7, 200, 3, true),
        "a hash collision files a second row"
    );
    assert!(auto_arena_push(VM, 9, 300, 4, true));
    let mut handles = auto_arena_snapshot(VM, None);
    handles.sort_unstable();
    assert_eq!(handles, vec![100, 200, 300]);

    let dead: HashSet<usize> = [100, 300].into_iter().collect();
    let (mut released, mut ids) = auto_arena_take_dead(VM, &dead);
    released.sort_unstable();
    ids.sort_unstable();
    assert_eq!(released, vec![100, 300]);
    assert_eq!(ids, vec![1, 2, 4]);
    assert_eq!(
        auto_arena_snapshot(VM, None),
        vec![200],
        "the live row stays"
    );
    assert_eq!(auto_arena_bucket_handles(VM, 9), Vec::<usize>::new());
    forget_vm_ffm_singletons(VM);
    assert!(auto_arena_snapshot(VM, None).is_empty());
}

/// Round 14 wave 4 (lane review4): one sweeper at a time. A second claim
/// while the first sweep is between its claim and its take fails, so its
/// stale dead set can never meet a handle the first one released and a
/// new live row re-filed.
#[test]
fn a_second_sweeper_is_refused_until_the_first_takes_its_dead() {
    const VM: usize = 0x00F7_1404;
    forget_vm_ffm_singletons(VM);
    assert!(!auto_arena_claim_sweep(VM), "no table, nothing to sweep");
    assert!(auto_arena_push(VM, 3, 30, 31, true));
    assert!(auto_arena_claim_sweep(VM));
    assert!(!auto_arena_claim_sweep(VM), "a sweep is running");
    let _ = auto_arena_snapshot(VM, None);
    let dead: HashSet<usize> = [30].into_iter().collect();
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert_eq!(released, vec![30usize]);
    assert_eq!(ids, vec![31i64]);
    // The first sweeper released handle 30; a new session's row reusing
    // that handle is live, and only a fresh claim may judge it.
    assert!(auto_arena_push(VM, 3, 30, 32, true));
    assert!(auto_arena_claim_sweep(VM), "the take ended the sweep");
    let (released, ids) = auto_arena_take_dead(VM, &HashSet::new());
    assert!(released.is_empty() && ids.is_empty());
    forget_vm_ffm_singletons(VM);
}

#[test]
fn a_kept_row_is_released_but_never_freed() {
    const VM: usize = 0x00F7_1402;
    forget_vm_ffm_singletons(VM);
    assert!(auto_arena_push(VM, 5, 10, 11, true));
    assert!(auto_arena_push(VM, 5, 20, 21, true));
    auto_arena_mark_kept(VM, 5, &[10]);
    let dead: HashSet<usize> = [10, 20].into_iter().collect();
    let (mut released, ids) = auto_arena_take_dead(VM, &dead);
    released.sort_unstable();
    assert_eq!(released, vec![10, 20], "both handles are released");
    assert_eq!(ids, vec![21], "the kept row's block is not freed");
    forget_vm_ffm_singletons(VM);
}

#[test]
fn a_vanished_row_is_not_refiled_and_the_sweep_is_amortised() {
    const VM: usize = 0x00F7_1403;
    forget_vm_ffm_singletons(VM);
    assert!(!auto_arena_push(VM, 1, 42, 1, false), "no row to append to");
    assert!(auto_arena_bucket_handles(VM, 1).is_empty());
    assert!(!auto_arena_sweep_due(VM, None, 0));
    for i in 0..AUTO_ARENA_SWEEP_MIN {
        assert!(auto_arena_push(VM, i as i32, 1000 + i, i as i64, true));
    }
    assert!(auto_arena_sweep_due(VM, None, 0));
    assert_eq!(auto_arena_snapshot(VM, None).len(), AUTO_ARENA_SWEEP_MIN);
    assert!(
        !auto_arena_sweep_due(VM, None, 0),
        "the snapshot starts a new period"
    );
    // Nothing died: every row survives, so the next sweep waits for as
    // many new rows as there are survivors.
    let (released, ids) = auto_arena_take_dead(VM, &HashSet::new());
    assert!(released.is_empty() && ids.is_empty());
    for i in 0..AUTO_ARENA_SWEEP_MIN {
        assert!(auto_arena_push(VM, -1, 5000 + i, 0, true));
    }
    assert!(auto_arena_sweep_due(VM, None, 0));
    forget_vm_ffm_singletons(VM);
}
