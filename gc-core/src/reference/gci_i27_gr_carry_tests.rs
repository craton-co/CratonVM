use super::*;

const X: usize = 0x100;
const W: usize = 0x140;
const Y: usize = 0x200;
const V: usize = 0x500;
const W_MOVED: usize = 0x7140;

fn scenario() -> ReferenceProcessor {
    let mut proc = ReferenceProcessor::new();
    proc.discover_reference(ReferenceType::Finalizer, X, X, None);
    proc.discover_reference(ReferenceType::Weak, W, Y, None);
    proc.discover_reference(ReferenceType::Weak, V, Y, None);
    proc
}

#[test]
fn gci_i27_gr_the_probe_round_s_undiscovered_rows_are_carried_and_remapped() {
    let marked = |a: usize| matches!(a, X | W | Y | V);
    let mut proc = scenario();
    proc.note_resurrected_finalizables(&[X, W, Y]);
    proc.note_stw_undiscovered_probe(&[(W, Y)], &[Y]);
    let _ = proc.process_references_with_finalizer_trace(&marked, None, 64, 0);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(W, W_MOVED);
    proc.carry_probe_undiscovered(&map, 7);
    // Survives the end of the pre-collection cycle (the remark is later).
    proc.finish_pre_gc_cycle();
    assert_eq!(proc.carried_undiscovered_for(7), vec![W_MOVED]);
    assert_eq!(proc.carried_undiscovered_for(7), vec![W_MOVED], "read-only");
    assert!(proc.carried_undiscovered_for(8).is_empty(), "a later collection");
    proc.drop_carried_undiscovered();
    assert!(proc.carried_undiscovered_for(7).is_empty());
}

#[test]
fn gci_i27_gr_a_round_without_a_probe_carries_nothing() {
    let marked = |a: usize| matches!(a, X | W | Y | V);
    let mut proc = scenario();
    proc.note_resurrected_finalizables(&[X, W, Y]);
    proc.note_stw_undiscovered_probe(&[(W, Y)], &[Y]);
    let _ = proc.process_references_with_finalizer_trace(&marked, None, 64, 0);
    // A second round (no probe) rewrites the record before the carry.
    let _ = proc.process_references_with_finalizer_trace(&marked, None, 64, 0);
    proc.carry_probe_undiscovered(&cratonvm_types::PointerMap::default(), 3);
    assert!(proc.carried_undiscovered_for(3).is_empty());
}
