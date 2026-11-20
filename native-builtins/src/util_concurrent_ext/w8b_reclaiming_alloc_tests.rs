use super::*;
use crate::test_utils::{
    mock_failed_retry_calls, mock_failed_retry_pays, mock_reclaims, mock_refuse_allocations,
    MockNativeContext,
};
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::error::{RuntimeError, VmError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};

const CLASS: &str = "java/util/concurrent/locks/ReentrantLock";

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        ))
    )
}

/// The pin stack's depth, read by pinning a probe and releasing it.
fn pin_depth(ctx: &mut MockNativeContext, probe: ObjectRef) -> usize {
    let at = ctx.pin_native_root(probe);
    ctx.unpin_native_roots(at);
    at
}

#[test]
fn an_unrefused_allocation_never_reclaims() {
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(0, true);
    let obj = try_alloc_concurrent_synthetic_reclaiming(&mut ctx, CLASS, 3).unwrap();
    assert!(ctx.object_num_fields(obj) >= 3);
    assert_eq!(mock_reclaims(), 0);
    mock_refuse_allocations(0, false);
}

#[test]
fn one_refusal_reclaims_once_and_the_retry_is_served() {
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(1, true);
    let obj = try_alloc_concurrent_synthetic_reclaiming(&mut ctx, CLASS, 3).unwrap();
    assert!(ctx.object_num_fields(obj) >= 3);
    assert_eq!(mock_reclaims(), 1);
    mock_refuse_allocations(0, false);
}

#[test]
fn a_refused_retry_is_a_catchable_oome_after_exactly_one_reclaim() {
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(2, true);
    let err = try_alloc_concurrent_synthetic_reclaiming(&mut ctx, CLASS, 3).unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(mock_reclaims(), 1, "one reclaim, no spinning");
    mock_refuse_allocations(0, false);
}

#[test]
fn a_declined_reclaim_does_not_retry() {
    let mut ctx = MockNativeContext::new();
    // Two refusals queued, reclaim says "not worth it": the second refusal
    // must still be pending afterwards, proving no retry was made.
    mock_refuse_allocations(2, false);
    let err = try_alloc_concurrent_synthetic_reclaiming(&mut ctx, CLASS, 3).unwrap_err();
    assert!(is_oom(&err));
    assert_eq!(mock_reclaims(), 1);
    assert!(ctx
        .try_alloc_object(cratonvm_types::ClassId::new(0), 0)
        .is_none());
    mock_refuse_allocations(0, false);
}

/// gci i13/oa item 4: the array doors hand the sized reclaim their
/// footprint (header plus 8-byte-aligned data), and a context that
/// implements only the unsized door (this mock) still reclaims, once,
/// through the trait's default `reclaim_before_alloc_retry_for`.
#[test]
fn gci_i13oa_array_doors_pass_the_footprint_and_default_to_the_unsized_door() {
    let header = cratonvm_types::ARRAY_DATA_OFFSET;
    assert_eq!(array_footprint(ArrayElementType::Byte, 10), header + 16);
    assert_eq!(array_footprint(ArrayElementType::Long, 3), header + 24);
    assert_eq!(array_footprint(ArrayElementType::Long, usize::MAX), usize::MAX);
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(1, true);
    let arr = new_array_reclaiming(&mut ctx, ArrayElementType::Int, 4, &mut [], "i13oa")
        .expect("the retry after the reclaim is served");
    assert_eq!(ctx.array_length(arr), 4);
    assert_eq!(mock_reclaims(), 1, "one reclaim through the default sized door");
    mock_refuse_allocations(0, false);
}

