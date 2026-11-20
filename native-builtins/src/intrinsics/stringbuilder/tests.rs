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
