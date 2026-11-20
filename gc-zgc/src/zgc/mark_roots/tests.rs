use super::*;

/// The filter must never answer "definitely not" for an owner in the
/// coherent snapshot it has published -- a false negative drops a live
/// edge and the object it names is swept while reachable.
///
/// Do not read the process-global registries after `rebuild` here. Mutator
/// tests may register an owner between the snapshot and that read; that is
/// precisely the concurrent mutation this filter reports through
/// `is_current` / `late_owners`, not an assertion the old snapshot can
/// satisfy.
#[test]
fn a_snapshotted_owner_is_never_rejected() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    let Some(bank) = filter.live_for_test() else {
        return; // the kill switch is set in this process
    };
    let keys = bank.key_sets.lock();
    for addr in &keys.mirror_owners {
        assert!(
            filter.may_own_extra_roots(*addr),
            "snapshotted mirror owner {addr:#x} rejected"
        );
    }
    for owner in &keys.metadata_owners {
        assert!(
            filter.may_own_extra_roots(*owner),
            "snapshotted metadata owner {owner:#x} rejected"
        );
    }
    for owner in &keys.overlay_owners {
        assert!(
            filter.may_own_extra_roots(*owner),
            "snapshotted overlay owner {owner:#x} rejected"
        );
    }
    for class_id in &keys.class_ids {
        assert!(
            filter.may_pin_loader(*class_id),
            "snapshotted class {class_id} rejected"
        );
    }
}

/// A provider that will not enumerate its owners must disable overlay
/// filtering ENTIRELY, for every address.
///
/// This is the soundness case, and it is not hypothetical: `zgc.rs`'s own
/// test provider returns `None` from `owner_addrs` while serving roots
/// from `roots_for_owner`. An address bloom built from an index that
/// provider is absent from would answer "definitely not" for the very
/// object it owns roots for, and the overlay would be swept while its
/// owner survived.
#[test]
fn a_provider_that_will_not_name_its_owners_disables_the_overlay_filter() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    let Some(bank) = filter.live_for_test() else {
        return; // the kill switch is set in this process; nothing to assert
    };
    // Simulate the decline rather than registering a provider: provider
    // registration is process-global and permanent, so a test that did it
    // for real would change every later test in this binary.
    bank.overlay_unfilterable.store(true, Ordering::Relaxed);
    for addr in [0x1000usize, 0x2000, 0xdead_beef_0000] {
        assert!(
            filter.may_own_overlay(addr),
            "{addr:#x} was excluded from the overlay lookup while a provider \
             had declined to enumerate its owners"
        );
    }
    // The other two tables are unaffected: their key sets are exact.
    bank.overlay_unfilterable.store(false, Ordering::Relaxed);
    if !bank.any_addr.load(Ordering::Relaxed) {
        assert!(!filter.may_own_overlay(0x1000));
    }
}


/// An unbuilt filter must answer "maybe" to everything: a caller that
/// forgets to arm it gets the old cost, not a dropped edge.
#[test]
fn an_unarmed_filter_proves_nothing() {
    let filter = ZMarkRootFilter::default();
    assert!(filter.may_own_extra_roots(0xdead_0000));
    assert!(filter.may_pin_loader(7));
    assert!(filter.may_pin_loader(u32::MAX));
    filter.rebuild();
    filter.disarm();
    assert!(filter.may_own_extra_roots(0xdead_0000));
    assert!(filter.may_pin_loader(7));
}

/// A class id past the bitmap must fall back rather than answer from a
/// bit it does not have.
#[test]
fn a_class_id_past_the_bitmap_takes_the_slow_path() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    let Some(bank) = filter.live_for_test() else {
        return; // the kill switch is set in this process
    };
    bank.any_class.store(true, Ordering::Relaxed);
    let past = (Z_ROOT_CLASS_WORDS * 64) as u32;
    // No overflow recorded: nothing that large is registered, so the
    // bitmap's "definitely not" still stands.
    assert!(!filter.may_pin_loader(past));
    bank.class_overflow.store(true, Ordering::Relaxed);
    assert!(
        filter.may_pin_loader(past),
        "with a class id past the bitmap registered, every out-of-range id \
         must take the real lookup"
    );
}

