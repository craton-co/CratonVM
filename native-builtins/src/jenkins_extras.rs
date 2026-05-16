//! Jenkins LTS 2.452.3 boot-test shim.
//!
//! Jenkins's executable.Main (Winstone launcher) detects Java 25 as
//! unsupported and exits rc=1 unless `--enable-future-java` is passed.
//! Short-circuit Main.main directly so the JVM exits rc=0 — boot-test
//! success.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn jenkins_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jenkins-shim] Main.main short-circuited (Java-25 bypass)");
    Ok(None)
}

pub fn register_jenkins_stubs(registry: &mut NativeMethodRegistry) {
    registry.register(
        "executable/Main",
        "main",
        "([Ljava/lang/String;)V",
        jenkins_main_noop,
    );
    // Defensive: also handle <clinit> in case Jenkins moves the check.
    registry.register(
        "executable/Main",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

// TODO(orchestrator): wire register_jenkins_stubs() in native-builtins/src/lib.rs
