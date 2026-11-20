use super::*;

// A stop-the-world collection, after the fact. X: a registered
// finalizable object. W (weak) names Y, P (phantom, queue QP) names Z;
// the pre-collection pass nulled both slots and the VM's probe found W
// and P from X, so it handed Y and Z to the collection as extra
// resurrection candidates. V is a discovered (strongly marked) weak
// reference to Y.
const X: usize = 0x100;
const W: usize = 0x140;
const P: usize = 0x180;
const Y: usize = 0x200;
const Z: usize = 0x240;
const V: usize = 0x500;
const QP: usize = 0x910;

fn scenario() -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, X, X, None);
    proc.discover_reference(ReferenceType::Weak, W, Y, None);
    proc.discover_reference(ReferenceType::Phantom, P, Z, Some(QP));
    proc.discover_reference(ReferenceType::Weak, V, Y, None);
    proc
}

fn round(
    proc: &mut ReferenceProcessor,
    marked: &dyn Fn(usize) -> bool,
) -> (ReferenceProcessingResult, Vec<usize>) {
    let result = proc.process_references_with_finalizer_trace(marked, None, 64, 0);
    let mut cleared = proc.take_newly_cleared();
    cleared.sort_unstable();
    (result, cleared)
}

/// X died: the collection resurrected X and, through it, W and P (their
/// slots null), and kept Y and Z only as candidates. HotSpot never
/// discovered W and P: their rows are left alone (the VM restores their
/// slots), while the DISCOVERED V to the same Y is still cleared
/// (SoftWeakFinalRefsPhase runs before the keep-alive phase).
#[test]
fn gci_i16fr_a_reference_only_a_dead_finalizable_reaches_keeps_its_referent() {
    let marked = |a: usize| matches!(a, X | W | P | Y | Z | V | QP);
    // Without the probe (default): W cleared with V, P enqueued.
    let mut off = scenario();
    off.note_resurrected_finalizables(&[X, W, P, Y, Z]);
    let (r_off, cleared_off) = round(&mut off, &marked);
    assert_eq!(cleared_off, vec![W, V]);
    assert!(r_off.to_enqueue.is_empty(), "Z marked: {:x?}", r_off.to_enqueue);

    let mut on = scenario();
    on.note_resurrected_finalizables(&[X, W, P, Y, Z]);
    on.note_stw_undiscovered_probe(&[(W, Y), (P, Z)], &[Y, Z]);
    let (r_on, cleared_on) = round(&mut on, &marked);
    assert_eq!(cleared_on, vec![V]);
    assert!(r_on.to_enqueue.is_empty(), "{:x?}", r_on.to_enqueue);
    assert!(!r_on.retired.contains(&W) && !r_on.retired.contains(&P));
    assert_eq!(r_on.stats.weak_refs_cleared, 1);
    // W and P are still active rows, so the VM's restore loop finds them.
    let active = on.weak_phantom_active_pairs();
    assert!(active.contains(&(W, Y)) && active.contains(&(P, Z)), "{active:x?}");
}

/// X is alive: the collection marked X, W and P strongly and kept Y and Z
/// only because they were candidates. HotSpot discovered W and P and found
/// Y and Z unreachable: W is cleared and P enqueued in THIS round, as if
/// the probe had not run (without the candidates-only note Z would read
/// marked and P would wait a collection).
#[test]
fn gci_i16fr_a_candidate_kept_only_by_the_probe_is_unmarked_for_the_round() {
    let marked = |a: usize| matches!(a, X | W | P | Y | Z | V | QP);
    let mut no_note = scenario();
    no_note.note_resurrected_finalizables(&[Y, Z]);
    let (r_no, cleared_no) = round(&mut no_note, &marked);
    assert_eq!(cleared_no, vec![W, V]);
    assert!(r_no.to_enqueue.is_empty(), "{:x?}", r_no.to_enqueue);

    let mut on = scenario();
    on.note_resurrected_finalizables(&[Y, Z]);
    on.note_stw_undiscovered_probe(&[(W, Y), (P, Z)], &[Y, Z]);
    let (r_on, cleared_on) = round(&mut on, &marked);
    assert_eq!(cleared_on, vec![W, V]);
    assert_eq!(r_on.to_enqueue, vec![(P, QP)]);
    assert!(r_on.to_finalize.is_empty(), "X is alive");
}

/// The probe describes one round: consumed by the round it precedes, and
/// dropped by `finish_pre_gc_cycle` when no round consumed it.
#[test]
fn gci_i16fr_the_probe_describes_one_round() {
    let marked = |a: usize| matches!(a, X | W | P | Y | Z | V | QP);
    let mut proc = scenario();
    proc.note_resurrected_finalizables(&[X, W, P, Y, Z]);
    proc.note_stw_undiscovered_probe(&[(W, Y), (P, Z)], &[Y, Z]);
    let _ = round(&mut proc, &marked);
    assert!(proc.stw_probe_pairs.is_empty());
    assert!(proc.stw_probe_candidates_only.is_empty());
    assert!(proc.undiscovered_this_round.is_empty());

    let mut stale = scenario();
    stale.note_stw_undiscovered_probe(&[(W, Y), (P, Z)], &[Y, Z]);
    stale.finish_pre_gc_cycle();
    assert!(stale.stw_probe_pairs.is_empty());
    assert!(stale.stw_probe_candidates_only.is_empty());
    // Nothing resurrected: W and P are processed normally (Y, Z dead).
    let dead_yz = |a: usize| matches!(a, X | W | P | V | QP);
    let (r, cleared) = round(&mut stale, &dead_yz);
    assert_eq!(cleared, vec![W, V]);
    assert_eq!(r.to_enqueue, vec![(P, QP)]);
}

#[test]
fn gci_i16fr_resolve_keeps_a_candidate_any_undiscovered_pair_names() {
    let resurrected: FxHashSet<usize> = [W, X].into_iter().collect();
    let candidates: FxHashSet<usize> = [Y, Z].into_iter().collect();
    // W resurrected (undiscovered), P not (discovered): Y kept, Z dead.
    let (undiscovered, dead) =
        resolve_stw_undiscovered_probe(&[(W, Y), (P, Z)], &candidates, &resurrected);
    assert_eq!(undiscovered, vec![W]);
    assert_eq!(dead.into_iter().collect::<Vec<_>>(), vec![Z]);
    // Two references to one referent: one undiscovered pair keeps it.
    let (undiscovered, dead) =
        resolve_stw_undiscovered_probe(&[(P, Y), (W, Y)], &candidates, &resurrected);
    assert_eq!(undiscovered, vec![W]);
    assert_eq!(dead.into_iter().collect::<Vec<_>>(), vec![Z]);
    // Empty probe: nothing.
    let (u, d) =
        resolve_stw_undiscovered_probe(&[], &FxHashSet::default(), &resurrected);
    assert!(u.is_empty() && d.is_empty());
}
