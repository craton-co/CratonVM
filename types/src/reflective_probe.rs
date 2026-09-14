// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Thread-local marker for "currently inside a reflective `Class.forName` /
//! `ClassUtils.isPresent` existence probe".
//!
//! # Why this exists (BUG-06)
//!
//! CratonVM fabricates *synthetic stub* classes for a set of enterprise-
//! framework package prefixes (`org/jboss/`, `io/smallrye/`, `io/quarkus/`,
//! `io/undertow/`, …) whenever a class under one of those prefixes is requested
//! but is not actually on the classpath. That fallback exists so WildFly /
//! Quarkus *bytecode* can link against types CratonVM handles natively or that
//! are genuinely optional.
//!
//! Those stubs must **not** satisfy a *reflective existence probe*. On a real
//! JVM `Class.forName("io.smallrye.mutiny.Multi", false, loader)` throws
//! `ClassNotFoundException` when the class is absent, and Spring's
//! `ReactiveAdapterRegistry` uses exactly such a probe (via
//! `ClassUtils.isPresent`) to decide whether to register its `MutinyRegistrar`.
//! When the stub makes the probe falsely succeed, `MutinyRegistrar.<clinit>`
//! runs and then dies looking up a method the stub does not have — an
//! `ExceptionInInitializerError` that cascades across ~20 reactive-messaging /
//! RSocket test classes.
//!
//! The `Class.forName` native wraps its class-resolution in a [`ProbeGuard`];
//! the class loader consults [`active`] before fabricating an enterprise-prefix
//! stub and instead reports the class as absent (CNFE) — matching HotSpot.
//! Genuine bytecode linkage never sets the flag, so WildFly/Quarkus stub
//! creation is unaffected.

use std::cell::Cell;

thread_local! {
    /// Re-entrant depth counter: nonzero means the current thread is resolving
    /// a class on behalf of a reflective `Class.forName` / `isPresent` probe.
    static PROBE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Increment the reflective-probe depth for the current thread. Prefer
/// [`ProbeGuard`] so the matching [`exit`] cannot be skipped on an early
/// return.
pub fn enter() {
    PROBE_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
}

/// Decrement the reflective-probe depth for the current thread.
pub fn exit() {
    PROBE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
}

/// True when the current thread is inside a reflective `Class.forName` /
/// `isPresent` probe.
pub fn active() -> bool {
    PROBE_DEPTH.with(|c| c.get() > 0)
}

/// RAII guard that marks the current thread as inside a reflective class-
/// existence probe for its lifetime. Re-entrant: nested guards stack via a
/// depth counter, so a `Class.forName` that triggers further reflective loads
/// stays correctly flagged until the outermost guard drops.
#[must_use = "the probe flag is cleared when the guard is dropped"]
pub struct ProbeGuard(());

impl ProbeGuard {
    /// Enter a reflective-probe scope; the flag clears when the returned guard
    /// is dropped (including on early return / unwind).
    pub fn new() -> Self {
        enter();
        ProbeGuard(())
    }
}

impl Default for ProbeGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        exit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_by_default() {
        assert!(!active());
    }

    #[test]
    fn guard_sets_and_clears() {
        assert!(!active());
        {
            let _g = ProbeGuard::new();
            assert!(active());
        }
        assert!(!active());
    }

    #[test]
    fn reentrant_depth() {
        assert!(!active());
        let outer = ProbeGuard::new();
        assert!(active());
        {
            let _inner = ProbeGuard::new();
            assert!(active());
        }
        // Still active after inner drops — outer still holds the scope.
        assert!(active());
        drop(outer);
        assert!(!active());
    }
}
