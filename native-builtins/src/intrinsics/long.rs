// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java/lang/Long` interpreter intrinsics.
//!
//! Group file for the interpreter intrinsic-dispatch fast path
//! (see `feature_roadmap_interpreter_intrinsic_table.md` and the shared
//! contract `intrinsic_table_contract.md`).
//!
//! Hard project rule (`feedback_no_synthetic_stubs`): NO synthetic stubs, NO
//! fake behavior. Every handler here is byte-for-byte behavior-identical to the
//! normal native-registry dispatch path because it DELEGATES verbatim to the
//! exact same `native_*` function the registry registers (in `lang_math.rs`).
//! The intrinsic table only optimizes *dispatch* (skips the `RwLock`, the
//! descriptor parse and the `FxHashMap` probe) — it never changes observable
//! behavior.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// `java/lang/Long.valueOf (J)Ljava/lang/Long;`
///
/// STATIC method — `args` is `[long]`. Note `Value::Long` occupies a single
/// slot in the decoded `args` slice (these are decoded `Value`s, not raw stack
/// words), so `args[0]` is the whole long.
///
/// Delegates verbatim to [`crate::lang_math::native_long_value_of`], the same
/// function registered under
/// `("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;")` in `lang_math.rs`.
/// Because dispatch lands in identical Rust code, the wrapper-allocation /
/// Long-cache identity behavior is exactly whatever the registry path produces
/// — there is no second code path to drift from.
pub fn intrinsic_long_value_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_math::native_long_value_of(ctx, args)
}

/// `java/lang/Long.longValue ()J`
///
/// INSTANCE method — `args` is `[receiver]` exactly as the native registry
/// path passes it.
///
/// Delegates verbatim to [`crate::lang_math::native_wrapper_long_value`], the
/// same function registered under `("java/lang/Long", "longValue", "()J")` in
/// `lang_math.rs`. (The registered callback is `native_wrapper_long_value`, not
/// a `native_long_long_value`; the contract handler name `intrinsic_long_long_value`
/// maps onto that exact registered native.)
pub fn intrinsic_long_long_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_math::native_wrapper_long_value(ctx, args)
}

/// `java/lang/Long.parseLong (Ljava/lang/String;)J`
///
/// STATIC method — `args` is `[stringRef]`.
///
/// Delegates verbatim to [`crate::lang_math::native_long_parse_long`], the same
/// function registered under
/// `("java/lang/Long", "parseLong", "(Ljava/lang/String;)J")` in `lang_math.rs`.
/// Because dispatch lands in identical Rust code, `NumberFormatException`
/// parity (roadmap §7) is automatic: a null string or an unparseable input
/// throws the identical exception with intrinsics on or off.
pub fn intrinsic_long_parse_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_math::native_long_parse_long(ctx, args)
}

#[cfg(test)]
mod tests {
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
}
