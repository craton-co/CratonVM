//! Jenkins LTS 2.452.3 boot-test shim.
//!
//! **HISTORY**: Previously this module short-circuited the Winstone
//! launcher's `executable.Main.main` (plus a defensive no-op `<clinit>`)
//! so the JVM exited rc=0 instead of running the real Java-version
//! detection in Jenkins's bootstrap.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Jenkins / Winstone bytecode now runs. This file is kept so the
//! call site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `executable/Main.main` and `<clinit>`.
pub fn register_jenkins_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Winstone Main bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_jenkins_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jenkins_stubs(&mut r);
    }
}
