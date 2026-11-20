use super::*;

// Helpers ---------------------------------------------------------------

fn always_dead(_addr: usize) -> bool {
    false
}
fn always_live(_addr: usize) -> bool {
    true
}
fn live_set(set: &[usize]) -> impl Fn(usize) -> bool + '_ {
    move |addr| set.contains(&addr)
}

// gengc-mark 2026-09-20 regressions ------------------------------------

// ORCHESTRATOR REMOVAL, 2026-09-20 — a test asserting the inverse of three
// older ones stood here: `a_weak_ref_is_cleared_even_when_a_kept_soft_ref_
// names_the_same_dead_object`. It set up a policy-KEPT soft entry under an
// `always_dead` mark predicate with a weak reference to the same referent —
// byte for byte the setup of tests 62, 63 and
// `cleaner_not_fired_while_referent_softly_reachable` — and asserted the
// weak reference MUST be cleared, where those three assert it must NOT be.
// Both cannot hold, and the older three have the JLS behind them: a soft
// reference the policy declines to clear leaves its referent SOFTLY
// REACHABLE, and a weak reference is cleared only once its referent is
// weakly reachable, i.e. neither strongly nor softly. The Phase 1 policy
// decision outranks `is_marked` for the rest of the cycle; that is what the
// soft-reachable closure is for.
//
// It was added alongside the `soft_is_live(e.referent)` screen it encoded,
// and was removed with it. The narrower hazard that screen was reaching for
// is real but keyed on the SoftReference INSTANCE rather than the referent,
// and is filed as
// `docs/feature-designs/generative-gc-architecture.md`.
// A test for that gap needs an unmarked `reference_obj`, which this one did
// not have (it passed `None`).

/// A weak reference to a referent a soft reference genuinely KEEPS ALIVE
/// must still be retained — the fix above must not over-clear.
#[test]
fn a_weak_ref_to_a_live_soft_referent_is_retained() {
    const REFERENT: usize = 0x100;
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 0x10, REFERENT, None);
    proc.discover_reference(ReferenceType::Weak, 0x20, REFERENT, Some(0x900));

    // The referent IS marked: strongly/softly reachable, so the JLS
    // strength ordering forbids clearing the weak reference.
    let live = [REFERENT];
    let _ = proc.process_references(&live_set(&live), 1024, 1_000_000);

    assert_eq!(
        proc.stats().weak_refs_cleared,
        0,
        "strong > soft > weak: a live soft referent keeps the weak ref intact",
    );
}

/// The condemned-soft set is address-keyed and must follow a compacting
/// collection, exactly like the identity and referent-class stamp tables.
///
/// Left stale it can collide with the NEW address of a different soft
/// entry, which then takes the condemned branch in `process_soft_refs` —
/// a branch that skips the idle check entirely. That clears a
/// `SoftReference` with no memory pressure at all.
#[test]
fn update_after_gc_relocates_the_condemned_soft_set() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x100, None);

    // `free_heap_mb == 0` makes the threshold 0, so the entry (timestamp 0,
    // "now" 5000) is condemned.
    let condemned = proc.condemn_idle_soft_refs(0, 5_000);
    assert_eq!(condemned, vec![(0x10, 0x100)]);

    // A compacting collection slides both the Reference and its referent.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10, 0x30);
    map.insert(0x100, 0x300);
    proc.update_after_gc(&map);

    assert_eq!(
        proc.soft_pre_nulled_active_pairs(),
        vec![(0x30, 0x300)],
        "the post-GC restore pass must still recognise the entry whose slot \
         the pre-GC pass nulled",
    );
}

// RETIRED 2026-09-21 with `ReferenceQueue` itself (see the retirement
// note above `ReferenceProcessor`): `remove_timeout_still_honours_its_deadline`
// asserted that an empty queue's wait lasts ~40 ms. It could not be
// ported. The only honest implementation of a wait on a `&mut self`
// queue is to return at once — nothing in the process can enqueue while
// the borrow is held — so the elapsed time this test pinned WAS the
// defect, stated as an expectation.

// 1. Discover soft reference -------------------------------------------
#[test]
fn discover_soft_ref() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    assert_eq!(proc.soft_refs.len(), 1);
    assert_eq!(proc.soft_refs[0].referent, 200);
}

// 2. Discover weak reference -------------------------------------------
#[test]
fn discover_weak_ref() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    assert_eq!(proc.weak_refs.len(), 1);
}

// 3. Discover phantom reference ----------------------------------------
#[test]
fn discover_phantom_ref() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
    assert_eq!(proc.phantom_refs.len(), 1);
}

// 4. Discover cleaner reference ----------------------------------------
#[test]
fn discover_cleaner_ref() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Cleaner, 100, 200, None);
    assert_eq!(proc.cleaner_refs.len(), 1);
}

// 5. Discover finalizer reference --------------------------------------
#[test]
fn discover_finalizer_ref() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 100, 200, Some(400));
    assert_eq!(proc.finalizer_refs.len(), 1);
}

// 6. Strong references are not tracked ---------------------------------
#[test]
fn strong_refs_not_tracked() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Strong, 100, 200, None);
    assert!(proc.soft_refs.is_empty());
    assert!(proc.weak_refs.is_empty());
    assert!(proc.phantom_refs.is_empty());
}

// 7. SoftRef cleared when time exceeds threshold -----------------------
#[test]
fn soft_ref_cleared_when_old() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    // last_access_time_ms = 0, current = 5000, free = 2 MB
    // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear
    let result = proc.process_references(&always_dead, 2, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert!(proc.soft_refs[0].cleared);
}

// 8. SoftRef kept when recently accessed --------------------------------
#[test]
fn soft_ref_kept_when_recent() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    proc.soft_refs[0].last_access_time_ms = 4500;
    // threshold = 1000 * 10 = 10000; idle = 5000 - 4500 = 500 < 10000
    let result = proc.process_references(&always_dead, 10, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 0);
}

// 9. SoftRef kept when referent is live --------------------------------
#[test]
fn soft_ref_kept_when_referent_live() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    let result = proc.process_references(&always_live, 0, 99999);
    assert_eq!(result.stats.soft_refs_cleared, 0);
}

// 10. WeakRef cleared when referent unreachable -------------------------
#[test]
fn weak_ref_cleared_when_dead() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(proc.weak_refs[0].cleared);
}

// 11. WeakRef kept when referent reachable ------------------------------
#[test]
fn weak_ref_kept_when_live() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    let result = proc.process_references(&always_live, 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 0);
}

// 12. PhantomRef enqueued when referent unreachable ---------------------
#[test]
fn phantom_ref_enqueued_when_dead() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    assert!(proc.phantom_refs[0].enqueued);
    assert_eq!(result.to_enqueue.len(), 1);
    assert_eq!(result.to_enqueue[0], (100, 300));
}

// 13. PhantomRef referent NOT cleared (Java 9+ semantics) --------------
#[test]
fn phantom_ref_not_cleared() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 100, 200, Some(300));
    let _result = proc.process_references(&always_dead, 100, 0);
    assert!(!proc.phantom_refs[0].cleared);
}

// 14. CleanerThread submit and drain -----------------------------------
#[test]
fn cleaner_submit_and_drain() {
    let ct = CleanerThread::new();
    ct.submit_action(0xA000);
    ct.submit_action(0xB000);
    assert_eq!(ct.pending_count(), 2);
    let drained = ct.drain_actions();
    assert_eq!(drained, vec![0xA000, 0xB000]);
    assert_eq!(ct.pending_count(), 0);
}

// 15. FinalizerThread enqueue and dequeue (FIFO) -----------------------
#[test]
fn finalizer_enqueue_dequeue_fifo() {
    let ft = FinalizerThread::new();
    ft.enqueue_unfinalized(1);
    ft.enqueue_unfinalized(2);
    ft.enqueue_unfinalized(3);
    assert_eq!(ft.pending_count(), 3);
    assert_eq!(ft.dequeue(), Some(1));
    assert_eq!(ft.dequeue(), Some(2));
    assert_eq!(ft.dequeue(), Some(3));
    assert_eq!(ft.dequeue(), None);
}

/// zt t31c: a held entry stays queued and rooted but is not handed out
/// (nor counted as deliverable) until the hold is released; a hold names
/// only queued entries, follows a relocation and is replaced, not merged.
#[test]
fn a_held_finalizable_is_not_dequeued_until_released() {
    let ft = FinalizerThread::new();
    ft.enqueue_unfinalized(1);
    ft.enqueue_unfinalized(2);
    ft.enqueue_unfinalized(3);
    ft.hold_until_judged(&[1, 3, 0x99]);
    assert_eq!(
        ft.pending_count(),
        1,
        "only the unheld entry is deliverable"
    );
    assert_eq!(
        ft.pending_addresses(),
        vec![1, 2, 3],
        "held entries stay roots"
    );
    assert!(!ft.enqueue_unfinalized(3), "a held entry is still queued");
    assert_eq!(ft.dequeue(), Some(2));
    assert_eq!(ft.dequeue(), None);
    assert_eq!(ft.pending_count(), 0);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(1, 0x1001);
    ft.update_after_gc(&map);
    assert_eq!(ft.dequeue(), None, "the hold follows the move");
    ft.hold_until_judged(&[3]);
    assert_eq!(ft.dequeue(), Some(0x1001), "a new hold replaces the old");
    assert!(ft.take_hold_released());
    assert!(!ft.take_hold_released(), "taken once");

    ft.release_held();
    assert!(ft.take_hold_released());
    assert_eq!(ft.pending_count(), 1);
    assert_eq!(ft.dequeue(), Some(3));
    ft.release_held();
    assert!(!ft.take_hold_released(), "an empty release reports nothing");
}

/// An object already waiting in the queue must not be enqueued again.
///
/// `finalizable_roots` hands the collector this queue's own contents so a
/// deferred entry stays rooted, and the collector then reports those
/// (genuinely unreachable) objects back as dead on every cycle — so without
/// the guard, `finalize()` runs once per GC the object waited through.
#[test]
fn a_queued_object_is_not_enqueued_twice() {
    let ft = FinalizerThread::new();
    assert!(ft.enqueue_unfinalized(0xBEEF), "first enqueue is accepted");
    assert!(
        !ft.enqueue_unfinalized(0xBEEF),
        "a second enqueue while queued is refused"
    );
    assert!(!ft.enqueue_unfinalized(0xBEEF), "and stays refused");
    assert_eq!(ft.pending_count(), 1);
    assert_eq!(ft.dequeue(), Some(0xBEEF));
    assert_eq!(ft.pending_count(), 0);
}

/// The membership index must survive a relocating collection: after
/// `update_after_gc` rewrites an entry, the NEW address is the one that
/// must be refused and the OLD one must no longer be indexed.
#[test]
fn the_queue_index_follows_a_relocation() {
    let ft = FinalizerThread::new();
    ft.enqueue_unfinalized(0x1000);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x1000, 0x2000);
    ft.update_after_gc(&map);

    assert_eq!(ft.pending_addresses(), vec![0x2000]);
    assert!(
        !ft.enqueue_unfinalized(0x2000),
        "the relocated address is the one now queued, and must be refused"
    );
    // The vacated address is no longer queued, so an object that legitimately
    // lands there later is still allowed in.
    assert!(
        ft.enqueue_unfinalized(0x1000),
        "the pre-relocation address must not stay indexed"
    );
    assert_eq!(ft.pending_count(), 2);
}

// 16, 17. RETIRED 2026-09-21 with `ReferenceQueue`:
// `reference_queue_enqueue_poll` and `reference_queue_capacity_evicts_oldest`.
// The second was the test that PINNED the evict-the-head overflow policy
// (survivors `[2, 3]`, and `true` returned from the `enqueue` that had
// just destroyed a different entry), which is why that policy could never
// be fixed as a drive-by. Both went with the type.

// 18. update_after_gc relocates addresses ------------------------------
#[test]
fn update_after_gc_relocates() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(100, 1100);
    map.insert(200, 1200);
    map.insert(300, 1300);
    proc.update_after_gc(&map);
    assert_eq!(proc.weak_refs[0].reference_obj, 1100);
    assert_eq!(proc.weak_refs[0].referent, 1200);
    assert_eq!(proc.weak_refs[0].queue_addr, Some(1300));
}

/// gcj j1yg-pause round 3: the rows set aside are exactly the active weak
/// / phantom rows whose `Reference` and referent are both old; the passes
/// see the rest; the restore puts every set-aside row back in its old
/// place, keeps what the passes removed removed, and keeps rows added in
/// the window after every older row. Stamps and the watched list keep the
/// set-aside rows meanwhile.
#[test]
fn gcj_j1yg_settled_old_rows_are_set_aside_and_restored_in_place() {
    let old = (10_000usize, 20_000usize);
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 10_100, 10_200, None); // old/old: aside
    proc.discover_reference(ReferenceType::Weak, 10_300, 500, None); // young referent
    proc.discover_reference(ReferenceType::Weak, 10_400, 10_500, None); // old/old: aside
    proc.discover_reference(ReferenceType::Weak, 600, 10_700, None); // young Reference
    proc.discover_reference(ReferenceType::Phantom, 10_800, 10_900, None); // aside
    proc.discover_reference(ReferenceType::Soft, 11_000, 11_100, None); // soft: never
    proc.identity_stamps.insert(10_100, 7);
    let before: Vec<usize> = proc.weak_refs.iter().map(|e| e.reference_obj).collect();

    assert_eq!(proc.stash_settled_old_rows(old), 3);
    assert_eq!(proc.settled_old_stashed(), 3);
    let seen: Vec<usize> = proc.weak_phantom_active_pairs().iter().map(|&(r, _)| r).collect();
    assert_eq!(seen, vec![10_300, 600], "the passes see only the rows a minor can change");
    assert_eq!(proc.soft_refs.len(), 1);
    let watched = proc.all_tracked_addrs();
    for a in [10_100, 10_200, 10_400, 10_500, 10_800, 10_900] {
        assert!(watched.contains(&a), "{a} stays watched while set aside");
    }
    // A pass removes a row and a row is added inside the window.
    proc.weak_refs.retain(|e| e.reference_obj != 600);
    proc.discover_reference(ReferenceType::Weak, 700, 800, None);
    proc.prune_identity_stamps();
    assert_eq!(proc.identity_stamps.get(&10_100), Some(&7), "a set-aside row keeps its stamp");

    proc.restore_settled_old_rows();
    assert_eq!(proc.settled_old_stashed(), 0);
    let after: Vec<usize> = proc.weak_refs.iter().map(|e| e.reference_obj).collect();
    let expected: Vec<usize> = before
        .iter()
        .copied()
        .filter(|&a| a != 600)
        .chain(std::iter::once(700))
        .collect();
    assert_eq!(after, expected);
    assert_eq!(proc.phantom_refs.len(), 1);
    // Restoring again is a no-op.
    proc.restore_settled_old_rows();
    assert_eq!(proc.weak_refs.len(), expected.len());
}

#[test]
fn gcj_pcp_a_settled_old_reference_with_a_young_queue_is_still_remapped() {
    let old = (10_000usize, 20_000usize);
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 10_100, 10_200, Some(500));
    proc.discover_reference(ReferenceType::Weak, 10_300, 10_400, Some(10_500));
    proc.discover_reference(ReferenceType::Phantom, 10_600, 10_700, None);
    assert_eq!(proc.stash_settled_old_rows(old), 2);
    let watch = proc.active_round_tracked_addrs();
    assert_eq!(watch, vec![10_100, 10_200, 500]);
    let complete = proc.all_tracked_addrs();
    for addr in [10_300, 10_400, 10_500, 10_600, 10_700] {
        assert!(complete.contains(&addr));
        assert!(!watch.contains(&addr));
    }
    let map = cratonvm_types::PointerMap::from_iter([(500, 800)]);
    proc.update_after_gc(&map);
    proc.restore_settled_old_rows();
    assert_eq!(proc.weak_refs[0].queue_addr, Some(800));
    assert_eq!(proc.weak_refs[1].queue_addr, Some(10_500));
    assert_eq!(proc.active_round_tracked_addrs(), proc.all_tracked_addrs());
}

/// A registration keeps a current row index current: the bind pass asks
/// `has_active_reference_row` once per bound row, and a registration that
/// marked the index stale made each of those rebuild it (quadratic).
#[test]
fn p3_registering_a_row_does_not_stale_a_current_row_index() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    assert!(proc.has_active_reference_row(100)); // builds the index
    proc.discover_reference(ReferenceType::Weak, 300, 400, None);
    assert!(!proc.app_row_index_stale);
    assert!(proc.has_active_reference_row(300));
}

/// P3 item 1: the G1 form sets aside by a predicate, and a pause that
/// collected old storage after all (another epoch) relocates the set-aside
/// rows through its map; the same epoch leaves them untouched.
#[test]
fn p3_settled_rows_are_relocated_only_when_the_epoch_moved() {
    for (epoch_at_restore, moved) in [(7u64, false), (8u64, true)] {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, 10_100, 10_200, None);
        proc.discover_reference(ReferenceType::Weak, 500, 10_300, None);
        let in_old = |a: usize| (10_000..20_000).contains(&a);
        assert_eq!(proc.stash_settled_rows(&in_old, 7), 1);
        let map = cratonvm_types::PointerMap::from_iter([(10_100, 10_900), (10_200, 10_950)]);
        proc.restore_settled_old_rows_after(epoch_at_restore, &map);
        assert_eq!(proc.settled_old_stashed(), 0);
        let row = proc.weak_refs.iter().find(|e| e.reference_obj != 500).unwrap();
        assert_eq!(
            (row.reference_obj, row.referent),
            if moved { (10_900, 10_950) } else { (10_100, 10_200) }
        );
    }
}

