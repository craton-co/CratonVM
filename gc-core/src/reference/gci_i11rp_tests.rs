use super::*;

/// A processor on HotSpot's soft LRU inputs, with the stamp switch as given.
fn hotspot_proc(stamp_gc_clock: bool) -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new();
    proc.hotspot_soft_lru = true;
    proc.soft_stamp_gc_clock = stamp_gc_clock;
    proc
}

/// R1: the finalizable objects the collection resurrected are this round's
/// FinalReference census, active rows only, for one round only; nothing
/// else about the round changes.
#[test]
fn gci_i11rp_resurrected_finalizables_are_counted_in_the_round_census() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, 0x100, 0x100, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x140, 0x140, None);
    proc.discover_reference(ReferenceType::Finalizer, 0x180, 0x180, None);
    // 0x180's row was flagged by an earlier channel report (queued, and
    // offered to the collector again while it waits).
    proc.mark_finalizer_enqueued(&[0x180]);
    // This collection resurrected 0x100, the queued 0x180 and a child.
    proc.note_resurrected_finalizables(&[0x100, 0x180, 0x200]);
    let result = proc.process_references(&|_| true, 64, 0);
    assert_eq!(result.stats.finalizer_refs_resurrected, 1);
    assert_eq!(result.stats.finalizer_refs_enqueued, 0);
    assert!(result.to_finalize.is_empty());
    // The set describes one collection.
    let again = proc.process_references(&|_| true, 64, 0);
    assert_eq!(again.stats.finalizer_refs_resurrected, 0);
}

/// R2: with the switch a read between two collections stamps the clock of
/// the first (HotSpot's `timestamp = clock`), so the collection after the
/// next one measures the whole interval and condemns an idle reference
/// HotSpot clears; without it the read's wall clock keeps it.
#[test]
fn gci_i11rp_soft_stamp_gc_clock_takes_hotspots_timestamp_equals_clock() {
    for (on, expect_condemned) in [(false, false), (true, true)] {
        let mut proc = hotspot_proc(on);
        // The previous collection ended at 10_000 with 1 MiB free: the
        // max interval is 1000 ms.
        proc.soft_gc_clock_ms = 10_000;
        proc.soft_free_mb_at_last_gc = Some(1);
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        // Read at wall clock 12_500, after that collection.
        proc.touch_soft_reference(0x100, 12_500);
        let stamp = proc.active_soft_stamps()[0].1;
        assert_eq!(stamp, if on { 10_000 } else { 12_500 }, "switch {on}");
        // The next collection ends at 13_000 (it kept the reference: its
        // clock was 10_000); the one after that judges it on 13_000.
        proc.commit_hotspot_soft_clock(1, 13_000);
        let condemned = proc.condemn_idle_soft_refs(1, 13_100);
        assert_eq!(!condemned.is_empty(), expect_condemned, "switch {on}");
    }
}

/// zt t11/gn3 (`CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE`): a round noted
/// as an armed young cycle's keeps the free-at-last-gc figure the last
/// whole-heap round stored, for that round only, and still commits the
/// clock; a note no round consumed does not outlive
/// `finish_pre_gc_cycle`. Kill control: the un-noted round stores its own
/// figure (the positive control that this test can fail).
#[test]
fn zt11gn3_an_armed_young_round_can_hold_the_soft_free_figure() {
    let mut proc = hotspot_proc(true);
    proc.soft_gc_clock_ms = 10_000;
    proc.soft_free_mb_at_last_gc = Some(250);
    proc.set_young_round_holds_soft_free(true);
    let _ = proc.process_references(&|_| true, 100, 20_000);
    assert_eq!(proc.soft_free_mb_at_last_gc, Some(250), "the minor held it");
    assert_eq!(proc.soft_gc_clock(), 20_000, "the clock commit is unchanged");
    proc.finish_pre_gc_cycle();
    let _ = proc.process_references(&|_| true, 100, 21_000);
    assert_eq!(proc.soft_free_mb_at_last_gc, Some(100), "the next round stores");
    proc.set_young_round_holds_soft_free(true);
    proc.finish_pre_gc_cycle();
    let _ = proc.process_references(&|_| true, 32, 22_000);
    assert_eq!(proc.soft_free_mb_at_last_gc, Some(32), "a stale note is dropped");
}

