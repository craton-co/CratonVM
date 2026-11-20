use super::*;

#[test]
fn gci_i17fz_retract_takes_only_the_named_candidates_back() {
    let mut heap = GenerationalHeap::with_sizes(64 * 1024, 64 * 1024);
    // No concurrent state yet: a no-op.
    heap.retract_concurrent_finalizer_candidates(&[0x2000]);
    let state = Arc::new(ConcurrentGcState::new());
    heap.enable_concurrent_gc(Arc::new(SatbQueue::new()), state.clone());
    state.publish_finalizer_candidates(vec![0x3000, 0x1000, 0x2000, 0x4000]);
    heap.retract_concurrent_finalizer_candidates(&[]);
    assert_eq!(state.finalizer_candidates(), vec![0x1000, 0x2000, 0x3000, 0x4000]);
    // Any order; an address never published changes nothing.
    heap.retract_concurrent_finalizer_candidates(&[0x4000, 0x9000, 0x2000]);
    assert_eq!(state.finalizer_candidates(), vec![0x1000, 0x3000]);
    // The retained list is not touched.
    state.note_remark_retained_finalizers(&[0x3000]);
    heap.retract_concurrent_finalizer_candidates(&[0x3000]);
    assert_eq!(state.finalizer_candidates(), vec![0x1000]);
    assert_eq!(state.take_remark_retained_finalizers(), vec![0x3000]);
}
