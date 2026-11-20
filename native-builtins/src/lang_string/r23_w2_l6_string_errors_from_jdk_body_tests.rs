use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// With no `java/lang/String` body to run (the mock declares none), the
/// native's own error stands: class and message as before, in every arm.
#[test]
fn without_a_declared_body_the_native_error_stands() {
    let mut ctx = MockNativeContext::new();
    let s = ctx.create_string("hello");
    let at = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(9)]);
    assert!(at.is_err(), "codePointAt(9) on a 5-char string fails: {at:?}");
    let count = native_string_code_point_count(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Int(3), Value::Int(9)],
    );
    assert!(count.is_err(), "codePointCount(3, 9) fails: {count:?}");
    let offset = native_string_offset_by_code_points(
        &mut ctx,
        &[Value::Object(Some(s)), Value::Int(0), Value::Int(9)],
    );
    assert!(offset.is_err(), "offsetByCodePoints(0, 9) fails: {offset:?}");
    // In range: the native's own answers, untouched.
    let ok = native_string_code_point_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(1)]);
    assert_eq!(ok.expect("in range"), Some(Value::Int('e' as i32)));
}