#[test]
fn gcj_pcp_restore_matches_the_stable_rank_order_with_reordering_and_duplicates() {
    for duplicate in [false, true] {
        for reverse in [false, true] {
            let mut proc = ReferenceProcessor::new();
            for i in 0..100usize {
                proc.discover_reference(ReferenceType::Weak, 10_000 + i * 8,
                    if i % 3 == 0 { 100 } else { 15_000 }, None);
            }
            if duplicate {
                proc.weak_refs[20].registration_seq = proc.weak_refs[19].registration_seq;
            }
            if reverse {
                proc.weak_refs.reverse();
            }
            let before = proc.weak_refs.clone();
            proc.stash_settled_old_rows((10_000, 20_000));
            proc.weak_refs.retain(|e| e.reference_obj % 16 != 0);
            proc.discover_reference(ReferenceType::Weak, 900, 100, None);
            let st = proc.settled_old_stash.as_ref().unwrap();
            let rank: FxHashMap<u64, usize> = before.iter().enumerate()
                .map(|(i, e)| (e.registration_seq, i)).collect();
            let mut want: Vec<(usize, usize)> = proc.weak_refs.iter().enumerate()
                .map(|(i, e)| (*rank.get(&e.registration_seq).unwrap_or(&(before.len() + i)),
                              e.reference_obj)).collect();
            want.extend(st.weak.iter().map(|e|
                (*rank.get(&e.registration_seq).unwrap_or(&before.len()), e.reference_obj)));
            want.sort_by_key(|&(k, _)| k);
            proc.restore_settled_old_rows();
            assert_eq!(proc.weak_refs.iter().map(|e| e.reference_obj).collect::<Vec<_>>(),
                want.into_iter().map(|(_, addr)| addr).collect::<Vec<_>>(),
                "duplicate={duplicate} reverse={reverse}");
        }
    }
}

// 19. remove_collected cleans up dead Reference objects -----------------
#[test]
fn all_tracked_addrs_covers_every_reference_kind() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 100, 200, None);
    proc.discover_reference(ReferenceType::Weak, 101, 201, Some(301));
    proc.discover_reference(ReferenceType::Phantom, 102, 202, None);
    proc.discover_reference(ReferenceType::Cleaner, 103, 203, None);
    proc.discover_reference(ReferenceType::Finalizer, 104, 204, None);

    let addrs = proc.all_tracked_addrs();
    // Soft / cleaner / finalizer entries were NOT published before the
    // old-gen-reclamation fix, so the collector emitted no survival proof
    // for them and the exact post-GC predicate could not be used.
    for a in [100, 200, 101, 201, 301, 102, 202, 103, 203, 104, 204] {
        assert!(
            addrs.contains(&a),
            "address {a} must be published as watched"
        );
    }
}

/// gcf f2/r: with `CRATONVM_REF_SETTLED_ROWS_UNTRACKED` a settled soft /
/// weak / phantom row publishes its `Reference` only; active rows, a row
/// whose enqueue is still pending, a Cleaner whose `clean()` is not out
/// yet, and every cleaner / finalizer row keep all three words.
#[test]
fn gcf_f2r_settled_rows_publish_only_their_reference() {
    let mut proc = ReferenceProcessor::new();
    proc.settled_rows_untracked = true;
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300)); // settled
    proc.discover_reference(ReferenceType::Weak, 101, 201, Some(301)); // active
    proc.discover_reference(ReferenceType::Soft, 102, 202, None); // settled, no queue
    proc.discover_reference(ReferenceType::Phantom, 103, 203, Some(303)); // settled
    proc.discover_reference(ReferenceType::Weak, 104, 204, Some(304)); // pending enqueue
    proc.discover_phantom_cleaner(105, 205, Some(305)); // clean() not emitted
    proc.discover_reference(ReferenceType::Finalizer, 106, 206, None);
    proc.discover_reference(ReferenceType::Cleaner, 107, 207, None);
    for e in proc.weak_refs.iter_mut() {
        if e.reference_obj == 100 || e.reference_obj == 104 {
            e.cleared = true;
            e.enqueued = true;
        }
    }
    proc.soft_refs[0].cleared = true;
    for e in proc.phantom_refs.iter_mut() {
        e.enqueued = true;
    }
    proc.finalizer_refs[0].enqueued = true;
    proc.cleaner_refs[0].cleared = true;
    proc.pending_queues.entry(304).or_default().push(104);

    let addrs = proc.all_tracked_addrs();
    for a in [100, 102, 103] {
        assert!(addrs.contains(&a), "a settled row keeps its Reference {a}");
    }
    for a in [200, 300, 202, 203, 303] {
        assert!(
            !addrs.contains(&a),
            "a settled row's {a} must not be watched"
        );
    }
    for a in [
        101, 201, 301, 104, 204, 304, 105, 205, 305, 106, 206, 107, 207,
    ] {
        assert!(addrs.contains(&a), "address {a} must stay watched");
    }

    // Once the Cleaner has run, its row settles too.
    proc.phantom_refs
        .iter_mut()
        .filter(|e| e.reference_obj == 105)
        .for_each(|e| e.action_emitted = true);
    let addrs = proc.all_tracked_addrs();
    assert!(addrs.contains(&105) && !addrs.contains(&205) && !addrs.contains(&305));
}

/// gcf f2/r: the switch is off by default and then nothing changes.
#[test]
fn gcf_f2r_settled_rows_stay_tracked_with_the_switch_off() {
    let mut proc = ReferenceProcessor::new();
    proc.settled_rows_untracked = false;
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    proc.weak_refs[0].cleared = true;
    proc.weak_refs[0].enqueued = true;
    assert_eq!(proc.all_tracked_addrs(), vec![100, 200, 300]);
}

#[test]
fn remove_collected_cleans_up() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    proc.discover_reference(ReferenceType::Weak, 101, 201, None);
    let live = [101usize];
    proc.remove_collected(&live_set(&live));
    assert_eq!(proc.weak_refs.len(), 1);
    assert_eq!(proc.weak_refs[0].reference_obj, 101);
}

// 19b. HIB-WEAKREF-RECYCLE.1 — the shape prune drops weak/phantom entries
//      whose Reference object has been recycled, and leaves the
//      finalizer/cleaner lists (whose reference_obj is an arbitrary object)
//      completely alone.
#[test]
fn retain_shaped_weak_phantom_prunes_recycled_and_spares_finalizers() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, None);
    proc.discover_reference(ReferenceType::Weak, 101, 201, None);
    proc.discover_reference(ReferenceType::Phantom, 102, 202, Some(302));
    // A finalizable object with a shape the weak/phantom test would reject.
    proc.discover_reference(ReferenceType::Finalizer, 103, 203, None);
    proc.discover_reference(ReferenceType::Cleaner, 104, 204, None);

    // 100 and 102 have been reclaimed and their memory recycled: whatever
    // occupies those addresses no longer has >= 2 instance fields.
    let still_a_reference = [101usize];
    proc.retain_shaped_weak_phantom(&live_set(&still_a_reference));

    assert_eq!(proc.weak_refs.len(), 1);
    assert_eq!(proc.weak_refs[0].reference_obj, 101);
    assert!(proc.phantom_refs.is_empty());
    // Untouched — pruning these on a field-count test would drop live
    // finalizable objects that legitimately declare fewer than two fields.
    assert_eq!(proc.finalizer_refs.len(), 1);
    assert_eq!(proc.cleaner_refs.len(), 1);
}

// 20. Stats tracking ---------------------------------------------------
#[test]
fn stats_tracking() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 10, 20, None);
    proc.discover_reference(ReferenceType::Weak, 11, 21, None);
    proc.discover_reference(ReferenceType::Phantom, 30, 40, Some(50));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.weak_refs_discovered, 2);
    assert_eq!(result.stats.weak_refs_cleared, 2);
    assert_eq!(result.stats.phantom_refs_discovered, 1);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
}

// 21. Multiple reference types in single processing --------------------
#[test]
fn mixed_reference_types() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 1, 2, Some(500));
    proc.discover_reference(ReferenceType::Weak, 3, 4, Some(500));
    proc.discover_reference(ReferenceType::Phantom, 5, 6, Some(600));
    proc.discover_reference(ReferenceType::Finalizer, 7, 8, Some(700));
    proc.discover_reference(ReferenceType::Cleaner, 9, 10, None);

    // All referents dead, soft ref old enough to clear
    let result = proc.process_references(&always_dead, 1, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
    assert_eq!(result.stats.cleaner_refs_processed, 1);
}

// 22. Policy configuration change --------------------------------------
#[test]
fn policy_configuration() {
    let mut proc = ReferenceProcessor::new();
    proc.set_soft_ref_lru_policy(500);
    proc.discover_reference(ReferenceType::Soft, 1, 2, None);
    // threshold = 500 * 4 = 2000; idle = 2500 > 2000 => clear
    let result = proc.process_references(&always_dead, 4, 2500);
    assert_eq!(result.stats.soft_refs_cleared, 1);
}

// 23. Empty processor returns empty result -----------------------------
#[test]
fn empty_processor() {
    let mut proc = ReferenceProcessor::new();
    let result = proc.process_references(&always_dead, 100, 0);
    assert!(result.to_enqueue.is_empty());
    assert!(result.to_finalize.is_empty());
    assert!(result.cleaner_actions.is_empty());
}

// 24. to_enqueue contains correct pairs --------------------------------
#[test]
fn to_enqueue_pairs() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 10, 20, Some(500));
    proc.discover_reference(ReferenceType::Weak, 11, 21, Some(600));
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.to_enqueue.len(), 2);
    // Both should be present (order may depend on HashMap iteration)
    let has_10 = result.to_enqueue.iter().any(|&(r, q)| r == 10 && q == 500);
    let has_11 = result.to_enqueue.iter().any(|&(r, q)| r == 11 && q == 600);
    assert!(has_10);
    assert!(has_11);
}

// 25. Finalization queue ordering (FIFO) -------------------------------
#[test]
fn finalization_queue_fifo() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 1, 100, None);
    proc.discover_reference(ReferenceType::Finalizer, 2, 200, None);
    proc.discover_reference(ReferenceType::Finalizer, 3, 300, None);
    let result = proc.process_references(&always_dead, 100, 0);
    // Once-only emission (bc math-ec 0x4 fix, 2026-06-10): process_references
    // now DRAINS the dead finalizers into the result in FIFO discovery order
    // rather than leaving them in the internal queue, so the interpreter
    // consumes them exactly once. The internal queue is therefore empty
    // afterwards.
    assert_eq!(result.to_finalize, vec![100, 200, 300]);
    assert_eq!(proc.dequeue_for_finalization(), None);
}

// 26. Reset stats works ------------------------------------------------
#[test]
fn reset_stats() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 1, 2, None);
    let _result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(proc.stats().weak_refs_cleared, 1);
    proc.reset_stats();
    assert_eq!(proc.stats().weak_refs_cleared, 0);
}

// 27. Weak ref not enqueued without queue ------------------------------
#[test]
fn weak_ref_no_queue_no_enqueue() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 10, 20, None);
    let result = proc.process_references(&always_dead, 100, 0);
    assert!(result.to_enqueue.is_empty());
    assert!(proc.weak_refs[0].cleared);
    assert!(!proc.weak_refs[0].enqueued);
}

// 28. Selective liveness -----------------------------------------------
#[test]
fn selective_liveness() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 1, 100, Some(500));
    proc.discover_reference(ReferenceType::Weak, 2, 200, Some(500));
    // Only referent 100 is live
    let live = [100usize];
    let result = proc.process_references(&live_set(&live), 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(!proc.weak_refs[0].cleared); // referent 100 alive
    assert!(proc.weak_refs[1].cleared); // referent 200 dead
}

// 28b. SOFT-CLEAR GAP (2026-08-15) -------------------------------------
//
// These five pin the fix for the defect the retired
// `zgc-resourceleakdetector-corpse-read` write-up left as an open
// question. `process_soft_refs` skips any entry whose referent
// `is_marked`, and the VM's markers trace a `Reference`'s slot 0 as an
// ordinary strong edge -- so a soft referent was always marked through its
// own `SoftReference` and the LRU policy underneath was unreachable on
// every collector. `condemn_idle_soft_refs` moves the decision ahead of
// the mark, where the VM can null the slot the way it already does for
// weak and phantom.

/// The policy picks out the idle entries and leaves the freshly-read one.
#[test]
fn r5w1_hotspot_lru_uses_the_previous_collections_clock_and_free_heap() {
    // Latched from the flag at construction.
    let latched = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_SOFTREF_HOTSPOT_LRU", Some("1"))],
        || ReferenceProcessor::new_with_policy(1000).hotspot_soft_lru,
    );
    assert!(latched, "the flag is latched by the constructor");
    assert!(
        !cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", None)],
            || ReferenceProcessor::new_with_policy(1000).hotspot_soft_lru,
        ),
        "unset: the legacy inputs"
    );
    // gen r5w4/defaults8: the VM's constructor latches the per-backend
    // default for its heap — HotSpot's inputs on the Generational heap,
    // the legacy ones elsewhere — and an explicit value wins on both.
    let for_heap = |v: Option<&str>, generational: bool| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", v)],
            || ReferenceProcessor::new_for_heap(None, generational).hotspot_soft_lru,
        )
    };
    assert!(
        for_heap(None, true),
        "unset, Generational: HotSpot's inputs"
    );
    assert!(!for_heap(None, false), "unset, G1/ZGC: the legacy inputs");
    assert!(for_heap(Some("1"), false), "explicit ON wins on G1/ZGC");
    assert!(
        !for_heap(Some("0"), true),
        "explicit OFF wins on Generational"
    );
    // zr z1/rf: the ZGC heap takes HotSpot's inputs unless `=0`.
    let for_zgc = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", v)],
            || ReferenceProcessor::new_for_backend(None, false, true).hotspot_soft_lru,
        )
    };
    assert!(for_zgc(None), "unset, ZGC: HotSpot's inputs");
    assert!(!for_zgc(Some("0")), "explicit OFF wins on ZGC");
    assert!(
        !cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_SOFTREF_HOTSPOT_LRU", None)],
            || ReferenceProcessor::new_for_backend(None, false, false).hotspot_soft_lru,
        ),
        "unset, G1: unchanged"
    );
    // The `-XX:SoftRefLRUPolicyMSPerMB` half is `new_with_policy`'s.
    assert_eq!(
        ReferenceProcessor::new_for_heap(Some(7), true).soft_ref_lru_policy_ms_per_mb,
        7
    );
    assert_eq!(
        ReferenceProcessor::new_for_heap(None, false).soft_ref_lru_policy_ms_per_mb,
        ReferenceProcessor::new().soft_ref_lru_policy_ms_per_mb
    );

    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.hotspot_soft_lru = true;
    // The wall-clock stamp arm (`CRATONVM_SOFTREF_STAMP_GC_CLOCK=0`; the
    // switch is default ON since 2026-10-17): the read at 9 500 below is
    // stamped 9 500, not the 9 000 clock. `gci_i11rp_tests` covers ON.
    proc.soft_stamp_gc_clock = false;
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    proc.touch_soft_reference(10, 1_000);

    // Before any collection the clock is 0: nothing is idle, whatever the
    // wall clock and the pre-collection free space say (the legacy inputs
    // condemn it: 4 s idle against a 0 ms threshold).
    assert!(proc.condemn_idle_soft_refs(0, 5_000).is_empty());
    // That collection's processing round ends: its "now" (5 000) becomes
    // the clock, and the free heap it was handed (0 MB) the next budget.
    let _ = proc.process_references(&always_live, 0, 0);
    assert_eq!(proc.soft_gc_clock_ms, 5_000);
    assert_eq!(proc.soft_free_mb_at_last_gc, Some(0));
    proc.finish_pre_gc_cycle();

    // The next collection: idle = 5 000 - 1 000 = 4 s > 0 ms (0 MB free
    // after the last collection) — condemned, although the caller now
    // reports 1 GB free before this one.
    assert_eq!(proc.condemn_idle_soft_refs(1024, 9_000), vec![(10, 100)]);
    let _ = proc.process_references(&always_live, 1024, 0);
    proc.finish_pre_gc_cycle();
    assert_eq!(proc.soft_gc_clock_ms, 9_000);

    // Read since the last collection (9 000): idle 0, never condemned by
    // the policy even with no room left.
    proc.touch_soft_reference(10, 9_500);
    let _ = proc.process_references(&always_live, 0, 0);
    proc.finish_pre_gc_cycle();
    assert!(proc.condemn_idle_soft_refs(0, 20_000).is_empty());
    // ...and with 1 GB free after the last collection an unread
    // reference is kept for ~1 024 s of clock.
    let mut roomy = ReferenceProcessor::new_with_policy(1000);
    roomy.hotspot_soft_lru = true;
    roomy.discover_reference(ReferenceType::Soft, 30, 300, None);
    roomy.touch_soft_reference(30, 1_000);
    let _ = roomy.condemn_idle_soft_refs(1024, 2_000);
    let _ = roomy.process_references(&always_live, 1024, 0);
    roomy.finish_pre_gc_cycle();
    assert!(roomy.condemn_idle_soft_refs(0, 500_000).is_empty());
}

#[test]
fn condemn_idle_soft_refs_selects_only_the_idle_entries() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 1_000);
    proc.touch_soft_reference(20, 61_000);
    // 1 MB allocatable => a 1000 ms idle threshold; "now" is the mutator
    // clock the processor observed (61_000), so entry 10 is 60 s idle and
    // entry 20 is 0 s idle.
    assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
}

