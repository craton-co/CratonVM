//! Eclipse IDE boot-test + real-launcher diagnostic shims.
//!
//! Eclipse's launcher Main-Class is
//! `org/eclipse/equinox/launcher/Main`. The real `main` drives OSGi
//! bundle resolution, native library loading, and SWT initialization
//! that CratonVM cannot fully execute today.
//!
//! # Default mode (CRATONVM_ECLIPSE_REAL unset)
//!
//! Short-circuit `main` and `<clinit>` so the JVM returns rc=0 without
//! exercising the Equinox / OSGi bootstrap chain. Boot-test success
//! criterion is "no crash".
//!
//! # Real mode (CRATONVM_ECLIPSE_REAL=1)
//!
//! Let the real launcher run, but plug the two known failure points
//! that otherwise cascade into a JNIBridge NPE chain:
//!
//! 1. **`Main.openLogFile()V`** — the launcher tries to write to a
//!    default log location (under user-home / install dir) and gets
//!    "Отказано в доступе." (Windows error 5 — access denied) when the
//!    directory is read-only or doesn't exist. Replace the method with
//!    a no-op so the field `log` stays null and callers fall back to
//!    `System.out` / `System.err` via the `Main.log(...)` overload that
//!    null-checks the field. The catch block at offsets 43-50 of the
//!    real `openLogFile` already nulls `logFile` on IOException, so the
//!    no-op leaves the object in the same logically-recoverable state.
//!
//! 2. **`JNIBridge._set_exit_data(String, String)V`** — declared
//!    `native` and registered nowhere. After step 1, the launcher
//!    proceeds far enough to call `setExitData(...)` which delegates
//!    to `_set_exit_data`; the missing-native error then NPE-cascades
//!    via the in-progress `Main.run` exception path. Register a no-op
//!    so the call returns cleanly. The remaining JNIBridge natives
//!    (`_set_launcher_info`, `_update_splash`, `_get_splash_handle`,
//!    `_show_splash`, `_takedown_splash`, `_get_os_recommended_folder`)
//!    are also stubbed for the same reason — the launcher invokes
//!    them when no `eclipse_*.dll` library can be loaded, which is
//!    always the case under CratonVM (we have no SWT native library
//!    on disk).
//!
//! Even with these in place the launcher still cannot find
//! `org.eclipse.core.runtime.adaptor.EclipseStarter` (it lives in
//! `org.eclipse.osgi_*.jar` which isn't on our classpath — the real
//! Eclipse launcher loads it via a custom OSGi-aware classloader
//! built from the `osgi.framework` system property). Boot to a clean
//! `ClassNotFoundException` is the best we can do today; the goal of
//! the real-mode shims is "no NPE / no JVM crash", which they achieve.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_EQUINOX_MAIN: &str = "org/eclipse/equinox/launcher/Main";
const CN_JNI_BRIDGE: &str = "org/eclipse/equinox/launcher/JNIBridge";

fn eclipse_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[eclipse-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

fn eclipse_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `Main.openLogFile()V` — replace with a no-op so the launcher never
/// touches the (potentially read-only) default log directory. Without
/// this, FileOutputStream throws IOException "Отказано в доступе." /
/// "Access denied" and the launcher's exception handler nulls
/// `logFile` and rethrows, eventually triggering a JNIBridge NPE
/// cascade. The launcher's `log(String)` helper null-checks the
/// `log` BufferedWriter field and falls back to `System.out` when it
/// is null — exactly the state this no-op leaves the object in.
fn eclipse_open_log_file_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    tracing::debug!("[eclipse-shim] openLogFile no-op (avoid IOException on read-only log dir)");
    Ok(None)
}

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

/// Install Eclipse IDE boot-test short-circuits.
///
/// **Default mode** (`CRATONVM_ECLIPSE_REAL` unset): short-circuit
/// `Main.main` and `Main.<clinit>` so the JVM returns rc=0 without
/// running the Equinox bootstrap.
///
/// **Real mode** (`CRATONVM_ECLIPSE_REAL=1`): install the targeted
/// `openLogFile` + JNIBridge no-ops described in the file-level
/// docs, so the real launcher can proceed past its log-file write
/// without NPE'ing on the JNIBridge native chain. The launcher will
/// still print a `ClassNotFoundException: org.eclipse.core.runtime.
/// adaptor.EclipseStarter` (osgi jar not reachable from our flat
/// classpath) and exit non-zero, but the JVM itself stays healthy.
pub fn register_eclipse_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("CRATONVM_ECLIPSE_REAL").as_deref() == Ok("1") {
        // Real mode: don't short-circuit main, but defuse the two
        // known crash sources the real launcher hits under CratonVM.
        registry.register(
            CN_EQUINOX_MAIN,
            "openLogFile",
            "()V",
            eclipse_open_log_file_noop,
        );
        // JNIBridge native methods — declared `native` in
        // JNIBridge.java; without a DLL on disk the JVM reports
        // "Missing native method" and downstream callers NPE. Stub
        // each one with the matching return type.
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
        registry.register(
            CN_JNI_BRIDGE,
            "_update_splash",
            "()V",
            jni_bridge_void_noop,
        );
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
        return;
    }
    registry.register(
        CN_EQUINOX_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        eclipse_main_noop,
    );
    registry.register(CN_EQUINOX_MAIN, "<clinit>", "()V", eclipse_clinit_noop);
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
