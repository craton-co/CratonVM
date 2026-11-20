use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- length ------------------------------------------------------------

#[test]
fn length_empty() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = intrinsic_string_length(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn length_hello() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("hello");
    let r = intrinsic_string_length(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(5)));
}

#[test]
fn length_null_receiver_returns_zero() {
    // Mirrors `native_string_length`'s null-tolerant behavior; the IC
    // guard would normally NPE earlier, but the handler itself matches
    // the native path byte-for-byte.
    let mut ctx = mock_ctx();
    let r = intrinsic_string_length(&mut ctx, &[Value::Object(None)]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

/// The intrinsic must return exactly what the native registry path
/// returns — this asserts equality against the delegated function.
#[test]
fn length_matches_native() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("differential");
    let args = [Value::Object(Some(s))];
    let intrinsic = intrinsic_string_length(&mut ctx, &args);
    let native = crate::lang_string::native_string_length(&mut ctx, &args);
    assert_eq!(intrinsic.unwrap(), native.unwrap());
}

// --- charAt ------------------------------------------------------------

#[test]
fn char_at_valid() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = intrinsic_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(1)]);
    assert_eq!(r.unwrap(), Some(Value::Int('b' as i32)));
}

#[test]
fn char_at_out_of_bounds_throws() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = intrinsic_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(5)]);
    assert!(r.is_err());
}

#[test]
fn char_at_negative_index_throws() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let r = intrinsic_string_char_at(&mut ctx, &[Value::Object(Some(s)), Value::Int(-1)]);
    assert!(r.is_err());
}

#[test]
fn char_at_null_receiver_throws() {
    let mut ctx = mock_ctx();
    let r = intrinsic_string_char_at(&mut ctx, &[Value::Object(None), Value::Int(0)]);
    assert!(r.is_err());
}

#[test]
fn char_at_matches_native() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("abc");
    let args = [Value::Object(Some(s)), Value::Int(2)];
    let intrinsic = intrinsic_string_char_at(&mut ctx, &args);
    let native = crate::lang_string::native_string_char_at(&mut ctx, &args);
    assert_eq!(intrinsic.unwrap(), native.unwrap());
}

// --- isEmpty -----------------------------------------------------------

#[test]
fn is_empty_true_for_empty() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("");
    let r = intrinsic_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

#[test]
fn is_empty_false_for_nonempty() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("x");
    let r = intrinsic_string_is_empty(&mut ctx, &[Value::Object(Some(s))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn is_empty_matches_native() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("nonempty");
    let args = [Value::Object(Some(s))];
    let intrinsic = intrinsic_string_is_empty(&mut ctx, &args);
    let native = crate::lang_string::native_string_is_empty(&mut ctx, &args);
    assert_eq!(intrinsic.unwrap(), native.unwrap());
}
