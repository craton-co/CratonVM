//! Neo4j Community Edition boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited Neo4j's entry
//! points (`org/neo4j/server/CommunityEntryPoint`,
//! `org/neo4j/server/startup/NeoBootstrapper`) with a fake no-op
//! `main([Ljava/lang/String;)V` plus a fake no-op `<clinit>`, so the
//! JVM exited rc=0 without running Neo4j's real server bootstrap
//! bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Neo4j bytecode now runs. This file is kept so the call site in
//! `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the Neo4j `main` / `<clinit>` entry points.
pub fn register_neo4j_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Neo4j CommunityEntryPoint bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_neo4j_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_neo4j_stubs(&mut r);
    }
}
