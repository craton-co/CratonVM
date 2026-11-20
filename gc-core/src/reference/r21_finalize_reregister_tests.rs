//! JIT round 21 wave 16, lane finalize-reinit
//! (`CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`; residual 4R of
//! `docs/feature-designs/generative-gc-architecture.md`): a
//! withdrawn finalizable object registers again at a later successful
//! `<init>` return, exactly once, and a constructed object's row is not
//! withdrawn.
use super::*;

fn processor() -> ReferenceProcessor {
    let mut rp = ReferenceProcessor::new();
    rp.set_reregister_at_init_return(true);
    rp
}

fn active_rows(rp: &ReferenceProcessor, obj: usize) -> usize {
    rp.finalizer_refs
        .iter()
        .filter(|e| e.reference_obj == obj && !e.cleared && !e.enqueued)
        .count()
}

/// Case (a): the first attempt threw before `super(..)` (withdrawn), the
/// retry's `<init>` returned: one row again, and a second return (the
/// outer frame of the retry's chain) adds none.
#[test]
fn a_withdrawn_object_registers_again_once_at_its_init_return() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.withdraw_finalizable(0x1000));
    assert!(!rp.is_registered_finalizable(0x1000));
    assert_eq!(rp.withdrawn_finalizables_len(), 1);
    assert!(rp.note_finalizable_init_returned(0x1000), "re-registered");
    assert!(rp.is_registered_finalizable(0x1000));
    assert_eq!(active_rows(&rp, 0x1000), 1);
    assert_eq!(rp.withdrawn_finalizables_len(), 0);
    assert!(!rp.note_finalizable_init_returned(0x1000), "idempotent");
    assert_eq!(active_rows(&rp, 0x1000), 1, "exactly one Finalizer");
    assert_eq!(active_rows(&rp, 0x2000), 1, "the other object is untouched");
}

/// Nothing changes for an object whose `<init>` never returns: the
/// withdrawal stands, and the entry dies with the object.
#[test]
fn an_object_whose_init_never_returns_stays_withdrawn_and_is_pruned() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(rp.withdraw_finalizable(0x1000));
    assert!(!rp.is_registered_finalizable(0x1000));
    // Wave 18: neither watched (no young-seed retention) nor a root.
    assert!(!rp.all_tracked_addrs().contains(&0x1000), "not watched");
    assert!(!rp.finalizer_referent_addresses().contains(&0x1000), "not a root");
    rp.remove_collected(&|a| a != 0x1000);
    assert_eq!(rp.withdrawn_finalizables_len(), 0, "dropped at the collection's end");
    assert!(!rp.note_finalizable_init_returned(0x1000));
    assert!(!rp.is_registered_finalizable(0x1000));
}

/// Wave 18: a collection between the failed attempt and the retry's
/// return forgets the entry (it is not watched, so no map can carry it):
/// the retry leaves the object unregistered, the pre-switch answer, and
/// an object moved onto the old address is never matched. A live entry
/// survives a collection-free interval, a concurrent sweep that did not
/// free its block included.
#[test]
fn a_collection_forgets_the_entry() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(rp.withdraw_finalizable(0x1000));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x5000);
    rp.update_after_gc(&map);
    assert_eq!(rp.withdrawn_finalizables_len(), 0);
    assert!(!rp.note_finalizable_init_returned(0x1000), "old address");
    assert!(!rp.note_finalizable_init_returned(0x5000), "new address");

    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.withdraw_finalizable(0x2000));
    let freed = |a: usize| a == 0x3000;
    let _ = rp.remove_reference_objects_registered_before(u64::MAX, &freed);
    assert_eq!(rp.withdrawn_finalizables_len(), 1, "a sweep that freed another block keeps it");
    let _ = rp.remove_reference_objects_registered_before(u64::MAX, &|a| a == 0x2000);
    assert_eq!(rp.withdrawn_finalizables_len(), 0, "a sweep that freed its block drops it");
}

