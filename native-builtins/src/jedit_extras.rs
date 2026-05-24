// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! jEdit 5.7.0 installer / editor boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited jEdit's entry
//! points (`installer/Install.main`, `org/gjt/sp/jedit/jEdit.main`)
//! with fake no-op `main([Ljava/lang/String;)V`, and additionally
//! registered fake no-op `<clinit>`s on the AWT graphics-environment
//! chain (`GraphicsEnvironment$LocalGE`, `PlatformGraphicsInfo`,
//! `Win32GraphicsEnvironment`, and — env-gated — `java/awt/Component`),
//! so the JVM exited rc=0 without running jEdit's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real jEdit bytecode now runs. This file is kept so the call site in
//! `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited jEdit's `main` entry points and the AWT graphics-env
/// `<clinit>` chain.
pub fn register_jedit_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real jEdit Install bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jedit_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jedit_stubs(&mut r);
    }
}
