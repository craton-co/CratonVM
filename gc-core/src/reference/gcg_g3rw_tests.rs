use super::*;

fn map(pairs: &[(usize, usize)]) -> cratonvm_types::PointerMap {
    let mut m = cratonvm_types::PointerMap::default();
    for &(a, b) in pairs {
        m.insert(a, b);
    }
    m
}

/// Allocation registers soft / weak / phantom objects only, as watched
/// unbound entries that no processing phase sees.
#[test]
fn gcg_g3rw_allocation_registers_unbound_references_only() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.note_allocated_reference(ReferenceType::Weak, 0x1000, 7, false);
    p.note_allocated_reference(ReferenceType::Soft, 0x1040, 8, false);
    p.note_allocated_reference(ReferenceType::Phantom, 0x1080, 9, true);
    p.note_allocated_reference(ReferenceType::Finalizer, 0x10c0, 10, false);
    p.note_allocated_reference(ReferenceType::Cleaner, 0x1100, 11, false);
    p.note_allocated_reference(ReferenceType::Weak, 0, 7, false);
    assert_eq!(p.allocated_unbound_len(), 3);
    assert_eq!(
        p.weak_ref_count() + p.soft_ref_count() + p.phantom_ref_count(),
        0
    );
    let tracked = p.all_tracked_addrs();
    for a in [0x1000usize, 0x1040, 0x1080] {
        assert!(tracked.contains(&a), "{a:#x} is watched");
    }
    let taken = p.take_allocated_unbound();
    assert_eq!(p.allocated_unbound_len(), 0);
    assert!(
        taken[2].row.runs_cleaner,
        "a phantom keeps its Cleaner flag"
    );
    assert!(!taken[0].row.runs_cleaner);
    assert_eq!(taken[0].class_id, 7);
}

/// A bind registers the row (with the allocation's sequence number), a
/// null referent is never discovered, and a row the bridge already
/// registered is not registered twice.
#[test]
fn gcg_g3rw_bind_registers_once_and_never_a_null_referent() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.note_allocated_reference(ReferenceType::Weak, 0x1000, 7, false);
    p.note_allocated_reference(ReferenceType::Weak, 0x2000, 7, false);
    p.note_allocated_reference(ReferenceType::Soft, 0x3000, 8, false);
    p.note_allocated_reference(ReferenceType::Phantom, 0x4000, 9, true);
    let snap = p.registration_seq();
    // The bridge ran for 0x3000's constructor.
    p.discover_reference(ReferenceType::Soft, 0x3000, 0x9300, None);
    let mut pending = p.take_allocated_unbound().into_iter();
    let w1 = pending.next().unwrap();
    let w2 = pending.next().unwrap();
    let s = pending.next().unwrap();
    let ph = pending.next().unwrap();
    assert_eq!(
        p.bind_allocated_reference(w1, 0x9100, Some(0xa000)),
        BindOutcome::Bound
    );
    assert_eq!(
        p.bind_allocated_reference(w2, 0, None),
        BindOutcome::NullReferent
    );
    assert_eq!(
        p.bind_allocated_reference(s, 0x9300, None),
        BindOutcome::AlreadyRegistered
    );
    assert_eq!(
        p.bind_allocated_reference(ph, 0x9400, None),
        BindOutcome::Bound
    );
    assert_eq!(p.weak_ref_count(), 1);
    assert_eq!(p.soft_ref_count(), 1, "the bridge's row only");
    assert_eq!(p.phantom_ref_count(), 1);
    assert_eq!(p.weak_refs[0].referent, 0x9100);
    assert_eq!(p.weak_refs[0].queue_addr, Some(0xa000));
    assert!(p.phantom_refs[0].runs_cleaner, "bound as a Cleaner");
    assert!(
        p.weak_refs[0].registration_seq < snap && p.phantom_refs[0].registration_seq < snap,
        "a bound row keeps the allocation's sequence number"
    );
    // So a concurrent sweep that began after the allocation and freed the
    // object prunes the bound row.
    let dropped = p.remove_reference_objects_registered_before(snap, &|a| a == 0x1000);
    assert_eq!(dropped, 1);
    assert_eq!(p.weak_ref_count(), 0);
    // An application `clear()`ed row does not block a bind (not active).
    p.note_allocated_reference(ReferenceType::Weak, 0x5000, 7, false);
    p.discover_reference(ReferenceType::Weak, 0x5000, 0x9500, None);
    assert!(p.retire_by_application(0x5000));
    let again = p.take_allocated_unbound().pop().unwrap();
    assert_eq!(
        p.bind_allocated_reference(again, 0x9600, None),
        BindOutcome::Bound
    );
}

