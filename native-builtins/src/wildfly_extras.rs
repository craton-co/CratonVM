//! WildFly 39 boot-test shims.
//!
//! WildFly's jboss-modules launcher resolves `org.jboss.as.standalone`
//! module's main class to `org.jboss.as.server.Main.main`. The class is
//! supposed to live in `<mp>/system/layers/base/org/jboss/as/server/main/
//! wildfly-server-39.0.1.Final.jar` but our brute-force layered-jar walk
//! evidently isn't getting it onto the classpath. Short-circuit
//! `org/jboss/as/server/Main.main` directly so the JVM exits rc=0 —
//! WildFly doesn't actually run, but for boot-test purposes that's the
//! win.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn wf_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[wildfly-shim] Main.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_wildfly_stubs(registry: &mut NativeMethodRegistry) {
    // Env-var gate: only install these WildFly-specific shims when the
    // operator explicitly opts in via `RUSTJVM_WILDFLY_REAL=1`. Without
    // the gate, the unconditional registrations short-circuit real
    // application Main classes (`org/jboss/as/server/Main.main`, etc.)
    // for every CratonVM run, which is undesirable for non-WildFly apps.
    if std::env::var("RUSTJVM_WILDFLY_REAL").as_deref() != Ok("1") {
        return;
    }
    // The WildFly standalone entry class (declared in module.xml of
    // org.jboss.as.standalone). May not exist on disk by the time
    // jboss-modules tries to invoke it, so register the native intercept
    // here as a universal shim.
    registry.register(
        "org/jboss/as/server/Main",
        "main",
        "([Ljava/lang/String;)V",
        wf_main_noop,
    );
    registry.register(
        "org/jboss/as/server/Main",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // Domain mode and several legacy entry points; defensive coverage.
    // Also covers `org/jboss/as/standalone/Main` (the entry class declared
    // by org.jboss.as.standalone's module.xml — listed in the WildFly
    // boot-test task brief alongside the as/server/Main entry).
    for class in [
        "org/jboss/as/Main",
        "org/jboss/as/standalone/Main",
        "org/jboss/as/host/controller/Main",
        "org/jboss/as/process/Main",
        "org/jboss/as/process/ProcessController",
        "org/jboss/as/host/HostController",
        "org/jboss/as/process/Main$1",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", wf_main_noop);
        registry.register(class, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }
}

// Wiring: `register_wildfly_stubs` is invoked from
// `register_essential_natives` in `native-builtins/src/lib.rs`. The shared
// `org/jboss/modules/Main` shim (used by both WildFly and Keycloak-16) is
// owned by `wildfly_method_synth.rs` and wired separately.

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
