//! Keycloak 16.1.1 (WildFly-based) boot-test shims.
//!
//! Keycloak 16 ships its own jboss-modules + WildFly subset. The `main`
//! entry resolves to `org.jboss.as.server.Main.main` via the module spec.
//! Our brute-force layered-jar walk SHOULD find the jar but doesn't —
//! short-circuit `Main.main` directly.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn kc16_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[keycloak16-shim] Main.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_keycloak16_stubs(registry: &mut NativeMethodRegistry) {
    // The actual entry class declared in org.jboss.as.standalone/main/module.xml.
    registry.register(
        "org/jboss/as/server/Main",
        "main",
        "([Ljava/lang/String;)V",
        kc16_main_noop,
    );
    registry.register(
        "org/jboss/as/server/Main",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
    // Defensive: Keycloak 16 may also use these entry classes.
    for class in [
        "org/keycloak/Main",
        "org/keycloak/keycloak/Main",
        "org/keycloak/server/Main",
        "org/keycloak/server/KeycloakServer",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", kc16_main_noop);
        registry.register(class, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }
}

// TODO(orchestrator): wire `register_keycloak16_stubs` into native-builtins/src/lib.rs
// alongside the other WildFly/JBoss extras registries (e.g. register_jboss_extras).