/// Case (b): an object whose `<init>` returned is constructed; a later
/// withdrawal (a re-`<init>` that throws before `super(..)`) refuses.
#[test]
fn a_constructed_object_is_not_withdrawn() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(!rp.note_finalizable_init_returned(0x1000), "registered already");
    assert_eq!(active_rows(&rp, 0x1000), 1);
    assert!(!rp.withdraw_finalizable(0x1000), "constructed: kept");
    assert!(rp.is_registered_finalizable(0x1000));
    assert_eq!(rp.withdrawn_finalizables_len(), 0);
    // The door half refuses the same way.
    rp.note_unconstructed_finalizable(0x1000, 3);
    assert!(!rp.take_unconstructed_finalizable(0x1000, 3));
    assert!(rp.is_registered_finalizable(0x1000));
}

/// A re-registered row is a new registration: a later failed attempt on
/// it (before any `<init>` returned on it again) cannot happen in verified
/// code, but the row the return marked is constructed and stays.
#[test]
fn a_reregistered_row_is_constructed() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(rp.withdraw_finalizable(0x1000));
    assert!(rp.note_finalizable_init_returned(0x1000));
    assert!(!rp.withdraw_finalizable(0x1000));
    assert_eq!(active_rows(&rp, 0x1000), 1);
}

/// An enqueued row (finalized already) is a verdict: the return adds no
/// second Finalizer and marks nothing.
#[test]
fn an_enqueued_object_gets_no_second_finalizer() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.finalizer_refs[0].enqueued = true;
    rp.finalizer_active_stale = true;
    assert!(!rp.note_finalizable_init_returned(0x1000));
    assert_eq!(rp.finalizer_refs.len(), 1);
}

/// The list is capped, oldest first, and one entry per address.
#[test]
fn the_withdrawn_list_is_capped_and_deduplicated() {
    let mut rp = processor();
    for i in 0..(WITHDRAWN_FINALIZABLES_MAX + 4) {
        let a = 0x1_0000 + i * 0x100;
        rp.discover_reference(ReferenceType::Finalizer, a, a, None);
        assert!(rp.withdraw_finalizable(a));
    }
    assert_eq!(rp.withdrawn_finalizables_len(), WITHDRAWN_FINALIZABLES_MAX);
    assert!(!rp.note_finalizable_init_returned(0x1_0000), "oldest dropped");
    // The same address withdrawn twice (a new object there) keeps one entry.
    rp.discover_reference(ReferenceType::Finalizer, 0x9000, 0x9000, None);
    assert!(rp.withdraw_finalizable(0x9000));
    rp.discover_reference(ReferenceType::Finalizer, 0x9000, 0x9000, None);
    assert!(rp.withdraw_finalizable(0x9000));
    assert_eq!(
        rp.withdrawn_finalizables
            .iter()
            .filter(|e| e.reference_obj == 0x9000)
            .count(),
        1
    );
}

/// `=0`: the pre-switch answers (no re-registration, a constructed row
/// is withdrawn, nothing kept).
#[test]
fn the_switch_off_restores_the_old_answers() {
    let mut rp = ReferenceProcessor::new();
    rp.set_reregister_at_init_return(false);
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(!rp.note_finalizable_init_returned(0x1000));
    assert!(!rp.finalizer_refs[0].constructed);
    assert!(rp.withdraw_finalizable(0x1000));
    assert_eq!(rp.withdrawn_finalizables_len(), 0);
    assert!(!rp.note_finalizable_init_returned(0x1000));
    assert!(!rp.is_registered_finalizable(0x1000));
}

/// JIT round 21 wave 22 (lane finalize,
/// `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`): a JNI re-`<init>` of
/// a registered object keeps ONE row and counts the extra `Finalizer` on
/// it; each `finalize()` run takes one, and only once the row is enqueued.
#[test]
fn a_reinit_counts_an_extra_finalizer_on_the_one_row() {
    let mut rp = processor();
    assert!(!rp.take_extra_finalizer(0x1000), "nothing noted: no scan");
    assert!(!rp.note_finalizer_registered_again(0x1000), "not registered");
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.note_finalizer_registered_again(0x1000));
    assert!(rp.note_finalizer_registered_again(0x1000), "a third Finalizer");
    assert_eq!(active_rows(&rp, 0x1000), 1, "still one row");
    assert!(!rp.withdraw_finalizable(0x1000), "constructed: kept");
    assert!(!rp.take_extra_finalizer(0x1000), "an active row owes no run yet");
    let _ = rp.process_references(&|_| false, 100, 0);
    assert!(rp.take_extra_finalizer(0x1000));
    assert!(rp.take_extra_finalizer(0x1000));
    assert!(!rp.take_extra_finalizer(0x1000), "two extra runs, no more");
    assert!(!rp.take_extra_finalizer(0x2000), "the other object owes none");
    assert_eq!(rp.extra_finalizers_outstanding(), 0, "wave 23: nothing left to scan for");
}