/// A roomy heap condemns nothing -- the LRU threshold scales with free
/// megabytes, so the same 60 s idle window is nowhere near it.
#[test]
fn condemn_idle_soft_refs_condemns_nothing_when_the_heap_is_roomy() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 1_000);
    proc.touch_soft_reference(20, 61_000);
    assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
    assert!(proc.soft_pre_nulled_active_pairs().is_empty());
}

/// The load-bearing one. The pre-mark verdict has to STAND even though
/// re-deriving it after the collection would say "keep": the collection
/// freed memory, so the post-GC threshold is larger, and the BTreeMap
/// range the ordinary scan walks is derived from that same larger
/// threshold -- here it selects no candidates at all. Without the
/// pre-condemned pass the referent is gone (its slot was nulled before the
/// mark) while the entry still reads as active, holding a dead address.
#[test]
fn a_pre_condemned_soft_ref_is_cleared_even_when_the_post_gc_threshold_would_keep_it() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 1_000);
    proc.touch_soft_reference(20, 61_000);
    assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

    // 10_000 MB free after the collection => a 10_000_000 ms threshold, so
    // the range scan's cutoff is 0 and it visits nothing.
    let result = proc.process_references(&always_dead, 10_000, 0);
    assert!(
        proc.soft_refs[0].cleared,
        "the pre-mark verdict must stand: this referent is already gone"
    );
    assert!(proc.soft_refs[0].enqueued);
    assert!(result.to_enqueue.contains(&(10, 900)));
    // The entry the policy KEPT is untouched, dead referent or not --
    // nothing nulled its slot, so the marker kept its referent alive and
    // `always_dead` is a fiction for it.
    assert!(!proc.soft_refs[1].cleared);
}

/// A condemned entry whose referent turned out to be strongly reachable is
/// kept, and is reported for the post-GC slot-0 restore.
#[test]
fn a_condemned_soft_ref_whose_referent_survived_is_offered_for_restore() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 1_000);
    proc.touch_soft_reference(20, 61_000);
    assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);

    let live = [100usize];
    proc.process_references(&live_set(&live), 1, 0);
    assert!(!proc.soft_refs[0].cleared);
    assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
}

/// The condemned set describes ONE collection. A later cycle that condemns
/// nothing must not inherit the previous cycle's verdict -- otherwise an
/// entry the policy has since decided to keep would be cleared the moment
/// its referent looked unmarked for any other reason.
#[test]
fn condemn_idle_soft_refs_resets_the_condemned_set_each_cycle() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 1_000);
    proc.touch_soft_reference(20, 61_000);
    assert_eq!(proc.condemn_idle_soft_refs(1, 0), vec![(10, 100)]);
    // Second cycle, roomy heap: nothing is condemned, so nothing carries
    // over.
    assert!(proc.condemn_idle_soft_refs(1024, 0).is_empty());
    assert!(proc.soft_pre_nulled_active_pairs().is_empty());
    proc.process_references(&always_dead, 1024, 0);
    assert!(
        !proc.soft_refs[0].cleared,
        "no slot was nulled this cycle, so no entry may be force-cleared"
    );
}

/// The last-ditch rule clears a reference the LRU policy would keep, which
/// is the whole point of having it: a program looping on its own
/// soft-referenced cache re-stamps the LRU clock every iteration, so its
/// idle window never opens however tight the heap becomes.
#[test]
fn the_last_ditch_rule_condemns_a_soft_ref_the_lru_policy_would_keep() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, Some(900));
    // Read just now: zero idle time, so the LRU policy keeps it even with
    // the heap reporting no free megabytes at all.
    proc.touch_soft_reference(10, 61_000);
    assert!(proc.condemn_idle_soft_refs(0, 61_000).is_empty());

    assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100)]);
    let result = proc.process_references(&always_dead, 0, 61_000);
    assert!(proc.soft_refs[0].cleared);
    assert!(result.to_enqueue.contains(&(10, 900)));
}

/// "Clear all soft references" means all the ones nothing else holds. A
/// condemned referent that is still strongly reachable was marked anyway
/// and must survive — otherwise the last-ditch collection would hand the
/// application a null for an object it can still reach by a strong path.
#[test]
fn the_last_ditch_rule_still_keeps_a_strongly_reachable_referent() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    proc.discover_reference(ReferenceType::Soft, 20, 200, None);
    proc.touch_soft_reference(10, 61_000);
    proc.touch_soft_reference(20, 61_000);
    assert_eq!(proc.condemn_all_soft_refs(), vec![(10, 100), (20, 200)]);
    let live = [100usize];
    proc.process_references(&live_set(&live), 0, 61_000);
    assert!(
        !proc.soft_refs[0].cleared,
        "referent 100 is strongly reachable"
    );
    assert!(
        proc.soft_refs[1].cleared,
        "referent 200 is only softly reachable"
    );
    assert_eq!(proc.soft_pre_nulled_active_pairs(), vec![(10, 100)]);
}

/// The ladder skips its extra collection when there is nothing to clear.
#[test]
fn has_active_soft_refs_tracks_the_uncleared_population() {
    let mut proc = ReferenceProcessor::new();
    assert!(!proc.has_active_soft_refs());
    proc.discover_reference(ReferenceType::Weak, 1, 2, None);
    assert!(!proc.has_active_soft_refs(), "a weak ref is not a soft ref");
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    assert!(proc.has_active_soft_refs());
    proc.condemn_all_soft_refs();
    proc.process_references(&always_dead, 0, 1);
    assert!(!proc.has_active_soft_refs());
}

/// gci i2/oo: a soft reference allocated since the last collection (not
/// yet bound to a row) still arms the last-ditch rung; an unbound weak or
/// phantom one does not.
#[test]
fn gci_i2oo_an_unbound_soft_reference_arms_the_last_ditch_rung() {
    let mut proc = ReferenceProcessor::new();
    proc.note_allocated_reference(ReferenceType::Weak, 0x1000, 7, false);
    proc.note_allocated_reference(ReferenceType::Phantom, 0x2000, 8, false);
    assert!(!proc.has_active_soft_refs(), "no soft reference allocated");
    proc.note_allocated_reference(ReferenceType::Soft, 0x3000, 9, false);
    assert!(proc.has_active_soft_refs(), "an unbound soft reference counts");
    let pending = proc.take_allocated_unbound();
    assert_eq!(pending.len(), 3);
    assert!(!proc.has_active_soft_refs(), "taken by the binder");
}

/// gci i13/oa item 2: the active soft referents, none while an unbound
/// soft reference is pending, and a cleared row drops out.
#[test]
fn gci_i13oa_active_soft_referents_are_known_only_when_every_row_is_bound() {
    let mut proc = ReferenceProcessor::new();
    assert_eq!(proc.active_soft_referents(), Some(vec![]));
    proc.discover_reference(ReferenceType::Weak, 1, 2, None);
    proc.discover_reference(ReferenceType::Soft, 10, 100, None);
    assert_eq!(proc.active_soft_referents(), Some(vec![100]));
    proc.note_allocated_reference(ReferenceType::Soft, 0x3000, 9, false);
    assert_eq!(proc.active_soft_referents(), None, "an unbound soft reference");
    let _ = proc.take_allocated_unbound();
    proc.condemn_all_soft_refs();
    proc.process_references(&always_dead, 0, 1);
    assert_eq!(proc.active_soft_referents(), Some(vec![]));
}

// 29. CleanerThread start/stop -----------------------------------------
#[test]
fn cleaner_thread_lifecycle() {
    let ct = CleanerThread::new();
    assert!(!ct.is_running());
    ct.start();
    assert!(ct.is_running());
    ct.stop();
    assert!(!ct.is_running());
}

// 30. FinalizerThread start/stop ---------------------------------------
#[test]
fn finalizer_thread_lifecycle() {
    let ft = FinalizerThread::new();
    assert!(!ft.is_running());
    ft.start();
    assert!(ft.is_running());
    ft.stop();
    assert!(!ft.is_running());
}

// 31. update_after_gc relocates finalization queue ----------------------
#[test]
fn update_after_gc_finalization_queue() {
    let mut proc = ReferenceProcessor::new();
    // process_references now DRAINS the finalization queue into its result
    // (once-only emission, see finalization_queue_fifo), so populate the
    // internal queue directly to isolate update_after_gc's relocation of a
    // still-pending entry (an object moved by a GC between enqueue and
    // consumption).
    proc.finalization_queue.push_back(100);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(100, 9999);
    proc.update_after_gc(&map);
    assert_eq!(proc.finalization_queue, vec![9999]);
}

// 32. Already-cleared entry skipped ------------------------------------
#[test]
fn already_cleared_skipped() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 1, 2, Some(500));
    proc.weak_refs[0].cleared = true;
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.stats.weak_refs_cleared, 0);
}

// 33. Phantom not re-enqueued ------------------------------------------
#[test]
fn phantom_not_re_enqueued() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 1, 2, Some(500));
    let r1 = proc.process_references(&always_dead, 100, 0);
    assert_eq!(r1.stats.phantom_refs_enqueued, 1);
    // Process again -- should not double-enqueue
    let r2 = proc.process_references(&always_dead, 100, 0);
    assert_eq!(r2.stats.phantom_refs_enqueued, 0);
}

// 34. Default trait impl -----------------------------------------------
#[test]
fn default_impl() {
    let proc = ReferenceProcessor::default();
    assert_eq!(proc.soft_ref_lru_policy_ms_per_mb, 1000);
}

// 35. to_finalize populated correctly ----------------------------------
#[test]
fn to_finalize_populated() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 1, 0xBEEF, None);
    let result = proc.process_references(&always_dead, 100, 0);
    assert_eq!(result.to_finalize, vec![0xBEEF]);
}

// ======================================================================
// M16-M20 fix verification tests
// ======================================================================

// 36-39 RETIRED (gcj j4/rf): the legacy address-keyed door they covered
// (`enqueue`'s resurrection refusal and cap, `was_finalized`,
// `cleanup_collected`) had no production caller and is gone;
// finalize-at-most-once is the processor row's.

// 40. FinalizerThread timeout constant
#[test]
fn finalizer_timeout_ms() {
    let ft = FinalizerThread::new();
    assert_eq!(ft.timeout_ms(), 2_000);
}

// 41, 42, 43. RETIRED 2026-09-21 with `ReferenceQueue`:
// `reference_queue_overflow_counter`, `reference_queue_remove_timeout_immediate`
// and `reference_queue_remove_timeout_empty`. 43 is the second of the two
// tests in the tree that asserted an elapsed duration produced by a wait
// that could not succeed; see the note on
// `remove_timeout_still_honours_its_deadline` above.

// 44. update_after_gc skips null target addresses (pointer validation)
#[test]
fn update_after_gc_skips_null_targets() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 100, 200, Some(300));
    let mut map = cratonvm_types::PointerMap::default();
    // Map reference_obj to 0 (invalid) — should be skipped
    map.insert(100usize, 0usize);
    // Map referent to valid address
    map.insert(200, 1200);
    // Map queue to 0 (invalid) — should be skipped
    map.insert(300, 0);
    proc.update_after_gc(&map);
    // reference_obj and queue should remain unchanged (null target skipped)
    assert_eq!(proc.weak_refs[0].reference_obj, 100);
    assert_eq!(proc.weak_refs[0].referent, 1200);
    assert_eq!(proc.weak_refs[0].queue_addr, Some(300));
}

// 45. update_after_gc skips null for finalization queue entries
#[test]
fn update_after_gc_finalization_null_skipped() {
    let mut proc = ReferenceProcessor::new();
    // Populate the queue directly — process_references now drains it (see
    // finalization_queue_fifo) — to isolate update_after_gc's null-target
    // skip behaviour.
    proc.finalization_queue.push_back(0xBEEF);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xBEEF, 0usize); // null target
    proc.update_after_gc(&map);
    // Should NOT have been relocated to 0
    assert_eq!(proc.finalization_queue, vec![0xBEEF]);
}

// 46. update_after_gc relocates across all reference types
#[test]
fn update_after_gc_all_ref_types() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 10, 20, Some(30));
    proc.discover_reference(ReferenceType::Weak, 40, 50, None);
    proc.discover_reference(ReferenceType::Phantom, 60, 70, Some(80));
    proc.discover_reference(ReferenceType::Cleaner, 90, 100, None);
    proc.discover_reference(ReferenceType::Finalizer, 110, 120, Some(130));

    let mut map = cratonvm_types::PointerMap::default();
    for old in (10..=130).step_by(10) {
        map.insert(old, old + 1000);
    }
    proc.update_after_gc(&map);

    assert_eq!(proc.soft_refs[0].reference_obj, 1010);
    assert_eq!(proc.soft_refs[0].referent, 1020);
    assert_eq!(proc.soft_refs[0].queue_addr, Some(1030));
    assert_eq!(proc.weak_refs[0].reference_obj, 1040);
    assert_eq!(proc.weak_refs[0].referent, 1050);
    assert_eq!(proc.phantom_refs[0].reference_obj, 1060);
    assert_eq!(proc.phantom_refs[0].queue_addr, Some(1080));
    assert_eq!(proc.cleaner_refs[0].reference_obj, 1090);
    assert_eq!(proc.finalizer_refs[0].reference_obj, 1110);
    assert_eq!(proc.finalizer_refs[0].queue_addr, Some(1130));
}

// 47. RETIRED 2026-09-21 with `ReferenceQueue`:
// `reference_queue_remove_blocking_with_data`. It exercised the ONE input
// for which `remove_blocking` returned without waiting — a queue that
// already had an entry — and so was green while the sixty-second cap it
// nominally covered was unreachable by any caller.

// ======================================================================
// V18: finalizer-reachable closure protects weak/soft refs
// ======================================================================

// 50. A weak ref whose referent is itself a to-be-finalized object must
//     NOT be cleared in the same cycle the finalizer is enqueued
//     (single-hop closure, no tracer needed).
#[test]
fn v18_weak_ref_to_finalizable_object_not_cleared() {
    let mut proc = ReferenceProcessor::new();
    // finalizable object lives at 0x200; nothing is marked.
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    // weak ref points directly at the finalizable object 0x200.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

    let result = proc.process_references(&always_dead, 64, 0);

    // The finalizer is enqueued for the object...
    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
    // ...but the weak ref to that same object must survive this cycle.
    assert_eq!(result.stats.weak_refs_cleared, 0);
    assert!(!proc.weak_refs[0].cleared);
}

// 51. With a tracer, a weak ref reachable only transitively (depth 2)
//     through a finalizable object is also protected.
#[test]
fn v18_transitive_closure_protects_weak_ref_with_tracer() {
    let mut proc = ReferenceProcessor::new();
    // 0x200 is finalizable; 0x500 is reachable only via 0x200's fields.
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

    // Tracer: from root 0x200, reach {0x200, 0x500}.
    let trace = |roots: &[usize]| -> Vec<usize> {
        let mut out = roots.to_vec();
        if roots.contains(&0x200) {
            out.push(0x500);
        }
        out
    };

    let result =
        proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);

    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
    // Transitively reachable referent must not be cleared this cycle.
    assert_eq!(result.stats.weak_refs_cleared, 0);
    assert!(!proc.weak_refs[0].cleared);
}

// 52. Residual gap: WITHOUT a tracer, a transitively-reachable (depth 2)
//     weak referent is still cleared. This documents the known gap that
//     requires an out-of-scope caller change to pass `trace_from`.
#[test]
fn v18_residual_gap_transitive_without_tracer() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    // weak referent 0x500 is NOT a direct finalizer referent.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, None);

    // Legacy entry point (no tracer): only direct referents protected.
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
}

// 53. Closure does not resurrect already-marked finalizers as roots, and
//     ordinary weak clearing of unrelated dead referents still happens.
#[test]
fn v18_unrelated_weak_ref_still_cleared() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    // weak ref to a totally unrelated dead object 0x999.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, None);

    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(proc.weak_refs[0].cleared);
}

// ======================================================================
// LRU-index leak fix: cleared soft refs are pruned from soft_ref_lru_index
// ======================================================================

// 54. Clearing a soft ref removes its key from the LRU index so the index
//     stays in sync with the live (uncleared) soft refs and is not
//     re-scanned on subsequent GCs.
#[test]
fn soft_ref_lru_index_pruned_on_clear() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    // One live key in the index after discovery.
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
    // threshold = 1000 * 2 = 2000; idle = 5000 > 2000 => clear.
    let result = proc.process_references(&always_dead, 2, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert!(proc.soft_refs[0].cleared);
    // The cleared entry's key must have been removed from the LRU index.
    assert_eq!(
        proc.soft_ref_lru_index.len(),
        0,
        "cleared soft ref left a stale LRU-index entry"
    );
}

// 55. A surviving soft ref keeps its LRU key while a sibling is cleared;
//     ordering/keys of survivors are preserved.
#[test]
fn soft_ref_lru_index_keeps_survivors() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // idx 0 referent dead, idx 1 referent live.
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    proc.discover_reference(ReferenceType::Soft, 101, 201, Some(301));
    assert_eq!(proc.soft_ref_lru_index.len(), 2);
    // Only referent 201 stays live; threshold small so the dead one clears.
    let live = [201usize];
    let result = proc.process_references(&live_set(&live), 1, 5000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert!(proc.soft_refs[0].cleared);
    assert!(!proc.soft_refs[1].cleared);
    // Exactly one (survivor) key remains, and it is the survivor's key.
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
    assert!(proc
        .soft_ref_lru_index
        .contains_key(&(proc.soft_refs[1].last_access_time_ms, 1)));
}

