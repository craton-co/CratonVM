//! These tests pin the contract-level guarantees that do NOT depend on a
//! live VM heap:
//!   * the handler signatures match `NativeCallback`
//!     (`fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult`), and
//!   * each handler is a distinct pure delegate to its `native_*`
//!     counterpart.
//!
//! Behaviour-level differential testing (intrinsic-on vs intrinsic-off
//! over a randomized input matrix, the `Integer`-cache identity invariant,
//! and `parseInt` `NumberFormatException` parity) requires a running
//! interpreter and is owned by the TESTS agent in
//! `vm/tests/intrinsic_diff.rs` — it cannot run from a unit test in this
//! leaf crate.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::{
    intrinsic_integer_int_value, intrinsic_integer_parse_int, intrinsic_integer_value_of,
};
use cratonvm_native_api::NativeCallback;

/// All three handlers must be usable wherever a `NativeCallback` is
/// expected (the registry/IC stores them as exactly that fn-pointer
/// type). If a signature ever drifts from the contract this fails to
/// compile.
#[test]
fn handlers_match_native_callback_signature() {
    let value_of: NativeCallback = intrinsic_integer_value_of;
    let int_value: NativeCallback = intrinsic_integer_int_value;
    let parse_int: NativeCallback = intrinsic_integer_parse_int;
    // Distinct call sites => distinct fn pointers; this also keeps the
    // bindings live so the coercions above are not optimised away.
    assert!(!std::ptr::eq(value_of as *const (), int_value as *const (),));
    assert!(!std::ptr::eq(
        int_value as *const (),
        parse_int as *const (),
    ));
    assert!(!std::ptr::eq(value_of as *const (), parse_int as *const (),));
}

/// The three intrinsics must be distinct handlers — guards against an
/// accidental copy-paste aliasing one to the wrong native.
#[test]
fn handlers_are_distinct() {
    let a: NativeCallback = intrinsic_integer_value_of;
    let b: NativeCallback = intrinsic_integer_int_value;
    let c: NativeCallback = intrinsic_integer_parse_int;
    assert_ne!(a as usize, b as usize);
    assert_ne!(b as usize, c as usize);
    assert_ne!(a as usize, c as usize);
}

/// The JIT site cache's two identity questions answer for their own
/// registered bodies, and not for each other's. (Not asserted against an
/// unrelated native: `native_wrapper_int_value` has the same body as
/// `native_wrapper_int_hash_code`, so an optimised build may merge the two,
/// and the fast arm then answers `intValue` identically, which is correct.)
#[test]
fn int_wrapper_identity_questions_name_their_own_bodies() {
    use super::{is_int_wrapper_equals_native, is_int_wrapper_hash_code_native};
    let hash: NativeCallback = crate::lang_math::native_wrapper_int_hash_code;
    let equals: NativeCallback = crate::lang_math::native_wrapper_int_equals;
    assert!(is_int_wrapper_hash_code_native(hash));
    assert!(is_int_wrapper_equals_native(equals));
    assert!(!is_int_wrapper_hash_code_native(equals));
    assert!(!is_int_wrapper_equals_native(hash));
}
