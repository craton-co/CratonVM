//! Arduino IDE 1.8.19 boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `processing.app.Base.main` with a fake no-op `main`, plus a fake
//! no-op `<clinit>`, so the JVM exited rc=0 without running the
//! Arduino IDE's real bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real Arduino IDE bytecode now runs. This file is kept so the call
//! site in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `processing/app/Base.main` and `<clinit>`.
pub fn register_arduino_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real Arduino Base bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_arduino_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_arduino_stubs(&mut r);
    }
}
