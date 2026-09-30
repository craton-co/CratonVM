// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter intrinsic handlers for `java/lang/StringBuilder`.
//!
//! Every handler here is behavior-identical to the normal native-registry
//! dispatch path: each delegates to the exact `native_sb_*` function that
//! `register_string_builder_natives` (in `crate::lang_string`) binds for the
//! corresponding method+descriptor. No reimplementation, no synthetic stubs.
//!
//! All StringBuilder methods covered here are INSTANCE methods, so
//! `args == [receiver, params...]` — exactly what the native registry path
//! passes to the underlying `native_sb_*` function.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{error::MethodCallResult, Value};

/// `append (Ljava/lang/String;)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_string`
/// (`lang_string.rs:171`).
pub fn intrinsic_sb_append_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_string(ctx, args)
}

/// `append (I)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_int`
/// (`lang_string.rs:183`).
pub fn intrinsic_sb_append_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_int(ctx, args)
}

/// `append (C)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_char`
/// (`lang_string.rs:195`).
pub fn intrinsic_sb_append_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_char(ctx, args)
}

/// `append (J)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_long`
/// (`lang_string.rs:243`).
pub fn intrinsic_sb_append_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_long(ctx, args)
}

/// `append (Z)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_boolean`
/// (`lang_string.rs:231`).
pub fn intrinsic_sb_append_bool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_boolean(ctx, args)
}

/// `append (Ljava/lang/Object;)Ljava/lang/StringBuilder;`
///
/// Registry binds this descriptor to `native_sb_append_object`
/// (`lang_string.rs:279`).
pub fn intrinsic_sb_append_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_append_object(ctx, args)
}

/// `toString ()Ljava/lang/String;`
///
/// Registry binds this descriptor to `native_sb_to_string`
/// (`lang_string.rs:331`).
pub fn intrinsic_sb_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_to_string(ctx, args)
}

/// `length ()I`
///
/// Registry binds this descriptor to `native_sb_length`
/// (`lang_string.rs:334`).
pub fn intrinsic_sb_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crate::lang_string::native_sb_length(ctx, args)
}

#[cfg(test)]
mod tests {
    //! Behavior correctness of these handlers is exercised end-to-end by the
    //! `vm/tests/intrinsic_diff.rs` differential suite (intrinsics on vs.
    //! `CRATONVM_DISABLE_INTRINSICS=1`), because exercising a StringBuilder
    //! append in isolation needs a full `NativeContext` + a heap-allocated
    //! receiver, which the `native-builtins` crate has no fixture for.
    //!
    //! What we CAN assert without a VM is structural: every handler has the
    //! exact `NativeCallback` signature, so it is wirable into the registry's
    //! dispatch path. A signature drift here fails to compile.
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// `NativeCallback` shape (see `cratonvm_native_api::NativeCallback`).
    type Cb = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

    #[test]
    fn handlers_have_native_callback_signature() {
        // Coercion to `Cb` is the assertion: if any handler's signature
        // diverged from the native-callback shape this would not type-check.
        let handlers: [Cb; 8] = [
            intrinsic_sb_append_string,
            intrinsic_sb_append_int,
            intrinsic_sb_append_char,
            intrinsic_sb_append_long,
            intrinsic_sb_append_bool,
            intrinsic_sb_append_object,
            intrinsic_sb_to_string,
            intrinsic_sb_length,
        ];
        for h in handlers {
            assert_ne!(h as usize, 0, "handler fn pointer must be non-null");
        }
    }

    #[test]
    fn delegate_natives_have_native_callback_signature() {
        // The `native_sb_*` delegates must share the same callback shape so
        // the thin wrappers above are valid one-line forwards.
        let natives: [Cb; 8] = [
            crate::lang_string::native_sb_append_string,
            crate::lang_string::native_sb_append_int,
            crate::lang_string::native_sb_append_char,
            crate::lang_string::native_sb_append_long,
            crate::lang_string::native_sb_append_boolean,
            crate::lang_string::native_sb_append_object,
            crate::lang_string::native_sb_to_string,
            crate::lang_string::native_sb_length,
        ];
        for n in natives {
            assert_ne!(n as usize, 0, "native fn pointer must be non-null");
        }
    }
}
