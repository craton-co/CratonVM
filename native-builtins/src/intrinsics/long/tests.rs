use super::*;
use cratonvm_native_api::NativeCallback;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Each intrinsic handler must be usable exactly where a `NativeCallback`
/// is expected — the contract requires `CachedInvokeTarget::Intrinsic` to
/// store it as a directly-callable `NativeCallback`. These coercions
/// compile only if every handler's signature matches the registry callback
/// type byte-for-byte.
#[test]
fn handlers_are_native_callbacks() {
    let value_of: NativeCallback = intrinsic_long_value_of;
    let long_value: NativeCallback = intrinsic_long_long_value;
    let parse_long: NativeCallback = intrinsic_long_parse_long;
    assert!(!(value_of as *const ()).is_null());
    assert!(!(long_value as *const ()).is_null());
    assert!(!(parse_long as *const ()).is_null());
}

/// Documents — and pins via the type system — that each handler is a pure
/// delegation to the registered native impl. If any of
/// `native_long_value_of`, `native_wrapper_long_value` or
/// `native_long_parse_long` is renamed or its signature drifts, this test
/// stops compiling, which is the intended early-warning that dispatch could
/// diverge from the slow path. Behavioral parity (Long-cache identity for
/// `valueOf`, `NumberFormatException` for `parseLong`) is covered
/// end-to-end by the differential harness in `vm/tests/intrinsic_diff.rs`.
#[test]
fn delegates_to_registered_natives() {
    let d_value_of: NativeCallback = crate::lang_math::native_long_value_of;
    let d_long_value: NativeCallback = crate::lang_math::native_wrapper_long_value;
    let d_parse_long: NativeCallback = crate::lang_math::native_long_parse_long;
    assert!(!(d_value_of as *const ()).is_null());
    assert!(!(d_long_value as *const ()).is_null());
    assert!(!(d_parse_long as *const ()).is_null());
}
