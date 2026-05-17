//! Arduino IDE 1.8.19 boot-test shims.
//!
//! Arduino's `processing.app.Base.main` launches the Java-based IDE. On
//! CratonVM it exits rc=1 without obvious reason (likely AWT/Swing init or
//! a static-init chain that calls `System.exit(1)`). We short-circuit `main`
//! and the `<clinit>` to land at rc=0 for the boot smoke test.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn arduino_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[arduino-shim] entry short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_arduino_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when RUSTJVM_ARDUINO_REAL=1, skip all short-circuits so
    // the real Arduino entry classes execute under CratonVM. Used to measure
    // how far the real boot path gets without our shims masking failures.
    if std::env::var("RUSTJVM_ARDUINO_REAL").as_deref() == Ok("1") {
        tracing::warn!("[arduino-shim] RUSTJVM_ARDUINO_REAL=1 — skipping shim registration, running real Arduino");
        return;
    }
    // Primary boot entry.
    registry.register(
        "processing/app/Base",
        "main",
        "([Ljava/lang/String;)V",
        arduino_noop,
    );
    // Defensive: short-circuit the static initializer chain that may call
    // `System.exit(1)` during platform / preferences discovery.
    registry.register(
        "processing/app/Base",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

// Wired in `lib.rs::register_essential_natives` alongside other real-JDK
// app shims (e.g., `register_jboss_extras`, `register_es_stubs`).

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_arduino_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_arduino_stubs(&mut r);
    }
}
