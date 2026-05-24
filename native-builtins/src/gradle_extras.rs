// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Gradle launcher boot-test shim.
//!
//! **HISTORY**: Previously this module short-circuited the Gradle
//! launcher entry points (`org.gradle.launcher.GradleMain`,
//! `org.gradle.launcher.Main`, `org.gradle.launcher.bootstrap.
//! EntryPoint`) with fake no-op `main([Ljava/lang/String;)V` plus fake
//! no-op `<clinit>`s, so the JVM exited rc=0 without running Gradle's
//! real launcher bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Gradle launcher bytecode now runs. This file is kept so the
//! call site in `lib.rs::register_essential_natives` continues to
//! compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the Gradle launcher `main` / `<clinit>` entry points.
pub fn register_gradle_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Gradle launcher bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_gradle_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_gradle_stubs(&mut r);
    }
}
