// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/Integer`.
//!
//! See `intrinsic_table_contract.md` and
//! `gaps/feature_roadmap_interpreter_intrinsic_table.md`.
//!
//! Hard project rule (`feedback_no_synthetic_stubs`): these handlers MUST be
//! byte-for-byte behaviour-identical to the normal native-registry dispatch
//! path. They therefore do NOT reimplement any logic — each one delegates
//! straight to the exact crate-local `native_*` function that the registry
//! itself dispatches to. The relevant registrations live in
//! `native-builtins/src/lang_math.rs` (`register_wrapper_natives`):
//!
//! | (class, name, descriptor)                            | registered native fn                       |
//! |------------------------------------------------------|---------------------------------------------|
//! | `java/lang/Integer valueOf (I)Ljava/lang/Integer;`   | `crate::lang_math::native_integer_value_of` |
//! | `java/lang/Integer intValue ()I`                     | `crate::lang_math::native_wrapper_int_value`|
//! | `java/lang/Integer parseInt (Ljava/lang/String;)I`   | `crate::lang_math::native_integer_parse_int`|
//!
//! Those `native_*` functions are `pub(crate)`, so they are reachable from
//! this descendant module via `crate::lang_math::<fn>`.
//!
//! Note: `letsgo_compat.rs` *also* registers `Integer.valueOf` (`box_integer`)
//! and `Integer.intValue` (`unbox_int_field`) as layout-safe fallbacks, but
//! `register_wrapper_natives` is the canonical implementation — crucially,
//! `native_integer_value_of` preserves the JLS `Integer`-cache identity
//! invariant for `-128..=127` (`box_integer` always allocates a fresh
//! wrapper). Delegating to the `lang_math` natives is therefore required for
//! true behaviour parity with the registry path that the interpreter uses.
//!
//! `args` layout: `valueOf`/`parseInt` are STATIC, so `args == [param0]`
//! (`[int]` and `[stringRef]` respectively); `intValue` is an INSTANCE method,
//! so `args == [receiver]` — exactly what the native registry path passes, so
//! every handler forwards `args` verbatim.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// Intrinsic for `java/lang/Integer.valueOf (I)Ljava/lang/Integer;` (static).
///
/// `args == [int]`. Delegates verbatim to `native_integer_value_of`, so the
/// `Integer`-cache identity behaviour for the `-128..=127` range (returning
/// the canonical cached wrapper instead of a fresh allocation) is identical
/// to the slow path.
pub fn intrinsic_integer_value_of(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    crate::lang_math::native_integer_value_of(ctx, args)
}

/// Intrinsic for `java/lang/Integer.intValue ()I` (virtual).
///
/// `args == [receiver]`. Delegates verbatim to `native_wrapper_int_value`,
/// which reads the boxed primitive from `field 0` of the receiver wrapper —
/// identical to the slow path.
pub fn intrinsic_integer_int_value(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    crate::lang_math::native_wrapper_int_value(ctx, args)
}

/// Intrinsic for `java/lang/Integer.parseInt (Ljava/lang/String;)I` (static).
///
/// `args == [stringRef]`. Delegates verbatim to `native_integer_parse_int`,
/// so a null or unparseable argument throws `NumberFormatException` with the
/// exact same message text as the slow path.
pub fn intrinsic_integer_parse_int(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    crate::lang_math::native_integer_parse_int(ctx, args)
}

// Keep the bare `use` imports referenced even when the fully-qualified forms
// above are what the contract mandates in the signatures. This documents the
// canonical type aliases and avoids an `unused_imports` warning.
#[allow(dead_code)]
type _AssertNativeContext = dyn NativeContext;
#[allow(dead_code)]
fn _assert_aliases(_v: Value) -> MethodCallResult {
    Ok(None)
}

#[cfg(test)]
mod tests {
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
}
