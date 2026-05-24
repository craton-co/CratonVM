// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Mindustry boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `mindustry/desktop/DesktopLauncher.main` with a fake no-op `main`,
//! plus a fake no-op `<clinit>`, so the JVM exited rc=0 without running
//! Mindustry's real LWJGL / Arc bootstrap bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Mindustry bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `DesktopLauncher.main` and `<clinit>`.
pub fn register_mindustry_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Mindustry DesktopLauncher bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_mindustry_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_mindustry_stubs(&mut r);
    }
}