/// Wave 23 (item 4 of `r21-review-w23-findings-FIXED-20261020.md`): the
/// outstanding count follows the rows -- raised per re-`<init>`, lowered
/// by a take, and by a collected or withdrawn row that still carries one.
#[test]
fn the_outstanding_count_follows_the_rows() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.note_finalizer_registered_again(0x1000));
    assert!(rp.note_finalizer_registered_again(0x2000));
    assert!(rp.note_finalizer_registered_again(0x2000));
    assert_eq!(rp.extra_finalizers_outstanding(), 3);
    let _ = rp.process_references(&|_| false, 100, 0);
    assert!(rp.take_extra_finalizer(0x1000));
    assert_eq!(rp.extra_finalizers_outstanding(), 2);
    // 0x2000 dies with its two owed runs (e.g. never finalized).
    rp.remove_collected(&|a| a == 0x1000);
    assert_eq!(rp.extra_finalizers_outstanding(), 0);
    assert!(!rp.take_extra_finalizer(0x2000));
    // A withdrawn row's count leaves with it (the switch-off arm of the
    // `constructed` refusal is the only way such a row is withdrawn).
    rp.discover_reference(ReferenceType::Finalizer, 0x3000, 0x3000, None);
    assert!(rp.note_finalizer_registered_again(0x3000));
    rp.set_reregister_at_init_return(false);
    assert!(rp.withdraw_finalizable(0x3000));
    assert_eq!(rp.extra_finalizers_outstanding(), 0);
}

/// The note the unwinder makes for a JNI upcall is dropped without a
/// withdrawal, and an older enqueued row at the address never answers for
/// the newest one.
#[test]
fn forgetting_a_note_withdraws_nothing_and_only_the_newest_row_answers() {
    let mut rp = processor();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.note_unconstructed_finalizable(0x1000, 7);
    assert!(!rp.forget_unconstructed_finalizable(0x1000, 8), "another epoch");
    assert!(rp.forget_unconstructed_finalizable(0x1000, 7));
    assert!(!rp.forget_unconstructed_finalizable(0x1000, 7), "taken");
    assert!(rp.is_registered_finalizable(0x1000), "nothing withdrawn");
    assert!(rp.note_finalizer_registered_again(0x1000));
    let _ = rp.process_references(&|_| false, 100, 0);
    // A new object at the address, registered and enqueued after it.
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    let _ = rp.process_references(&|_| false, 100, 0);
    assert!(!rp.take_extra_finalizer(0x1000), "the newest row owes none");
}

/// JIT round 21 wave 25, lane initwitness
/// (`CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS`): the `<init>`-return hook
/// moves the active row's witness, only that row's, and only while the
/// hook is armed; a row that is not active answers `None`.
#[test]
fn the_init_return_witness_moves_per_active_row() {
    let mut rp = processor();
    assert_eq!(rp.finalizable_init_returns(0x1000), None, "no row");
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert_eq!(rp.finalizable_init_returns(0x1000), Some(0));
    assert!(!rp.note_finalizable_init_returned(0x1000), "not re-registered");
    assert_eq!(rp.finalizable_init_returns(0x1000), Some(1), "moved");
    assert_eq!(rp.finalizable_init_returns(0x2000), Some(0), "another row: untouched");
    rp.set_reregister_at_init_return(false);
    assert!(!rp.note_finalizable_init_returned(0x1000));
    assert_eq!(rp.finalizable_init_returns(0x1000), Some(1), "hook off: never moves");
    rp.set_reregister_at_init_return(true);
    assert!(rp.withdraw_finalizable(0x2000));
    assert_eq!(rp.finalizable_init_returns(0x2000), None, "withdrawn: no active row");
}
