use super::*;
use cratonvm_native_api::NativeHeapAccess;
use crate::test_utils::MockNativeContext;


/// Every way the console print natives let a line go nowhere carries a
/// note: the closed stream, a throwing sink, a failed autoflush and both
/// arms of the final fall-through (in `stream_write` and in
/// `stream_writeln_inner`).
#[test]
fn zt7cl2_every_silent_print_exit_is_noted() {
    let src = include_str!("lib_parts/arrays_range_or_throw.rs");
    // Each call appears once, at its site (this list spells the quotes
    // escaped, so it does not match itself).
    for call in [
        "note_print_loss(&*ctx, \"closed\"",
        "note_print_loss(&*scope, \"sink-threw\"",
        "note_print_loss(&*ctx, \"autoflush-threw\"",
    ] {
        assert_eq!(src.matches(call).count(), 1, "{call}");
    }
    let fallthrough = "note_print_loss(&*scope, why, Some(this), None, 0)";
    assert_eq!(src.matches(fallthrough).count(), 2, "two sites");
}
