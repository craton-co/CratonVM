#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::{mock_ctx, mock_reclaims, mock_refuse_allocations, MockNativeContext};
use cratonvm_types::error::{RuntimeError, VmError};
use cratonvm_types::ObjectRef;

/// Clears the thread-local knob even when an assertion panics, so a
/// sequential (`--test-threads=1`) run cannot leak refusals into the next
/// test on this thread.
struct KnobReset;
impl Drop for KnobReset {
    fn drop(&mut self) {
        mock_refuse_allocations(0, false);
    }
}

fn is_oom(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { .. }
        ))
    )
}

fn fresh_sb(ctx: &mut MockNativeContext) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/lang/StringBuilder")
        .unwrap();
    ctx.alloc_object(cid, 4)
}

#[test]
fn sb_capacity_constructor_reclaims_once_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let sb = fresh_sb(&mut ctx);
    mock_refuse_allocations(1, true);
    native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(32)])
        .expect("the retry after the reclaim is served");
    assert_eq!(mock_reclaims(), 1);
    let len = native_sb_length(&mut ctx, &[Value::Object(Some(sb))]).unwrap();
    assert_eq!(len, Some(Value::Int(0)));
}

#[test]
fn sb_capacity_constructor_raises_oome_after_one_reclaim() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let sb = fresh_sb(&mut ctx);
    mock_refuse_allocations(2, true);
    let err = native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(32)])
        .unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(mock_reclaims(), 1, "one reclaim, no spinning");
}

#[test]
fn sb_capacity_past_the_vm_limit_does_not_collect() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let sb = fresh_sb(&mut ctx);
    mock_refuse_allocations(1, true);
    let err =
        native_sb_init_capacity(&mut ctx, &[Value::Object(Some(sb)), Value::Int(i32::MAX)])
            .unwrap_err();
    assert!(is_oom(&err));
    assert_eq!(
        mock_reclaims(),
        0,
        "no collection can serve Integer.MAX_VALUE"
    );
}

#[test]
fn sb_string_constructor_reclaims_and_keeps_the_content() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let sb = fresh_sb(&mut ctx);
    let s = ctx.create_string("abc");
    mock_refuse_allocations(1, true);
    native_sb_init_string(&mut ctx, &[Value::Object(Some(sb)), Value::Object(Some(s))])
        .expect("served after the reclaim");
    assert_eq!(mock_reclaims(), 1);
    mock_refuse_allocations(0, false);
    assert_eq!(
        sb_read_chars(&ctx, sb),
        "abc".encode_utf16().collect::<Vec<u16>>()
    );
}

#[test]
fn to_char_array_reclaims_once_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hey");
    mock_refuse_allocations(1, true);
    let r = native_string_to_char_array(&mut ctx, &[Value::Object(Some(s))]).unwrap();
    assert_eq!(mock_reclaims(), 1);
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected a char[], got {r:?}");
    };
    assert_eq!(ctx.array_length(arr), 3);
    assert_eq!(ctx.get_array_element(arr, 1), Value::Int(i32::from(b'e')));
}

#[test]
fn to_char_array_raises_oome_when_the_retry_is_refused() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hey");
    mock_refuse_allocations(2, true);
    let err = native_string_to_char_array(&mut ctx, &[Value::Object(Some(s))]).unwrap_err();
    assert!(is_oom(&err), "expected OutOfMemoryError, got {err:?}");
    assert_eq!(mock_reclaims(), 1);
}

#[test]
fn get_bytes_reclaims_once_and_is_served() {
    let _reset = KnobReset;
    let mut ctx = mock_ctx();
    let s = ctx.create_string("ok");
    mock_refuse_allocations(1, true);
    let r = native_string_get_bytes(&mut ctx, &[Value::Object(Some(s))]).unwrap();
    assert_eq!(mock_reclaims(), 1);
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected a byte[], got {r:?}");
    };
    assert_eq!(ctx.array_length(arr), 2);
}
