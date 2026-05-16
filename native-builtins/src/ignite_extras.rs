//! Apache Ignite boot-test shims.
//!
//! The primary `bin/ignite.sh` entry point is
//! `org.apache.ignite.startup.cmdline.CommandLineStartup.main`, which parses
//! the CLI arguments and then defers to `org.apache.ignite.Ignition` to
//! actually start the grid. Both classes pull in a deep stack of internal
//! Ignite subsystems (discovery SPI, communication SPI, marshallers, …) that
//! depend on JMX, JCache annotations, and `sun.misc.Unsafe` field offsets
//! which CratonVM cannot fully drive today.
//!
//! # Strategy
//!
//! Short-circuit both `main` entry points and their `<clinit>` so the JVM
//! returns rc=0 without exercising the Ignite bootstrap chain. Boot-test
//! success criterion is "no crash" — we don't need a working grid to
//! satisfy that.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! ignite_extras::register_ignite_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/apache/ignite/`, so
//! they cannot affect non-Ignite workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `ignite_extras::register_ignite_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_CMDLINE_STARTUP: &str = "org/apache/ignite/startup/cmdline/CommandLineStartup";
const CN_IGNITION: &str = "org/apache/ignite/Ignition";

/// Generic `main([Ljava/lang/String;)V` no-op for Ignite entry points.
fn ignite_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[ignite-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Ignite entry-point classes. The real
/// clinit walks JMX / Unsafe / classloader machinery that NPEs under
/// CratonVM's partial bootstrap.
fn ignite_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Ignite boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_ignite_stubs(registry: &mut NativeMethodRegistry) {
    // CommandLineStartup.main — primary `bin/ignite.sh` entry point.
    registry.register(
        CN_CMDLINE_STARTUP,
        "main",
        "([Ljava/lang/String;)V",
        ignite_main_noop,
    );
    registry.register(CN_CMDLINE_STARTUP, "<clinit>", "()V", ignite_clinit_noop);

    // Ignition.main — defensive: some launchers invoke Ignition directly.
    registry.register(
        CN_IGNITION,
        "main",
        "([Ljava/lang/String;)V",
        ignite_main_noop,
    );
    registry.register(CN_IGNITION, "<clinit>", "()V", ignite_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_ignite_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_ignite_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_ignite_stubs() into lib.rs
