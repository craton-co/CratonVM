//! Sonatype Nexus Repository (OSS) boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.sonatype.nexus.karaf.NexusMain.main` with a fake no-op `main`,
//! plus fake no-op `<clinit>`s on `NexusMain`, `NexusFileLock`, and
//! `NonResettableLogManager`, so the JVM exited rc=0 without running
//! Nexus's real Karaf / Felix OSGi bootstrap bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Nexus bytecode now runs. This file is kept so the call site in
//! `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `NexusMain.main` and the launcher `<clinit>`s.
pub fn register_nexus_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Nexus NexusMain bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_nexus_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_nexus_stubs(&mut r);
    }
}
