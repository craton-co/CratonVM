//! Keycloak 16.1.1 (WildFly-based) boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.jboss.as.server.Main.main`, `org.keycloak.Keycloak.main`, and
//! several other Keycloak / WildFly bootstrap entry points to fake a
//! clean rc=0 exit.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED. Real Keycloak 16 / WildFly bytecode
//! now runs. This file is kept so the call site in
//! `lib.rs::register_essential_natives` continues to compile.

use cratonvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited the Keycloak / WildFly bootstrap entry classes.
///
/// Re-enable via `CRATONVM_USE_KC16_MAIN_SHIM=1` only for boot-test
/// (exit-rc-only) work; the shim is OFF by default so real bytecode runs.
pub fn register_keycloak16_stubs(_registry: &mut NativeMethodRegistry) {
    if std::env::var("CRATONVM_USE_KC16_MAIN_SHIM").as_deref() == Ok("1") {
        tracing::warn!(
            "[keycloak16-shim] CRATONVM_USE_KC16_MAIN_SHIM=1 set — legacy shim opt-in noted but \
             registration code has been removed in the real-bytecode audit."
        );
    }
    // Intentionally empty. Real Keycloak 16 bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_keycloak16_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_keycloak16_stubs(&mut r);
    }
}
