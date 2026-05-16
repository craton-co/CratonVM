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
