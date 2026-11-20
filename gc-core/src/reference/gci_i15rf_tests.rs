use super::*;

// X: a dead finalizable object (unmarked). W (weak) and PH (phantom) are
// unmarked `Reference` objects only X reaches -- never discovered by the
// marker. Their referents Y and Y3 are unmarked. V is a MARKED (discovered)
// weak reference to the same Y.
const X: usize = 0x100;
const W: usize = 0x140;
const PH: usize = 0x180;
const Y: usize = 0x200;
const Y3: usize = 0x240;
const V: usize = 0x500;
const QP: usize = 0x910;

fn scenario() -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, X, X, None);
    proc.discover_reference(ReferenceType::Weak, W, Y, None);
    proc.discover_reference(ReferenceType::Phantom, PH, Y3, Some(QP));
    proc.discover_reference(ReferenceType::Weak, V, Y, None);
    proc
}

fn marked(addr: usize) -> bool {
    matches!(addr, V | QP)
}

/// The remark's tracer: a `Reference`'s slot 0 is not an edge, so X
/// reaches the `Reference` objects W and PH but not Y / Y3.
fn trace(roots: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    for &r in roots {
        out.push(r);
        if r == X {
            out.push(W);
            out.push(PH);
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
fn gci_i15rf_undiscovered_rows_are_left_alone_by_the_noted_round() {
    // Not noted: W is cleared and PH enqueued with the discovered V.
    let mut off = scenario();
    let (r_off, cleared_off) = round(&mut off);
    assert_eq!(cleared_off, vec![W, V]);
    assert_eq!(r_off.to_enqueue, vec![(PH, QP)]);
    assert!(r_off.retired.contains(&W) && r_off.retired.contains(&PH));

    // Noted: W and PH are untouched (not cleared, enqueued or retired);
    // the DISCOVERED V to the same referent is still cleared, and X is
    // still finalized -- HotSpot's SoftWeakFinalRefsPhase clears V before
    // the keep-alive phase reaches Y through the undiscovered W.
    let mut on = scenario();
    on.note_undiscovered_references(&[W, PH]);
    let (r_on, cleared_on) = round(&mut on);
    assert_eq!(cleared_on, vec![V]);
    assert!(r_on.to_enqueue.is_empty(), "{:x?}", r_on.to_enqueue);
    assert!(!r_on.retired.contains(&W) && !r_on.retired.contains(&PH));
    assert_eq!(r_on.to_finalize, vec![X]);
    assert_eq!(r_on.stats.weak_refs_cleared, 1);
    assert_eq!(r_on.stats.phantom_refs_enqueued, 0);
}

#[test]
fn gci_i15rf_the_undiscovered_note_describes_one_round() {
    // Emptied by the round it precedes: the next round processes W again.
    let mut proc = scenario();
    proc.note_undiscovered_references(&[W, PH]);
    let _ = round(&mut proc);
    assert!(proc.undiscovered_this_round.is_empty());
    let (r2, cleared2) = round(&mut proc);
    assert_eq!(cleared2, vec![W]);
    assert_eq!(r2.to_enqueue, vec![(PH, QP)]);

    // Dropped by `finish_pre_gc_cycle` when no round consumed it.
    let mut stale = scenario();
    stale.note_undiscovered_references(&[W, PH]);
    stale.finish_pre_gc_cycle();
    let (_, cleared) = round(&mut stale);
    assert_eq!(cleared, vec![W, V]);
}

#[test]
fn gci_i15rf_an_undiscovered_soft_row_is_neither_cleared_nor_a_survivor_root() {
    // S (soft, unmarked, only X reaches it) names Y2; T (weak, marked)
    // names Y2 too. Condemned by `condemn_all_soft_refs` (the last-ditch
    // rule), S would be cleared; noted, it is left alone. And it is not a
    // soft survivor root: T, a discovered weak reference to Y2, is still
    // cleared (a survivor root would have kept Y2 weakly live).
    const S: usize = 0x160;
    const Y2: usize = 0x220;
    const T: usize = 0x520;
    let build = || {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Soft, S, Y2, None);
        proc.discover_reference(ReferenceType::Weak, T, Y2, None);
        proc
    };
    let is_marked = |a: usize| a == T;
    let mut off = build();
    let _ = off.condemn_all_soft_refs();
    let _ = off.process_references_with_finalizer_trace(&is_marked, None, 64, 0);
    let mut cleared_off = off.take_newly_cleared();
    cleared_off.sort_unstable();
    assert_eq!(cleared_off, vec![S, T]);

    let mut on = build();
    let _ = on.condemn_all_soft_refs();
    on.note_undiscovered_references(&[S]);
    let _ = on.process_references_with_finalizer_trace(&is_marked, None, 64, 0);
    assert_eq!(on.take_newly_cleared(), vec![T]);
}
