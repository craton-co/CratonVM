use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these unit tests run the heap single-threaded.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

/// The young set is both semispaces, whole capacity; an old object is in
/// neither and a fresh young one is in one of them.
#[test]
fn gci_i13vg_young_ranges_are_both_semispaces() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
    let ranges = heap
        .parked_old_only_young_ranges()
        .expect("no concurrent mark is open on a fresh heap");
    assert_eq!(ranges.len(), 2);
    let inside = |a: usize| ranges.iter().any(|&(lo, hi)| lo <= a && a < hi);
    assert_eq!(ranges[1], heap.young_inactive_semispace_range());
    let monitors = NoMonitors;
    let mut roots: Vec<ObjectRef> = vec![heap.alloc_object(ClassId::new(0), 2)];
    assert!(inside(roots[0].as_ptr() as usize), "a young object is in the young set");
    for _ in 0..PROMOTION_AGE {
        heap.collect_garbage(&stw(), &mut roots, &monitors);
    }
    assert!(heap.is_in_old(roots[0].as_ptr()), "precondition: tenured");
    let ranges = heap.parked_old_only_young_ranges().expect("still no mark");
    assert!(
        !ranges.iter().any(|&(lo, hi)| {
            let a = roots[0].as_ptr() as usize;
            lo <= a && a < hi
        }),
        "an old object is outside the young set"
    );
}

/// A staged old-only section is a root of the same pause's major, on both
/// collection paths (the non-moving `System.gc()` old sweep and the
/// moving cycle's Phase 5 under `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`),
/// in place (no compaction, so addresses answer). Without the stage the
/// same two old objects -- reachable from nothing else -- are freed, which
/// shows the major ran and the section is what kept them. The door leaves
/// nothing staged behind.


/// A section staged for a collection that runs no major (a plain young
/// cycle) is dropped when that collection returns: it never reaches a
/// later collection's major.
#[test]
fn gci_i13vg_an_unconsumed_section_is_cleared_by_the_door() {
    let heap = GenerationalHeap::with_sizes(64 * 1024, 256 * 1024).with_fixed_promotion_age();
    let monitors = NoMonitors;
    let mut roots: Vec<ObjectRef> = vec![heap.alloc_object(ClassId::new(0), 2)];
    heap.stage_parked_old_only_roots(&[roots[0]]);
    assert_eq!(heap.parked_old_only_roots.lock().len(), 1);
    heap.collect_garbage(&stw(), &mut roots, &monitors);
    assert!(heap.parked_old_only_roots.lock().is_empty());
    // Staging replaces, never appends.
    heap.stage_parked_old_only_roots(&[roots[0], roots[0]]);
    heap.stage_parked_old_only_roots(&[roots[0]]);
    assert_eq!(heap.take_parked_old_only_roots().len(), 1);
    assert!(heap.take_parked_old_only_roots().is_empty(), "taken once");
}