/// zt t10/gn2 (GN-20 retired): on HotSpot's soft inputs with the stamp on
/// the GC clock and rows bound at allocation (the default of both launcher
/// modes over a real JDK), a round with no pre-collection condemn and no
/// caller clock -- what a ZGC generational young cycle that kept its
/// referents runs -- leaves the soft clock where the last whole-heap
/// round put it, as HotSpot 25's minor does. Positive control: a
/// wall-clock touch (the `get()` / constructor bridge, or the
/// `CRATONVM_SOFTREF_STAMP_GC_CLOCK=0` arm) between the rounds makes the
/// same round advance the clock -- the only way such a round can.
#[test]
fn zt10gn2_an_armed_young_round_leaves_the_soft_clock() {
    for wall_touch in [false, true] {
        let mut proc = hotspot_proc(true);
        // The last major: condemned at 10_000 and committed it.
        proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
        let _ = proc.condemn_idle_soft_refs(64, 10_000);
        let _ = proc.process_references(&|_| true, 64, 0);
        proc.finish_pre_gc_cycle();
        assert_eq!(proc.soft_gc_clock(), 10_000);
        // After it: a `SoftReference` allocated at wall 12_000 and bound
        // by the next pre-collection pass (stamped on the clock), and a
        // Java `timestamp` refresh with the published clock.
        proc.note_allocated_reference(ReferenceType::Soft, 0x300, 7, false);
        let pending = proc.take_allocated_unbound();
        assert_eq!(pending.len(), 1);
        for p in pending {
            let _ = proc.bind_allocated_reference(p, 0x400, None);
        }
        proc.touch_soft_reference(0x100, 10_000);
        if wall_touch {
            proc.touch_soft_reference(0x100, 12_500);
        }
        // The young round: no condemn, caller clock 0.
        let _ = proc.process_references(&|_| true, 64, 0);
        let expect = if wall_touch { 12_500 } else { 10_000 };
        assert_eq!(proc.soft_gc_clock(), expect, "wall touch {wall_touch}");
    }
}

/// R2: values already on the clock's scale are kept, a wall-clock read is
/// capped, a row with no known access takes the clock, the allocation
/// stamp is the clock, and a new clock invalidates the fast-path stamps
/// (only with the switch).
#[test]
fn gci_i11rp_soft_stamp_keeps_clock_scale_values_and_bumps_the_epoch() {
    let mut proc = hotspot_proc(true);
    // Before the first round there is no clock: wall stamps, as before.
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 5_000);
    assert_eq!(proc.active_soft_stamps(), vec![(0x100, 5_000)]);
    let epoch = proc.soft_touch_epoch_now();
    proc.commit_hotspot_soft_clock(64, 8_000);
    assert_eq!(proc.soft_gc_clock(), 8_000);
    assert_eq!(proc.soft_touch_epoch_now(), epoch + 1);
    // The same clock again: nothing to invalidate.
    proc.commit_hotspot_soft_clock(64, 8_000);
    assert_eq!(proc.soft_touch_epoch_now(), epoch + 1);
    // A clock-scale value (the Java `timestamp` field) below the clock.
    proc.touch_soft_reference(0x100, 6_000);
    assert_eq!(proc.active_soft_stamps(), vec![(0x100, 6_000)]);
    // A wall-clock read is capped at the clock.
    proc.touch_soft_reference(0x100, 9_999);
    assert_eq!(proc.active_soft_stamps(), vec![(0x100, 8_000)]);

    // No observed access: the constructor's `timestamp = clock`.
    let mut fresh = hotspot_proc(true);
    fresh.soft_gc_clock_ms = 4_000;
    fresh.discover_reference(ReferenceType::Soft, 0x300, 0x400, None);
    assert_eq!(fresh.active_soft_stamps(), vec![(0x300, 4_000)]);

    // Discovery at allocation: the unbound row and the bound one.
    let mut alloc = hotspot_proc(true);
    alloc.soft_gc_clock_ms = 4_000;
    alloc.note_allocated_reference(ReferenceType::Soft, 0x500, 7, false);
    let pending = alloc.take_allocated_unbound();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].row.last_access_time_ms, 4_000);
    let p = pending.into_iter().next().unwrap();
    assert_eq!(alloc.bind_allocated_reference(p, 0x600, None), BindOutcome::Bound);
    assert_eq!(alloc.active_soft_stamps(), vec![(0x500, 4_000)]);

    // Off: wall stamps and no epoch bump at the commit.
    let mut off = hotspot_proc(false);
    off.soft_gc_clock_ms = 4_000;
    off.discover_reference(ReferenceType::Soft, 0x700, 0x800, None);
    off.touch_soft_reference(0x700, 9_000);
    assert_eq!(off.active_soft_stamps(), vec![(0x700, 9_000)]);
    let e0 = off.soft_touch_epoch_now();
    off.commit_hotspot_soft_clock(64, 12_000);
    assert_eq!(off.soft_touch_epoch_now(), e0);
}