// 56. Re-processing after a clear does not re-walk the cleared entry: the
//     index no longer contains its key, and a second pass is a no-op.
#[test]
fn soft_ref_lru_index_no_rescan_after_clear() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 100, 200, Some(300));
    let r1 = proc.process_references(&always_dead, 2, 5000);
    assert_eq!(r1.stats.soft_refs_cleared, 1);
    assert_eq!(proc.soft_ref_lru_index.len(), 0);
    // Second pass: nothing to clear (already cleared) and index stays empty.
    let r2 = proc.process_references(&always_dead, 2, 5000);
    assert_eq!(r2.stats.soft_refs_cleared, 0);
    assert_eq!(proc.soft_ref_lru_index.len(), 0);
}

// ======================================================================
// PERF: O(1) touch_soft_reference via address index
// ======================================================================

// 57. touch_soft_reference re-keys the matching entry in the LRU index
//     using the O(1) address index (same effect the old linear scan had).
#[test]
fn touch_soft_reference_rekeys_lru_index() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, Some(0x100));
    proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, Some(0x200));
    // Initial keys are both at timestamp 0.
    assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
    assert!(proc.soft_ref_lru_index.contains_key(&(0, 1)));

    // Touch the second soft ref (idx 1) to time 5000.
    proc.touch_soft_reference(0xBB, 5000);
    assert_eq!(proc.soft_refs[1].last_access_time_ms, 5000);
    // Old key removed, new key inserted; first entry untouched.
    assert!(!proc.soft_ref_lru_index.contains_key(&(0, 1)));
    assert!(proc.soft_ref_lru_index.contains_key(&(5000, 1)));
    assert!(proc.soft_ref_lru_index.contains_key(&(0, 0)));
    // Index size unchanged (re-key, not add).
    assert_eq!(proc.soft_ref_lru_index.len(), 2);
}

// INT-8: reference_object_addresses returns weak/soft/phantom Reference
// OBJECT addresses (cleared included), never finalizer/cleaner entries.
#[test]
fn reference_object_addresses_covers_weak_soft_phantom_only() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, None);
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
    proc.discover_reference(ReferenceType::Finalizer, 0x40, 0x40, None);
    proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None);
    let mut addrs = proc.reference_object_addresses();
    addrs.sort_unstable();
    assert_eq!(addrs, vec![0x10, 0x20, 0x30]);
}

// Regression for the netty `System.gc()`-called-twice bug: a
// WeakReference discovered but not yet cleared must be reported: the list
// is the candidate set for the frame rescue
// (`push_pending_references_held_by_frames`), which keeps a liveness-dead
// local `r = new WeakReference<>(x, q)` alive. Once cleared, it is no
// longer a candidate -- see `pending_reference_object_addresses`'s doc.
#[test]
fn pending_reference_object_addresses_excludes_cleared_and_finalizer_cleaner() {
    let mut proc = ReferenceProcessor::new();
    // gc-common w1-d: queued, because a queue-less reference is no longer
    // a frame-rescue candidate at all — see `queueless_and_enqueued_references_are_not_rooted`.
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x97));
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, Some(0x98));
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
    proc.discover_reference(ReferenceType::Finalizer, 0x40, 0x40, None);
    proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None);

    let mut addrs = proc.pending_reference_object_addresses();
    addrs.sort_unstable();
    assert_eq!(
        addrs,
        vec![0x10, 0x20, 0x30],
        "every not-yet-cleared weak/soft/phantom Reference object must be reported"
    );

    // Clear the weak ref (its referent, 0x11, is judged dead) -- once
    // cleared, it should drop out of the pending set. Soft/phantom are
    // untouched by `process_weak_refs`, so they stay pending.
    proc.process_weak_refs(&|addr| addr != 0x11);
    let mut addrs = proc.pending_reference_object_addresses();
    addrs.sort_unstable();
    assert_eq!(
        addrs,
        vec![0x20, 0x30],
        "a cleared reference no longer needs synthetic rooting; untouched ones still do"
    );
}

// INT-8: cleaner_pending_object_addresses excludes self-referent
// (finalizer-style) registrations and already-fired actions.
#[test]
fn cleaner_pending_object_addresses_excludes_self_and_fired() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Cleaner, 0x50, 0x51, None); // pending
    proc.discover_reference(ReferenceType::Cleaner, 0x60, 0x60, None); // self-referent
    proc.discover_reference(ReferenceType::Cleaner, 0x70, 0x71, None); // will fire
                                                                       // Fire 0x70's action: its referent 0x71 is dead.
    let result = proc.process_references(&|addr| addr != 0x71, 64, 0);
    assert!(result.cleaner_actions.contains(&0x70));
    let addrs = proc.cleaner_pending_object_addresses();
    assert_eq!(addrs, vec![0x50]);
}

// INT-8: soft_survivor_referents returns kept (uncleared, unenqueued)
// soft referents — the set the remark driver must resurrect.
#[test]
fn soft_survivor_referents_tracks_kept_entries() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
    proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None);
    // Plenty of free heap: policy keeps both even though 0x31 is dead.
    let _ = proc.process_references(&|addr| addr != 0x31, 1024, 0);
    let mut kept = proc.soft_survivor_referents();
    kept.sort_unstable();
    // Whatever the policy decided, the accessor must mirror the
    // uncleared set exactly.
    let expected: Vec<usize> = proc
        .soft_refs
        .iter()
        .filter(|e| !e.cleared && !e.enqueued)
        .map(|e| e.referent)
        .collect();
    let mut expected_sorted = expected;
    expected_sorted.sort_unstable();
    assert_eq!(kept, expected_sorted);
    assert!(kept.contains(&0x21), "live soft referent is always kept");
}

// 58. touch on an unknown address is a no-op (O(1) miss, no scan needed).
#[test]
fn touch_soft_reference_unknown_addr_noop() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
    proc.touch_soft_reference(0xDEAD, 5000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
}

// 59. touch with an unadvanced timestamp is a no-op (preserves the
//     "only re-insert when the timer actually advances" fast path).
#[test]
fn touch_soft_reference_same_timestamp_noop() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
    proc.touch_soft_reference(0xAA, 7000);
    // Touching again with the same time must not churn the index.
    proc.touch_soft_reference(0xAA, 7000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 7000);
    assert!(proc.soft_ref_lru_index.contains_key(&(7000, 0)));
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
}

// 60. After remove_collected compacts soft_refs, the address index is
//     rebuilt so touch still resolves the (now-shifted) survivor.
#[test]
fn touch_soft_reference_after_remove_collected() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // idx 0 (0xAA) will be collected; idx 1 (0xBB) survives -> shifts to 0.
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
    proc.discover_reference(ReferenceType::Soft, 0xBB, 0x20, None);
    let live = [0xBBusize];
    proc.remove_collected(&live_set(&live));
    assert_eq!(proc.soft_refs.len(), 1);
    assert_eq!(proc.soft_refs[0].reference_obj, 0xBB);

    // Touch the survivor at its NEW position; LRU key must use idx 0.
    proc.touch_soft_reference(0xBB, 9000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 9000);
    assert!(proc.soft_ref_lru_index.contains_key(&(9000, 0)));
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
    // The collected entry's address no longer resolves.
    proc.touch_soft_reference(0xAA, 9999);
    assert_eq!(proc.soft_ref_lru_index.len(), 1);
}

// 61. After update_after_gc relocates reference_obj, the address index is
//     rebuilt so touch resolves the NEW address (not the stale one).
#[test]
fn touch_soft_reference_after_update_after_gc() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, None);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xAAusize, 0xCCusize); // reference_obj 0xAA -> 0xCC
    proc.update_after_gc(&map);
    assert_eq!(proc.soft_refs[0].reference_obj, 0xCC);

    // Old address must no longer resolve; new address must.
    proc.touch_soft_reference(0xAA, 1234);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
    proc.touch_soft_reference(0xCC, 1234);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 1234);
    assert!(proc.soft_ref_lru_index.contains_key(&(1234, 0)));
}

// ======================================================================
// JLS reachability ordering (strong > soft > weak): a weak ref must not be
// cleared while its referent is still softly reachable through a surviving
// soft reference.
// ======================================================================

// 62. A weak ref to an object reachable ONLY transitively (depth 2) through
//     a surviving soft referent must NOT be cleared (requires a tracer).
#[test]
fn weak_ref_kept_alive_by_surviving_soft_referent_transitive() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // Soft referent 0x200 is recently accessed, so Phase 1 retains it.
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
    proc.touch_soft_reference(0x100, 5000);
    // Weak referent 0x500 is reachable only via 0x200's fields (depth 2).
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x500, Some(0x400));

    // Tracer: from the surviving soft referent 0x200 reach {0x200, 0x500}.
    let trace = |roots: &[usize]| -> Vec<usize> {
        let mut out = roots.to_vec();
        if roots.contains(&0x200) {
            out.push(0x500);
        }
        out
    };

    // free_heap large + recent access => soft ref survives Phase 1.
    let result =
        proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

    // The soft ref survived...
    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert!(!proc.soft_refs[0].cleared);
    // ...so the transitively-reachable weak referent must NOT be cleared.
    assert_eq!(result.stats.weak_refs_cleared, 0);
    assert!(!proc.weak_refs[0].cleared);
}

// 63. A weak ref pointing DIRECTLY at a surviving soft referent must not be
//     cleared even without a tracer (single-hop soft keep-alive).
#[test]
fn weak_ref_to_surviving_soft_referent_not_cleared_no_tracer() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // Soft referent 0x200 retained (recent access, plenty of free heap).
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
    proc.touch_soft_reference(0x100, 5000);
    // Weak ref points straight at the still-soft-reachable object 0x200.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

    let result = proc.process_references(&always_dead, 64, 5000);

    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert_eq!(result.stats.weak_refs_cleared, 0);
    assert!(!proc.weak_refs[0].cleared);
}

// 64. When the soft ref is itself CLEARED in Phase 1 (idle past the LRU
//     threshold), its dead referent must NOT protect a weak ref to it.
#[test]
fn weak_ref_not_protected_when_soft_ref_cleared() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // last_access_time_ms = 0, current = 5000, free = 1 MB => threshold
    // 1000; idle 5000 > 1000 => soft ref cleared in Phase 1.
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, Some(0x400));

    let result = proc.process_references(&always_dead, 1, 5000);

    assert_eq!(result.stats.soft_refs_cleared, 1);
    assert!(proc.soft_refs[0].cleared);
    // The soft ref no longer keeps 0x200 alive, so the weak ref clears.
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(proc.weak_refs[0].cleared);
}

// 65. The soft-survivor closure does not over-retain: an unrelated dead
//     weak referent is still cleared while a soft ref survives.
#[test]
fn unrelated_weak_ref_still_cleared_with_surviving_soft_ref() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
    proc.touch_soft_reference(0x100, 5000); // soft ref survives
                                            // Weak ref to a totally unrelated dead object, not reachable from 0x200.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x999, Some(0x400));

    let trace = |roots: &[usize]| -> Vec<usize> { roots.to_vec() }; // 0x200 -> {0x200}
    let result =
        proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 5000);

    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert_eq!(result.stats.weak_refs_cleared, 1);
    assert!(proc.weak_refs[0].cleared);
}

// ======================================================================
// SOFT-POLICY FIX (2026-07-26): the LRU must survive a caller that has no
// clock. Both production call sites in `vm/src/runtime/interpreter.rs`
// pass `current_time_ms == 0`, which used to make `process_soft_refs`
// structurally incapable of clearing anything. See the
// `last_observed_clock_ms` field doc.
// ======================================================================

// 66. With a `0` caller clock, the mutator clock observed through
//     `touch_soft_reference` is used instead, so an idle soft ref clears.
#[test]
fn soft_policy_uses_mutator_clock_when_caller_passes_zero() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, Some(0x180));
    // `SoftReference.get()` stamps a wall-clock time on the entry.
    proc.touch_soft_reference(0x100, 1_000_000);
    // Time passes; another (unrelated / unknown) touch advances only the
    // processor's clock. This also pins that the clock is recorded BEFORE
    // the unknown-address early return.
    proc.touch_soft_reference(0xDEAD, 1_100_000);

    // Exactly what the interpreter passes today.
    let result = proc.process_references(&always_dead, 64, 0);

    // threshold = 1000 ms/MB * 64 MB = 64_000; idle = 100_000 > 64_000.
    assert_eq!(
        result.stats.soft_refs_cleared, 1,
        "soft-ref LRU must run even when the collector supplies no clock"
    );
    assert!(proc.soft_refs[0].cleared);
}

// 67. A soft ref created after the clock is known carries a *creation*
//     timestamp (like `SoftReference`'s constructor), so it is not
//     instantly stale against a wall-clock "now". Non-soft types keep
//     stamping 0 — the field is meaningless for them.
#[test]
fn soft_policy_creation_stamp_protects_fresh_ref() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // Establish the mutator clock first (some earlier SoftReference.get()).
    proc.touch_soft_reference(0xDEAD, 5_000_000);

    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    assert_eq!(
        proc.soft_refs[0].last_access_time_ms, 5_000_000,
        "a freshly discovered soft ref must be stamped at creation"
    );
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x400, None);
    assert_eq!(
        proc.weak_refs[0].last_access_time_ms, 0,
        "only soft refs carry an LRU timestamp"
    );

    // Production (64, 0): idle == 0, so the fresh ref must survive.
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert!(!proc.soft_refs[0].cleared);
}

// 68. A caller that DOES supply a coherent clock keeps full control: the
//     larger of the two wins, so ZGC's backend and every test behave
//     exactly as before the fix.
#[test]
fn soft_policy_caller_clock_dominates_observed_clock() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 1_000);

    // threshold = 1000 * 1 = 1000; idle = 1_000_000 - 1_000 > 1000.
    let result = proc.process_references(&always_dead, 1, 1_000_000);
    assert_eq!(result.stats.soft_refs_cleared, 1);
}

// ======================================================================
// SPEC FIX (2026-07-26): phases 3-4 honour the soft/finalizer closures.
// ======================================================================

// 69. A `Cleaner` must NOT fire while its referent is still softly
//     reachable. The live case is `DirectByteBuffer`: firing here frees
//     native memory still reachable through `SoftReference.get()`.
#[test]
fn cleaner_not_fired_while_referent_softly_reachable() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // Soft ref retains 0x200 (recently touched, ample headroom).
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    // A Cleaner registered against that very object.
    proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x200, None);

    let result = proc.process_references(&always_dead, 64, 5_000);

    assert_eq!(result.stats.soft_refs_cleared, 0, "soft ref must survive");
    assert!(
        result.cleaner_actions.is_empty(),
        "cleaner fired for a still-soft-reachable referent"
    );
    assert!(!proc.cleaner_refs[0].cleared);
}

// 70. `finalize()` must NOT be scheduled while the object is still softly
//     reachable.
#[test]
fn finalizer_not_enqueued_while_referent_softly_reachable() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x200, None);

    let result = proc.process_references(&always_dead, 64, 5_000);

    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert_eq!(result.stats.finalizer_refs_enqueued, 0);
    assert!(result.to_finalize.is_empty());
}

// 71. A phantom must wait until the referent has been finalized, then fire
//     on the following cycle (bounded one-cycle delay, never permanent).
#[test]
fn phantom_waits_one_cycle_for_finalization() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    proc.discover_reference(ReferenceType::Phantom, 0x300, 0x200, Some(0x400));

    let r1 = proc.process_references(&always_dead, 64, 0);
    assert_eq!(r1.stats.finalizer_refs_enqueued, 1);
    assert_eq!(
        r1.stats.phantom_refs_enqueued, 0,
        "phantom must not fire before finalization"
    );

    // The finalizer entry is now flagged enqueued, so it stops rooting the
    // object and the phantom is delivered.
    let r2 = proc.process_references(&always_dead, 64, 0);
    assert_eq!(r2.stats.phantom_refs_enqueued, 1);
    assert!(r2.to_enqueue.iter().any(|&(r, q)| r == 0x300 && q == 0x400));
}

// 72. Mutually-reachable finalizable objects must BOTH be enqueued in the
//     same cycle. This pins the deliberate asymmetry: the finalizer phase
//     folds in the soft closure but NOT the finalizer closure, otherwise
//     each object would block the other forever.
#[test]
fn mutually_reachable_finalizers_both_enqueued() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x200, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x300, 0x400, None);

    // 0x200 and 0x400 reference each other.
    let trace = |roots: &[usize]| -> Vec<usize> {
        let mut out = roots.to_vec();
        if roots.contains(&0x200) {
            out.push(0x400);
        }
        if roots.contains(&0x400) {
            out.push(0x200);
        }
        out
    };

    let result =
        proc.process_references_with_finalizer_trace(&always_dead, Some(&trace), 64, 0);
    assert_eq!(
        result.stats.finalizer_refs_enqueued, 2,
        "finalizer-reachability must not block finalization"
    );
}

// 73. No over-retention: an unrelated dead cleaner/phantom referent still
//     fires while a soft reference survives elsewhere.
#[test]
fn unrelated_cleaner_and_phantom_still_fire_with_surviving_soft_ref() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    // Unrelated dead referents.
    proc.discover_reference(ReferenceType::Cleaner, 0x300, 0x999, None);
    proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

    let result = proc.process_references(&always_dead, 64, 5_000);

    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert!(result.cleaner_actions.contains(&0x300));
    assert_eq!(result.stats.phantom_refs_enqueued, 1);
}

