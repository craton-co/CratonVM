//! Apache NetBeans boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited the NetBeans
//! launcher entry classes (`org/netbeans/Main`, `org/netbeans/MainImpl`)
//! with fake no-op `main([Ljava/lang/String;)V` plus fake no-op
//! `<clinit>`s, so the JVM exited rc=0 without running NetBeans's real
//! module-bootstrap bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED per the "no synthetic stubs" policy.
//! Real NetBeans bytecode now runs. This file is kept so the call site
//! in `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited `Main.main` / `<clinit>` and `MainImpl.main` / `<clinit>`.
pub fn register_netbeans_stubs(_registry: &mut NativeMethodRegistry) {
    // Intentionally empty. Real NetBeans Main bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_netbeans_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_netbeans_stubs(&mut r);
    }
}