/// gcg g5/rs: a soft row bound from an allocation starts at its
/// allocation's clock (the JDK constructor's `timestamp = clock`), not at
/// `0`, which every LRU policy reads as infinitely idle. Positive control:
/// a row discovered with no touch still reads `0`.
#[test]
fn gcg_g5rs_a_bound_soft_row_starts_at_its_allocation_clock() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    let before = allocation_clock_ms();
    p.note_allocated_reference(ReferenceType::Soft, 0x1000, 8, false);
    p.note_allocated_reference(ReferenceType::Weak, 0x2000, 7, false);
    let mut pending = p.take_allocated_unbound().into_iter();
    let soft = pending.next().unwrap();
    let weak = pending.next().unwrap();
    assert!(soft.row.last_access_time_ms >= before && before != 0);
    assert_eq!(weak.row.last_access_time_ms, 0, "only soft rows carry one");
    assert_eq!(
        p.bind_allocated_reference(soft, 0x9100, None),
        BindOutcome::Bound
    );
    let stamped = p.soft_refs[0].last_access_time_ms;
    assert!(stamped >= before, "the bound row kept the allocation stamp");
    assert!(
        p.soft_ref_lru_index.contains_key(&(stamped, 0)),
        "and the LRU index is keyed on it"
    );
    // Positive control, on a processor no touch has fed a clock: the
    // bridge-less discovery path stamps `0`.
    let mut q = ReferenceProcessor::new_with_policy(1000);
    q.discover_reference(ReferenceType::Soft, 0x3000, 0x9300, None);
    assert_eq!(q.soft_refs[0].last_access_time_ms, 0, "positive control");
}

/// Unbound entries move with their objects, die with them (a collection's
/// survivor predicate, the concurrent sweep's freed spans), and a requeued
/// one keeps its sequence number.
#[test]
fn gcg_g3rw_unbound_entries_relocate_and_prune_like_rows() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.note_allocated_reference(ReferenceType::Weak, 0x1000, 7, false);
    p.note_allocated_reference(ReferenceType::Weak, 0x2000, 7, false);
    p.note_allocated_reference(ReferenceType::Phantom, 0x3000, 9, false);
    // An empty map is the early return; a real one relocates.
    p.update_after_gc(&map(&[]));
    p.update_after_gc(&map(&[(0x1000, 0x8000)]));
    let addrs: Vec<usize> = p
        .allocated_unbound
        .iter()
        .map(|a| a.row.reference_obj)
        .collect();
    assert_eq!(addrs, vec![0x8000, 0x2000, 0x3000]);
    p.remove_collected(&|a| a != 0x2000);
    assert_eq!(p.allocated_unbound_len(), 2, "the dead one is pruned");
    let snap = p.registration_seq();
    p.note_allocated_reference(ReferenceType::Weak, 0x3000, 7, false);
    // A sweep that began at `snap` freed 0x3000: the old entry goes, the
    // one allocated on the freed block after the snapshot stays.
    p.remove_reference_objects_registered_before(snap, &|a| a == 0x3000);
    let left: Vec<(usize, u64)> = p
        .allocated_unbound
        .iter()
        .map(|a| (a.row.reference_obj, a.row.registration_seq))
        .collect();
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].0, 0x8000);
    assert_eq!(left[1], (0x3000, snap));
    // A constructor that has not run yet: requeued, same sequence number.
    let mut taken = p.take_allocated_unbound();
    let last = taken.pop().unwrap();
    p.requeue_allocated_unbound(last);
    assert_eq!(p.allocated_unbound[0].row.registration_seq, snap);
    // The narrow pre-pass prunes them too.
    p.remove_collected_reference_objects(&|_| false);
    assert_eq!(p.allocated_unbound_len(), 0);
}

/// The cleaner and finalizer queues skip a `0` relocation target, as the
/// processor's rows do (`update_after_gc_skips_null_targets`); a real
/// target still relocates.
#[test]
fn gcg_g3rw_cleaner_and_finalizer_queues_skip_null_targets() {
    let cleaner = CleanerThread::new();
    cleaner.submit_action(0x100);
    cleaner.submit_action(0x200);
    cleaner.note_queues_to_notify(&[0x300, 0x400]);
    cleaner.update_after_gc(&map(&[
        (0x100, 0),
        (0x200, 0x2200),
        (0x300, 0),
        (0x400, 0x4400),
    ]));
    assert_eq!(cleaner.drain_actions(), vec![0x100, 0x2200]);
    assert_eq!(cleaner.drain_queues_to_notify(), vec![0x300, 0x4400]);

    let fin = FinalizerThread::new();
    assert!(fin.enqueue_unfinalized(0x500));
    assert!(fin.enqueue_unfinalized(0x600));
    fin.update_after_gc(&map(&[(0x500, 0), (0x600, 0x6600)]));
    assert_eq!(fin.dequeue(), Some(0x500));
    assert_eq!(fin.dequeue(), Some(0x6600));
}