/// The empty case is the common one and must cost nothing: with no table
/// written, every question is answered by one flag.
#[test]
fn an_empty_process_rejects_every_address() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    let Some(bank) = filter.live_for_test() else {
        return; // the kill switch is set in this process
    };
    if bank.any_addr.load(Ordering::Relaxed) {
        return; // another test in this process registered something
    }
    for addr in [0x1000usize, 0x2000, 0xffff_0000] {
        assert!(!filter.may_own_extra_roots(addr));
    }
}

/// A rebuild must write the bank that is **not** published, so a reader
/// that raced past the disarm keeps reading a complete snapshot.
///
/// This is the whole of the 2026-09-21 change, and it is not decoration:
/// `zgc.rs`'s `collect_garbage` calls `rebuild()` *before*
/// `finish_concurrent_mark`, i.e. while the concurrent mark workers are
/// still live and still calling `extra_root_probe`. With one bank in place,
/// the clearing loop is visible to a reader that has already executed its
/// `armed` load — and this filter's `false` is a proof, so what that reader
/// gets is not a slower answer but a dropped live edge.
///
/// A single-bank regression is caught by the middle assertion: it would
/// find the marker word cleared.
#[test]
fn a_rebuild_writes_the_other_bank_and_leaves_the_published_one_intact() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    let first = filter.state.load(Ordering::Relaxed);
    if first == Z_ROOT_DISARMED {
        return; // the kill switch is set in this process
    }

    // A word only this bank has, standing in for the snapshot a racing
    // reader is in the middle of consulting.
    const MARKER: u64 = 0xDEAD_BEEF_CAFE_F00D;
    filter.live_for_test().expect("just armed").addr_bloom[0].store(MARKER, Ordering::Relaxed);

    filter.rebuild();
    let second = filter.state.load(Ordering::Relaxed);
    assert_ne!(
        second, first,
        "a rebuild must publish the OTHER bank; publishing the same one is \
         the single-bank arrangement with extra steps"
    );
    assert_eq!(
        filter.banks[(first - 1) & 1].addr_bloom[0].load(Ordering::Relaxed),
        MARKER,
        "the rebuild cleared the bank a concurrent reader could still be \
         reading — that reader's `false` is a proof, and it is now wrong"
    );

    // Two banks, alternating, so the arrangement is bounded rather than a
    // growing list of snapshots nobody frees.
    filter.rebuild();
    assert_eq!(filter.state.load(Ordering::Relaxed), first);
}

/// The three read entry points must never disagree: they are one probe of
/// one snapshot, and `zgc.rs` uses the pair form while the tests here state
/// the properties in terms of the single-question forms.
#[test]
fn the_three_read_entry_points_agree() {
    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    for addr in [0usize, 0x1000, 0x2000, 0xdead_beef_0000, usize::MAX & !7] {
        let (pins, overlay) = filter.extra_root_probe(addr);
        assert_eq!(filter.may_own_extra_roots(addr), pins, "addr={addr:#x}");
        assert_eq!(filter.may_own_overlay(addr), overlay, "addr={addr:#x}");
    }
    filter.disarm();
    for addr in [0x1000usize, 0x2000] {
        assert_eq!(filter.extra_root_probe(addr), (true, true));
        assert!(filter.may_own_extra_roots(addr));
        assert!(filter.may_own_overlay(addr));
    }
}

// ---- stage 1 + stage 2: the concurrent validity machinery --------------
//
// `handoff-i-arm-root-filter-concurrently.md`. These tests write the
// PROCESS-GLOBAL pin registries, so they take a lock against each other.
// Other `gc` unit tests (the g1 ones among them) DO write the registries
// in the same binary, without this lock (corrected zs s5/mc3, residual 14
// of the s2/mc2 page): the assertions below are written to hold whatever
// else is registered -- they test this table's own rows and the
// generation moving, never an exact registry size. The `cratonvm-types`
// unit tests are a different binary.

static REGISTRY_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// A VM identity and an address range no other test in this crate uses.
const R_VM: usize = 0x00F0_0D01;
const R_LOADER: usize = 0x0070_0000;
const R_MIRROR: usize = 0x0070_0080;

