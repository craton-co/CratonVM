/// zt t20/x16: the only heap arms; a second live heap, or one already
/// armed, refuses.
#[test]
fn zt20x16_the_load_barrier_arms_only_a_single_heap_process() {
    assert!(!super::zgc_load_barrier_arming_refused(1, 0));
    assert!(super::zgc_load_barrier_arming_refused(2, 0));
    assert!(super::zgc_load_barrier_arming_refused(1, 1));
    assert!(super::zgc_load_barrier_arming_refused(3, 1));
}
