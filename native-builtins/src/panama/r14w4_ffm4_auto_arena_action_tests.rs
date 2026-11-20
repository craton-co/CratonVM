use super::*;
use std::collections::HashSet;

#[test]
fn a_row_with_a_pending_action_outlives_its_session_until_the_action_ran() {
    const VM: usize = 0x00F7_1441;
    forget_vm_ffm_singletons(VM);
    auto_arena_file_pending_row(VM, 5, 50);
    assert!(
        auto_arena_push(VM, 5, 50, 51, false),
        "later blocks join the row"
    );
    assert!(auto_arena_add_pending(VM, 5, 50));
    let dead: HashSet<usize> = [50].into_iter().collect();
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert!(released.is_empty() && ids.is_empty(), "two actions pending");
    assert!(!auto_arena_sub_pending(VM, 5, 50), "one still pending");
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert!(released.is_empty() && ids.is_empty(), "one action pending");
    assert!(auto_arena_sub_pending(VM, 5, 50), "the last one ran");
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert_eq!(released, vec![50usize]);
    assert_eq!(ids, vec![51i64]);
    assert!(!auto_arena_add_pending(VM, 5, 50), "the row is gone");
    forget_vm_ffm_singletons(VM);
}

#[test]
fn a_kept_row_is_released_but_never_freed_after_its_last_action() {
    const VM: usize = 0x00F7_1442;
    forget_vm_ffm_singletons(VM);
    auto_arena_file_pending_row(VM, 6, 60);
    assert!(auto_arena_push(VM, 6, 60, 61, false));
    auto_arena_mark_kept(VM, 6, &[60]);
    assert!(!auto_arena_sub_pending(VM, 6, 60), "kept: not free to go");
    let dead: HashSet<usize> = [60].into_iter().collect();
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert_eq!(released, vec![60usize]);
    assert!(ids.is_empty(), "kept: the handle goes, the blocks stay");
    forget_vm_ffm_singletons(VM);
}

/// The mock has no weak handles (`add_weak_global_root` answers 0): the
/// lookup answers `None`, i.e. the caller keeps the old behaviour.
#[test]
fn without_a_weak_handle_the_action_is_not_counted() {
    let mut ctx = crate::test_utils::mock_ctx();
    let session = try_alloc_concurrent_synthetic(&mut ctx, "cratonvm/test/Session", 1).unwrap();
    assert_eq!(auto_arena_action_pending(&mut ctx, session), None);
}
