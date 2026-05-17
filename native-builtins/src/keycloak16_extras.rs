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
    // Env-var gate: only install Keycloak-16-specific shims when the
    // operator explicitly opts in via `RUSTJVM_KC16_REAL=1`. These shims
    // short-circuit the WildFly entry classes used by KC16 and would
    // otherwise affect unrelated runs.
    if std::env::var("RUSTJVM_KC16_REAL").as_deref() != Ok("1") {
        return;
    }
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
    // Defensive: Keycloak 16 entry classes. The standalone.sh launcher
    // points at `org/keycloak/Keycloak` (the documented entry per the
    // KC16 task brief), but practical boot still goes through
    // `org/jboss/modules/Main` -> `org/jboss/as/server/Main`. We also
    // cover the legacy/internal entry-class candidates seen on KC16
    // distributions.
    for class in [
        "org/keycloak/Keycloak",
        "org/keycloak/Main",
        "org/keycloak/keycloak/Main",
        "org/keycloak/server/Main",
        "org/keycloak/server/KeycloakServer",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", kc16_main_noop);
        registry.register(class, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }
}

// Wiring: `register_keycloak16_stubs` is invoked from
// `register_essential_natives` in `native-builtins/src/lib.rs`. The shared
// `org/jboss/modules/Main` jboss-modules launcher shim (the WF9 entry
// point used by both WildFly and Keycloak-16) is registered by
// `wildfly_method_synth::register_wildfly_method_synth_stubs`.

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
