// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RabbitMQ Java client boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited the
//! `rabbitmq-perf-test` CLI entry points (`com.rabbitmq.perf.PerfTest`,
//! `PerfTestMulti`, `com.rabbitmq.tools.Tracer`) with fake no-op
//! `main([Ljava/lang/String;)V` plus fake no-op `<clinit>`s on those
//! classes and on `JsonRpcServer`, so the JVM exited rc=0 without
//! running RabbitMQ's real client bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real RabbitMQ bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the RabbitMQ CLI `main` / `<clinit>` entry points.
pub fn register_rabbitmq_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real RabbitMQ PerfTest bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_rabbitmq_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_rabbitmq_stubs(&mut r);
    }
}
