// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/String`.
//!
//! See `intrinsic_table_contract.md` and
//! `feature_roadmap_interpreter_intrinsic_table.md`.
//!
//! These handlers are the interpreter fast-path entry points for the hottest
//! `String` leaf methods. They MUST be behavior-identical to the normal native
//! registry dispatch — so each one delegates directly to the same crate-local
//! `native_*` implementation that the registry binds (see
//! `native-builtins/src/lib.rs` registrations for `java/lang/String`):
//!
//!   - `length  ()I`  -> `crate::lang_string::native_string_length`
//!   - `charAt  (I)C` -> `crate::lang_string::native_string_char_at`
//!   - `isEmpty ()Z`  -> `crate::lang_string::native_string_is_empty`
//!
//! `java/lang/String` is `final`, so every call site is effectively
//! monomorphic — the receiver class-id guard in the IC is exact.
//!
//! `args` is `[receiver, params...]` for these instance methods, exactly as
//! the native registry path passes them. Exception behavior (NPE on a null
//! receiver for `charAt`, `StringIndexOutOfBoundsException` for an out-of-range
//! index) is preserved because it comes entirely from the delegated function.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// `java/lang/String.length ()I` — code-unit count of the receiver string.
///
/// Delegates verbatim to `crate::lang_string::native_string_length`.
pub fn intrinsic_string_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_string_length(ctx, args)
}

/// `java/lang/String.charAt (I)C` — the UTF-16 code unit at the given index.
///
/// Delegates verbatim to `crate::lang_string::native_string_char_at`, which
/// throws `NullPointerException` for a null receiver and
/// `StringIndexOutOfBoundsException` for an out-of-range index.
pub fn intrinsic_string_char_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_string_char_at(ctx, args)
}

/// `java/lang/String.isEmpty ()Z` — `true` iff the receiver has length 0.
///
/// Delegates verbatim to `crate::lang_string::native_string_is_empty`.
pub fn intrinsic_string_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_string_is_empty(ctx, args)
}

#[cfg(test)]
mod tests;
