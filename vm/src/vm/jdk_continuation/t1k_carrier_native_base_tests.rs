use super::CarrierNativeBase;
use crate::runtime::interpreter::{replace_exec_depth, replace_reinvokable_entry_depth};

#[test]
fn a_carrier_slice_sets_the_execute_depth_and_the_entry_mark_aside() {
    let outer_depth = replace_exec_depth(7);
    let outer_mark = replace_reinvokable_entry_depth(5);
    let base = CarrierNativeBase::enter();
    assert_eq!(replace_exec_depth(0), 0, "the slice counts from 0");
    assert_eq!(replace_reinvokable_entry_depth(0), 0, "no carrier mark in the slice");
    // What the slice leaves behind is dropped: its levels have unwound.
    let _ = replace_exec_depth(3);
    base.restore();
    assert_eq!(replace_exec_depth(outer_depth), 7, "the carrier's count is back");
    assert_eq!(replace_reinvokable_entry_depth(outer_mark), 5, "the carrier's mark is back");
}
