// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ByteBuddy boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited the
//! `ByteBuddyProbe.main` entry point (default-package and
//! `org/test/ByteBuddyProbe` variants) with a fake no-op `main`, plus
//! fake no-op `<clinit>`s on the ByteBuddy internal classes
//! (`ByteBuddy`, `TypePool$Default$Resolution`,
//! `TypeDescription$Generic$LazyProjection`,
//! `MethodGraph$Compiler$Default`, `JavaDispatcher`) so the probe
//! exited rc=0 without running ByteBuddy's real bytecode-generation
//! path.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real ByteBuddy bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `ByteBuddyProbe.main` and the ByteBuddy internal
/// class-init chain.
pub fn register_bytebuddy_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real ByteBuddy bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and does not panic.
    #[test]
    fn register_bytebuddy_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_bytebuddy_stubs(&mut r);
    }
}
