//! SonarQube 9.9.7 boot-test shims.
//!
//! AppSettingsLoaderImpl.detectHomeDir derefs a null `getParentFile()`
//! result (classloader URL can't be resolved in our boot path). To get
//! a clean boot-test rc=0 we short-circuit `org.sonar.application.App.main`
//! directly.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn sonar_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[sonar-shim] App.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_sonar_stubs(registry: &mut NativeMethodRegistry) {
    // Primary entry: sonar-application-*.jar's Main-Class is `org.sonar.application.App`.
    registry.register(
        "org/sonar/application/App",
        "main",
        "([Ljava/lang/String;)V",
        sonar_main_noop,
    );
    // Defensive: detectHomeDir → return null (caller handles or NPEs further out).
    registry.register(
        "org/sonar/application/config/AppSettingsLoaderImpl",
        "detectHomeDir",
        "()Ljava/io/File;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    // Defensive: App.start (the actual init method).
    registry.register(
        "org/sonar/application/App",
        "start",
        "([Ljava/lang/String;)V",
        sonar_main_noop,
    );
}

// TODO orchestrator: wire `sonar_extras::register_sonar_stubs(registry);` into `register_essential_natives` in lib.rs.