// ======================================================================
// 74-83. gengc-round3, lane `refdriver`: the ORDER of
// `remove_collected` relative to the processing phases.
//
// These tests exist in PAIRS. The `..._post_pass_..` half pins what the
// historical order (process, THEN prune) does; the `..._pre_pass_..` half
// states what the order the generational driver now runs (prune, THEN
// process) does instead. Reading a pair side by side is the whole
// behavioural diff of the change, which is why they are written as a pair
// rather than as a single updated expectation.
//
// The shared scenario models the real driver rather than `always_dead`:
// `survivor` is the collector's survivor predicate, and the point of every
// one of these cases is that a REFERENCE OBJECT and its REFERENT get
// different answers from it.
// ======================================================================

/// Only the addresses in `live` survived the collection. Modelled on the
/// driver's `is_marked` (`watched_pre_gc_addr_survived`), which answers
/// per-address and is emphatically not `always_dead`.
fn survivor(live: &[usize]) -> impl Fn(usize) -> bool + '_ {
    move |addr: usize| live.contains(&addr)
}

// 74. THE GAP (gengc-mark). Historical order: a `SoftReference` whose own
//     instance died still seeds `soft_live`, so a `WeakReference` to the
//     same (dead) referent is left uncleared and `get()` hands back
//     reclaimable storage.
#[test]
fn dead_softref_instance_roots_the_weak_closure_under_post_pass_ordering() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // The SoftReference instance 0x100 is itself dead; so is its referent.
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    // A live WeakReference aimed at the same object.
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

    // Nothing survived except the WeakReference object itself.
    let live = [0x300usize];
    let result = proc.process_references(&survivor(&live), 64, 5_000);
    proc.remove_collected(&survivor(&live));

    // PINNED DEFECT: the weak ref was NOT cleared, because the dead soft
    // row survived into the soft-survivor-root construction.
    assert_eq!(
        result.stats.weak_refs_cleared, 0,
        "pins the historical (defective) behaviour this change fixes"
    );
    // ...and the soft row is only dropped afterwards, too late to matter.
    assert!(proc.soft_refs.is_empty());
}

// 75. The same scenario under the driver's new order: the dead soft row is
//     gone before Phase 1, so nothing roots the weak closure and the weak
//     reference clears. This is the gap page's requested test.
#[test]
fn dead_softref_instance_no_longer_roots_the_weak_closure_under_pre_pass_ordering() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

    let live = [0x300usize];
    proc.remove_collected_reference_objects(&survivor(&live));
    // The dead SoftReference's row never reaches Phase 1.
    assert!(proc.soft_refs.is_empty());

    let result = proc.process_references(&survivor(&live), 64, 5_000);

    assert_eq!(
        result.stats.weak_refs_cleared, 1,
        "a weak ref to an object that is not softly reachable must clear"
    );
    assert!(proc.weak_refs[0].cleared);
}

// 76. THE FENCE, restated against the pre-pass. A LIVE `SoftReference`
//     whose referent the LRU policy retains is unmarked BY CONSTRUCTION —
//     that is the entire point of a soft reference — and the pre-pass must
//     not touch it. This is the case both previous attempts got wrong, and
//     it is the reason the prune is keyed on the reference object's
//     survival rather than on anything about the referent.
#[test]
fn pre_pass_keeps_a_live_softref_whose_policy_retained_referent_is_unmarked() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    // The SoftReference instance survives; its referent does not appear in
    // the survivor set at all (nothing strongly reaches it).
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);

    let live = [0x100usize, 0x300usize];
    proc.remove_collected_reference_objects(&survivor(&live));
    assert_eq!(proc.soft_refs.len(), 1, "a live SoftReference must survive");

    // Ample headroom + recent access => Phase 1 retains the referent.
    let result = proc.process_references(&survivor(&live), 64, 5_000);

    assert_eq!(result.stats.soft_refs_cleared, 0);
    assert_eq!(
        result.stats.weak_refs_cleared, 0,
        "JLS strong > soft > weak: a softly-reachable referent's weak ref \
         must not be cleared"
    );
}

// 77-78. PHANTOM. Historical order: a `PhantomReference` whose own
//     instance died in this cycle is still enqueued (against a queue
//     nobody can poll, through an address the driver then refuses to
//     write). Pre-pass order: the row is gone, so it is not enqueued at
//     all — which is what HotSpot does, since it never DISCOVERS an
//     unreachable Reference in the first place.
#[test]
fn dead_phantom_instance_still_enqueues_under_post_pass_ordering() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

    let live: [usize; 0] = [];
    let result = proc.process_references(&survivor(&live), 64, 0);

    assert_eq!(result.stats.phantom_refs_enqueued, 1);
    assert!(proc.phantom_refs[0].enqueued);
}

#[test]
fn dead_phantom_instance_is_dropped_before_it_enqueues_under_pre_pass_ordering() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 0x500, 0x888, Some(0x600));

    let live: [usize; 0] = [];
    proc.remove_collected_reference_objects(&survivor(&live));
    let result = proc.process_references(&survivor(&live), 64, 0);

    assert!(proc.phantom_refs.is_empty());
    assert_eq!(result.stats.phantom_refs_enqueued, 0);
    assert!(result.to_enqueue.is_empty());
}

// 79-80. `jdk.internal.ref.Cleaner` (a phantom that RUNS). Same pair. The
//     emitted action is the CLEANER OBJECT's address, so when the cleaner
//     object itself died the historical order emits an action the VM
//     driver then declines (`is_stale_young`); the pre-pass order never
//     emits it.
#[test]
fn dead_cleaner_phantom_still_emits_an_action_under_post_pass_ordering() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_phantom_cleaner(0x700, 0x888, None);

    let live: [usize; 0] = [];
    let result = proc.process_references(&survivor(&live), 64, 0);

    assert!(result.cleaner_actions.contains(&0x700));
}

#[test]
fn dead_cleaner_phantom_emits_no_action_under_pre_pass_ordering() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_phantom_cleaner(0x700, 0x888, None);

    let live: [usize; 0] = [];
    proc.remove_collected_reference_objects(&survivor(&live));
    let result = proc.process_references(&survivor(&live), 64, 0);

    assert!(result.cleaner_actions.is_empty());
}

// 81. A LIVE cleaner whose referent died still fires under the pre-pass —
//     the case that actually matters for `DirectByteBuffer`. The pre-pass
//     must not cost a single real cleaner delivery.
#[test]
fn live_cleaner_phantom_with_dead_referent_still_fires_under_pre_pass_ordering() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_phantom_cleaner(0x700, 0x888, None);

    let live = [0x700usize];
    proc.remove_collected_reference_objects(&survivor(&live));
    let result = proc.process_references(&survivor(&live), 64, 0);

    assert!(result.cleaner_actions.contains(&0x700));
}

// 82. THE ONE THAT MAKES THE NARROW PASS NECESSARY. The VM registers a
//     finalizable object as `reference_obj == referent == the object`
//     (`SharedVm::register_finalizable`), and `Finalizer.register` does the
//     same through the `Cleaner` wire type. For those rows "the reference
//     object did not survive" IS "the object is dead", i.e. exactly when
//     the row must fire. A pre-pass over ALL FIVE lists would therefore
//     switch finalization off; `remove_collected_reference_objects` leaves
//     both lists alone, and `finalize()` is still scheduled.
#[test]
fn pre_pass_never_prunes_self_referential_finalizer_and_cleaner_rows() {
    let mut proc = ReferenceProcessor::new();
    // Exactly the shape `register_finalizable` produces.
    proc.discover_reference(ReferenceType::Finalizer, 0xF00, 0xF00, None);
    // Exactly the shape the `Finalizer.register` native produces (wire 3).
    proc.discover_reference(ReferenceType::Cleaner, 0xC00, 0xC00, None);

    let live: [usize; 0] = [];
    proc.remove_collected_reference_objects(&survivor(&live));

    assert_eq!(
        proc.finalizer_refs.len(),
        1,
        "finalization must not be pruned away"
    );
    assert_eq!(proc.cleaner_refs.len(), 1);

    let result = proc.process_references(&survivor(&live), 64, 0);
    assert_eq!(result.stats.finalizer_refs_enqueued, 1);
    assert!(result.to_finalize.contains(&0xF00));
    assert!(result.cleaner_actions.contains(&0xC00));

    // The full `remove_collected` at the driver's tail is what retires
    // them, AFTER the verdict has been taken. That ordering is the
    // contract, and it is unchanged.
    proc.remove_collected(&survivor(&live));
    assert!(proc.finalizer_refs.is_empty());
    assert!(proc.cleaner_refs.is_empty());
}

// 83. DOUBLE-CALL SAFETY. ZGC still runs `remove_collected` at its own
//     tail and the VM driver runs it at the end of every cycle, so the
//     pre-pass means the same predicate is applied twice (three times for
//     ZGC). It must be a no-op after the first, and in particular the soft
//     position indices must stay consistent — a stale `(timestamp, idx)`
//     key is an out-of-bounds index in `process_soft_refs`, not a wrong
//     answer.
#[test]
fn remove_collected_is_idempotent_across_the_pre_pass_and_the_tail_call() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.discover_reference(ReferenceType::Soft, 0x110, 0x210, None);
    proc.discover_reference(ReferenceType::Soft, 0x120, 0x220, None);
    proc.touch_soft_reference(0x110, 5_000);
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
    proc.discover_reference(ReferenceType::Phantom, 0x400, 0x220, None);
    proc.discover_reference(ReferenceType::Finalizer, 0xF00, 0xF00, None);

    let live = [0x110usize, 0x300usize, 0xF00usize];

    proc.remove_collected_reference_objects(&survivor(&live));
    let after_pre = (
        proc.soft_refs.len(),
        proc.weak_refs.len(),
        proc.phantom_refs.len(),
    );
    assert_eq!(after_pre, (1, 1, 0));

    // Second application of the same predicate: nothing more to remove.
    proc.remove_collected_reference_objects(&survivor(&live));
    assert_eq!(
        (
            proc.soft_refs.len(),
            proc.weak_refs.len(),
            proc.phantom_refs.len()
        ),
        after_pre
    );

    // The driver's tail call, twice (the second stands in for ZGC's own).
    proc.remove_collected(&survivor(&live));
    proc.remove_collected(&survivor(&live));
    assert_eq!(
        (
            proc.soft_refs.len(),
            proc.weak_refs.len(),
            proc.phantom_refs.len()
        ),
        after_pre
    );
    assert_eq!(proc.finalizer_refs.len(), 1);

    // Every surviving soft entry is addressable by BOTH indices at its
    // current position, and neither index names a position that no longer
    // exists.
    assert_eq!(proc.soft_ref_lru_index.len(), proc.soft_refs.len());
    for (&(_, key_idx), &val_idx) in proc.soft_ref_lru_index.iter() {
        assert_eq!(key_idx, val_idx);
        assert!(key_idx < proc.soft_refs.len(), "stale LRU index position");
    }
    proc.touch_soft_reference(0x110, 6_000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 6_000);

    // And the phases still run against the compacted Vec.
    let _ = proc.process_references(&survivor(&live), 64, 6_000);
}

// ======================================================================
// gc-common w1-d (2026-09-23)
// ======================================================================

// W1D-1. Only an UNSETTLED reference WITH A QUEUE is in the candidate set
//        for the frame rescue (it was the synthetic root set until w2-b):
//        an enqueued phantom is delivered (and was rooted forever before),
//        a queue-less reference has nothing to deliver.
#[test]
fn queueless_and_enqueued_references_are_not_rooted() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, None);
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x99));
    proc.discover_reference(ReferenceType::Weak, 0x40, 0x41, Some(0x98));
    let mut addrs = proc.pending_reference_object_addresses();
    addrs.sort_unstable();
    assert_eq!(
        addrs,
        vec![0x30, 0x40],
        "queue-less rows are not frame-rescue candidates"
    );

    // The phantom's referent dies: it is enqueued (never `cleared`), and
    // from then on it must stop being a candidate.
    let result = proc.process_references(&|a| a != 0x31, 64, 0);
    assert!(result.to_enqueue.contains(&(0x30, 0x99)));
    assert_eq!(proc.pending_reference_object_addresses(), vec![0x40]);
}

// W1D-2. The restore gate: only slots the pre-GC pass reported nulling.
#[test]
fn the_restore_gate_names_exactly_the_slots_the_pass_nulled() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
    let _ = proc.weak_phantom_active_triples();
    // The pass found 0x10's slot non-null (and nulled it); 0x20's was null.
    // Class id 0 (java.lang.Object) must still count as nulled.
    proc.stamp_referent_class(0x10, 0);
    assert!(proc.was_nulled_by_pre_gc_pass(0x10));
    assert!(!proc.was_nulled_by_pre_gc_pass(0x20));
    // A new pass forgets the previous one.
    let _ = proc.weak_phantom_active_triples();
    assert!(!proc.was_nulled_by_pre_gc_pass(0x10));
}

// W1D-3. `Reference.clear()` then the referent dies: HotSpot never
//        enqueues a reference whose referent the program cleared.
#[test]
fn a_weak_ref_the_application_cleared_is_retired_and_never_enqueued() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
    let _ = proc.weak_phantom_active_triples();
    proc.stamp_referent_class(0x20, 7); // only 0x20's slot held its referent
    assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
    // Both referents die this cycle.
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(
        result.to_enqueue,
        vec![(0x20, 0x90)],
        "only the reference the collector cleared may be delivered"
    );
    // The retired row emits no slot write either.
    assert_eq!(proc.take_newly_cleared(), vec![0x20]);
    // And it is no longer a synthetic root.
    assert!(proc.pending_reference_object_addresses().is_empty());
    // Idempotent.
    assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 0);
}

// W1D-4. Retirement never touches a phantom, and is inert with no pass.
#[test]
fn retirement_spares_phantoms_and_needs_a_pass() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
    assert_eq!(
        proc.retire_entries_the_pre_gc_pass_found_cleared(),
        0,
        "no pass ran: nothing was examined"
    );
    let _ = proc.weak_phantom_active_triples();
    // Nothing stamped: the weak row is retired, the phantom is not.
    assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.to_enqueue, vec![(0x30, 0x90)]);
}

// W1D-5. A condemned soft ref whose slot was already null is retired and
//        loses its LRU key.
#[test]
fn a_condemned_soft_ref_found_already_null_is_retired() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, Some(0x90));
    let _ = proc.weak_phantom_active_triples();
    assert_eq!(proc.condemn_all_soft_refs(), vec![(0x10, 0x11)]);
    assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
    assert!(proc.soft_refs[0].cleared && proc.soft_refs[0].enqueued);
    assert!(proc.soft_ref_lru_index.is_empty());
    let result = proc.process_references(&always_dead, 0, 0);
    assert!(result.to_enqueue.is_empty());
    assert_eq!(result.stats.soft_refs_cleared, 0);
}

// W1D-6. A refused restore becomes a collector clear, delivered by the
//        NEXT round, and survives a relocation in between.
#[test]
fn a_refused_restore_is_cleared_and_enqueued_on_the_next_round() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
    assert!(proc.clear_after_refused_restore(0x10));
    assert!(
        !proc.clear_after_refused_restore(0x30),
        "a phantom must never be fired by a refusal"
    );
    assert!(!proc.clear_after_refused_restore(0x10), "once only");
    assert_eq!(
        proc.take_newly_cleared(),
        Vec::<usize>::new(),
        "slot is already null"
    );

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10, 0x50);
    map.insert(0x90, 0x95);
    proc.update_after_gc(&map);

    let result = proc.process_references(&always_live, 64, 0);
    assert_eq!(result.to_enqueue, vec![(0x50, 0x95)]);
    assert!(!proc.phantom_refs[0].enqueued, "phantom untouched");
}

// W1D-7. A soft refusal takes the same route.
#[test]
fn a_refused_soft_restore_is_cleared() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, Some(0x90));
    assert!(proc.clear_after_refused_restore(0x10));
    assert!(proc.soft_refs[0].cleared);
    assert!(proc.soft_ref_lru_index.is_empty());
    let result = proc.process_references(&always_live, 64, 0);
    assert_eq!(result.to_enqueue, vec![(0x10, 0x90)]);
}

// W1D-8. `finish_pre_gc_cycle` stops a young pause's condemnation from
//        being read by a later pass that has none of its own (G1 remark).
#[test]
fn a_finished_cycle_leaves_no_condemnation_for_the_next_pass() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
    proc.touch_soft_reference(0x10, 1_000_000);
    // Young pause: heap tight, entry condemned; referent strongly
    // reachable, so it is kept and restored.
    assert_eq!(
        proc.condemn_idle_soft_refs(0, 1_000_001),
        vec![(0x10, 0x11)]
    );
    let _ = proc.process_references(&always_live, 0, 1_000_001);
    assert!(!proc.soft_refs[0].cleared);
    proc.finish_pre_gc_cycle();
    // "Remark": slot hidden so the referent reads unmarked, heap roomy,
    // entry freshly read — the LRU policy keeps it.
    let _ = proc.process_references(&always_dead, 1024, 1_000_002);
    assert!(
        !proc.soft_refs[0].cleared,
        "a stale condemnation must not clear a soft ref the policy keeps"
    );
}

