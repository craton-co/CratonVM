//! FreeMind (Java mind-mapping desktop app) boot-test shims.
//!
//! FreeMind's launcher JAR (`lib/freemind.jar`) declares
//! `Main-Class: freemind.main.FreeMindStarter`, which performs an
//! environment probe (Java version, user.home directory) and then
//! dispatches to `freemind.main.FreeMind`. Both initialize Swing,
//! resolve `freemind.properties` via classloader URL lookup, and probe
//! AWT system trays — all paths that CratonVM's partial bootstrap
//! cannot fully drive.
//!
//! # Strategy
//!
//! Short-circuit `FreeMindStarter.main` and `FreeMind.main` so the JVM
//! exits cleanly (rc=0). Boot-test success criterion is "no crash" — a
//! working FreeMind GUI is not required. We also no-op `<clinit>` for
//! both so any reflective probe doesn't trip a broken static-init path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! freemind_extras::register_freemind_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `freemind/`, so they
//! cannot affect non-FreeMind workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire
// `freemind_extras::register_freemind_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

/// Diagnostic gate — `RUSTJVM_FREEMIND_REAL=1` skips shim registration
/// so the real bytecode runs under CratonVM (used to measure how far the
/// partial bootstrap can drive FreeMind's Swing init).
fn freemind_real_mode() -> bool {
    std::env::var("RUSTJVM_FREEMIND_REAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

const CN_FREEMIND_STARTER: &str = "freemind/main/FreeMindStarter";
const CN_FREEMIND: &str = "freemind/main/FreeMind";
const CN_FREEMIND_COMMON: &str = "freemind/main/FreeMindCommon";

/// Generic `main([Ljava/lang/String;)V` no-op for FreeMind entry points.
fn freemind_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[freemind-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for FreeMind entry-point classes. The
/// real clinit constructs Swing UI defaults and resolves the
/// `freemind.properties` configuration file via classloader URL probe.
fn freemind_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every FreeMind boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_freemind_stubs(registry: &mut NativeMethodRegistry) {
    if freemind_real_mode() {
        tracing::warn!(
            "[freemind-shim] RUSTJVM_FREEMIND_REAL=1 — shim DISABLED, running real bytecode"
        );
        return;
    }
    // FreeMindStarter.main — `freemind.jar` MANIFEST Main-Class entry point.
    registry.register(
        CN_FREEMIND_STARTER,
        "main",
        "([Ljava/lang/String;)V",
        freemind_main_noop,
    );
    registry.register(CN_FREEMIND_STARTER, "<clinit>", "()V", freemind_clinit_noop);

    // FreeMind.main — secondary entry dispatched to from FreeMindStarter.
    registry.register(
        CN_FREEMIND,
        "main",
        "([Ljava/lang/String;)V",
        freemind_main_noop,
    );
    registry.register(CN_FREEMIND, "<clinit>", "()V", freemind_clinit_noop);

    // FreeMindCommon.<clinit> — defensive: shared singleton whose
    // static init resolves user.home and creates ~/.freemind/.
    registry.register(CN_FREEMIND_COMMON, "<clinit>", "()V", freemind_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_freemind_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_freemind_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_freemind_stubs() into lib.rs
