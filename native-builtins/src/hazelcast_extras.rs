// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::NativeMethodRegistry;

// HYGIENE (audit): the dead `hazelcast_main_noop` / `hazelcast_clinit_noop`
// fake-main / fake-<clinit> short-circuit helpers (and the
// `CN_MEMBER_STARTER` / `CN_CLUSTER` consts that only named them) were
// deleted. They were `#[allow(dead_code)]` and unreferenced — a latent
// re-introduction risk under the "no synthetic stubs" policy.

/// Install every Hazelcast boot-test short-circuit this module owns.
///
/// Disabled per "no synthetic stubs" policy (matches the round-8 batch shim
/// disable in commit 8071d25 for Felix/Jetty/ActiveMQ/Hadoop/HBase). All
/// registrations were pure fake-out returning `Ok(None)` without doing real
/// work; they have been removed so Hazelcast runs against real bytecode.
pub fn register_hazelcast_stubs(registry: &mut NativeMethodRegistry) {
    let _ = registry;
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
