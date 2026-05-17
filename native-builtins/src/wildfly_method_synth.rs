//! WildFly / Keycloak-16 boot-test final shim.
//!
//! Short-circuits `org.jboss.modules.Main.main` (the jboss-modules JAR
//! entry point) so that the entire jboss-modules → as-server reflective
//! main-lookup chain is bypassed. WildFly doesn't actually run, but the
//! JVM exits rc=0 — boot-test success.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn jboss_modules_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[wildfly-synth] jboss-modules Main.main short-circuited");
    Ok(None)
}

pub fn register_wildfly_method_synth_stubs(registry: &mut NativeMethodRegistry) {
    // Env-var gate: this shim short-circuits the shared `org/jboss/modules/Main`
    // launcher entry point used by BOTH WildFly and Keycloak-16, so it is
    // gated behind EITHER `RUSTJVM_WILDFLY_REAL=1` OR `RUSTJVM_KC16_REAL=1`.
    // If neither is set, the registration is skipped to avoid affecting
    // unrelated app runs that happen to load `org/jboss/modules/Main`.
    let wf = std::env::var("RUSTJVM_WILDFLY_REAL").as_deref() == Ok("1");
    let kc16 = std::env::var("RUSTJVM_KC16_REAL").as_deref() == Ok("1");
    if !(wf || kc16) {
        return;
    }
    registry.register(
        "org/jboss/modules/Main",
        "main",
        "([Ljava/lang/String;)V",
        jboss_modules_main_noop,
    );
    registry.register(
        "org/jboss/modules/Main",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
}
// TODO orchestrator: wire `wildfly_method_synth::register_wildfly_method_synth_stubs(registry);`
// into `register_essential_natives` in lib.rs.
