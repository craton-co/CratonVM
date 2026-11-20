use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::collections::HashSet;

/// The defect FFR2-2 closes: a sweep that claimed first snapshots without
/// the row (its action was pending), so the round-14 path left it to the
/// NEXT sweep. Now the last action takes it; the handle waits for the
/// running sweep's take, which releases it.
#[test]
fn the_last_action_takes_its_row_even_while_a_sweep_runs() {
    const VM: usize = 0x00F7_1531;
    forget_vm_ffm_singletons(VM);
    assert!(auto_arena_push(VM, 5, 50, 51, true));
    assert!(auto_arena_push(VM, 5, 50, 52, false));
    assert!(auto_arena_add_pending(VM, 5, 50));
    assert!(auto_arena_claim_sweep(VM), "a sweep is running");
    assert!(
        auto_arena_snapshot(VM, None).is_empty(),
        "it skipped the pending row"
    );
    let (release, mut ids) = auto_arena_finish_action(VM, 5, 50).expect("switch on");
    ids.sort_unstable();
    assert_eq!(
        release, None,
        "not while a sweep may hold a stale verdict on it"
    );
    assert_eq!(ids, vec![51i64, 52]);
    assert!(
        auto_arena_bucket_handles(VM, 5).is_empty(),
        "the row is gone"
    );
    let (released, freed) = auto_arena_take_dead(VM, &HashSet::new());
    assert_eq!(released, vec![50usize], "the sweep's take releases it");
    assert!(freed.is_empty());
    forget_vm_ffm_singletons(VM);
}

#[test]
fn only_the_last_action_takes_the_row_and_a_kept_row_stays() {
    const VM: usize = 0x00F7_1532;
    forget_vm_ffm_singletons(VM);
    assert!(auto_arena_push(VM, 3, 30, 31, true));
    assert!(auto_arena_add_pending(VM, 3, 30));
    assert!(auto_arena_add_pending(VM, 3, 30));
    assert_eq!(
        auto_arena_finish_action(VM, 3, 30),
        Some((None, Vec::new()))
    );
    assert_eq!(
        auto_arena_bucket_handles(VM, 3),
        vec![30],
        "one still pending"
    );
    assert_eq!(
        auto_arena_finish_action(VM, 3, 30),
        Some((Some(30), vec![31i64]))
    );
    assert!(auto_arena_bucket_handles(VM, 3).is_empty());
    assert_eq!(
        auto_arena_finish_action(VM, 3, 30),
        Some((None, Vec::new())),
        "a second run finds nothing"
    );

    assert!(auto_arena_push(VM, 4, 40, 41, true));
    assert!(auto_arena_add_pending(VM, 4, 40));
    auto_arena_mark_kept(VM, 4, &[40]);
    assert_eq!(
        auto_arena_finish_action(VM, 4, 40),
        Some((None, Vec::new()))
    );
    assert_eq!(
        auto_arena_bucket_handles(VM, 4),
        vec![40],
        "kept: the sweep's"
    );
    let dead: HashSet<usize> = [40].into_iter().collect();
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert_eq!(released, vec![40usize]);
    assert!(ids.is_empty(), "kept: never freed");
    forget_vm_ffm_singletons(VM);
}


/// End to end through `auto_arena_action_done` on a mock whose handle
/// still resolves (the round-14 path would have kept the row): the blocks
/// are freed and the handle released.
#[test]
fn action_done_frees_whether_or_not_the_handle_cleared() {
    const VM: usize = 0x00F7_1534;
    forget_vm_ffm_singletons(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let session = try_alloc_concurrent_synthetic(&mut ctx, "cratonvm/test/Session", 1).unwrap();
    let handle = ctx.add_global_root(session);
    assert_ne!(handle, 0);
    let (id, _ptr) = ctx.allocate_native_memory(16, 8).expect("mock allocation");
    assert!(auto_arena_push(VM, 7, handle, id, true));
    assert!(auto_arena_add_pending(VM, 7, handle));
    assert_eq!(auto_arena_action_done(&mut ctx, 7, handle), 1);
    assert_eq!(ctx.resolve_global_root(handle), None, "released");
    assert!(auto_arena_bucket_handles(VM, 7).is_empty());
    forget_vm_ffm_singletons(VM);
}

/// A closed shared session's row is swept like an auto arena's once its
/// handle cleared; after an address-only view it frees nothing.
#[test]
fn closed_shared_rows_free_when_dead_unless_an_address_only_view_exists() {
    const VM: usize = 0x00F7_1535;
    forget_vm_ffm_singletons(VM);
    auto_arena_file_closed_shared_row(VM, 9, 90, vec![91, 92]);
    auto_arena_file_closed_shared_row(VM, 9, 95, vec![96]);
    let mut handles = auto_arena_snapshot(VM, None);
    handles.sort_unstable();
    assert_eq!(handles, vec![90, 95]);
    let dead: HashSet<usize> = [90].into_iter().collect();
    let (released, mut ids) = auto_arena_take_dead(VM, &dead);
    ids.sort_unstable();
    assert_eq!((released, ids), (vec![90usize], vec![91i64, 92]));
    assert!(!auto_arena_address_only_views(VM));
    auto_arena_note_address_only_view(VM);
    assert!(auto_arena_address_only_views(VM));
    let dead: HashSet<usize> = [95].into_iter().collect();
    let (released, ids) = auto_arena_take_dead(VM, &dead);
    assert_eq!(released, vec![95usize], "the handle goes");
    assert!(
        ids.is_empty(),
        "the blocks stay: a view may still read them"
    );
    forget_vm_ffm_singletons(VM);
}

/// The mock has no weak handles: nothing is filed, the ids stay unfreed
/// (the pre-wave-3 leak), and an empty hand-off is trivially done.
#[test]
fn without_a_weak_handle_the_shared_blocks_are_not_filed() {
    const VM: usize = 0x00F7_1536;
    forget_vm_ffm_singletons(VM);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let session = try_alloc_concurrent_synthetic(&mut ctx, "cratonvm/test/Session", 1).unwrap();
    assert!(shared_close_defer_free(&mut ctx, session, Vec::new()));
    assert!(!shared_close_defer_free(&mut ctx, session, vec![1, 2]));
    assert!(auto_arena_snapshot(VM, None).is_empty());
    forget_vm_ffm_singletons(VM);
}



/// Round 15 wave 5 (lane upcall1; the synthetic `asByteBuffer` UAF): the
/// flag the confined close walk and `pe_arena_close` read is per VM.
#[test]
fn an_address_only_view_is_seen_by_its_vm_only() {
    const VM: usize = 0x00F7_1539;
    forget_vm_ffm_singletons(VM);
    forget_vm_ffm_singletons(VM + 1);
    let mut ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    assert!(!address_only_view_made(&ctx));
    auto_arena_note_address_only_view(VM);
    assert!(address_only_view_made(&ctx));
    ctx.set_vm_identity(VM + 1);
    assert!(!address_only_view_made(&ctx));
    forget_vm_ffm_singletons(VM);
    forget_vm_ffm_singletons(VM + 1);
}
