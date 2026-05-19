//! WildFly 39 boot-test shims.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.jboss.as.server.Main.main` (plus several legacy / domain-mode
//! entry points) so the JVM exited rc=0 without running WildFly bytecode.
//!
//! **CURRENT STATE (real-bytecode audit)**: every short-circuit
//! registration has been REMOVED. Real WildFly bytecode now runs. This
//! file is kept so the call site in `lib.rs::register_essential_natives`
//! continues to compile.

use rustjvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously short-
/// circuited every WildFly bootstrap entry-class `main(String[])`.
///
/// Re-enable via `RUSTJVM_USE_WILDFLY_MAIN_SHIM=1` only for boot-test
/// (exit-rc-only) work; the shim is OFF by default so real bytecode runs.
pub fn register_wildfly_stubs(_registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_USE_WILDFLY_MAIN_SHIM").as_deref() == Ok("1") {
        tracing::warn!(
            "[wildfly-shim] RUSTJVM_USE_WILDFLY_MAIN_SHIM=1 set — legacy shim opt-in noted but \
             registration code has been removed in the real-bytecode audit."
        );
    }
    // Intentionally empty. Real WildFly Main.main bytecode runs.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_wildfly_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_wildfly_stubs(&mut r);
    }
}
