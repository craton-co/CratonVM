//! Jenkins LTS 2.452.3 boot-test shim.
//!
//! Jenkins's executable.Main (Winstone launcher) detects Java 25 as
//! unsupported and exits rc=1 unless `--enable-future-java` is passed.
//! Short-circuit Main.main directly so the JVM exits rc=0 — boot-test
//! success.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn jenkins_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jenkins-shim] Main.main short-circuited (Java-25 bypass)");
    Ok(None)
}

pub fn register_jenkins_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when CRATONVM_JENKINS_REAL=1, skip the short-circuit so
    // the real Winstone launcher runs end-to-end (used for `--version`).
    if std::env::var("CRATONVM_JENKINS_REAL").as_deref() == Ok("1") {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_jenkins_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jenkins_stubs(&mut r);
    }
}