/// An unarmed filter proved nothing, so there is nothing to repair and
/// nothing that could have gone stale.
///
/// This is the shape every non-concurrent caller sees, and it must cost a
/// load and a branch rather than four table walks.
#[test]
fn an_unarmed_filter_has_nothing_to_repair() {
    let filter = ZMarkRootFilter::default();
    assert!(
        filter.is_current(),
        "nothing published, nothing to invalidate"
    );
    let late = filter.late_owners();
    assert!(late.addrs.is_empty());
    assert!(late.certifiable);
    assert_eq!(late.sources_moved, 0);
}

/// **The hole the generation check does not close, and the pass that does.**
///
/// A loader that is absent from the snapshot is *proved* to own no mirrors,
/// and the marker skips its mirror edge. If that loader then defines a class
/// while the concurrent phase runs, the mirror is one the marker will never
/// visit: the loader is already marked, so nothing re-claims it and nothing
/// re-scans it. Noticing afterwards that the generation moved does not
/// un-hand-out the proof.
///
/// `late_owners` is what makes the armed filter sound rather than merely
/// fast, and this test states that in the only terms that matter: the
/// mirror address must come back out.
#[test]
fn an_owner_registered_after_the_snapshot_comes_back_from_late_owners() {
    let _g = REGISTRY_LOCK.lock();
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    if filter.live_for_test().is_none() {
        return; // the kill switch is set in this process
    }
    let proved_absent = !filter.may_own_extra_roots(R_LOADER);

    cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);

    assert!(
        !filter.is_current(),
        "a registration must invalidate the snapshot; without this the \
         per-drain-batch check is blind and the filter keeps proving a \
         negative that stopped being true"
    );
    if proved_absent {
        assert!(
            !filter.may_own_extra_roots(R_LOADER),
            "the snapshot is a snapshot -- it still answers the stale \
             `false`, which is exactly why the mark-end repair exists"
        );
    }

    let late = filter.late_owners();
    assert!(
        late.addrs.contains(&(R_MIRROR as u64)),
        "the mirror of a loader registered mid-cycle must be re-pushed; \
         got {:#x?}",
        late.addrs
    );
    assert!(late.sources_moved >= 1);

    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
}

/// **A disarm mid-cycle must not make the repair silently empty.**
///
/// `is_current() == false` tells a caller to disarm, and that is the whole
/// point of the check: stop handing out proofs. It says nothing about the
/// proofs already handed out, and those are precisely what has to be
/// repaired. If `late_owners` keyed off `state` rather than `repair_bank`,
/// the one path that most needs the repair -- the cycle where something
/// really did register -- is the one where it would return nothing.
#[test]
fn a_disarm_mid_cycle_does_not_discard_the_repair() {
    let _g = REGISTRY_LOCK.lock();
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    if filter.live_for_test().is_none() {
        return;
    }
    cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);
    // What a per-drain-batch `is_current()` failure asks the caller to do.
    filter.disarm();
    assert!(
        filter.may_own_extra_roots(R_LOADER),
        "disarmed: prove nothing"
    );

    let late = filter.late_owners();
    assert!(
        late.addrs.contains(&(R_MIRROR as u64)),
        "disarming stops NEW proofs; it does not repair the old ones"
    );

    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
}

/// **A row that disappears during the cycle is unrepairable, and must be
/// reported as such rather than repaired badly.**
///
/// The difference set can only re-push an owner that is still an owner at
/// mark end. An owner added and then removed inside the cycle can have had
/// a wrong `false` taken against it and leaves nothing behind to push, so
/// no comparison of key sets recovers it. A removal is the one observable
/// sign that the append-only precondition has been broken, and the caller
/// must then decline to certify the mark set -- which is a fallback to a
/// stop-the-world mark, not a dropped edge.
#[test]
fn a_row_removed_during_the_cycle_refuses_to_certify() {
    let _g = REGISTRY_LOCK.lock();
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);
    cratonvm_types::mirror_pin::add_mirror_pin(R_VM, R_LOADER, R_MIRROR);

    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    if filter.live_for_test().is_none() {
        return;
    }
    cratonvm_types::mirror_pin::forget_vm_mirror_pins(R_VM);

    let late = filter.late_owners();
    assert!(
        !late.certifiable,
        "a snapshot key set that is no longer a subset of the live one \
         means this mark set cannot be certified"
    );
    // zs s2/mc2: and the refusal names the mirror table.
    assert_eq!(late.refused & Z_LATE_REFUSED_MIRROR, Z_LATE_REFUSED_MIRROR);
}

