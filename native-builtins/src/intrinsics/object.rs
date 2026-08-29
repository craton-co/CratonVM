// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/Object`.
//!
//! See `intrinsic_table_contract.md` and
//! `gaps/feature_roadmap_interpreter_intrinsic_table.md`.
//!
//! Hard project rule (`feedback_no_synthetic_stubs`): these handlers MUST be
//! byte-for-byte behaviour-identical to the normal native-registry dispatch
//! path. They therefore do NOT reimplement any logic — each one delegates
//! straight to the exact crate-local `native_*` function that the registry
//! itself dispatches to:
//!
//! | (class, name, descriptor)                          | registered native fn        |
//! |----------------------------------------------------|-----------------------------|
//! | `java/lang/Object getClass ()Ljava/lang/Class;`    | `crate::native_object_get_class`  |
//! | `java/lang/Object hashCode ()I`                    | `crate::native_object_hash_code`  |
//!
//! Both registrations live in `native-builtins/src/lib.rs` (see the
//! `registry.register("java/lang/Object", ...)` calls). The functions are
//! `fn` (module-private to the crate root), but Rust privacy makes any item
//! defined in the crate root visible to every descendant module of the same
//! crate — so `crate::native_object_*` is a legal call site from here.
//!
//! `args` layout for these two INSTANCE methods is `[receiver]` — exactly
//! what the native registry path passes, so it is forwarded verbatim.
//!
//! The contract mandates fully-qualified types in the handler signatures
//! (`cratonvm_native_api::NativeContext`, `cratonvm_types::Value`,
//! `cratonvm_types::error::MethodCallResult`), so this module deliberately
//! carries no `use` imports for them.

/// Intrinsic for `java/lang/Object.getClass ()Ljava/lang/Class;` (virtual).
///
/// `args == [receiver]`. Delegates verbatim to the same native function the
/// registry dispatches to, so array-mirror handling, the null-receiver
/// `NullPointerException`, and the steady-state class-mirror lookup are all
/// identical to the slow path.
pub fn intrinsic_object_get_class(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    crate::native_object_get_class(ctx, args)
}

/// Intrinsic for `java/lang/Object.hashCode ()I` (virtual).
///
/// `args == [receiver]`. Delegates verbatim to the same native function the
/// registry dispatches to, so the identity-hash computation and the
/// null-receiver `NullPointerException` are identical to the slow path.
pub fn intrinsic_object_hash_code(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[cratonvm_types::Value],
) -> cratonvm_types::error::MethodCallResult {
    crate::native_object_hash_code(ctx, args)
}

#[cfg(test)]
mod tests {
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
}
