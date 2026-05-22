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

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the Elasticsearch launcher `main` / `<clinit>` and the
/// Log4j static-init chain.
pub fn register_es_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Elasticsearch launcher bytecode runs.
}

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
