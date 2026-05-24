// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! GlassFish ASMain boot-test shim.
//!
//! **HISTORY**: Previously this module short-circuited
//! `com.sun.enterprise.glassfish.bootstrap.ASMain.main` with a fake
//! no-op `main`, plus fake no-op `<clinit>`s on `ASMain`,
//! `StartupContextUtil`, and `Which`, so the JVM exited rc=0 without
//! running GlassFish's real install-root detection bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real GlassFish bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `ASMain.main` and the bootstrap helper `<clinit>`s.
pub fn register_glassfish_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real GlassFish ASMain bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_glassfish_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_glassfish_stubs(&mut r);
    }
}
