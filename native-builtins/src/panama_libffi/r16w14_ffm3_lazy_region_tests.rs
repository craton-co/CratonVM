//! JIT round 16 wave 14 (lane ffm3): the lazy in-native state of a
//! downcall (`CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`) falls back as asked
//! when the context declines, and otherwise is left and re-entered over
//! the downcall's own band, which holds the calling frame.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::NativeThreadAccess;

#[test]
fn a_declined_lazy_entry_stays_counted_or_takes_the_fallback() {
    let mut ctx = mock_ctx();
    {
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region_lazy(&mut ctx, None);
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
        assert!(!active_downcall_gc_safe());
        guard.leave_region(&mut ctx);
        guard.enter_region_lazy(&mut ctx, Some(true));
        assert!(matches!(
            active_downcall_region(),
            DowncallRegion::OpenQuiet(..)
        ));
        guard.leave_region(&mut ctx);
        assert_eq!(ctx.blocking_region_counts(), (1, 1));
    }
    assert_eq!(ctx.lazy_native_log.0, 0, "the mock declined every entry");
    assert_eq!(active_downcall_region(), DowncallRegion::Counted);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn a_lazy_region_is_left_and_reentered_over_its_own_band() {
    let mut ctx = mock_ctx();
    ctx.lazy_native_accept = true;
    let spill = CalleeSavedSpill::capture();
    let here = &spill as *const CalleeSavedSpill as usize;
    {
        let guard = ActiveContextGuard::install(&mut ctx);
        guard.enter_region_lazy(&mut ctx, Some(true));
        let DowncallRegion::OpenLazy(lo, hi) = active_downcall_region() else {
            panic!("the accepting context must enter the lazy state");
        };
        assert!(
            lo < here && here < hi,
            "the calling frame and its register spill lie inside the band"
        );
        assert_eq!(lo & 7, 0);
        assert!(active_downcall_gc_safe());
        // An up-call: leave, run Java, back in over the same band.
        leave_downcall_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::LeftLazy(lo, hi));
        assert!(!active_downcall_gc_safe());
        assert!(!leave_active_downcall_region(), "already left");
        reenter_downcall_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::OpenLazy(lo, hi));
        // A JNIEnv function C calls.
        assert!(leave_active_downcall_region());
        reenter_active_downcall_region();
        assert_eq!(active_downcall_region(), DowncallRegion::OpenLazy(lo, hi));
        guard.leave_region(&mut ctx);
        assert_eq!(active_downcall_region(), DowncallRegion::Counted);
        assert_eq!(ctx.lazy_native_log.0, 3, "three entries");
        assert_eq!(ctx.lazy_native_log.1, 3, "each one left once");
        assert_eq!(ctx.lazy_native_log.2, (lo, hi));
    }
    assert_eq!(
        ctx.blocking_region_counts(),
        (0, 0),
        "no deposit-based region was opened"
    );
    spill.keep_alive();
}

/// The band's stack top is asked of the OS once per thread and cached.
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn the_band_runs_from_below_the_caller_to_the_cached_stack_top() {
    let local = 0u64;
    let here = std::hint::black_box(&local) as *const u64 as usize;
    let Some((lo, hi)) = lazy_band_from_here() else {
        panic!("the stack top of a test thread is known");
    };
    assert!(lo < here && here < hi);
    assert_eq!(LAZY_STACK_TOP.with(Cell::get), hi);
    assert_eq!(lazy_band_from_here().map(|b| b.1), Some(hi));
}
