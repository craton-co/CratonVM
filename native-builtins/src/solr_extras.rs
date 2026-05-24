// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Apache Solr 9.4.1 boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited `main` and
//! `<clinit>` on the Solr CLI classes (`SolrCLI`, `CoreContainer`,
//! `SolrDispatchFilter`, `StartCommand`, `StopCommand`, `SolrLogPostTool`)
//! so the JVM exited rc=0 without running Solr's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Solr bytecode now runs. This file is kept so the call site in
//! `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `main`/`<clinit>` on Solr's CLI entry classes.
pub fn register_solr_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Solr SolrCLI bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_solr_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_solr_stubs(&mut r);
    }
}
