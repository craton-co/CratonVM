use super::*;

// X: a dead finalizable object; Y: reachable only through X; Z: strongly
// reachable (marked). W1 -> X (queued), W2 -> Y, W3 -> Z, P -> X (phantom).
const X: usize = 0x100;
const Y: usize = 0x200;
const Z: usize = 0x400;
const W1: usize = 0x300;
const W2: usize = 0x340;
const W3: usize = 0x440;
const P: usize = 0x380;
const Q1: usize = 0x900;
const QP: usize = 0x910;

fn scenario() -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, X, X, None);
    proc.discover_reference(ReferenceType::Weak, W1, X, Some(Q1));
    proc.discover_reference(ReferenceType::Weak, W2, Y, None);
    proc.discover_reference(ReferenceType::Weak, W3, Z, None);
    proc.discover_reference(ReferenceType::Phantom, P, X, Some(QP));
    proc
}

fn marked(addr: usize) -> bool {
    matches!(addr, Z | W1 | W2 | W3 | P | Q1 | QP)
}

/// The remark's tracer: every UNMARKED object reachable from the roots
/// (the roots themselves included), as `g1_remark_reference_closure`.
fn trace(roots: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    for &r in roots {
        out.push(r);
        if r == X {
            out.push(Y);
        }
    }
    out
}

fn round(proc: &mut ReferenceProcessor) -> (ReferenceProcessingResult, Vec<usize>) {
    let result = proc.process_references_with_finalizer_trace(&marked, Some(&trace), 64, 0);
    let mut cleared = proc.take_newly_cleared();
    cleared.sort_unstable();
    (result, cleared)
}

#[test]
fn gci_i14gq_a_noted_round_clears_weak_refs_before_finalization() {
    // Not noted (the switch off): the finalizer-reachable closure keeps
    // both weak references -- the G1 remark's behaviour before the fix.
    let mut off = scenario();
    let (r_off, cleared_off) = round(&mut off);
    assert!(cleared_off.is_empty(), "{cleared_off:x?}");
    assert_eq!(r_off.to_finalize, vec![X]);
    assert!(r_off.to_enqueue.is_empty(), "{:x?}", r_off.to_enqueue);

    // Noted: W1 and W2 are cleared (W1 enqueued), W3 to a strongly
    // reachable object is kept, X is still finalized and the phantom to X
    // is still held back (phantom after finalization).
    let mut on = scenario();
    on.note_weak_before_final_round();
    let (r_on, cleared_on) = round(&mut on);
    assert_eq!(cleared_on, vec![W1, W2]);
    assert_eq!(r_on.to_finalize, vec![X]);
    assert_eq!(r_on.to_enqueue, vec![(W1, Q1)]);
    assert_eq!(r_on.stats.weak_refs_cleared, 2);
    assert_eq!(r_on.stats.phantom_refs_enqueued, 0);
    assert!(!r_on.retired.contains(&P));
}

#[test]
fn gci_i14gq_the_note_describes_one_round() {
    // Consumed by the round it precedes.
    let mut proc = scenario();
    proc.note_weak_before_final_round();
    let _ = round(&mut proc);
    assert!(!proc.weak_before_final_this_round);

    // Dropped by `finish_pre_gc_cycle` when no round consumed it.
    let mut stale = scenario();
    stale.note_weak_before_final_round();
    stale.finish_pre_gc_cycle();
    let (_, cleared) = round(&mut stale);
    assert!(cleared.is_empty(), "{cleared:x?}");
}

#[test]
fn full_soft_policy_cycle_keeps_one_previous_clock_across_its_rounds() {
    let mut proc = ReferenceProcessor::new();
    proc.hotspot_soft_lru = true;
    proc.soft_stamp_gc_clock = true;
    proc.soft_ref_lru_policy_ms_per_mb = 20;
    proc.soft_gc_clock_ms = 1_000;
    proc.soft_free_mb_at_last_gc = Some(64);
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    assert!(proc.try_begin_soft_policy_cycle());
    assert!(!proc.try_begin_soft_policy_cycle(), "another door must wait");
    for now in [3_000, 4_000] {
        let _ = proc.process_references_with_finalizer_trace(&|a| a == 0x100, None, 32, now);
        assert!(proc.take_newly_cleared().is_empty());
        assert_eq!(proc.hotspot_soft_policy(1), (1_000, 1_280));
        assert_eq!(proc.soft_gc_clock(), now, "master clock still publishes in the pause");
    }
    assert_eq!(proc.finish_soft_policy_cycle(), 4_000);
    assert!(proc.try_begin_soft_policy_cycle(), "completion releases the next door");
    assert_eq!(proc.finish_soft_policy_cycle(), 4_000);
    assert_eq!(proc.hotspot_soft_policy(1), (4_000, 640));
    let _ = proc.process_references_with_finalizer_trace(&|a| a == 0x100, None, 32, 5_000);
    assert_eq!(proc.take_newly_cleared().len(), 1, "the next collection sees the age");
}

