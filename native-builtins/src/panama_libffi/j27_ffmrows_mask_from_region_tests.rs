//! FFM417-5 (j27 wave 4, lane FFMROWS): the take-over mask is decided from
//! the region the downcall actually ENTERED, not from the one it asked for.
use super::*;
use crate::test_utils::mock_ctx;

/// A lazy entry the context declines with no fallback leaves the call
/// COUNTED, so the switch must mask it; the old `!lazy && switch` left it
/// unmasked (a counted C call open to the take-over signal's `EINTR`).
#[test]
fn a_declined_lazy_entry_is_counted_and_so_masked() {
    let mut ctx = mock_ctx();
    let guard = ActiveContextGuard::install(&mut ctx);
    guard.enter_region_lazy(&mut ctx, None);
    assert_eq!(active_downcall_region(), DowncallRegion::Counted);
    assert!(downcall_masks_takeover(active_downcall_region(), true));
    assert!(!downcall_masks_takeover(active_downcall_region(), false));
    drop(guard);
}

/// An open region is excluded from pauses: never masked, switch or not.
#[test]
fn an_open_region_is_never_masked() {
    let mut ctx = mock_ctx();
    let guard = ActiveContextGuard::install(&mut ctx);
    guard.enter_region(&mut ctx, false);
    assert!(active_downcall_gc_safe());
    assert!(!downcall_masks_takeover(active_downcall_region(), true));
    guard.leave_region(&mut ctx);
    drop(guard);
}
