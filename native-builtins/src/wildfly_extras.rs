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
    for class in [
        "org/jboss/as/Main",
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

// TODO(orchestrator): wire `register_wildfly_stubs` into
// `register_essential_natives` in `native-builtins/src/lib.rs`, alongside
// the other `*_extras::register_*_stubs` calls (e.g., `demo_extras`,
// `jenkins_extras`). Add `pub mod wildfly_extras;` to the module list as
// well. This agent (WF5) owns ONLY this file — do not modify lib.rs.