#[test]
fn full_soft_policy_cycle_does_not_defer_last_ditch_clearing() {
    let mut proc = ReferenceProcessor::new();
    proc.hotspot_soft_lru = true;
    proc.soft_gc_clock_ms = 1_000;
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    assert!(proc.try_begin_soft_policy_cycle());
    assert_eq!(proc.condemn_all_soft_refs(), vec![(0x100, 0x200)]);
    let _ = proc.process_references_with_finalizer_trace(&|a| a == 0x100, None, 64, 1_001);
    assert_eq!(proc.take_newly_cleared().len(), 1);
    assert_eq!(proc.finish_soft_policy_cycle(), 1_001);
}

#[test]
fn full_soft_policy_cycle_without_a_round_leaves_inputs_unchanged() {
    let mut proc = ReferenceProcessor::new();
    proc.hotspot_soft_lru = true;
    proc.soft_gc_clock_ms = 1_000;
    proc.soft_free_mb_at_last_gc = Some(64);
    assert!(proc.try_begin_soft_policy_cycle());
    assert_eq!(proc.finish_soft_policy_cycle(), 1_000);
    assert_eq!(proc.soft_free_mb_at_last_gc, Some(64));
}

/// G-3: a noted round clock is committed by the round (HotSpot's
/// `update_soft_ref_master_clock` = now), only on HotSpot's soft inputs,
/// and never moves the clock backwards; the round's own soft verdict still
/// uses the previous clock.
#[test]
fn gci_i14gq_a_noted_round_clock_is_committed_by_the_round() {
    let mut proc = ReferenceProcessor::new();
    proc.hotspot_soft_lru = true;
    proc.soft_gc_clock_ms = 10_000;
    proc.soft_free_mb_at_last_gc = Some(1); // max interval 1000 ms
    // Read at 9_500 on the previous clock's scale: idle 500 ms, kept.
    proc.discover_reference(ReferenceType::Soft, 0x100, 0x200, None);
    proc.touch_soft_reference(0x100, 9_500);
    proc.note_round_clock(20_000);
    let _ = proc.process_references_with_finalizer_trace(&|a| a == 0x100, None, 1, 0);
    assert!(proc.take_newly_cleared().is_empty(), "judged on the previous clock");
    assert_eq!(proc.soft_gc_clock(), 20_000);
    // Not noted: the round commits only what it knows (the last touch).
    let mut plain = ReferenceProcessor::new();
    plain.hotspot_soft_lru = true;
    plain.soft_gc_clock_ms = 10_000;
    let _ = plain.process_references_with_finalizer_trace(&|_| true, None, 1, 0);
    assert_eq!(plain.soft_gc_clock(), 10_000);
    // Monotone, and a no-op off HotSpot's inputs.
    proc.note_round_clock(15_000);
    let _ = proc.process_references_with_finalizer_trace(&|_| true, None, 1, 0);
    assert_eq!(proc.soft_gc_clock(), 20_000);
    let mut wall = ReferenceProcessor::new();
    wall.hotspot_soft_lru = false;
    wall.note_round_clock(20_000);
    assert_eq!(wall.soft_clock_pending_ms, 0);
}

/// The stop-the-world contract: when the collector resurrected X (marked,
/// noted as resurrected) the Phase 0 closure is empty and the note changes
/// nothing -- the round already clears both weak references.
#[test]
fn gci_i14gq_the_note_is_inert_after_a_resurrecting_collection() {
    for note in [false, true] {
        let mut proc = scenario();
        proc.note_resurrected_finalizables(&[X, Y]);
        if note {
            proc.note_weak_before_final_round();
        }
        let all = |a: usize| marked(a) || a == X || a == Y;
        let result = proc.process_references_with_finalizer_trace(&all, Some(&trace), 64, 0);
        let mut cleared = proc.take_newly_cleared();
        cleared.sort_unstable();
        assert_eq!(cleared, vec![W1, W2], "note {note}");
        assert!(result.to_finalize.is_empty(), "note {note}");
    }
}
