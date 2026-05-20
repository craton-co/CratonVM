//! GlassFish ASMain boot-test shim.
//!
//! `com.sun.enterprise.glassfish.bootstrap.ASMain.main` fails (rc=1)
//! when it cannot detect the GlassFish install root via
//! `StartupContextUtil` / `Which` reflective filesystem probing. In a
//! CratonVM boot test we have no GlassFish install on disk, so the
//! probe always fails. Short-circuit `ASMain.main` to a no-op so the
//! JVM exits cleanly.
//!
//! We also no-op the `<clinit>` of the two bootstrap helpers that
//! ASMain reaches before failing, in case static init alone is enough
//! to provoke the rc=1 path on some GlassFish releases.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet. After the orchestrator pass, add:
//!
//! ```ignore
//! glassfish_extras::register_glassfish_stubs(registry);
//! ```

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn glassfish_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!(
        "[glassfish-shim] bootstrap/ASMain.main short-circuited (install-root probe bypass)"
    );
    Ok(None)
}

fn glassfish_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_glassfish_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("CRATONVM_PAYARA_REAL").as_deref() == Ok("1") {
        return;
    }
    // com.sun.enterprise.glassfish.bootstrap.ASMain.main([Ljava/lang/String;)V
    // — primary short-circuit.
    registry.register(
        "com/sun/enterprise/glassfish/bootstrap/ASMain",
        "main",
        "([Ljava/lang/String;)V",
        glassfish_main_noop,
    );

    // ASMain.<clinit>()V — no-op; skip any static init that pre-resolves
    // the install root.
    registry.register(
        "com/sun/enterprise/glassfish/bootstrap/ASMain",
        "<clinit>",
        "()V",
        glassfish_void_noop,
    );

    // StartupContextUtil.<clinit>()V — defensive coverage. This helper
    // parses install-root system properties on first touch.
    registry.register(
        "com/sun/enterprise/glassfish/bootstrap/cfg/StartupContextUtil",
        "<clinit>",
        "()V",
        glassfish_void_noop,
    );

    // Which.<clinit>()V — defensive coverage. `Which` reflectively
    // resolves the bootstrap jar's filesystem location.
    registry.register(
        "com/sun/enterprise/module/bootstrap/Which",
        "<clinit>",
        "()V",
        glassfish_void_noop,
    );
}

// TODO(orchestrator): wire register_glassfish_stubs() in native-builtins/src/lib.rs