// W1D-9. The prune's index rebuild does not resurrect cleared soft keys,
//        and a prune that removes nothing rebuilds nothing.
#[test]
fn remove_collected_keeps_cleared_soft_rows_out_of_the_lru_index() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x21, None);
    proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None);
    // Clear 0x10 (referent dead, idle, no headroom).
    let _ = proc.process_references(&|a| a != 0x11, 0, 5_000);
    assert!(proc.soft_refs[0].cleared);
    // Drop 0x30's Reference object: a real removal, so a rebuild.
    proc.remove_collected(&|a| a != 0x30);
    assert_eq!(proc.soft_refs.len(), 2);
    assert_eq!(
        proc.soft_ref_lru_index.len(),
        1,
        "cleared row got its key back"
    );
    assert!(proc.soft_ref_lru_index.contains_key(&(0, 1)));
    // The address index still covers every row.
    proc.touch_soft_reference(0x10, 6_000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 6_000);
    // A prune that removes nothing leaves the indices alone.
    let before = proc.soft_ref_lru_index.clone();
    proc.remove_collected(&always_live);
    assert_eq!(proc.soft_ref_lru_index, before);
}

// W1D-10. A finalized object's address is recycled: a fresh object there
//         must still be finalized.
#[test]
fn a_recycled_address_is_finalized_again_through_enqueue_unfinalized() {
    let ft = FinalizerThread::new();
    assert!(ft.enqueue_unfinalized(0xA0));
    assert_eq!(ft.dequeue(), Some(0xA0));
    assert!(
        ft.enqueue_unfinalized(0xA0),
        "a never-enqueued row is fresh"
    );
    assert!(
        !ft.enqueue_unfinalized(0xA0),
        "queue duplicates still refused"
    );
    assert_eq!(ft.pending_count(), 1);
}

// W1D-12. Deferred cleaner actions are visible for rooting.
#[test]
fn cleaner_pending_addresses_lists_the_queue() {
    let ct = CleanerThread::new();
    ct.submit_action(0x10);
    ct.submit_action(0x20);
    assert_eq!(ct.pending_addresses(), vec![0x10, 0x20]);
    let _ = ct.drain_actions();
    assert!(ct.pending_addresses().is_empty());
}

// -----------------------------------------------------------------------
// gc-common w2-f — the application settles a reference itself.
// -----------------------------------------------------------------------

// W2F-1. `phantom.clear()` then the referent dies: never delivered
//        (HotSpot never discovers a cleared reference). The pre-GC
//        inference could not do this for a phantom.
#[test]
fn an_application_cleared_phantom_is_never_delivered() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Phantom, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Phantom, 0x20, 0x21, Some(0x90));
    assert!(proc.retire_by_application(0x10));
    assert!(!proc.retire_by_application(0x10), "idempotent");
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.to_enqueue, vec![(0x20, 0x90)]);
    assert_eq!(
        proc.pending_reference_object_addresses(),
        Vec::<usize>::new(),
        "a settled phantom is no longer a synthetic root"
    );
}

// W2F-2. G1's final remark runs `process_references` with no
//        pre-collection pass: a weak/soft reference the application
//        cleared since the last pause must not be cleared-and-enqueued
//        there.
#[test]
fn a_reference_cleared_before_a_remark_is_not_enqueued_by_it() {
    let mut proc = ReferenceProcessor::new_with_policy(0);
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, Some(0x90));
    assert!(proc.retire_by_application(0x10));
    assert!(proc.retire_by_application(0x30));
    // No `weak_phantom_active_triples` / condemnation: the remark shape.
    let result = proc.process_references(&always_dead, 0, 10_000);
    assert!(result.to_enqueue.is_empty(), "{:?}", result.to_enqueue);
    assert!(proc.take_newly_cleared().is_empty(), "no slot write either");
    assert!(
        proc.soft_ref_lru_index.is_empty(),
        "settled soft row dropped its LRU key"
    );
}

// W2F-3. A row the collector already settled keeps its verdict: a
//        reference the GC enqueued is still delivered after a clear().
#[test]
fn a_collector_verdict_is_not_undone_by_a_later_application_clear() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    // A refused restore: cleared, enqueue deferred to the next round.
    assert!(proc.clear_after_refused_restore(0x10));
    assert!(!proc.retire_by_application(0x10));
    let result = proc.process_references(&always_live, 64, 0);
    assert_eq!(result.to_enqueue, vec![(0x10, 0x90)]);
}

// W2F-4. A `jdk.internal.ref.Cleaner` the application enqueued or cleared
//        is never RUN by the collector. The old `mark_manually_enqueued`
//        set `enqueued` without `action_emitted`, and the gather step runs
//        every `runs_cleaner && enqueued && !action_emitted` phantom.
#[test]
fn a_settled_jdk_cleaner_is_never_run_by_the_collector() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_phantom_cleaner(0x10, 0x11, Some(0x90));
    assert!(proc.mark_manually_enqueued(0x10));
    let result = proc.process_references(&always_dead, 64, 0);
    assert!(result.cleaner_actions.is_empty());
    assert!(result.to_enqueue.is_empty());
}

// W2F-5. `Finalizer.register`'s self-referent `Cleaner`-list row is not
//        an application-settleable reference: an `enqueue()`/`clear()` on
//        a finalizable object that is itself a `Reference` must not switch
//        its `finalize()` off.
#[test]
fn a_self_referent_cleaner_row_is_not_retired() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Cleaner, 0x10, 0x10, None);
    assert!(!proc.mark_manually_enqueued(0x10));
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.cleaner_actions, vec![0x10]);
}

// W2F-6. The lazy index follows prunes, relocations and fresh discoveries,
//        and prefers the active row when an address is shared.
#[test]
fn the_retirement_index_follows_prunes_moves_and_recycled_addresses() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Weak, 0x20, 0x21, Some(0x90));
    proc.discover_reference(ReferenceType::Phantom, 0x30, 0x31, Some(0x90));
    assert!(!proc.retire_by_application(0x99), "unknown address");
    // Built now. A prune shifts 0x20 down to position 0.
    proc.remove_collected(&|a| a != 0x10);
    assert!(proc.retire_by_application(0x20));
    assert!(proc.weak_refs[0].cleared && proc.weak_refs[0].reference_obj == 0x20);
    // A move.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x30, 0x40);
    proc.update_after_gc(&map);
    assert!(!proc.retire_by_application(0x30), "the vacated address");
    assert!(proc.retire_by_application(0x40));
    // A recycled address: the settled row at 0x20 and a fresh one there.
    proc.discover_reference(ReferenceType::Weak, 0x20, 0x22, Some(0x90));
    assert!(
        proc.retire_by_application(0x20),
        "the fresh (active) row wins"
    );
    assert!(proc.weak_refs.iter().all(|e| e.cleared));
    // And again after a rebuild.
    proc.discover_reference(ReferenceType::Soft, 0x20, 0x23, Some(0x90));
    proc.remove_collected(&|a| a != 0x40);
    assert!(proc.retire_by_application(0x20));
    assert!(proc.soft_refs[0].cleared);
}

// gcd d2/g. Two ACTIVE rows at one address (a dead `WeakReference`'s row
// the concurrent sweep has not pruned yet, and a new `SoftReference` on
// the freed block): a rebuilt index hands the application's clear to the
// LATER registration, not to the row in the later list.
#[test]
fn gcd_d2g_the_later_registration_owns_a_shared_active_address() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Weak, 0x50, 0x51, Some(0x90));
    assert!(!proc.retire_by_application(0x99), "builds the index");
    // Any prune marks the index stale; the fresh row below is then not
    // inserted incrementally, so the lookup rebuilds.
    proc.remove_collected(&|a| a != 0x50);
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x11, Some(0x90));
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x12, Some(0x90));
    assert!(proc.retire_by_application(0x10));
    assert!(
        proc.soft_refs[0].cleared,
        "the later (soft) registration is retired"
    );
    assert!(
        proc.weak_refs
            .iter()
            .filter(|e| e.reference_obj == 0x10)
            .all(|e| !e.cleared),
        "not the earlier weak row at the same address"
    );
}

// gcd d3/n (`gcd-d2g-soft-touch-index-prefers-a-dead-row-at-a-reused-
// address`). A dead `SoftReference`'s row the concurrent sweep has not
// pruned yet and a new `SoftReference` on the freed block share `0x10`:
// `get()` on the new one touches the NEW row -- at discovery, after a
// prune's rebuild and after a relocation's rebuild -- and the steal moves
// the stamped-touch epoch.
#[test]
fn gcd_d3n_a_soft_touch_lands_on_the_later_registration_at_a_shared_address() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None); // seq 0, dead
    proc.discover_reference(ReferenceType::Soft, 0x30, 0x31, None); // seq 1
    let epoch = proc.soft_touch_epoch_now();
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x12, None); // seq 2, new
    assert_ne!(
        proc.soft_touch_epoch_now(),
        epoch,
        "a steal bumps the epoch"
    );
    proc.touch_soft_reference(0x10, 5_000);
    assert_eq!(proc.soft_refs[2].last_access_time_ms, 5_000, "the new row");
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 0, "not the dead one");
    // A prune elsewhere rebuilds the index: still the new row.
    proc.remove_collected(&|a| a != 0x30);
    assert_eq!(proc.soft_refs.len(), 2);
    proc.touch_soft_reference(0x10, 6_000);
    assert_eq!(proc.soft_refs[1].referent, 0x12);
    assert_eq!(proc.soft_refs[1].last_access_time_ms, 6_000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
    // A relocation rebuild (some soft row moved): still the new row.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10usize, 0x70usize);
    proc.update_after_gc(&map);
    proc.touch_soft_reference(0x70, 7_000);
    assert_eq!(proc.soft_refs[1].last_access_time_ms, 7_000);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 0);
    // The LRU index stays in step with the rows it names.
    for (&(ts, idx), &v) in proc.soft_ref_lru_index.iter() {
        assert_eq!(idx, v);
        assert_eq!(proc.soft_refs[idx].last_access_time_ms, ts);
    }
    // A plain re-discovery of a fresh address steals nothing.
    let epoch = proc.soft_touch_epoch_now();
    proc.discover_reference(ReferenceType::Soft, 0x90, 0x91, None);
    assert_eq!(proc.soft_touch_epoch_now(), epoch);
}

// r11w13-rt-array-alias-audit-jni-refs-gc, open item 1 (round 11 wave
// 16, lane vh): a plain class-id stamp keeps its old meaning, and a
// kind-aware stamp tells an `Integer[]` from an `Integer` that share the
// component's class id.
#[test]
fn a_kind_aware_referent_stamp_refuses_the_array_component_alias() {
    const INTEGER: u32 = 9;
    let integer = u64::from(INTEGER);
    // Plain stamp: class id only, kind ignored (the pre-wave-16 screen).
    assert!(referent_stamp_admits(integer, INTEGER, false));
    assert!(referent_stamp_admits(integer, INTEGER, true));
    assert!(!referent_stamp_admits(integer, 10, false));
    // Kind-aware stamps.
    let plain = integer | REFERENT_STAMP_KIND_KNOWN;
    let array = integer | REFERENT_STAMP_KIND_KNOWN | REFERENT_STAMP_ARRAY;
    assert!(referent_stamp_admits(plain, INTEGER, false));
    assert!(
        !referent_stamp_admits(plain, INTEGER, true),
        "Integer[] reused an Integer's address"
    );
    assert!(referent_stamp_admits(array, INTEGER, true));
    assert!(
        !referent_stamp_admits(array, INTEGER, false),
        "Integer reused an Integer[]'s address"
    );
    assert!(!referent_stamp_admits(array, 10, true));
    // `java.lang.Object` (class 0) becomes stampable once the kind is known.
    let object = REFERENT_STAMP_KIND_KNOWN;
    assert!(referent_stamp_admits(object, 0, false));
    assert!(
        !referent_stamp_admits(object, 0, true),
        "an Object[] is not an Object"
    );
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.stamp_referent_class(0xAB, object);
    assert_eq!(proc.referent_class_stamp(0xAB), Some(object));
}

// gc JNI retirement (2026-10-10): a lambda proxy's class id (a counter
// from `0x8000_0000`) is admitted by its own kind-aware stamp. With the
// kind in bits 31 / 30 of a `u32` the stamp read back as class 8 and the
// restore screen cleared every `WeakReference` to a live lambda.
#[test]
fn a_lambda_proxy_class_id_is_admitted_by_its_own_kind_aware_stamp() {
    for proxy in [0x8000_0000u32, 0x8000_0008, 0xC000_0001, u32::MAX] {
        let stamp = u64::from(proxy) | REFERENT_STAMP_KIND_KNOWN;
        assert!(referent_stamp_admits(stamp, proxy, false), "{proxy:#x}");
        assert!(
            !referent_stamp_admits(stamp, proxy & 0x3FFF_FFFF, false),
            "{proxy:#x}"
        );
        assert!(
            !referent_stamp_admits(stamp, proxy, true),
            "{proxy:#x} is no array"
        );
        assert!(
            referent_stamp_admits(u64::from(proxy), proxy, false),
            "plain {proxy:#x}"
        );
    }
}

// W2F-7. `update_after_gc`'s skips: an empty map, or one that moves no
//        key (a non-moving cycle's identity entries), leaves every index
//        resolving exactly as before.
#[test]
fn a_map_that_moves_no_key_leaves_every_index_resolvable() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, 0xAA, 0x10, Some(0x90));
    proc.stamp_reference(0xAA, 7);
    proc.stamp_referent_class(0xAA, 9);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xAA, 0xAA);
    map.insert(0x10, 0x10);
    proc.update_after_gc(&map);
    proc.update_after_gc(&cratonvm_types::PointerMap::default());
    assert_eq!(proc.identity_stamp(0xAA), Some(7));
    assert_eq!(proc.referent_class_stamp(0xAA), Some(9));
    proc.touch_soft_reference(0xAA, 1234);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 1234);
    assert!(proc.retire_by_application(0xAA));
}

// ======================================================================
// gc-common w3-d (2026-09-23): the reference-delivery thread's channel
// ======================================================================

// W3D-1. A request made before the wait is not lost, and a served signal
//        waits (and times out) until the next request.
#[test]
fn a_delivery_request_is_never_lost() {
    let s = DeliverySignal::new();
    assert!(
        !s.wait_for_request(Duration::from_millis(1)),
        "nothing owed yet"
    );
    s.request();
    assert!(s.wait_for_request(Duration::from_millis(1)));
    // Still owed until a batch covering it finishes.
    assert!(s.wait_for_request(Duration::from_millis(1)));
    let ticket = s.begin_batch();
    s.finish_batch(ticket);
    assert!(!s.wait_for_request(Duration::from_millis(1)));
}

// W3D-2. Work requested DURING a batch is owed another batch: the ticket is
//        the request count when the batch opened.
#[test]
fn work_requested_during_a_batch_is_owed_another_batch() {
    let s = DeliverySignal::new();
    s.request();
    let ticket = s.begin_batch();
    s.request(); // e.g. a finalizer's own allocation collected
    s.finish_batch(ticket);
    assert!(s.wait_for_request(Duration::from_millis(1)));
    assert!(
        !s.wait_until_served(Duration::from_millis(1)),
        "the second request is not served yet"
    );
    let ticket = s.begin_batch();
    s.finish_batch(ticket);
    assert!(s.wait_until_served(Duration::from_millis(1)));
}

// W3D-3. `System.gc()`'s wait returns once a batch opened after its request
//        finishes on another thread, and a stale ticket never moves
//        `served` backwards.
#[test]
fn a_requester_waits_for_the_batch_that_covers_it() {
    let s = Arc::new(DeliverySignal::new());
    s.request();
    let server = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || {
            assert!(s.wait_for_request(Duration::from_secs(10)));
            let ticket = s.begin_batch();
            s.finish_batch(ticket);
            s.finish_batch(0); // stale: must not undo the batch above
        })
    };
    assert!(s.wait_until_served(Duration::from_secs(10)));
    server.join().unwrap();
    assert!(s.wait_until_served(Duration::from_millis(1)));
}

// W3D-4. Queue wake-ups: deduplicated, relocated with the actions,
//        reported as roots, and gone once drained.
#[test]
fn queue_wake_ups_are_deduplicated_relocated_and_rooted() {
    let ct = CleanerThread::new();
    assert!(!ct.has_pending_work());
    ct.note_queues_to_notify(&[0x30, 0x10, 0x30]);
    ct.note_queues_to_notify(&[0x10, 0x20]);
    ct.note_queues_to_notify(&[]);
    assert!(ct.has_pending_work());
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10, 0x90);
    map.insert(0x20, 0x30); // lands on another entry's address: one entry
    ct.update_after_gc(&map);
    let mut rooted = ct.pending_addresses();
    rooted.sort_unstable();
    assert_eq!(rooted, vec![0x30, 0x90]);
    assert_eq!(ct.drain_queues_to_notify(), vec![0x30, 0x90]);
    assert!(!ct.has_pending_work());
    assert!(ct.pending_addresses().is_empty());
    // Actions and wake-ups are separate: draining one leaves the other.
    ct.submit_action(0x40);
    ct.note_queues_to_notify(&[0x50]);
    assert_eq!(ct.drain_actions(), vec![0x40]);
    assert!(ct.has_pending_work());
    assert_eq!(ct.pending_count(), 0);
}

