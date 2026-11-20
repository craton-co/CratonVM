// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `java/lang/System` interpreter intrinsics.
//!
//! Group file for the interpreter intrinsic-dispatch fast path
//! (see `feature_roadmap_interpreter_intrinsic_table.md` and the shared
//! contract `intrinsic_table_contract.md`).
//!
//! Hard project rule (`feedback_no_synthetic_stubs`): NO synthetic stubs, NO
//! fake behavior. Every handler here is byte-for-byte behavior-identical to the
//! normal native-registry dispatch path because it DELEGATES verbatim to the
//! exact same `native_*` function the registry registers. The intrinsic table
//! only optimizes *dispatch* (skips the `RwLock`, the descriptor parse and the
//! `FxHashMap` probe) — it never changes observable behavior.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// `java/lang/System.arraycopy (Ljava/lang/Object;ILjava/lang/Object;II)V`
///
/// STATIC method — `args` is `[src, srcPos, dest, destPos, length]` exactly as
/// the native registry path passes them (no receiver for a static call).
///
/// Delegates verbatim to [`crate::lang_system::native_system_arraycopy`], the
/// same function registered under
/// `("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V")`
/// in `lib.rs`. Because dispatch lands in identical Rust code, exception parity
/// (roadmap §7) is automatic: `NullPointerException`, `ArrayStoreException` and
/// `ArrayIndexOutOfBoundsException` are thrown identically with intrinsics on
/// or off.
pub fn intrinsic_system_arraycopy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_system::native_system_arraycopy(ctx, args)
}

#[cfg(test)]
mod tests;
