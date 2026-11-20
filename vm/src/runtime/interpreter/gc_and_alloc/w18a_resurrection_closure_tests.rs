use super::resurrected_pre_gc_addresses;
use cratonvm_types::PointerMap;

/// The common pause: no dead finalizer, no closure -- nothing noted.
#[test]
fn no_dead_finalizer_and_no_closure_notes_nothing() {
    let map = PointerMap::default();
    assert!(resurrected_pre_gc_addresses(&[0x100, 0x140], &map, &[], Vec::new()).is_empty());
}

/// Depth 1 is the w6-d filter, unchanged: a candidate whose
/// post-collection address is a dead finalizer, at its PRE-collection
/// address. The closure follows it.
#[test]
fn the_depth_one_filter_is_unchanged_and_the_closure_follows_it() {
    // 0x100 moved to 0x900 and was resurrected; 0x140 was resurrected in
    // place; 0x180 was simply alive.
    let map = PointerMap::from_iter([(0x100, 0x900)]);
    let got = resurrected_pre_gc_addresses(
        &[0x100, 0x140, 0x180],
        &map,
        &[0x900, 0x140],
        vec![0x240],
    );
    assert_eq!(got, vec![0x100, 0x140, 0x240]);
}

/// A collector's closure is noted even when the VM's own depth-1 set is
/// empty.
#[test]
fn a_closure_alone_is_noted() {
    let map = PointerMap::default();
    assert_eq!(
        resurrected_pre_gc_addresses(&[0x100], &map, &[], vec![0x240, 0x280]),
        vec![0x240, 0x280]
    );
}

/// The consumer end to end, with an injected closure standing in for a
/// collector that reports one: the weak reference to the object reachable
/// only THROUGH the finalizable parent is cleared and enqueued; a weak
/// reference to an object the strong closure reached keeps its referent.
#[test]
fn an_injected_closure_clears_the_weak_reference_to_the_child() {
    const PARENT: usize = 0x200;
    const CHILD: usize = 0x240;
    const SHARED: usize = 0x280;
    let mut proc = cratonvm_gc::ReferenceProcessor::new();
    proc.discover_reference(cratonvm_gc::ReferenceType::Finalizer, PARENT, PARENT, None);
    proc.discover_reference(cratonvm_gc::ReferenceType::Weak, 0x300, CHILD, Some(0x400));
    proc.discover_reference(cratonvm_gc::ReferenceType::Weak, 0x310, SHARED, Some(0x400));
    let map = PointerMap::default();
    let pre_gc = resurrected_pre_gc_addresses(&[PARENT], &map, &[PARENT], vec![CHILD]);
    assert_eq!(pre_gc, vec![PARENT, CHILD]);
    proc.note_resurrected_finalizables(&pre_gc);
    // Everything is "marked": the collection marked the child only through
    // the resurrection drain, which is what the noted set says.
    let result = proc.process_references(&|_addr: usize| true, 0, 1_000_000);
    assert_eq!(result.to_enqueue, vec![(0x300, 0x400)]);
}
