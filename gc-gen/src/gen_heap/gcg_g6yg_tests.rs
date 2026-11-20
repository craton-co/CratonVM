use super::*;

/// gcg g6/yg: the parallel copy is refused on a moving cycle with both
/// generations nearly full (the vg3 abort), kept otherwise, and the
/// in-place arm keeps its own opt-in whatever the risk.
#[test]
fn g6yg_a_full_heap_moving_cycle_copies_serially() {
    // (par_evac, in_place, par_in_place, risk) -> permitted
    assert!(parallel_young_copy_permitted(true, false, false, false));
    assert!(
        !parallel_young_copy_permitted(true, false, false, true),
        "the abort case"
    );
    assert!(!parallel_young_copy_permitted(false, false, false, false));
    assert!(
        parallel_young_copy_permitted(true, true, true, true),
        "in-place keeps parallel"
    );
    assert!(!parallel_young_copy_permitted(true, true, false, false));
}

/// gcg g6/yg (vg4 `yg4_beans_gen_*`): 38 pauses, 22 of them ran an
/// unrequested major: `Copy` books 38 (every young half that ran) and
/// `MarkSweepCompact` 22, and the young halves' time moves from the full
/// collector's sum to the young one's. No escalation: unchanged.
#[test]
fn g6yg_escalated_pauses_book_their_young_half_under_copy() {
    let all = (38, 10_000, 40);
    let old = (22, 8_000, 25);
    let beans = |(a, o)| cratonvm_gc_core::gc_metrics::serial_collectors(a, o);
    let before = beans((all, old));
    assert_eq!(
        (before[0].count, before[1].count),
        (16, 22),
        "the vg4 undercount"
    );
    let after = beans(serial_totals_with_escalations(all, old, (22, 3_000)));
    assert_eq!((after[0].count, after[1].count), (38, 22));
    assert_eq!(after[0].time_ms, (2_000 + 3_000) / 1_000);
    assert_eq!(after[1].time_ms, (8_000 - 3_000) / 1_000);
    assert_eq!(serial_totals_with_escalations(all, old, (0, 0)), (all, old));
    // A young-half sum above the old sum cannot underflow.
    assert_eq!(
        serial_totals_with_escalations(all, old, (1, 9_999)).1,
        (22, 0, 25)
    );
}

/// gcg g6/yg: a requested major leaves no mark; an unrequested one does,
/// and taking it clears it.
#[test]
fn g6yg_only_an_unrequested_major_marks_the_young_half() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    let other = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    assert!(
        heap.take_escalation_mark().is_none(),
        "a new heap has no mark"
    );
    heap.note_escalation_mark(true);
    assert!(
        heap.take_escalation_mark().is_none(),
        "System.gc() books MarkSweepCompact only"
    );
    heap.note_escalation_mark(false);
    assert!(
        other.take_escalation_mark().is_none(),
        "per heap, not ambient"
    );
    assert!(heap.take_escalation_mark().is_some());
    assert!(heap.take_escalation_mark().is_none(), "taken once");
}

/// gcg h1/ob: an escalated young pause books the split the VM's `-Xlog`
/// reads -- the mark's instant and the old occupancy its decision saw --
/// once; a requested major and a cycle without a decision book none, and a
/// fresh cycle entry drops a split nobody took.
#[test]
fn gch_h1ob_escalated_pause_books_one_split_with_the_decisions_old_figure() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(true);
    heap.note_escalation_decision(4096);
    heap.note_escalation_mark(false);
    heap.note_collection_pause(std::time::Duration::from_micros(500), false);
    let split = heap.take_escalation_split();
    let booked = true;
    assert_eq!(split.is_some(), booked, "booked with the Copy half only");
    if let Some(s) = split {
        assert_eq!(s.old_used_at_mark, 4096);
    }
    assert!(heap.take_escalation_split().is_none(), "taken once");
    // A requested major: no mark, so no split.
    heap.note_escalation_decision(4096);
    heap.note_escalation_mark(true);
    heap.note_collection_pause(std::time::Duration::from_micros(500), false);
    assert!(
        heap.take_escalation_split().is_none(),
        "System.gc() splits nothing"
    );
    // A mark with no decision books nothing (no old figure to print).
    let _ = heap.take_escalation_mark();
    heap.note_escalation_mark(false);
    heap.note_collection_pause(std::time::Duration::from_micros(500), false);
    assert!(
        heap.take_escalation_split().is_none(),
        "no decision, no split"
    );
    // A cycle entry's take clears a split nobody took.
    heap.note_escalation_decision(8);
    heap.note_escalation_mark(false);
    heap.note_collection_pause(std::time::Duration::from_micros(500), false);
    let _ = heap.take_escalation_mark();
    assert!(
        heap.take_escalation_split().is_none(),
        "a fresh cycle drops it"
    );
    cratonvm_gc_core::gc_quiescence::set_old_gen_reclaimed(false);
}
