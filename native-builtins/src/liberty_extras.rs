//! Open Liberty (WLP) boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited the Liberty
//! kernel-boot entry classes (`EnvCheck`, `Launcher`, `UtilityMain`)
//! with fake no-op `main([Ljava/lang/String;)V`, faked
//! `Launcher.createPlatform` to return a canned `null`, and registered
//! fake no-op `<clinit>`s on each entry class, so the JVM exited rc=0
//! without running Liberty's real kernel-boot bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Open Liberty bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the Liberty `main` / `createPlatform` / `<clinit>` entry
/// points.
pub fn register_liberty_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Open Liberty Launcher bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_liberty_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_liberty_stubs(&mut r);
    }
}
