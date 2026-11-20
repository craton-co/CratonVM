use super::*;

#[test]
fn records_both_directions_independently() {
    let c = BranchCounters::new(4);
    c.record(1, true);
    c.record(1, true);
    c.record(1, false);
    assert_eq!(c.counts(1), (2, 1));
    assert_eq!(c.counts(0), (0, 0));
    assert_eq!(c.observed().collect::<Vec<_>>(), vec![(1, 2, 1)]);
}

/// A per-method arm is sticky, reports only its first call, and is seen
/// by every later caller of the slot's own length.
#[test]
fn a_method_arm_is_sticky_and_shared_by_the_slots_own_length() {
    let c = BranchCounters::new(4);
    assert!(!c.is_armed());
    assert!(c.arm(), "the first arm arms");
    assert!(!c.arm(), "a second one reports it already was");
    assert!(c.is_armed());

    let mut p = MethodProfile::default();
    let first = p.flat_branches_for(4);
    assert!(!first.is_armed());
    first.arm();
    let again = p.flat_branches_for(4);
    assert!(Arc::ptr_eq(&first, &again), "one array per slot");
    assert!(again.is_armed());
}

/// Round 11 wave 14 (r11-tier-profile-replay-residuals item 2): a caller
/// with another length is another version of the method. It gets counters
/// of its own, which never reach the slot's, and the slot is marked so
/// its record never seeds a later run. The owner's handle stays live.
#[test]
fn another_versions_length_gets_detached_counters_and_marks_the_slot() {
    let mut p = MethodProfile::default();
    let owner = p.flat_branches_for(8);
    owner.record(2, true);
    assert_eq!(p.recorded_code_len(), 8);

    let other = p.flat_branches_for(64);
    assert!(!Arc::ptr_eq(&owner, &other));
    other.record(40, true);
    other.record(2, false);
    assert_eq!(p.recorded_code_len(), CODE_LEN_CONFLICT);

    // The owner's handle is still the slot's array, not an orphan.
    assert!(Arc::ptr_eq(&owner, &p.flat_branches_for(8)));
    owner.record(2, true);
    let snap = p.snapshot();
    assert_eq!(snap.branches[&2].taken, 2);
    assert_eq!(
        snap.branches[&2].not_taken, 0,
        "the other version's pc 2 is not folded"
    );
    assert!(!snap.branches.contains_key(&40));
    assert_eq!(
        snap.recorded_code_len(),
        CODE_LEN_CONFLICT,
        "a snapshot keeps the mark"
    );
}

/// A pc past the end is DROPPED, never a panic: this runs on the
/// interpreter's dispatch loop, and a disagreement between the recorded
/// code length and the pc executing must not take the VM down.
#[test]
fn an_out_of_range_pc_is_dropped_rather_than_panicking() {
    let c = BranchCounters::new(2);
    c.record(9999, true);
    assert_eq!(c.counts(9999), (0, 0));
    assert!(c.observed().next().is_none());
}

/// Saturating, not wrapping. `record_receiver`'s comment states what
/// wrapping costs — the majority direction reading as the minority — and
/// this counter is read by the same consumers.
#[test]
fn a_counter_at_the_ceiling_stays_there() {
    let c = BranchCounters::new(1);
    c.taken[0].store(u32::MAX, Ordering::Relaxed);
    c.record(0, true);
    c.record(0, true);
    assert_eq!(c.counts(0), (u32::MAX, 0));
}

/// A pinned counter never passes through `0` on the way to staying pinned.
/// The old `fetch_add`-then-restore form wrapped first, so a reader in
/// between saw a maximally-hot branch as never-taken — and a writer that
/// slipped in restarted the count from zero.
#[test]
fn a_pinned_counter_is_never_momentarily_zero() {
    let c = Arc::new(BranchCounters::new(1));
    c.taken[0].store(u32::MAX, Ordering::Relaxed);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (c, stop) = (c.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut lowest = u32::MAX;
            while !stop.load(Ordering::Relaxed) {
                lowest = lowest.min(c.counts(0).0);
            }
            lowest
        })
    };
    for _ in 0..200_000 {
        c.record(0, true);
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        reader.join().expect("reader thread"),
        u32::MAX,
        "a saturated counter must never be observed below its ceiling"
    );
}

/// The fold is what every compiler-side reader actually sees, and it must
/// ADD the two sources rather than let one shadow the other: a method can
/// legitimately be recorded through both (a frame with a
/// `CachedBytecodeMethod` uses the fast path, one without does not).
#[test]
fn snapshot_adds_the_flat_counters_to_the_map() {
    let mut p = MethodProfile::default();
    p.record_branch(3, true); // map side
    let flat = p.flat_branches_for(8);
    flat.record(3, true); // fast side, same pc
    flat.record(5, false);

    let snap = p.snapshot();
    assert_eq!(snap.branches[&3].taken, 2, "both sources must be counted");
    assert_eq!(snap.branches[&5].not_taken, 1);
    assert!(
        snap.flat_branches.is_none(),
        "a snapshot must not fold twice",
    );
}

/// Round nio20 wave 2, lane L3: the fast-path invoke cells are folded into
/// `call_sites` (added to a slow-path row at the same pc), never into
/// `branches`, and they survive a regrow of the array.
#[test]
fn snapshot_folds_the_fast_path_call_counts_into_call_sites() {
    let mut p = MethodProfile::default();
    p.record_call_site(4); // slow path: once, at resolution
    let flat = p.flat_branches_for(16);
    for _ in 0..1000 {
        flat.record_call(4);
    }
    flat.record_call(9);
    flat.record(2, true);
    flat.record_call(99); // out of range: dropped, no panic
    let snap = p.snapshot();
    assert_eq!(snap.call_sites[&4], 1001);
    assert_eq!(snap.call_sites[&9], 1);
    assert_eq!(snap.call_site_count(4), CallSiteEvidence::Direct(1001));
    assert!(!snap.branches.contains_key(&4), "an invoke pc is no branch");
    assert_eq!(snap.branches[&2].taken, 1);
    assert_eq!(
        flat.observed_calls().collect::<Vec<_>>(),
        vec![(4, 1000), (9, 1)]
    );
}

/// A second-version caller does not disturb what the owner recorded:
/// `snapshot` still reports every observation made through the slot.
#[test]
fn another_version_keeps_the_observations_already_recorded() {
    let mut p = MethodProfile::default();
    p.flat_branches_for(4).record(2, true);
    let _other = p.flat_branches_for(64);
    assert_eq!(p.snapshot().branches[&2].taken, 1);
}
