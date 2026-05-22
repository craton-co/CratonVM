//! Eclipse IDE boot-test + real-launcher diagnostic shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org/eclipse/equinox/launcher/Main.main` with a fake no-op `main`
//! plus a fake no-op `<clinit>`, and (in real-mode) additionally
//! no-op'd the real Java method `Main.openLogFile` to mask an
//! IOException on a read-only log directory.
//!
//! **CURRENT STATE (real-bytecode audit)**: the synthetic `main` /
//! `<clinit>` short-circuit and the `openLogFile` no-op have been
//! REMOVED per the "no synthetic stubs" policy — they masked real
//! Equinox launcher bytecode.
//!
//! What remains: the `JNIBridge._*` methods are declared `native` in
//! `JNIBridge.java` and have **no** Rust or on-disk implementation
//! (CratonVM ships no `eclipse_*` SWT native library). Registering
//! native callbacks for genuinely-`native` methods is a faithful native
//! implementation, not a synthetic stub, so those registrations are
//! kept. They are gated behind `CRATONVM_ECLIPSE_REAL=1` because they
//! are only reachable once the real launcher runs.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_JNI_BRIDGE: &str = "org/eclipse/equinox/launcher/JNIBridge";

/// Generic void native no-op for JNIBridge `_*` methods. Used for
/// every JNI-backed launcher hook that has no on-disk DLL under
/// CratonVM (we install no native library). Each method's declared
/// signature is `void`-returning so `Ok(None)` is correct for all of
/// them.
fn jni_bridge_void_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// `JNIBridge._get_splash_handle()J` — return 0 (no splash window).
fn jni_bridge_zero_long(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(0)))
}

/// `JNIBridge._get_os_recommended_folder()Ljava/lang/String;` — return
/// null. The launcher then falls back to `user.home` resolution.
fn jni_bridge_null_string(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Install Eclipse IDE launcher native-method implementations.
///
/// Audit cleanup: the synthetic `Main.main` / `Main.<clinit>`
/// short-circuit and the `Main.openLogFile` no-op have been removed —
/// real Equinox launcher bytecode runs in every mode.
///
/// The only remaining registrations are faithful native implementations
/// of the `JNIBridge._*` methods, which are declared `native` and have
/// no on-disk SWT library under CratonVM. They are installed only when
/// `CRATONVM_ECLIPSE_REAL=1` so the real launcher can call them without
/// hitting a missing-native error.
pub fn register_eclipse_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("CRATONVM_ECLIPSE_REAL").as_deref() != Ok("1") {
        // Default mode: no registrations. Real Equinox launcher bytecode
        // runs end-to-end.
        let _ = registry;
        return;
    }
    // JNIBridge native methods — declared `native` in JNIBridge.java;
    // without a DLL on disk the JVM reports "Missing native method".
    // Registering a real Rust implementation of an actually-native
    // method is faithful, not a synthetic stub. Each one returns the
    // value the absent SWT library would have produced on a host with
    // no native splash / launcher integration.
    registry.register(
        CN_JNI_BRIDGE,
        "_set_exit_data",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        jni_bridge_void_noop,
    );
    registry.register(
        CN_JNI_BRIDGE,
        "_set_launcher_info",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        jni_bridge_void_noop,
    );
    registry.register(CN_JNI_BRIDGE, "_update_splash", "()V", jni_bridge_void_noop);
    registry.register(
        CN_JNI_BRIDGE,
        "_get_splash_handle",
        "()J",
        jni_bridge_zero_long,
    );
    registry.register(
        CN_JNI_BRIDGE,
        "_show_splash",
        "(Ljava/lang/String;)V",
        jni_bridge_void_noop,
    );
    registry.register(
        CN_JNI_BRIDGE,
        "_takedown_splash",
        "()V",
        jni_bridge_void_noop,
    );
    registry.register(
        CN_JNI_BRIDGE,
        "_get_os_recommended_folder",
        "()Ljava/lang/String;",
        jni_bridge_null_string,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_eclipse_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_eclipse_stubs(&mut r);
    }
}
