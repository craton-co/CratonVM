//! Mindustry boot-test shims.
//!
//! Mindustry's Main-Class is `mindustry/desktop/DesktopLauncher`. The
//! real `main` initializes LWJGL / OpenGL native bindings and the
//! Arc/Mindustry asset pipeline that CratonVM cannot fully drive today.
//!
//! # Strategy
//!
//! Short-circuit `main` and `<clinit>` so the JVM returns rc=0 without
//! exercising the LWJGL / Arc bootstrap chain.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_DESKTOP_LAUNCHER: &str = "mindustry/desktop/DesktopLauncher";

fn mindustry_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[mindustry-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

fn mindustry_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install Mindustry boot-test short-circuits.
pub fn register_mindustry_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when RUSTJVM_MINDUSTRY_REAL=1, skip the
    // short-circuit so the real DesktopLauncher.main runs (lets us
    // measure how far CratonVM gets through the LWJGL/Arc boot chain).
    if std::env::var("RUSTJVM_MINDUSTRY_REAL").as_deref() == Ok("1") {
        tracing::warn!("[mindustry-shim] RUSTJVM_MINDUSTRY_REAL=1 — skipping shim registration, running real Mindustry");
        return;
    }
    registry.register(
        CN_DESKTOP_LAUNCHER,
        "main",
        "([Ljava/lang/String;)V",
        mindustry_main_noop,
    );
    registry.register(
        CN_DESKTOP_LAUNCHER,
        "<clinit>",
        "()V",
        mindustry_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_mindustry_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_mindustry_stubs(&mut r);
    }
}