/// zr z8/nr: the reclaim's retry is refused too (another thread took the
/// room the collection made); the VM's failed-retry hook pays the skipped
/// clear-soft rung and the THIRD attempt is served. Before z8/nr the
/// helper raised `OutOfMemoryError` after the second attempt.
#[test]
fn a_failed_retry_takes_the_skipped_rung_before_the_error() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 4);
    mock_refuse_allocations(2, true);
    mock_failed_retry_pays(true);
    let mut live = [src];
    let dst = new_array_reclaiming(&mut ctx, ArrayElementType::Int, 8, &mut live, "t")
        .expect("the attempt after the repaid rung is served");
    assert_eq!(ctx.array_length(dst), 8);
    assert_eq!(mock_failed_retry_calls(), 1);
    // A hook that pays nothing: the error, after the same two attempts.
    mock_refuse_allocations(2, true);
    mock_failed_retry_pays(false);
    assert!(new_array_reclaiming(&mut ctx, ArrayElementType::Int, 8, &mut live, "t").is_err());
    assert_eq!(mock_failed_retry_calls(), 1);
    // The object and reference-array ladders take the same step.
    mock_refuse_allocations(2, true);
    mock_failed_retry_pays(true);
    let mut none: [ObjectRef; 0] = [];
    assert!(new_ref_array_reclaiming(&mut ctx, ClassId::new(0), 3, &mut none, "t").is_ok());
    mock_refuse_allocations(2, true);
    assert!(try_alloc_concurrent_synthetic_reclaiming(&mut ctx, CLASS, 3).is_ok());
    mock_refuse_allocations(0, false);
    mock_failed_retry_pays(false);
}

/// zr z8/nr: the generic ladder stops at the first served attempt.
#[test]
fn retry_after_reclaim_walks_the_rungs_in_order() {
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(0, true);
    mock_failed_retry_pays(true);
    let mut tries = 0;
    let got = retry_after_reclaim(&mut ctx, |_| {
        tries += 1;
        (tries == 2).then_some(tries)
    });
    assert_eq!(got, Some(2), "served after the repaid rung");
    assert_eq!(mock_reclaims(), 1);
    assert_eq!(mock_failed_retry_calls(), 1);
    mock_refuse_allocations(0, false);
    mock_failed_retry_pays(false);
    let got: Option<()> = retry_after_reclaim(&mut ctx, |_| None);
    assert_eq!(got, None, "the reclaim declined: nothing to retry");
    mock_failed_retry_pays(false);
}

#[test]
fn the_non_reclaiming_funnel_is_unchanged_by_the_knob() {
    // `try_alloc_concurrent_synthetic` never takes the fallible door, so a
    // queued refusal does not reach it and it never reclaims.
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(1, true);
    let obj = try_alloc_concurrent_synthetic(&mut ctx, CLASS, 3).unwrap();
    assert!(ctx.object_num_fields(obj) >= 3);
    assert_eq!(mock_reclaims(), 0);
    mock_refuse_allocations(0, false);
}

#[test]
fn array_ladder_pins_its_live_refs_across_the_reclaim_and_releases_them() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 4);
    let other = ctx.new_array(ArrayElementType::Byte, 2);
    let before = pin_depth(&mut ctx, src);
    mock_refuse_allocations(1, true);
    let mut live = [src, other];
    let dst = new_array_reclaiming(&mut ctx, ArrayElementType::Int, 8, &mut live, "t")
        .expect("the retry after the reclaim is served");
    assert_eq!(ctx.array_length(dst), 8);
    assert_eq!(mock_reclaims(), 1);
    // The mock does not move objects, so the refreshed refs are the
    // originals; what matters is that they came back through the pins
    // and that every pin was released.
    assert_eq!(live, [src, other]);
    assert_eq!(pin_depth(&mut ctx, src), before, "pins leaked");
    mock_refuse_allocations(0, false);
}

#[test]
fn ref_array_ladder_raises_oome_when_the_retry_is_refused() {
    let mut ctx = MockNativeContext::new();
    let src = ctx.new_array(ArrayElementType::Int, 1);
    let before = pin_depth(&mut ctx, src);
    mock_refuse_allocations(2, true);
    let mut live = [src];
    let err = new_ref_array_reclaiming(
        &mut ctx,
        cratonvm_types::ClassId::new(0),
        4,
        &mut live,
        "Arrays.copyOf",
    )
    .unwrap_err();
    assert!(is_oom(&err));
    assert_eq!(mock_reclaims(), 1);
    assert_eq!(
        pin_depth(&mut ctx, src),
        before,
        "pins leaked on the OOME path"
    );
    mock_refuse_allocations(0, false);
}

#[test]
fn an_empty_live_set_still_reclaims() {
    let mut ctx = MockNativeContext::new();
    mock_refuse_allocations(1, true);
    let arr = new_array_reclaiming(&mut ctx, ArrayElementType::Long, 3, &mut [], "t")
        .expect("served after the reclaim");
    assert_eq!(ctx.array_length(arr), 3);
    assert_eq!(mock_reclaims(), 1);
    mock_refuse_allocations(0, false);
}
