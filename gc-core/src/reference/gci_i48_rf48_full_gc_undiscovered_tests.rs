use super::*;

const X: usize = 0x100;
const W: usize = 0x140;
const Y: usize = 0x200;

#[test]
fn gci_i48_rf48_the_staged_set_is_taken_only_armed_and_once() {
    let mut proc = ReferenceProcessor::new();
    proc.stage_full_gc_undiscovered(&[W, 0, X, W], 4);
    assert!(proc.take_armed_full_gc_undiscovered(4).is_empty(), "not armed");
    assert!(proc.arm_full_gc_undiscovered(4));
    assert_eq!(proc.take_armed_full_gc_undiscovered(4), vec![X, W]);
    assert!(proc.take_armed_full_gc_undiscovered(4).is_empty(), "taken");
    // The stage survives the end of a pre-collection cycle (the remark's
    // own round ends with one) until the door arms it.
    proc.stage_full_gc_undiscovered(&[W], 5);
    proc.finish_pre_gc_cycle();
    assert!(proc.arm_full_gc_undiscovered(5));
    assert_eq!(proc.take_armed_full_gc_undiscovered(5), vec![W]);
}

#[test]
fn gci_i48_rf48_a_stale_or_empty_stage_never_arms() {
    let mut proc = ReferenceProcessor::new();
    proc.stage_full_gc_undiscovered(&[W], 4);
    assert!(!proc.arm_full_gc_undiscovered(5), "a collection ran since the remark");
    assert!(!proc.arm_full_gc_undiscovered(4), "dropped by the stale arm");
    proc.stage_full_gc_undiscovered(&[], 6);
    assert!(!proc.arm_full_gc_undiscovered(6));
    // Armed, then read at a later count: dropped, never handed out.
    proc.stage_full_gc_undiscovered(&[W], 7);
    assert!(proc.arm_full_gc_undiscovered(7));
    assert!(proc.take_armed_full_gc_undiscovered(8).is_empty());
    assert!(proc.take_armed_full_gc_undiscovered(7).is_empty());
    // The door's end drops an armed set nobody took.
    proc.stage_full_gc_undiscovered(&[W], 9);
    assert!(proc.arm_full_gc_undiscovered(9));
    proc.drop_full_gc_undiscovered();
    assert!(proc.take_armed_full_gc_undiscovered(9).is_empty());
}

/// The compaction pause's round: the queued finalizable `X` is
/// resurrected again and, `W`'s slot 0 left alone by the pass, its
/// resurrection reached `Y` too. Not noted, `W` is cleared (`Y` is only
/// finalizer-reachable); noted (the taken set), the row is left alone.
#[test]
fn gci_i48_rf48_the_compaction_round_leaves_the_taken_rows_alone() {
    let all_marked = |_: usize| true;
    let build = || {
        let mut proc = ReferenceProcessor::new();
        proc.discover_reference(ReferenceType::Weak, W, Y, None);
        proc.note_resurrected_finalizables(&[X, W, Y]);
        proc
    };
    let mut off = build();
    let _ = off.process_references_with_finalizer_trace(&all_marked, None, 64, 0);
    assert_eq!(off.take_newly_cleared(), vec![W]);

    let mut on = build();
    on.stage_full_gc_undiscovered(&[W], 3);
    assert!(on.arm_full_gc_undiscovered(3));
    let held = on.take_armed_full_gc_undiscovered(3);
    on.note_undiscovered_references(&held);
    let r = on.process_references_with_finalizer_trace(&all_marked, None, 64, 0);
    assert!(on.take_newly_cleared().is_empty());
    assert!(!r.retired.contains(&W));
}

/// gcj j4/rf (rp residuals item 1): the pause's order -- the triples mark
/// `W` examined, then the taken set notes it and the null pass skips it --
/// must not let the post-collection retirer settle `W` as an application
/// clear. A row examined and not nulled that is NOT held is still retired.
#[test]
fn gcj_j4_rf_the_retirer_leaves_a_held_row_active() {
    const W2: usize = 0x180;
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Weak, W, Y, None);
    proc.discover_reference(ReferenceType::Weak, W2, Y, None);
    proc.stage_full_gc_undiscovered(&[W], 3);
    assert!(proc.arm_full_gc_undiscovered(3));
    let _ = proc.weak_phantom_active_triples();
    let held = proc.take_armed_full_gc_undiscovered(3);
    proc.note_undiscovered_references(&held);
    assert_eq!(proc.retire_entries_the_pre_gc_pass_found_cleared(), 1);
    let w = proc
        .weak_refs
        .iter()
        .find(|e| e.reference_obj == W)
        .unwrap();
    assert!(!w.cleared && !w.enqueued, "the held row stays active");
    let w2 = proc
        .weak_refs
        .iter()
        .find(|e| e.reference_obj == W2)
        .unwrap();
    assert!(
        w2.cleared && w2.enqueued,
        "an unheld row is retired as before"
    );
}