#[test]
fn deferred_reference_enqueues_root_and_relocate_both_participants() {
    let delivery = CleanerThread::new();
    delivery.submit_reference_enqueues(&[(0x10, 0x20), (0x30, 0x20)]);
    assert!(delivery.has_reference_enqueues());
    assert!(!delivery.has_pending_work(), "the JDK handler owns this work");
    let mut roots = delivery.pending_addresses();
    roots.sort_unstable();
    assert_eq!(roots, vec![0x10, 0x20, 0x20, 0x30]);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10, 0x30);
    map.insert(0x20, 0x40);
    map.insert(0x30, 0x50);
    delivery.update_after_gc(&map);
    assert_eq!(
        delivery.drain_reference_enqueues(),
        vec![(0x30, 0x40), (0x50, 0x40)]
    );
    assert!(!delivery.has_reference_enqueues());
    assert!(!delivery.has_pending_work());
    assert!(delivery.pending_addresses().is_empty());
}

// W3D-5. One start claim per VM, handed back on a failed spawn; the
//        delivery thread's id round-trips (thread id 0 included).
#[test]
fn the_delivery_thread_is_claimed_once_and_its_id_round_trips() {
    let ft = FinalizerThread::new();
    assert!(!ft.delivery_thread_claimed());
    assert_eq!(ft.delivery_thread(), None);
    assert!(!ft.request_delivery(), "nobody serves yet");
    assert!(ft.claim_delivery_thread_start());
    assert!(!ft.claim_delivery_thread_start());
    ft.release_delivery_thread_start();
    assert!(ft.claim_delivery_thread_start());
    ft.set_delivery_thread(0);
    assert_eq!(ft.delivery_thread(), Some(0));
    assert!(ft.request_delivery());
    // Both requests above were recorded for the (starting) thread.
    let signal = ft.delivery_signal();
    assert!(signal.wait_for_request(Duration::from_millis(1)));
}

// gci i42/rp: a delivery thread that unwound is retired -- unpublished,
// its claim handed back -- so the next request may start a fresh one; a
// different thread's id changes nothing (thread id 0 included).
#[test]
fn gci_i42rp_a_retired_delivery_thread_frees_the_claim_for_a_new_one() {
    let ft = FinalizerThread::new();
    assert!(ft.claim_delivery_thread_start());
    ft.set_delivery_thread(7);
    assert!(!ft.retire_delivery_thread(8), "another thread's id changes nothing");
    assert_eq!(ft.delivery_thread(), Some(7));
    assert!(ft.delivery_thread_claimed());
    assert!(ft.retire_delivery_thread(7));
    assert_eq!(ft.delivery_thread(), None);
    assert!(!ft.delivery_thread_claimed());
    assert!(
        ft.claim_delivery_thread_start(),
        "the next request may start a fresh thread"
    );
    ft.set_delivery_thread(0);
    assert!(ft.retire_delivery_thread(0));
    assert_eq!(ft.delivery_thread(), None);
    assert!(!ft.delivery_thread_claimed());
}

// gci i42/rp: a thread that unwinds before it published itself still
// holds the start claim, and hands it back.
#[test]
fn gci_i42rp_a_thread_that_unwinds_before_it_serves_hands_its_claim_back() {
    let ft = FinalizerThread::new();
    assert!(ft.claim_delivery_thread_start());
    assert_eq!(ft.delivery_thread(), None);
    assert!(ft.retire_delivery_thread(3));
    assert!(!ft.delivery_thread_claimed());
    assert!(ft.claim_delivery_thread_start());
}

// W4D-1. A sliced wait keeps ONE target: requests made after the ticket
//        was taken do not extend it, and a batch opened before the ticket
//        does not satisfy it.
#[test]
fn a_ticket_is_satisfied_by_the_first_batch_that_covers_it() {
    let s = DeliverySignal::new();
    s.request();
    let early = s.begin_batch(); // opened before our request
    s.request();
    let target = s.ticket();
    s.finish_batch(early);
    assert!(
        !s.wait_for_ticket(target, Duration::from_millis(1)),
        "a batch opened before the ticket does not cover it"
    );
    let covering = s.begin_batch();
    s.request(); // another thread's later request
    s.finish_batch(covering);
    assert!(
        s.wait_for_ticket(target, Duration::from_millis(1)),
        "a later request must not move a ticket already taken"
    );
    assert!(
        !s.wait_until_served(Duration::from_millis(1)),
        "the later request itself is still owed a batch"
    );
}

// W4D-2. A last-ditch soft-clear request is per processor, consumed
//        exactly once, and repeat requests before a collection collapse.
#[test]
fn a_clear_all_soft_request_is_taken_exactly_once() {
    let mut rp = ReferenceProcessor::new();
    assert!(!rp.take_clear_all_soft_request(), "nothing requested");
    rp.request_clear_all_soft_refs();
    rp.request_clear_all_soft_refs();
    assert!(rp.take_clear_all_soft_request());
    assert!(
        !rp.take_clear_all_soft_request(),
        "consumed by one collection"
    );
    // Another VM's processor never sees it.
    let mut other = ReferenceProcessor::new();
    rp.request_clear_all_soft_refs();
    assert!(!other.take_clear_all_soft_request());
    assert!(rp.take_clear_all_soft_request());
}

// W4D-3. A reported finalizable object is queued past the old capacity:
//        its row is already flagged enqueued, so a refusal would lose its
//        `finalize()` for good.
#[test]
fn enqueue_unfinalized_never_drops_past_the_old_capacity() {
    let ft = FinalizerThread::new();
    for i in 0..=FINALIZER_QUEUE_MAX_CAPACITY {
        assert!(ft.enqueue_unfinalized((i + 1) * 16));
    }
    assert_eq!(ft.pending_count(), FINALIZER_QUEUE_MAX_CAPACITY + 1);
    assert!(
        !ft.enqueue_unfinalized(16),
        "a duplicate already waiting is still refused"
    );
    assert_eq!(ft.dequeue(), Some(16), "FIFO order is kept");
}

// W5D-1. `in_flight` covers a batch from its request to its finish,
//        including the stretch in which the batch has emptied the queues
//        but is still running what it took (the window
//        `waitForReferenceProcessing` exists to wait out).
#[test]
fn a_batch_is_in_flight_from_request_to_finish() {
    let s = DeliverySignal::new();
    assert!(!s.in_flight(), "nothing requested");
    s.request();
    assert!(s.in_flight(), "requested, not started");
    let t = s.begin_batch();
    assert!(s.in_flight(), "running");
    s.finish_batch(t);
    assert!(!s.in_flight(), "served");
    let t = s.begin_batch();
    s.request(); // arrives mid-batch
    s.finish_batch(t);
    assert!(s.in_flight(), "a request made during a batch is still owed");
}

// zt t15/x12: the finalizer step is its own wait, independent of the
// batch's tickets (a request made during the step keeps the batch owed,
// never the step).
#[test]
fn zt15x12_the_finalizer_step_is_waited_for_on_its_own() {
    let s = DeliverySignal::new();
    assert!(!s.finalizer_step_running());
    assert!(s.wait_for_finalizer_step(Duration::ZERO), "no step: no wait");
    s.request();
    let t = s.begin_batch();
    s.begin_finalizer_step();
    assert!(s.finalizer_step_running());
    assert!(
        !s.wait_for_finalizer_step(Duration::from_millis(1)),
        "a running step is waited for"
    );
    s.end_finalizer_step();
    s.request(); // the batch's notification step is fed meanwhile
    assert!(s.in_flight(), "the batch is still owed");
    assert!(
        s.wait_for_finalizer_step(Duration::ZERO),
        "but its finalizer step has ended"
    );
    s.finish_batch(t);
    let s = std::sync::Arc::new(DeliverySignal::new());
    s.begin_finalizer_step();
    let peer = std::sync::Arc::clone(&s);
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        peer.end_finalizer_step();
    });
    assert!(s.wait_for_finalizer_step(Duration::from_secs(10)), "woken by the end");
    h.join().unwrap();
}

// ======================================================================
// gc-common w6-d (2026-09-24)
// ======================================================================

/// Deterministic generator for the model tests below (no `rand` here).
struct W6dLcg(u64);
impl W6dLcg {
    fn advance(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.advance() % n
    }
}

/// The VM's stamped touch (`NativeContextImpl::touch_soft_reference`),
/// with the object's `SoftReference.timestamp` field modelled by `fields`:
/// compare the field with the stamp WITHOUT the lock; otherwise touch and
/// stamp.
fn w6d_stamped_touch(
    proc: &mut ReferenceProcessor,
    fields: &mut FxHashMap<usize, i64>,
    addr: usize,
    now_ms: u64,
) {
    let epoch = proc.soft_touch_epoch();
    let clock = proc.soft_touch_clock();
    let observed = proc.soft_touch_observed_ms();
    // gcj j4/rf: the epoch first, then the clock, as the VM reads them.
    let e = epoch.load(Ordering::Acquire);
    let key = soft_touch_key_ms(now_ms, clock.load(Ordering::Acquire));
    if let Some(stamp) = soft_touch_stamp(key, e) {
        if fields.get(&addr) == Some(&stamp) {
            observed.fetch_max(now_ms, Ordering::AcqRel);
            return;
        }
    }
    proc.touch_soft_reference(addr, now_ms);
    // No stamp for a `0` clock: reset the field, as the VM does, so it
    // stops vouching for the row state an earlier stamp named.
    let stamp = proc.soft_touch_stamp_now(now_ms).unwrap_or(0);
    if let Some(field) = fields.get_mut(&addr) {
        *field = stamp;
    }
}

fn w6d_assert_same_soft_state(a: &ReferenceProcessor, b: &ReferenceProcessor, at: &str) {
    let rows = |p: &ReferenceProcessor| -> Vec<(usize, usize, u64, bool, bool)> {
        p.soft_refs
            .iter()
            .map(|e| {
                (
                    e.reference_obj,
                    e.referent,
                    e.last_access_time_ms,
                    e.cleared,
                    e.enqueued,
                )
            })
            .collect()
    };
    assert_eq!(rows(a), rows(b), "{at}: soft rows differ");
    assert_eq!(
        a.soft_ref_lru_index, b.soft_ref_lru_index,
        "{at}: LRU index differs"
    );
    assert_eq!(
        a.soft_ref_addr_index, b.soft_ref_addr_index,
        "{at}: address index differs"
    );
    assert_eq!(
        a.observed_clock_ms(),
        b.observed_clock_ms(),
        "{at}: mutator clock differs"
    );
    assert_eq!(
        a.soft_gc_clock_ms, b.soft_gc_clock_ms,
        "{at}: soft clock differs"
    );
}

/// One random history, replayed through the locked touch (`plain`) and the
/// stamped one (`stamped`). Everything else is applied to both.
///
/// gcj j4/rf: `keyed` runs both under HotSpot's LRU with GC-clock stamps,
/// and the stamped one keys its fast path on that clock
/// (`soft_touch_keyed_on_clock`).
fn w6d_run_soft_touch_model(seed: u64, steps: usize, keyed: bool) {
    const SLOTS: u64 = 10;
    let mut rng = W6dLcg(seed);
    let mut plain = ReferenceProcessor::new_with_policy(1);
    let mut stamped = ReferenceProcessor::new_with_policy(1);
    if keyed {
        for p in [&mut plain, &mut stamped] {
            p.hotspot_soft_lru = true;
            p.soft_stamp_gc_clock = true;
            p.soft_clock_origin_ms = 900;
        }
        stamped.soft_touch_keyed_on_clock = true;
    } else {
        stamped.soft_touch_keyed_on_clock = false;
    }
    // Live SoftReference objects: address -> their `timestamp` field
    // (a fresh object reads 0). A dead object leaves this map at once; its
    // row may linger until a prune, which is how a recycled address meets
    // a stale row.
    let mut live: FxHashMap<usize, i64> = FxHashMap::default();
    let mut now: u64 = 1_000;
    for step in 0..steps {
        let at = format!("seed {seed} step {step}");
        match rng.below(100) {
            // `new SoftReference<>(x)`: discover, then the constructor's touch.
            0..=14 => {
                let addr = 0x1000 * (1 + rng.below(SLOTS) as usize);
                if live.contains_key(&addr) {
                    continue;
                }
                live.insert(addr, 0);
                let referent = 0x100 + 8 * rng.below(6) as usize;
                plain.discover_reference(ReferenceType::Soft, addr, referent, None);
                stamped.discover_reference(ReferenceType::Soft, addr, referent, None);
                plain.touch_soft_reference(addr, now);
                w6d_stamped_touch(&mut stamped, &mut live, addr, now);
            }
            // `get()` on a live object.
            15..=59 => {
                let addrs: Vec<usize> = {
                    let mut v: Vec<usize> = live.keys().copied().collect();
                    v.sort_unstable();
                    v
                };
                if addrs.is_empty() {
                    continue;
                }
                let addr = addrs[rng.below(addrs.len() as u64) as usize];
                plain.touch_soft_reference(addr, now);
                w6d_stamped_touch(&mut stamped, &mut live, addr, now);
            }
            // The clock: mostly the same millisecond, sometimes forward,
            // rarely backwards (a `SystemTime` step), rarely `0`.
            60..=69 => {
                now = match rng.below(40) {
                    0 => 0,
                    1..=2 => now.saturating_sub(1),
                    3..=24 => now,
                    _ => now + rng.below(3),
                };
            }
            // An object dies; its row stays until a prune.
            70..=77 => {
                let mut v: Vec<usize> = live.keys().copied().collect();
                v.sort_unstable();
                if !v.is_empty() {
                    let victim = v[rng.below(v.len() as u64) as usize];
                    live.remove(&victim);
                }
            }
            // A collection: prune the dead, then maybe move survivors
            // (to free addresses; stale rows may sit there).
            78..=89 => {
                let alive = live.clone();
                let is_live = |a: usize| alive.contains_key(&a);
                if rng.below(2) == 0 {
                    plain.remove_collected(&is_live);
                    stamped.remove_collected(&is_live);
                }
                let mut map = cratonvm_types::PointerMap::default();
                let mut moved: FxHashMap<usize, i64> = FxHashMap::default();
                let mut sorted: Vec<(usize, i64)> =
                    live.iter().map(|(&a, &f)| (a, f)).collect();
                sorted.sort_unstable();
                for (addr, field) in sorted {
                    let target = if rng.below(3) == 0 {
                        0x1000 * (1 + rng.below(SLOTS) as usize)
                    } else {
                        addr
                    };
                    let target = if moved.contains_key(&target)
                        || (target != addr && live.contains_key(&target))
                    {
                        addr
                    } else {
                        target
                    };
                    if moved.contains_key(&target) {
                        // Unreachable by construction (no survivor may move
                        // onto another's current address); the length check
                        // below abandons the relocation if it ever is.
                        continue;
                    }
                    if rng.below(2) == 0 || target != addr {
                        map.insert(addr, target);
                    }
                    moved.insert(target, field);
                }
                if moved.len() == live.len() {
                    plain.update_after_gc(&map);
                    stamped.update_after_gc(&map);
                    live = moved;
                }
            }
            // Reference processing, with random pressure and marks.
            90..=95 => {
                let marks: Vec<bool> = (0..6).map(|_| rng.below(2) == 0).collect();
                let is_marked = |a: usize| -> bool {
                    if (0x100..0x100 + 8 * 6).contains(&a) {
                        marks[(a - 0x100) / 8]
                    } else {
                        true
                    }
                };
                let free_mb = rng.below(3) as usize;
                let clock = if rng.below(2) == 0 { 0 } else { now };
                let _ = plain.process_references(&is_marked, free_mb, clock);
                let _ = stamped.process_references(&is_marked, free_mb, clock);
            }
            // The pre-collection condemnation, then its close-out.
            _ => {
                let free_mb = rng.below(3) as usize;
                let a = plain.condemn_idle_soft_refs(free_mb, now);
                let b = stamped.condemn_idle_soft_refs(free_mb, now);
                assert_eq!(a, b, "{at}: condemned sets differ");
                plain.finish_pre_gc_cycle();
                stamped.finish_pre_gc_cycle();
            }
        }
        w6d_assert_same_soft_state(&plain, &stamped, &at);
    }
}

// W6D-1. The stamped touch leaves the processor exactly as the locked
//        touch would, over random histories of discovery, `get()`, clock
//        steps (including backwards), deaths without a prune (recycled
//        addresses meeting stale rows), prunes, relocations, processing
//        and condemnation.
#[test]
fn w6d_stamped_soft_touch_is_equivalent_to_the_locked_touch() {
    for seed in 1..=96u64 {
        w6d_run_soft_touch_model(seed, 600, false);
    }
}

// gcj j4/rf (rp residuals item 5): the same equivalence with the fast
// path keyed on the GC clock under HotSpot's LRU, where most `get()`s of
// one collection interval share one stamp; and that keying does skip
// touches the millisecond key would not.
#[test]
fn gcj_j4_rf_a_clock_keyed_soft_touch_is_equivalent_to_the_locked_touch() {
    for seed in 1..=96u64 {
        w6d_run_soft_touch_model(seed, 600, true);
    }
    let mut proc = ReferenceProcessor::new_with_policy(1);
    proc.hotspot_soft_lru = true;
    proc.soft_stamp_gc_clock = true;
    proc.soft_clock_origin_ms = 900;
    proc.soft_touch_keyed_on_clock = true;
    proc.discover_reference(ReferenceType::Soft, 0x1000, 0x100, None);
    let mut fields: FxHashMap<usize, i64> = FxHashMap::default();
    fields.insert(0x1000, 0);
    w6d_stamped_touch(&mut proc, &mut fields, 0x1000, 1_000);
    let first = fields[&0x1000];
    // A later millisecond: same stamp (the row stays at the clock), and
    // the skipped touch still raises the observed clock.
    w6d_stamped_touch(&mut proc, &mut fields, 0x1000, 1_005);
    assert_eq!(fields[&0x1000], first);
    assert_eq!(proc.last_observed_clock_ms, 1_000, "the lock was not taken");
    assert_eq!(proc.observed_clock_ms(), 1_005);
    assert_eq!(proc.soft_refs[0].last_access_time_ms, 900);
}

