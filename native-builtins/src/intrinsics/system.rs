// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java/lang/System` interpreter intrinsics.
//!
//! Group file for the interpreter intrinsic-dispatch fast path
//! (see `gaps/feature_roadmap_interpreter_intrinsic_table.md` and the shared
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
mod tests {
    use super::*;
    use cratonvm_native_api::NativeCallback;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// The intrinsic handler must be usable exactly where a `NativeCallback`
    /// is expected — the contract requires `CachedInvokeTarget::Intrinsic` to
    /// store it as a directly-callable `NativeCallback`. This compiles only if
    /// the signature matches the registry callback type byte-for-byte.
    #[test]
    fn handler_is_a_native_callback() {
        let cb: NativeCallback = intrinsic_system_arraycopy;
        // Function-pointer identity: the intrinsic handler is the same fn the
        // contract expects to install in the inline cache. A non-zero address
        // is all we can assert without the (privately-scoped) VM heap harness.
        assert!(!(cb as *const ()).is_null());
    }

    /// Documents — and pins via the type system — that this handler is a pure
    /// delegation to the registered native impl. If `native_system_arraycopy`
    /// is renamed or its signature drifts, this test stops compiling, which is
    /// the intended early-warning that dispatch could diverge from the slow
    /// path. Behavioral parity (NPE / ArrayStoreException /
    /// ArrayIndexOutOfBoundsException, overlap handling, partial-commit) is
    /// covered end-to-end by the differential harness in
    /// `vm/tests/intrinsic_diff.rs`.
    ///
    /// This used to also credit "the heap-backed `tests_extracted.rs` cases".
    /// That file was never wired into the crate — no `mod tests_extracted;`
    /// existed anywhere in the tree — so it was never compiled and those cases
    /// never ran. It was deleted in the wave-3 stub sweep; the claim is removed
    /// with it rather than left pointing at coverage that did not exist.
    #[test]
    fn delegates_to_registered_native() {
        let delegate: NativeCallback = crate::lang_system::native_system_arraycopy;
        let handler: NativeCallback = intrinsic_system_arraycopy;
        // Both are valid `NativeCallback`s with the identical signature; the
        // handler body forwards `(ctx, args)` to `delegate` unchanged.
        assert!(!(delegate as *const ()).is_null());
        assert!(!(handler as *const ()).is_null());
    }
}
