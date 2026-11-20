use super::*;

/// `segment_bounded_limits` lowers the top only to a base inside the
/// limits; `0` and an outside base leave them unchanged.
#[test]
fn t1e2_segment_bounded_limits() {
    assert_eq!(segment_bounded_limits((100, 1000), 0), (100, 1000));
    assert_eq!(segment_bounded_limits((100, 1000), 500), (100, 500));
    assert_eq!(segment_bounded_limits((100, 1000), 1000), (100, 1000));
    assert_eq!(segment_bounded_limits((100, 1000), 50), (100, 1000));
    assert_eq!(segment_bounded_limits((0, 0), 500), (0, 0));
}

/// The carrier's chain, residue and stack top leave for the slice and
/// come back; the slice's own (balanced) entry never reaches the carrier.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn t1e2_a_chain_segment_sets_the_carrier_chain_aside() {
    let before = current_thread_jit_depth();
    let carrier_depth = push_jit_entry();
    assert_eq!(carrier_depth, before + 1);
    let (low, high) = current_thread_stack_limits();
    assert!(high != 0);
    let base = (current_stack_pointer() + 2048) & !7;
    assert!(base < high);
    {
        let segment = ChainSegment::enter(base, None);
        assert_eq!(current_thread_jit_depth(), 0, "the slice starts empty");
        assert_eq!(current_thread_stack_limits(), (low, base), "own stack ends at the base");
        assert_eq!(own_stack_top(), base);
        assert!(sp_on_own_stack(current_stack_pointer()));
        assert_eq!(cratonvm_gc::shadow_stack::current_segment_top(), base);
        // A balanced slice entry.
        assert_eq!(push_jit_entry(), 1);
        assert!(pop_jit_entry().is_some());
        drop(segment);
    }
    assert_eq!(current_thread_jit_depth(), carrier_depth, "the carrier's chain is back");
    assert_eq!(current_thread_stack_limits(), (low, high));
    assert_eq!(cratonvm_gc::shadow_stack::current_segment_top(), 0);
    assert!(pop_jit_entry().is_some());
    assert_eq!(current_thread_jit_depth(), before);
}

/// A slice entry whose guard was bypassed (leaked) is retired by the drop,
/// not handed to the carrier.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn t1e2_a_leaked_slice_entry_does_not_reach_the_carrier() {
    let before = current_thread_jit_depth();
    let base = (current_stack_pointer() + 2048) & !7;
    let segment = ChainSegment::enter(base, None);
    let _ = push_jit_entry();
    assert_eq!(current_thread_jit_depth(), 1);
    drop(segment);
    assert_eq!(current_thread_jit_depth(), before, "the leaked entry is retired");
}
