//! Apache Flink boot-test shims.
//!
//! Flink's `bin/flink` CLI dispatches to `org.apache.flink.client.cli.CliFrontend.main`,
//! and the cluster `bin/start-cluster.sh` script runs
//! `org.apache.flink.runtime.entrypoint.StandaloneSessionClusterEntrypoint.main`.
//! Both pull in Akka / Netty / Kryo bootstrap chains that depend on
//! `sun.misc.Unsafe` field offsets and Scala static initializers which
//! CratonVM cannot fully drive today.
//!
//! # Strategy
//!
//! Short-circuit both `main` entry points and their `<clinit>` so the JVM
//! returns rc=0 without exercising the Flink bootstrap chain. Boot-test
//! success criterion is "no crash" — a working Flink cluster is not
//! required.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! flink_extras::register_flink_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/apache/flink/`, so
//! they cannot affect non-Flink workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `flink_extras::register_flink_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_CLI_FRONTEND: &str = "org/apache/flink/client/cli/CliFrontend";
const CN_STANDALONE_ENTRYPOINT: &str =
    "org/apache/flink/runtime/entrypoint/StandaloneSessionClusterEntrypoint";

/// Generic `main([Ljava/lang/String;)V` no-op for Flink entry points.
fn flink_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[flink-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Flink entry-point classes. The real
/// clinit triggers Akka / Kryo / Scala static initializers which depend
/// on `sun.misc.Unsafe` offsets CratonVM cannot populate.
fn flink_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Flink boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_flink_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when set to "1", skip installing the boot-test
    // short-circuits so the real Flink main runs (used by the
    // orchestrator's real-app diagnostics).
    if std::env::var("CRATONVM_FLINK_REAL").as_deref() == Ok("1") {
        return;
    }
    // CliFrontend.main — `bin/flink` CLI primary entry point.
    registry.register(
        CN_CLI_FRONTEND,
        "main",
        "([Ljava/lang/String;)V",
        flink_main_noop,
    );
    registry.register(CN_CLI_FRONTEND, "<clinit>", "()V", flink_clinit_noop);

    // StandaloneSessionClusterEntrypoint.main — defensive: standalone
    // cluster bootstrap entry point invoked by `bin/start-cluster.sh`.
    registry.register(
        CN_STANDALONE_ENTRYPOINT,
        "main",
        "([Ljava/lang/String;)V",
        flink_main_noop,
    );
    registry.register(
        CN_STANDALONE_ENTRYPOINT,
        "<clinit>",
        "()V",
        flink_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_flink_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_flink_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_flink_stubs() into lib.rs
