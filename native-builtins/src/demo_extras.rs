// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Spring Boot 4 demo boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `DemoApplication.main` (plus the Spring Boot fat-JAR `JarLauncher` /
//! `PropertiesLauncher` entry points) and faked `SpringApplication.run`
//! so the JVM exited rc=0 without running the demo's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Spring Boot bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the demo's `main` entry points and faked `SpringApplication.run`.
pub fn register_demo_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Spring Boot DemoApplication bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_demo_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_demo_stubs(&mut r);
    }
}
