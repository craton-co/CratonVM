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

#[allow(dead_code)]
const CN_CLI_FRONTEND: &str = "org/apache/flink/client/cli/CliFrontend";
#[allow(dead_code)]
const CN_STANDALONE_ENTRYPOINT: &str =
    "org/apache/flink/runtime/entrypoint/StandaloneSessionClusterEntrypoint";

/// Generic `main([Ljava/lang/String;)V` no-op for Flink entry points.
#[allow(dead_code)]
fn flink_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[flink-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Flink entry-point classes. The real
/// clinit triggers Akka / Kryo / Scala static initializers which depend
/// on `sun.misc.Unsafe` offsets CratonVM cannot populate.
#[allow(dead_code)]
fn flink_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Flink boot-test short-circuit this module owns.
///
/// Disabled per "no synthetic stubs" policy (matches the round-8 batch shim
/// disable in commit 8071d25). All registrations were pure fake-out returning
/// `Ok(None)` without doing real work; they have been removed so Flink runs
/// against real bytecode.
pub fn register_flink_stubs(registry: &mut NativeMethodRegistry) {
    let _ = registry;
    let _ = CN_CLI_FRONTEND;
    let _ = CN_STANDALONE_ENTRYPOINT;
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
