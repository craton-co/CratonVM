// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/Integer`.
//!
//! See `intrinsic_table_contract.md` and
//! `feature_roadmap_interpreter_intrinsic_table.md`.
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
//! Those `native_*` functions are `pub`, so they are reachable from
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

/// Is `callback` the registered body of `Integer.hashCode()I` — the int-stored
/// wrappers' shared `hashCode` (`Character`, `Byte` and `Short` register the
/// same function)?
///
/// Asked by the VM's JIT native-site cache (`vm/src/jit/helpers.rs`,
/// `LeafNativeKind::IntWrapperHashCode`) once per resolved call site, so it can
/// answer a hit without the argument decode and the native-call funnel. Keyed
/// on the callback's IDENTITY, not on a name: a triple re-registered to a
/// different body is never answered by the fast arm. The VM crate cannot name
/// the `pub` native, which is why the question lives here.
///
/// `HashMap.hash` and `HashMap.getNode` reach these two on every `put`/`get`
/// with an `Integer` key (`CratonBench hashmap`: 30 000 000 calls, every one
/// through the funnel, because an inline cache can never hold a native).
pub fn is_int_wrapper_hash_code_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let own: cratonvm_native_api::NativeCallback = crate::lang_math::native_wrapper_int_hash_code;
    callback as usize == own as usize
}

/// Is `callback` the registered body of `Integer.equals(Object)Z` — the
/// int-stored wrappers' shared `equals` (`Boolean`, `Character`, `Byte`,
/// `Short`)? See [`is_int_wrapper_hash_code_native`].
pub fn is_int_wrapper_equals_native(callback: cratonvm_native_api::NativeCallback) -> bool {
    let own: cratonvm_native_api::NativeCallback = crate::lang_math::native_wrapper_int_equals;
    callback as usize == own as usize
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
mod tests;