/// A class that gains a pinned defining loader mid-cycle must re-push that
/// LOADER, because `loader_pin` is keyed by class id and the value is the
/// edge.
#[test]
fn a_class_pinned_after_the_snapshot_repushes_its_loader() {
    // Inside the class bitmap, so the exclusion below is exact rather than
    // an `class_overflow` fallback.
    const R_CLASS: u32 = 60_001;
    let _g = REGISTRY_LOCK.lock();
    cratonvm_types::loader_pin::forget_vm_loader_pins(R_VM);

    let filter = ZMarkRootFilter::default();
    filter.rebuild();
    if filter.live_for_test().is_none() {
        return;
    }
    assert!(
        !filter.may_pin_loader(R_CLASS),
        "an unregistered class id must be excluded -- otherwise the stale \
         proof this test is about was never taken"
    );
    cratonvm_types::loader_pin::set_loader_pin(R_VM, R_CLASS, R_LOADER);
    assert!(!filter.is_current());

    let late = filter.late_owners();
    assert!(
        late.addrs.contains(&(R_LOADER as u64)),
        "the defining loader of a class pinned mid-cycle must be re-pushed"
    );

    cratonvm_types::loader_pin::forget_vm_loader_pins(R_VM);
}

/// Updating row values leaves every owner present. The registry returns
/// hash-map iteration order, so survival checks must not assume it is sorted.
#[test]
fn updating_many_pin_rows_without_removing_owners_still_certifies() {
    const VM: usize = 0x00F0_0D18;
    const ARENA: std::ops::Range<usize> = 0x0078_0000..0x0079_0000;
    let _g = REGISTRY_LOCK.lock();
    let entries: Vec<(usize, usize)> = (0..32)
        .map(|i| (ARENA.start + (31 - i) * 24, ARENA.start + (31 - i) * 24 + 16))
        .collect();
    for mirror in [true, false] {
        let replace = |rows: &[(usize, usize)]| {
            if mirror {
                cratonvm_types::mirror_pin::replace_mirror_pins(VM, rows);
            } else {
                cratonvm_types::metadata_pin::replace_metadata_pins(VM, rows);
            }
        };
        replace(&entries);
        let filter = ZMarkRootFilter::default();
        filter.rebuild_within(ARENA);
        let updated: Vec<_> = entries.iter().map(|&(o, v)| (o, v + 8)).collect();
        replace(&updated);
        assert!(
            filter.late_owners_within(ARENA).certifiable,
            "mirror={mirror}: changing values must not invent an owner removal"
        );
        // A real removal must still force a safe stop-the-world fallback.
        replace(&updated[1..]);
        assert!(!filter.late_owners_within(ARENA).certifiable);
        replace(&[]);        }    }
#[test]
fn visit_extra_object_edges_of_visits_loader_pin_without_addr_bloom_hit() {
    const VM: usize = 0x00F0_0D99;
    const CID: u32 = 9812;
    let heap = super::super::ZgcRealHeap::new_shared(16 * 1024 * 1024);
    let mirror = heap.arena_base + 0x1000;
    let obj_base = heap.arena_base + 0x2000;
    cratonvm_types::loader_pin::forget_vm_loader_pins(VM);
    cratonvm_types::loader_pin::set_loader_pin(VM, CID, mirror);
    heap.counters
        .mark_root_filter
        .rebuild_within(heap.arena_base..heap.arena_end);
    let mut seen = Vec::new();
    heap.visit_extra_object_edges_of(obj_base, CID, &mut |edge| seen.push(edge));
    cratonvm_types::loader_pin::forget_vm_loader_pins(VM);
    assert_eq!(seen, vec![mirror as u64]);
}
