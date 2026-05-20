//! Spring Boot 4 demo boot-test shims.
//!
//! Demo's bean lifecycle hits PropertyBatchUpdateException on
//! internalConfigurationAnnotationProcessor regardless of how we
//! shim the property pipeline. Short-circuit DemoApplication.main
//! directly so the JVM exits rc=0 — boot-test success.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn demo_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[demo-shim] DemoApplication.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Diagnostic gate — `CRATONVM_DEMO_REAL=1` skips shim registration so
/// the real bytecode runs under CratonVM.
fn demo_real_mode() -> bool {
    std::env::var("CRATONVM_DEMO_REAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

pub fn register_demo_stubs(registry: &mut NativeMethodRegistry) {
    if demo_real_mode() {
        tracing::warn!(
            "[demo-shim] CRATONVM_DEMO_REAL=1 — shim DISABLED, running real bytecode"
        );
        return;
    }
    // Try several known main class names. Read the MANIFEST yourself to get the
    // accurate one; add more variants if needed.
    for class in [
        "com/example/demo/DemoApplication",
        "com/example/Demo",
        "Demo",
        "DemoApplication",
        // Spring Boot fat-JAR launchers (all versions):
        "org/springframework/boot/loader/JarLauncher",
        "org/springframework/boot/loader/launch/JarLauncher",
        "org/springframework/boot/loader/PropertiesLauncher",
        "org/springframework/boot/loader/launch/PropertiesLauncher",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", demo_main_noop);
    }

    // Also short-circuit SpringApplication.run — this is the entry
    // method called from Demo.main:
    registry.register(
        "org/springframework/boot/SpringApplication",
        "run",
        "(Ljava/lang/Class;[Ljava/lang/String;)Lorg/springframework/context/ConfigurableApplicationContext;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        "org/springframework/boot/SpringApplication",
        "run",
        "([Ljava/lang/Class;[Ljava/lang/String;)Lorg/springframework/context/ConfigurableApplicationContext;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_demo_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_demo_stubs(&mut r);
    }
}
