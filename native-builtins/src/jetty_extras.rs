// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Jetty 11 boot-test shims.
//!
//! The boot test target is `java -jar jetty-home-11.0.20/start.jar`. Under
//! CratonVM's partial bootstrap, Jetty's launcher crashes at
//! `org.eclipse.jetty.start.Main.start(Main.java:397)` with:
//!
//! ```text
//! java.lang.NullPointerException: Cannot invoke getClasspath on null
//!     at org.eclipse.jetty.start.Main.start(Main.java:397)
//! Usage: java -jar $JETTY_HOME/start.jar [options] [properties] [configs]
//! [cratonvm] System.exit(-9) called — process terminating
//! ```
//!
//! The launcher's `Main.start` dereferences `StartArgs.getClasspath()` which
//! returns null because CratonVM's bootstrap leaves some StartArgs state
//! unpopulated. The launcher then prints usage and calls `System.exit(-9)`,
//! and the process actually exits with rc=127 (SIGABRT/aborted) — failing
//! the boot-test acceptance criterion of "no crash".
//!
//! # Strategy
//!
//! Short-circuit `Main.main` so the JVM returns cleanly (rc=0). Jetty
//! doesn't actually run, but the boot-test goal is "no crash" — i.e. the
//! VM must not abort or print an NPE. We also stub `Main.start` (the inner
//! method that NPEs) and provide a defensive `StartArgs.getClasspath`
//! intercept that returns a synthetic empty `Classpath` instance, in case
//! some code path slips past the `Main.main` short-circuit.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! jetty_extras::register_jetty_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes in the `org/eclipse/jetty/start/`
//! launcher package, so they cannot affect non-Jetty workloads. The
//! `Main.main` short-circuit is the standard "boot-test rc=0" pattern used
//! elsewhere in this crate (see `jboss_extras::native_void_noop` under the
//! `CRATONVM_WILDFLY_SHORTCIRCUIT` env gate).
//
// TODO orchestrator: wire `jetty_extras::register_jetty_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

use cratonvm_native_api::NativeMethodRegistry;

// HYGIENE (audit): the dead Jetty short-circuit helpers were deleted —
// `jetty_main_noop`, `jetty_void_noop`, `jetty_start_noop`,
// `jetty_start_args_get_classpath`, `jetty_main_process_command_line`,
// `jetty_return_false`, `jetty_return_null` — along with the
// `CN_MAIN` / `CN_START_ARGS` / `CN_CLASSPATH` consts that only named
// them. They were `#[allow(dead_code)]` and unreferenced (synthetic
// StartArgs/Classpath objects, predicates wired false, getters wired
// null, Main.main / Main.start no-ops) — a latent re-introduction risk
// under the "no synthetic stubs" policy.

/// Install every Jetty boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_jetty_stubs(registry: &mut NativeMethodRegistry) {
    // DISABLED per "no synthetic stubs" policy. Every registration in this
    // module was a fake-out: synthetic empty `StartArgs` / `Classpath`
    // objects, predicates wired to return false, getters wired to return
    // null, and Main.main / Main.start no-ops. These masked the real
    // failure path. The orchestrator wants the first real-bytecode failure
    // surfaced, then dispatches follow-up fix agents.
    let _ = registry;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jetty_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jetty_stubs(&mut r);
    }
}
