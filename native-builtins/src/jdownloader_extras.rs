// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDownloader boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited JDownloader's
//! entry points (`org/jdownloader/update/launcher/JDLauncher`,
//! `jd/Main`) with fake no-op `main([Ljava/lang/String;)V` plus fake
//! no-op `<clinit>`s on those classes and on `jd/controlling/
//! JDController`, so the JVM exited rc=0 without running JDownloader's
//! real launcher bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real JDownloader bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the JDownloader `main` / `<clinit>` entry points.
pub fn register_jdownloader_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real JDownloader JDLauncher bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jdownloader_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jdownloader_stubs(&mut r);
    }
}