// W6D-2. The stamp never collides with a fresh object's field, declines a
//        missing clock, and changes with the epoch.
#[test]
fn w6d_the_soft_touch_stamp_encodes_clock_and_epoch() {
    assert_eq!(soft_touch_stamp(0, 7), None, "no clock, no stamp");
    assert_eq!(soft_touch_stamp(1u64 << 43, 0), None, "out of range");
    let a = soft_touch_stamp(1_700_000_000_000, 5).expect("a real clock stamps");
    assert_ne!(a, 0, "a stamp is never a fresh field");
    assert_ne!(
        Some(a),
        soft_touch_stamp(1_700_000_000_000, 6),
        "a new epoch voids the stamp"
    );
    assert_ne!(
        Some(a),
        soft_touch_stamp(1_700_000_000_001, 5),
        "a new millisecond voids the stamp"
    );
}

// W6D-3. The epoch moves exactly when the address -> row resolution may:
//        a moving map and a soft prune bump it; an empty map, a touch, a
//        discovery and processing do not.
#[test]
fn w6d_the_soft_touch_epoch_moves_with_moves_and_prunes() {
    let mut proc = ReferenceProcessor::new();
    let e0 = proc.soft_touch_epoch_now();
    proc.discover_reference(ReferenceType::Soft, 0x10, 0x11, None);
    proc.touch_soft_reference(0x10, 5_000);
    let _ = proc.process_references(&always_live, 64, 5_000);
    proc.update_after_gc(&cratonvm_types::PointerMap::default());
    assert_eq!(proc.soft_touch_epoch_now(), e0);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x10, 0x20);
    proc.update_after_gc(&map);
    let e1 = proc.soft_touch_epoch_now();
    assert!(e1 > e0, "a moving map bumps");
    proc.remove_collected(&always_live);
    assert_eq!(proc.soft_touch_epoch_now(), e1, "nothing pruned, no bump");
    proc.remove_collected(&always_dead);
    assert!(proc.soft_touch_epoch_now() > e1, "a soft prune bumps");
}

// W6D-4. A weak reference to a finalizable object the collection found
//        unreachable and resurrected is CLEARED (HotSpot processes Weak
//        before Final); the phantom to it waits for finalization, and the
//        noted set lasts one round.
#[test]
fn w6d_a_weak_ref_to_a_resurrected_finalizable_is_cleared() {
    const OBJ: usize = 0x200;
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, OBJ, OBJ, None);
    proc.discover_reference(ReferenceType::Weak, 0x300, OBJ, Some(0x400));
    proc.discover_reference(ReferenceType::Soft, 0x500, OBJ, None);
    proc.discover_reference(ReferenceType::Phantom, 0x600, OBJ, Some(0x400));
    // The collector marked OBJ only to resurrect it.
    proc.note_resurrected_finalizables(&[OBJ]);
    let result = proc.process_references(&always_live, 0, 1_000_000);
    assert!(
        proc.weak_refs[0].cleared,
        "weak cleared before finalization"
    );
    assert!(proc.soft_refs[0].cleared, "soft cleared under pressure");
    assert!(
        !proc.phantom_refs[0].enqueued,
        "the phantom waits until the object is finalized"
    );
    assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
    assert!(
        result.to_finalize.is_empty(),
        "the resurrected row is flagged by `mark_finalizer_enqueued`, not phase 3"
    );
    // One round only: a second weak ref to OBJ is kept next time.
    proc.discover_reference(ReferenceType::Weak, 0x700, OBJ, None);
    let _ = proc.process_references(&always_live, 0, 1_000_000);
    assert!(
        !proc.weak_refs[1].cleared,
        "the noted set does not span rounds"
    );
}

// W6D-5. Without a noted set nothing changes (the V18 rows above), and a
//        strongly reachable object that merely has a finalizer keeps its
//        weak references; `finish_pre_gc_cycle` drops an unconsumed set.
#[test]
fn w6d_only_the_noted_objects_lose_their_weak_refs() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x210, 0x210, None);
    proc.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
    proc.discover_reference(ReferenceType::Weak, 0x310, 0x210, None);
    proc.note_resurrected_finalizables(&[0x200]);
    let _ = proc.process_references(&always_live, 64, 0);
    assert!(proc.weak_refs[0].cleared);
    assert!(!proc.weak_refs[1].cleared, "0x210 is strongly reachable");

    let mut other = ReferenceProcessor::new();
    other.discover_reference(ReferenceType::Weak, 0x300, 0x200, None);
    other.note_resurrected_finalizables(&[0x200]);
    other.finish_pre_gc_cycle();
    let _ = other.process_references(&always_live, 64, 0);
    assert!(
        !other.weak_refs[0].cleared,
        "a finished cycle's set is gone"
    );
}

// W7D-1. Depth 2 needs nothing new from the processor: handed the whole
//        resurrection closure (the finalizable object AND what the
//        collector marked only through it), a weak reference to the child
//        is cleared, the phantom to the child still waits for
//        finalization, and a child that is also strongly reachable (never
//        in the closure, by the collector's contract) keeps its weak
//        reference. `handoff-w7d-collectors-report-the-resurrection-closure`.
#[test]
fn w7d_the_resurrection_closure_clears_weak_refs_at_depth_two() {
    const PARENT: usize = 0x200;
    const CHILD: usize = 0x240;
    const SHARED: usize = 0x280;
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, PARENT, PARENT, None);
    proc.discover_reference(ReferenceType::Weak, 0x300, CHILD, Some(0x400));
    proc.discover_reference(ReferenceType::Weak, 0x310, SHARED, Some(0x400));
    proc.discover_reference(ReferenceType::Phantom, 0x320, CHILD, Some(0x400));
    proc.discover_reference(ReferenceType::Soft, 0x330, CHILD, None);
    // The collector marked PARENT and CHILD only in its resurrection
    // drain; SHARED was reached by the strong closure first.
    proc.note_resurrected_finalizables(&[PARENT, CHILD]);
    let result = proc.process_references(&always_live, 0, 1_000_000);
    assert!(proc.weak_refs[0].cleared, "weak to the child cleared");
    assert!(
        !proc.weak_refs[1].cleared,
        "a strongly reachable object keeps it"
    );
    assert!(
        proc.soft_refs[0].cleared,
        "soft to the child cleared under pressure"
    );
    assert!(
        !proc.phantom_refs[0].enqueued,
        "the phantom to the child waits until the parent is finalized"
    );
    assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
    assert!(result.to_finalize.is_empty());
}

// W7D-2. The GC-notification hand-off mark is raised by a door and taken
//        exactly once by the delivery thread's batch.
#[test]
fn w7d_the_gc_notification_hand_off_mark_is_taken_once() {
    let ft = FinalizerThread::new();
    assert!(!ft.take_gc_notifications_handed_off(), "starts lowered");
    ft.note_gc_notifications_handed_off();
    ft.note_gc_notifications_handed_off();
    assert!(ft.take_gc_notifications_handed_off());
    assert!(!ft.take_gc_notifications_handed_off(), "taken once");
}

// W18G-1. A second registration of a finalizable object whose row is
//         still active is a no-op: one row, one `finalize()`.
//         `handoff-w2d-finalizer-registration-native` item 4.
#[test]
fn w18g_a_second_finalizer_registration_is_a_no_op() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x140, 0x140, None);
    assert_eq!(proc.finalizer_refs.len(), 2);
    assert!(proc.is_registered_finalizable(0x100));
    let result = proc.process_references(&always_dead, 64, 0);
    assert_eq!(result.to_finalize, vec![0x100, 0x140]);
    assert_eq!(result.stats.finalizer_refs_enqueued, 2);
}

// W18G-2. A row that was already ENQUEUED does not refuse its address: a
//         new finalizable object allocated where a finalized one used to
//         live must be registered (the w5-d warning on this handoff).
#[test]
fn w18g_an_enqueued_row_does_not_refuse_a_recycled_address() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    // Warm the index so the enqueue below has to invalidate it.
    assert!(proc.is_registered_finalizable(0x100));
    let first = proc.process_references(&always_dead, 64, 0);
    assert_eq!(first.to_finalize, vec![0x100]);
    assert!(!proc.is_registered_finalizable(0x100));
    // The same verdict through the resurrection channel.
    proc.discover_reference(ReferenceType::Finalizer, 0x180, 0x180, None);
    proc.mark_finalizer_enqueued(&[0x180]);
    assert!(!proc.is_registered_finalizable(0x180));

    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x180, 0x180, None);
    assert_eq!(
        proc.finalizer_refs.len(),
        4,
        "both recycled addresses registered"
    );
    let second = proc.process_references(&always_dead, 64, 0);
    assert_eq!(second.to_finalize, vec![0x100, 0x180]);
}

// W18G-3. The index follows a moving collection and a prune: the old
//         address stops refusing, the new one refuses.
#[test]
fn w18g_the_registration_index_follows_relocation_and_prune() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
    assert!(proc.is_registered_finalizable(0x100));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0x100, 0x900);
    proc.update_after_gc(&map);
    assert!(!proc.is_registered_finalizable(0x100));
    assert!(proc.is_registered_finalizable(0x900));
    proc.discover_reference(ReferenceType::Finalizer, 0x900, 0x900, None);
    assert_eq!(
        proc.finalizer_refs.len(),
        2,
        "the moved object is still registered once"
    );

    // The row at 0x200 is pruned (its object is gone without a verdict);
    // a new object there registers again.
    proc.remove_collected(&|a| a != 0x200);
    assert!(!proc.is_registered_finalizable(0x200));
    proc.discover_reference(ReferenceType::Finalizer, 0x200, 0x200, None);
    assert_eq!(proc.finalizer_refs.len(), 2);
    assert!(proc.is_registered_finalizable(0x200));
}

/// gen r5w3/unload7 (`gengc-r5w1-refs5-proposal-processor-reports-what-it-retired`):
/// the round's `retired` list plus the pre-pass's dropped ACTIVE rows is
/// exactly the before/after difference of `active_reference_object_set`
/// the generational remark used to compute with two hash-set builds.
#[test]
fn r5w3_retired_rows_equal_the_active_set_difference() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    let queue = 0x9000usize;
    // Weak: dead referent (retires, enqueued), live referent (stays).
    proc.discover_reference(ReferenceType::Weak, 0x1000, 0x2000, Some(queue));
    proc.discover_reference(ReferenceType::Weak, 0x1100, 0x2100, None);
    // Soft: idle and dead (retires), strongly reachable (stays).
    proc.discover_reference(ReferenceType::Soft, 0x1200, 0x2200, None);
    proc.discover_reference(ReferenceType::Soft, 0x1210, 0x2210, None);
    // Phantom: dead referent (retires), live referent (stays).
    proc.discover_reference(ReferenceType::Phantom, 0x1300, 0x2300, Some(queue));
    proc.discover_reference(ReferenceType::Phantom, 0x1310, 0x2310, None);
    // A weak row whose own `Reference` died: the pre-pass drops it.
    proc.discover_reference(ReferenceType::Weak, 0x1400, 0x2400, None);
    // A finalizer row never appears in either set.
    proc.discover_reference(ReferenceType::Finalizer, 0x2500, 0x2500, None);

    let live = [
        0x1000usize,
        0x1100,
        0x1200,
        0x1210,
        0x1300,
        0x1310,
        queue,
        0x2100,
        0x2210,
        0x2310,
    ];
    let before = proc.active_reference_object_set();
    let pruned = proc.remove_collected_reference_objects(&survivor(&live));
    // `free_heap_mb = 0`: every idle soft entry with an unmarked referent
    // is condemned by the LRU rule.
    let result = proc.process_references(&survivor(&live), 0, 1_000_000);
    let after = proc.active_reference_object_set();

    assert_eq!(pruned, 1, "the dead weak Reference's active row");
    let diff: rustc_hash::FxHashSet<usize> = before.difference(&after).copied().collect();
    let mut reported: rustc_hash::FxHashSet<usize> = result.retired.iter().copied().collect();
    assert_eq!(
        reported.len(),
        result.retired.len(),
        "no row reported twice"
    );
    assert!(
        !reported.contains(&0x2500),
        "finalizer rows are not retirements"
    );
    for kept in [0x1100usize, 0x1210, 0x1310] {
        assert!(!reported.contains(&kept), "{kept:#x} is still active");
    }
    reported.insert(0x1400);
    assert_eq!(reported, diff);
    // A second round retires nothing new.
    let again = proc.process_references(&survivor(&live), 0, 1_000_000);
    assert!(again.retired.is_empty(), "{:?}", again.retired);
}

/// gcd d1/c (`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`):
/// the concurrent sweep's prune drops the soft / weak / phantom rows of
/// the `Reference`s it freed, registered before its snapshot, and only
/// those: a row registered after the snapshot at a freed (re-issued)
/// address, a row outside the freed range and every finalizer row stay.
#[test]
fn gcd_d1c_sweep_prune_drops_only_rows_registered_before_it_in_freed_blocks() {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Weak, 0x1000, 0x5000, None);
    proc.discover_reference(ReferenceType::Soft, 0x1010, 0x5010, None);
    proc.discover_reference(ReferenceType::Phantom, 0x1020, 0x5020, Some(0x9000));
    proc.discover_reference(ReferenceType::Weak, 0x2000, 0x5030, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x1030, 0x1030, None);
    let snapshot = proc.registration_seq();
    assert_eq!(snapshot, 5, "one number per discovered row");
    // A NEW `Reference` allocated on a block the sweep is freeing.
    proc.discover_reference(ReferenceType::Weak, 0x1000, 0x6000, None);
    assert_eq!(proc.registration_seq(), 6);

    // The sweep freed [0x1000, 0x1040).
    let freed = |a: usize| (0x1000..0x1040).contains(&a);
    let dropped = proc.remove_reference_objects_registered_before(snapshot, &freed);
    assert_eq!(dropped, 3, "the dead weak, soft and phantom rows");
    let weak: Vec<(usize, usize)> = proc
        .weak_refs
        .iter()
        .map(|e| (e.reference_obj, e.referent))
        .collect();
    assert_eq!(weak, vec![(0x2000, 0x5030), (0x1000, 0x6000)]);
    assert!(proc.soft_refs.is_empty());
    assert!(proc.phantom_refs.is_empty());
    assert_eq!(
        proc.finalizer_refs.len(),
        1,
        "finalizer rows are never pruned here"
    );
    // Idempotent.
    assert_eq!(
        proc.remove_reference_objects_registered_before(snapshot, &freed),
        0
    );
    // The pre-pass it shares a body with is unchanged.
    assert_eq!(proc.remove_collected_reference_objects(&|a| a != 0x2000), 1);
    assert_eq!(proc.weak_refs.len(), 1);
}

/// gcd d9/e (`docs/feature-designs/generative-gc-architecture.md`):
/// a refused restore's DEFERRED enqueue pair goes with its row when the
/// `Reference` dies before the next round -- through the pre-pass, the
/// concurrent sweep's prune, `remove_collected` and the shape screen --
/// so the next round never emits a dead (possibly re-issued) address. A
/// live one is still delivered, and a later registration at a re-issued
/// address keeps its own pair.
#[test]
fn gcd_d9e_a_deferred_enqueue_dies_with_its_reference() {
    let refused = |r: &mut ReferenceProcessor, obj: usize| {
        r.discover_reference(ReferenceType::Weak, obj, obj + 1, Some(0x90));
        assert!(r.clear_after_refused_restore(obj));
    };
    // The pre-pass (and, through the same body, the sweep prune).
    let mut proc = ReferenceProcessor::new();
    refused(&mut proc, 0x10);
    refused(&mut proc, 0x20);
    assert_eq!(proc.remove_collected_reference_objects(&|a| a != 0x10), 0);
    assert_eq!(proc.pending_enqueues_dropped(), 1);
    let result = proc.process_references(&always_live, 64, 0);
    assert_eq!(
        result.to_enqueue,
        vec![(0x20, 0x90)],
        "the live one is delivered"
    );

    // The concurrent sweep's prune, with a later registration on the
    // re-issued block that has its own deferred pair on ANOTHER queue.
    let mut proc = ReferenceProcessor::new();
    refused(&mut proc, 0x10);
    let snapshot = proc.registration_seq();
    proc.discover_reference(ReferenceType::Weak, 0x10, 0x77, Some(0xa0));
    assert!(
        proc.clear_after_refused_restore(0x10),
        "the new row (the old one is settled)"
    );
    let freed = |a: usize| a == 0x10;
    let _ = proc.remove_reference_objects_registered_before(snapshot, &freed);
    assert_eq!(
        proc.process_references(&always_live, 64, 0).to_enqueue,
        vec![(0x10, 0xa0)],
        "the freed Reference's pair is gone, the new one's is kept"
    );

    // `remove_collected` and the shape screen.
    let mut proc = ReferenceProcessor::new();
    refused(&mut proc, 0x10);
    proc.remove_collected(&|a| a != 0x10);
    let result = proc.process_references(&always_live, 64, 0);
    assert!(result.to_enqueue.is_empty());
    let mut proc = ReferenceProcessor::new();
    refused(&mut proc, 0x10);
    proc.retain_shaped_weak_phantom(&|a| a != 0x10);
    let result = proc.process_references(&always_live, 64, 0);
    assert!(result.to_enqueue.is_empty());
    assert_eq!(proc.pending_enqueues_dropped(), 1);
}
