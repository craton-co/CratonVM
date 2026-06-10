// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Elasticsearch 8.x boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited the Elasticsearch
//! launcher entry points (`CliToolLauncher`, `server.cli.Elasticsearch`,
//! `bootstrap.Elasticsearch`, `cli.Command`) by registering a fake no-op
//! `main([Ljava/lang/String;)V` plus fake no-op `<clinit>`s on those
//! classes and on the Log4j `LogManager` / `ServiceLoaderUtil` /
//! `ProviderUtil` static-init chain, so the JVM exited rc=0 without
//! running Elasticsearch's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Elasticsearch bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Register ES-specific natives.
///
/// Currently a no-op: the Elasticsearch boot path runs entirely on real
/// bytecode. (An earlier build registered a synthetic
/// `InternalSettingsPreparer.loadOverrides` override to work around
/// `path.home` not being applied, but the real root cause was a CratonVM
/// `HashMap.putAll(Map)` / `new HashMap<>(Map)` bug that dropped entries when
/// the source was a `TreeMap` / `Collections$UnmodifiableNavigableMap` rather
/// than a `HashMap` — fixed generically in `native-collections`
/// (`collect_entries_any`). The synthetic override was removed per the
/// no-synthetic-stubs policy.)
pub fn register_es_stubs(_registry: &mut NativeMethodRegistry) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_es_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_es_stubs(&mut r);
    }
}
