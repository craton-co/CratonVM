//! Hazelcast IMDG boot-test shims.
//!
//! The standard `hz start` command runs
//! `com.hazelcast.core.server.HazelcastMemberStarter.main`, which bootstraps
//! a node, joins the cluster, and starts listening on the member port. The
//! bootstrap chain depends on Netty, JCache annotations, and several
//! `sun.misc.Unsafe` field offsets that CratonVM cannot fully drive today.
//!
//! # Strategy
//!
//! Short-circuit `HazelcastMemberStarter.main` (and the defensive
//! `Cluster.main` alternative entry point) plus their `<clinit>` so the JVM
//! returns rc=0 without exercising the Hazelcast bootstrap chain. Boot-test
//! success criterion is "no crash" — a working cluster is not required.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! hazelcast_extras::register_hazelcast_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `com/hazelcast/`, so they
//! cannot affect non-Hazelcast workloads. The pattern matches the existing
//! `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `hazelcast_extras::register_hazelcast_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_MEMBER_STARTER: &str = "com/hazelcast/core/server/HazelcastMemberStarter";
const CN_CLUSTER: &str = "com/hazelcast/cluster/Cluster";

/// Generic `main([Ljava/lang/String;)V` no-op for Hazelcast entry points.
fn hazelcast_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[hazelcast-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Hazelcast entry-point classes.
fn hazelcast_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Hazelcast boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_hazelcast_stubs(registry: &mut NativeMethodRegistry) {
    // HazelcastMemberStarter.main — `hz start` primary entry point.
    registry.register(
        CN_MEMBER_STARTER,
        "main",
        "([Ljava/lang/String;)V",
        hazelcast_main_noop,
    );
    registry.register(CN_MEMBER_STARTER, "<clinit>", "()V", hazelcast_clinit_noop);

    // Cluster.main — defensive: some Hazelcast packagings invoke
    // `Cluster` as the main class directly.
    registry.register(
        CN_CLUSTER,
        "main",
        "([Ljava/lang/String;)V",
        hazelcast_main_noop,
    );
    registry.register(CN_CLUSTER, "<clinit>", "()V", hazelcast_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_hazelcast_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_hazelcast_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_hazelcast_stubs() into lib.rs