/// R4: two ACTIVE rows at one address (a dead `Reference`'s and a new one
/// on the recycled block): a refusal converts the row of the refused pair;
/// by address alone, the later registration; never a phantom.
#[test]
fn gci_i11rp_a_refused_restore_converts_the_row_of_its_pair() {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, 0x100, 0x200, Some(0x900));
    proc.discover_reference(ReferenceType::Weak, 0x100, 0x300, Some(0x910));
    assert!(proc.clear_after_refused_restore_of(0x100, 0x200));
    assert!(proc.weak_refs[0].cleared);
    assert!(!proc.weak_refs[1].cleared);
    assert_eq!(proc.pending_queues.get(&0x900), Some(&vec![0x100]));
    assert!(!proc.pending_queues.contains_key(&0x910));

    let mut by_addr = ReferenceProcessor::new();
    by_addr.discover_reference(ReferenceType::Weak, 0x100, 0x200, Some(0x900));
    by_addr.discover_reference(ReferenceType::Weak, 0x100, 0x300, Some(0x910));
    assert!(by_addr.clear_after_refused_restore(0x100));
    assert!(!by_addr.weak_refs[0].cleared);
    assert!(by_addr.weak_refs[1].cleared);
    assert_eq!(by_addr.pending_queues.get(&0x910), Some(&vec![0x100]));
    // An unknown referent falls back to the later registration too.
    let mut unknown = ReferenceProcessor::new();
    unknown.discover_reference(ReferenceType::Weak, 0x100, 0x200, None);
    unknown.discover_reference(ReferenceType::Weak, 0x100, 0x300, None);
    assert!(unknown.clear_after_refused_restore_of(0x100, 0x999));
    assert!(!unknown.weak_refs[0].cleared);
    assert!(unknown.weak_refs[1].cleared);

    // A soft row is found although the address index names a newer,
    // already settled row at the same address.
    let mut soft = ReferenceProcessor::new();
    soft.discover_reference(ReferenceType::Soft, 0x500, 0x600, None);
    soft.discover_reference(ReferenceType::Soft, 0x500, 0x700, None);
    soft.soft_refs[1].cleared = true;
    assert!(soft.clear_after_refused_restore_of(0x500, 0x600));
    assert!(soft.soft_refs[0].cleared);
    assert!(!soft.clear_after_refused_restore(0x500), "nothing active is left");

    let mut phantom = ReferenceProcessor::new();
    phantom.discover_reference(ReferenceType::Phantom, 0x800, 0x810, Some(0x820));
    assert!(!phantom.clear_after_refused_restore_of(0x800, 0x810));
    assert!(!phantom.phantom_refs[0].enqueued);
}

/// R5: the production door (`enqueue_unfinalized`) has no address gate:
/// a dequeued address is accepted again (its row decides), and a queued
/// one follows a relocation. (The legacy gate is gone, gcj j4/rf.)
#[test]
fn gci_i11rp_production_finalization_has_no_address_gate() {
    let ft = FinalizerThread::new();
    assert!(ft.enqueue_unfinalized(0xA0));
    assert_eq!(ft.dequeue(), Some(0xA0));
    assert!(ft.enqueue_unfinalized(0xA0));
    assert_eq!(ft.dequeue(), Some(0xA0));

    assert!(ft.enqueue_unfinalized(0xB0));
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xB0, 0xC0);
    ft.update_after_gc(&map);
    assert_eq!(ft.dequeue(), Some(0xC0));
}

/// R7: the refused-restore count is the processor's own (one per VM).
#[test]
fn gci_i11rp_restore_refusals_are_counted_per_processor() {
    let mut a = ReferenceProcessor::new();
    let b = ReferenceProcessor::new();
    a.note_restore_refused();
    a.note_restore_refused();
    assert_eq!(a.restore_refusals(), 2);
    assert_eq!(b.restore_refusals(), 0);
}

/// R10 (gci i20/gn): before the first round, the switch stamps the
/// construction-time origin, as HotSpot stamps the VM-start clock; off, the
/// stamp is unchanged.
#[test]
fn gci_i20gn_soft_stamp_before_the_first_round_is_the_clock_origin() {
    for on in [false, true] {
        let mut proc = hotspot_proc(on);
        proc.soft_gc_clock_ms = 0;
        proc.soft_clock_origin_ms = 1_000;
        assert_eq!(proc.soft_stamp(5_000), if on { 1_000 } else { 5_000 }, "switch {on}");
        assert_eq!(proc.soft_stamp(0), if on { 1_000 } else { 0 }, "switch {on}");
        proc.soft_gc_clock_ms = 9_000;
        assert_eq!(proc.soft_stamp(12_000), if on { 9_000 } else { 12_000 }, "switch {on}");
    }
}

#[test]
fn java_soft_clock_initialization_uses_the_owning_processors_origin_and_rounds() {
    let mut a = ReferenceProcessor::new();
    let mut b = ReferenceProcessor::new();
    a.soft_clock_origin_ms = 1_000;
    b.soft_clock_origin_ms = 2_000;
    assert_eq!(a.soft_clock_for_java_initialization(), 1_000);
    assert_eq!(b.soft_clock_for_java_initialization(), 2_000);
    a.commit_hotspot_soft_clock(64, 5_000);
    assert_eq!(a.soft_clock_for_java_initialization(), 5_000);
    assert_eq!(b.soft_clock_for_java_initialization(), 2_000);
}
