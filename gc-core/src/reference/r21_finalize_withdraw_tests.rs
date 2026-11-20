//! JIT round 21 wave 10, lane finalize: an unconstructed finalizable
//! object's registration is withdrawn, and only an active one.
use super::*;

#[test]
fn withdraw_removes_the_active_row_and_reopens_the_address() {
    let mut rp = ReferenceProcessor::new();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.is_registered_finalizable(0x1000));
    assert!(rp.withdraw_finalizable(0x1000));
    assert!(!rp.is_registered_finalizable(0x1000));
    assert!(rp.is_registered_finalizable(0x2000), "another row is kept");
    assert_eq!(rp.finalizer_refs.len(), 1);
    // A second withdrawal has nothing to do.
    assert!(!rp.withdraw_finalizable(0x1000));
    // The address registers again (a new object there).
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    assert!(rp.is_registered_finalizable(0x1000));
    assert_eq!(rp.finalizer_refs.len(), 2);
}

/// Wave 11 (review item 2): a withdrawal keeps the active set current
/// (drops the address in place), so the next registration does not
/// rebuild it from every row.
#[test]
fn a_withdrawal_keeps_the_active_set_current() {
    let mut rp = ReferenceProcessor::new();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    assert!(rp.is_registered_finalizable(0x2000));
    assert!(!rp.finalizer_active_stale);
    assert!(rp.withdraw_finalizable(0x1000));
    assert!(!rp.finalizer_active_stale, "no rebuild owed");
    assert!(!rp.finalizer_active.contains(&0x1000));
    assert!(rp.finalizer_active.contains(&0x2000));
}

/// Wave 13 (review rev-native item 2): the withdrawal finds its row from
/// the tail and removes exactly that row. The other rows keep their
/// registration order, and a verdict row (enqueued) at the same address,
/// left by an earlier object there, stays.
#[test]
fn a_withdrawal_removes_one_active_row_and_keeps_the_order() {
    let mut rp = ReferenceProcessor::new();
    for i in 0..8usize {
        let a = 0x1_0000 + i * 0x100;
        rp.discover_reference(ReferenceType::Finalizer, a, a, None);
    }
    // An earlier object at 0x9000 already has its verdict (enqueued); a
    // new object at the same address registers after it.
    rp.discover_reference(ReferenceType::Finalizer, 0x9000, 0x9000, None);
    let old = rp.finalizer_refs.len() - 1;
    rp.finalizer_refs[old].enqueued = true;
    rp.finalizer_active_stale = true;
    rp.discover_reference(ReferenceType::Finalizer, 0x9000, 0x9000, None);
    assert_eq!(rp.finalizer_refs.len(), 10);

    // The tail row (the newest registration).
    assert!(rp.withdraw_finalizable(0x9000));
    assert_eq!(rp.finalizer_refs.len(), 9);
    assert!(rp.finalizer_refs[8].reference_obj == 0x9000 && rp.finalizer_refs[8].enqueued);
    assert!(!rp.is_registered_finalizable(0x9000));
    // A middle row.
    assert!(rp.withdraw_finalizable(0x1_0300));
    let order: Vec<usize> = rp.finalizer_refs.iter().map(|e| e.reference_obj).collect();
    assert_eq!(
        order,
        vec![0x1_0000, 0x1_0100, 0x1_0200, 0x1_0400, 0x1_0500, 0x1_0600, 0x1_0700, 0x9000]
    );
    assert!(!rp.withdraw_finalizable(0x1_0300), "taken once");
    assert!(rp.is_registered_finalizable(0x1_0700));
    assert!(!rp.finalizer_active_stale);
}

#[test]
fn an_enqueued_row_is_a_verdict_and_stays() {
    let mut rp = ReferenceProcessor::new();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.finalizer_refs[0].enqueued = true;
    rp.finalizer_active_stale = true;
    assert!(!rp.withdraw_finalizable(0x1000));
    assert_eq!(rp.finalizer_refs.len(), 1);
}

#[test]
fn a_door_takes_only_its_own_entry_of_its_own_epoch() {
    let mut rp = ReferenceProcessor::new();
    rp.discover_reference(ReferenceType::Finalizer, 0x1000, 0x1000, None);
    rp.discover_reference(ReferenceType::Finalizer, 0x2000, 0x2000, None);
    rp.note_unconstructed_finalizable(0x1000, 7);
    // Another address, another epoch: nothing.
    assert!(!rp.take_unconstructed_finalizable(0x2000, 7));
    assert!(!rp.take_unconstructed_finalizable(0x1000, 8));
    assert!(rp.is_registered_finalizable(0x1000));
    assert!(rp.take_unconstructed_finalizable(0x1000, 7));
    assert!(!rp.is_registered_finalizable(0x1000));
    assert!(rp.is_registered_finalizable(0x2000));
    // Taken once.
    assert!(!rp.take_unconstructed_finalizable(0x1000, 7));
}

#[test]
fn a_newer_epoch_drops_older_entries_and_the_list_is_capped() {
    let mut rp = ReferenceProcessor::new();
    rp.note_unconstructed_finalizable(0x1000, 1);
    rp.note_unconstructed_finalizable(0x2000, 2);
    assert_eq!(rp.unconstructed_finalizables, vec![(0x2000, 2)]);
    for i in 0..(UNCONSTRUCTED_FINALIZABLES_MAX + 5) {
        rp.note_unconstructed_finalizable(0x10_000 + i * 16, 3);
    }
    assert_eq!(rp.unconstructed_finalizables.len(), UNCONSTRUCTED_FINALIZABLES_MAX);
    assert_eq!(rp.unconstructed_finalizables[0].0, 0x10_000 + 5 * 16, "oldest dropped");
    // A duplicate is not noted twice.
    rp.note_unconstructed_finalizable(0x10_000 + 5 * 16, 3);
    assert_eq!(rp.unconstructed_finalizables.len(), UNCONSTRUCTED_FINALIZABLES_MAX);
}
