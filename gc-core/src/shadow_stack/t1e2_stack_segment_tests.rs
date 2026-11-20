use super::*;

/// The own-thread half: a set top bounds `SlotBand::current_thread`'s high
/// end, and clearing it restores the whole stack.
#[test]
fn t1e2_a_segment_top_bounds_the_own_band() {
    let whole = SlotBand::current_thread();
    if whole.hi == usize::MAX {
        return; // no stack introspection on this target
    }
    let probe = 0u8;
    let here = std::hint::black_box(&probe) as *const u8 as usize;
    // Halfway between this frame and the stack's top: a fixed offset can
    // land past the top on a test thread with only a few frames above.
    let top = (here + (whole.hi - here) / 2) & !7;
    assert!(here < top && top < whole.hi);
    let prev = replace_current_segment_top(top);
    assert_eq!(prev, 0);
    let bounded = SlotBand::current_thread();
    assert_eq!(bounded.hi, top);
    assert!(bounded.lo < top);
    assert_eq!(replace_current_segment_top(prev), top);
    assert_eq!(SlotBand::current_thread().hi, whole.hi);
}

/// The cross-thread half: a capture at or above the base goes to the
/// owner, below it (or for another tid) to the claimant.
#[test]
fn t1e2_captures_route_by_address() {
    let os_tid = 0x7e2_0001;
    assert_eq!(publish_stack_segment(os_tid, 0x9000, 77), None);
    let segs = stack_segments_snapshot();
    assert_eq!(segment_owner_of_capture(&segs, os_tid, 0x9000), Some(77));
    assert_eq!(segment_owner_of_capture(&segs, os_tid, 0x9100), Some(77));
    assert_eq!(segment_owner_of_capture(&segs, os_tid, 0x8ff8), None);
    assert_eq!(segment_owner_of_capture(&segs, os_tid + 1, 0x9100), None);
    restore_stack_segment(os_tid, None);
    let segs = stack_segments_snapshot();
    assert_eq!(segment_owner_of_capture(&segs, os_tid, 0x9100), None);
}
