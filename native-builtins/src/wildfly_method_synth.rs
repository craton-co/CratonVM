//! WildFly / Keycloak-16 boot-test final shim.
//!
//! **HISTORY**: Previously this module short-circuited
//! `org.jboss.modules.Main.main` (the jboss-modules JAR entry point) so
//! that the entire jboss-modules → as-server reflective main-lookup chain
//! was bypassed and the JVM exited rc=0 without ever running WildFly.
//!
//! **CURRENT STATE (real-bytecode audit)**: the short-circuit registration
//! has been REMOVED. CratonVM must run the real `Main.main` bytecode for
//! WildFly / Keycloak-16. This file is kept so that the call site in
//! `lib.rs::register_essential_natives` continues to compile (the
//! `register_wildfly_method_synth_stubs` function is now a no-op).

use rustjvm_native_api::NativeMethodRegistry;

/// Audit cleanup: no longer registers any natives. Previously installed
/// a no-op `org/jboss/modules/Main.main` to fake a clean rc=0 exit.
///
/// Re-enable by setting `RUSTJVM_USE_WILDFLY_MAIN_SHIM=1` only if you
/// need the legacy boot-test (exit-rc-only) behavior. The shim is OFF by
/// default so real bytecode runs.
pub fn register_wildfly_method_synth_stubs(_registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_USE_WILDFLY_MAIN_SHIM").as_deref() == Ok("1") {
        tracing::warn!(
            "[wildfly-synth] RUSTJVM_USE_WILDFLY_MAIN_SHIM=1 set — legacy shim opt-in noted but \
             registration code has been removed in the real-bytecode audit. \
             Set this var has no effect; revert this file from git history \
             if you genuinely need the old behavior."
        );
    }
    // Intentionally empty. Real `org/jboss/modules/Main.main` bytecode runs.
}
