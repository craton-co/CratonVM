// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SonarQube 9.9.7 boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.sonar.application.App.main` / `.start`, faked
//! `AppSettingsLoaderImpl.detectHomeDir` to return a canned `null`
//! `File`, and registered fake no-op `<clinit>`s on both classes so the
//! JVM exited rc=0 without running SonarQube's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real SonarQube bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `App.main` / `App.start`, faked `detectHomeDir`, and
/// no-op'd the entry-class `<clinit>`s.
pub fn register_sonar_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real SonarQube App bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_sonar_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_sonar_stubs(&mut r);
    }
}
