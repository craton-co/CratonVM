// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::NativeMethodRegistry;

// HYGIENE (audit): the dead `ignite_main_noop` / `ignite_clinit_noop`
// fake-main / fake-<clinit> short-circuit helpers (and the
// `CN_CMDLINE_STARTUP` / `CN_IGNITION` consts that only named them) were
// deleted. They were `#[allow(dead_code)]` and unreferenced — a latent
// re-introduction risk under the "no synthetic stubs" policy.

/// Install every Ignite boot-test short-circuit this module owns.
///
/// Disabled per "no synthetic stubs" policy (matches the round-8 batch shim
/// disable in commit 8071d25). All registrations were pure fake-out returning
/// `Ok(None)` without doing real work; they have been removed so Ignite runs
/// against real bytecode.
pub fn register_ignite_stubs(registry: &mut NativeMethodRegistry) {
    let _ = registry;
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
