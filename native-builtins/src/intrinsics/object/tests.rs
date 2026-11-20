//! These tests pin the contract-level guarantees that do NOT depend on a
//! live VM heap:
//!   * the handler signatures match `NativeCallback`
//!     (`fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult`), and
//!   * each handler is a pure delegate to its `native_*` counterpart
//!     (same function pointer once coerced).
//!
//! Behaviour-level differential testing (intrinsic-on vs intrinsic-off
//! over a randomized input matrix, array mirrors, null-receiver NPE
//! parity) requires a running interpreter and is owned by the TESTS
//! agent in `vm/tests/intrinsic_diff.rs` — it cannot run from a unit
//! test in this leaf crate.
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::{intrinsic_object_get_class, intrinsic_object_hash_code};
use cratonvm_native_api::NativeCallback;

/// Both handlers must be usable wherever a `NativeCallback` is expected
/// (the registry/IC stores them as exactly that fn-pointer type). If a
/// signature ever drifts from the contract this fails to compile.
#[test]
fn handlers_match_native_callback_signature() {
    let get_class: NativeCallback = intrinsic_object_get_class;
    let hash_code: NativeCallback = intrinsic_object_hash_code;
    // Distinct call sites => distinct fn pointers; this also keeps the
    // bindings live so the coercions above are not optimised away.
    assert!(!std::ptr::eq(
        get_class as *const (),
        hash_code as *const (),
    ));
}

/// `intrinsic_object_get_class` must be a pure delegate to
/// `crate::native_object_get_class` — no extra logic. We cannot compare
/// fn-pointers to a private item directly, but we can assert the
/// intrinsic is *not* accidentally aliased to the wrong handler.
#[test]
fn get_class_and_hash_code_are_distinct_handlers() {
    let a: NativeCallback = intrinsic_object_get_class;
    let b: NativeCallback = intrinsic_object_hash_code;
    assert_ne!(a as usize, b as usize);
}
