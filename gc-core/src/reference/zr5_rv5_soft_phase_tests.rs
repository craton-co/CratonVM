use super::*;

const SOFT: usize = 0x100;
const X: usize = 0x200;
const WEAK: usize = 0x300;
const FIN: usize = 0x500;
const CLEANER: usize = 0x600;

/// One live object `X` (the collector marked it: a conservative
/// compiled-frame word, or a real path, holds it) with a condemned soft
/// reference, a weak reference and a `Cleaner`, plus a finalizer row
/// when `finalizable`. Every address is marked.
fn fixture(finalizable: bool) -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new_with_policy(1000);
    proc.discover_reference(ReferenceType::Soft, SOFT, X, None);
    proc.discover_reference(ReferenceType::Weak, WEAK, X, None);
    if finalizable {
        proc.discover_reference(ReferenceType::Finalizer, FIN, X, None);
    }
    proc.discover_reference(ReferenceType::Cleaner, CLEANER, X, None);
    // The last-ditch rule condemned every soft reference.
    assert_eq!(proc.condemn_all_soft_refs(), vec![(SOFT, X)]);
    proc
}

fn all_marked(_: usize) -> bool {
    true
}

/// The pre-fix composition: the set folded into `is_marked` for every
/// phase.
fn folded_run(finalizable: bool) -> ReferenceProcessingResult {
    let mut old = fixture(finalizable);
    let forced: FxHashSet<usize> = [X].into_iter().collect();
    let folded = |a: usize| !forced.contains(&a) && all_marked(a);
    old.process_references(&folded, 64, 5_000)
}

/// **The forced verdict clears the soft reference and touches nothing
/// else.** Before zr z5/rv5 the VM folded the set into `is_marked`
/// (`!forced.contains(a) && is_marked(a)`); this test runs that
/// composition as the witness: it cleared the weak reference of the live
/// object and fired its `Cleaner` in the same round.
#[test]
fn a_soft_phase_forced_dead_referent_keeps_its_weak_and_phantom_rows() {
    let mut proc = fixture(false);
    proc.note_soft_phase_forced_dead([X]);
    let r = proc.process_references(&all_marked, 64, 5_000);
    assert_eq!(
        r.stats.soft_refs_cleared, 1,
        "the soft reference is cleared"
    );
    assert!(proc.soft_refs[0].cleared);
    assert_eq!(
        r.stats.weak_refs_cleared, 0,
        "X is alive: its weak ref stays"
    );
    assert!(!proc.weak_refs[0].cleared);
    assert!(r.cleaner_actions.is_empty(), "no Cleaner for a live X");
    // One collection only: the next round sees the plain verdict.
    let r2 = proc.process_references(&all_marked, 64, 5_000);
    assert_eq!(r2.stats.weak_refs_cleared, 0);
    assert!(r2.cleaner_actions.is_empty());

    // Witness: the pre-fix composition, the same inputs.
    let w = folded_run(false);
    assert_eq!(w.stats.soft_refs_cleared, 1);
    assert_eq!(
        w.stats.weak_refs_cleared, 1,
        "witness: the folded predicate cleared the weak ref of a live object"
    );
    assert!(
        !w.cleaner_actions.is_empty(),
        "witness: the folded predicate fired the Cleaner of a live object"
    );
}

/// A FINALIZABLE referent: the fix clears its soft reference and
/// schedules no `finalize()`. The folded predicate made it a finalizer
/// root, whose closure kept the soft reference alive -- so cp's clear
/// never happened for a finalizable referent.
#[test]
fn a_finalizable_forced_referent_is_soft_cleared_and_not_finalized() {
    let mut proc = fixture(true);
    proc.note_soft_phase_forced_dead([X]);
    let r = proc.process_references(&all_marked, 64, 5_000);
    assert_eq!(r.stats.soft_refs_cleared, 1);
    assert_eq!(r.stats.weak_refs_cleared, 0);
    assert_eq!(r.stats.finalizer_refs_enqueued, 0);
    assert!(r.to_finalize.is_empty());
    assert!(r.cleaner_actions.is_empty());
    let w = folded_run(true);
    assert_eq!(
        w.stats.soft_refs_cleared, 0,
        "witness: the finalizer closure kept the soft ref the rule meant to clear"
    );
}

/// The set is per collection: `finish_pre_gc_cycle` drops an unconsumed
/// one, the round consumes it, and a null address is never recorded.
#[test]
fn the_soft_phase_set_never_spans_two_collections() {
    let mut proc = fixture(false);
    proc.note_soft_phase_forced_dead([X, 0]);
    assert_eq!(proc.soft_phase_forced_dead.len(), 1);
    proc.finish_pre_gc_cycle();
    assert!(proc.soft_phase_forced_dead.is_empty());
    let mut proc = fixture(false);
    proc.note_soft_phase_forced_dead([X]);
    let _ = proc.process_references(&all_marked, 64, 5_000);
    assert!(
        proc.soft_phase_forced_dead.is_empty(),
        "consumed by the round"
    );
}
