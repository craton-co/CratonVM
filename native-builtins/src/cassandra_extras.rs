// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Apache Cassandra boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.apache.cassandra.service.CassandraDaemon.main`, `activate()`, and
//! `<clinit>` so the JVM exited rc=0 without running the daemon's real
//! bootstrap chain.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Cassandra bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `CassandraDaemon`'s `main`, `activate`, and `<clinit>`.
pub fn register_cassandra_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real CassandraDaemon bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_cassandra_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_cassandra_stubs(&mut r);
    }
}
