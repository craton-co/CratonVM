//! Apache Spark boot-test shims.
//!
//! Spark's `bin/spark-submit` shell wrapper delegates to two main classes
//! depending on platform: `org.apache.spark.launcher.Main` (the cross-platform
//! launcher) prints the actual `java` command line, and
//! `org.apache.spark.deploy.SparkSubmit.main` is the in-JVM driver entry
//! point. Both pull in Scala's `scala.Predef` / `scala.collection` static
//! initializers which depend on `sun.misc.Unsafe` field offsets that
//! CratonVM cannot fully drive today.
//!
//! # Strategy
//!
//! Short-circuit both `main` entry points and their `<clinit>` so the JVM
//! returns rc=0 without exercising the Scala/Spark bootstrap chain. Boot-
//! test success criterion is "no crash" — a working Spark driver is not
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
//! spark_extras::register_spark_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/apache/spark/`, so
//! they cannot affect non-Spark workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `spark_extras::register_spark_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

#[allow(dead_code)]
const CN_SPARK_SUBMIT: &str = "org/apache/spark/deploy/SparkSubmit";
#[allow(dead_code)]
const CN_LAUNCHER_MAIN: &str = "org/apache/spark/launcher/Main";

/// Generic `main([Ljava/lang/String;)V` no-op for Spark entry points.
#[allow(dead_code)]
fn spark_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[spark-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Spark entry-point classes. The real
/// clinit triggers Scala static initializers which depend on
/// `sun.misc.Unsafe` offsets CratonVM cannot populate.
#[allow(dead_code)]
fn spark_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Spark boot-test short-circuit this module owns.
///
/// Disabled per "no synthetic stubs" policy (matches the round-8 batch shim
/// disable in commit 8071d25). All registrations were pure fake-out returning
/// `Ok(None)` without doing real work; they have been removed so Spark runs
/// against real bytecode.
pub fn register_spark_stubs(registry: &mut NativeMethodRegistry) {
    let _ = registry;
    let _ = CN_SPARK_SUBMIT;
    let _ = CN_LAUNCHER_MAIN;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_spark_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_spark_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_spark_stubs() into lib.rs
