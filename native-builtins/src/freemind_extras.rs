// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! FreeMind (Java mind-mapping desktop app) boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited FreeMind's entry
//! points (`freemind/main/FreeMindStarter`, `freemind/main/FreeMind`)
//! with fake no-op `main([Ljava/lang/String;)V` plus fake no-op
//! `<clinit>`s on those classes and on `FreeMindCommon`, so the JVM
//! exited rc=0 without running FreeMind's real Swing bootstrap bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real FreeMind bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the FreeMind `main` / `<clinit>` entry points.
pub fn register_freemind_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real FreeMind FreeMindStarter bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_freemind_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_freemind_stubs(&mut r);
    }
}
