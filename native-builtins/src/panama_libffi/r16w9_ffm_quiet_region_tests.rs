//! JIT round 16 wave 9 (lane ffm): a downcall's region is left and
//! re-entered the way it was opened, every entry is paired with exactly
//! one exit, and the state nests with the guards. The mock does not
//! override the quiet pair, so its trait defaults count one ordinary
//! blocking region per entry (`blocking_region_counts`).
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::NativeThreadAccess;

#[test]
fn a_quiet_region_is_left_and_reentered_quietly_and_paired() {
    let mut ctx = mock_ctx();
    {
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region(&mut ctx, true);
        assert!(matches!(
            active_downcall_region(),
            DowncallRegion::OpenQuiet(..)
        ));
        assert!(active_downcall_gc_safe());
        assert_eq!(ctx.blocking_region_counts(), (1, 0));
        // An upcall: leave, run Java, back in.
        leave_downcall_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
        assert!(!active_downcall_gc_safe());
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
        // A second leave while left (a JNIEnv call in the upcall's Java)
        // ends nothing.
        assert!(!leave_active_downcall_region());
        leave_downcall_region(&mut ctx);
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
        {
            // A nested downcall in the upcall's Java starts counted and
            // gives the outer state back.
            let mut inner_ctx = mock_ctx();
            let inner = ActiveContextGuard::install(&mut inner_ctx);
            assert_eq!(active_downcall_region(), DowncallRegion::Counted);
            inner.enter_region(&mut inner_ctx, true);
            inner.leave_region(&mut inner_ctx);
            assert_eq!(inner_ctx.blocking_region_counts(), (1, 1));
        }
        assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
        reenter_downcall_region(&mut ctx);
        assert!(matches!(
            active_downcall_region(),
            DowncallRegion::OpenQuiet(..)
        ));
        assert_eq!(ctx.blocking_region_counts(), (2, 1));
        // A JNIEnv function C calls: leaves through the active context.
        assert!(leave_active_downcall_region());
        assert_eq!(active_downcall_region(), DowncallRegion::LeftQuiet);
        reenter_active_downcall_region();
        assert!(matches!(
            active_downcall_region(),
            DowncallRegion::OpenQuiet(..)
        ));
        assert_eq!(ctx.blocking_region_counts(), (3, 2));
        guard.leave_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
        assert_eq!(ctx.blocking_region_counts(), (3, 3));
        // Nothing left to end.
        guard.leave_region(&mut ctx);
        assert_eq!(ctx.blocking_region_counts(), (3, 3));
    }
    assert_eq!(active_downcall_region(), DowncallRegion::Counted);
    assert!(!has_active_context());
}

#[test]
fn a_full_region_is_left_and_reentered_the_full_way() {
    let mut ctx = mock_ctx();
    {
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region(&mut ctx, false);
        assert_eq!(active_downcall_region(), DowncallRegion::Open);
        leave_downcall_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::Left);
        reenter_downcall_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::Open);
        guard.leave_region(&mut ctx);
        assert_eq!(ctx.blocking_region_counts(), (2, 2));
    }
    assert_eq!(active_downcall_region(), DowncallRegion::Counted);
}

#[test]
fn the_trait_default_quiet_pair_is_one_ordinary_region() {
    let mut ctx = mock_ctx();
    let sync = ctx.begin_native_region_quiet();
    assert_eq!(ctx.blocking_region_counts(), (1, 0));
    ctx.end_native_region_quiet(sync);
    assert_eq!(ctx.blocking_region_counts(), (1, 1));
}
