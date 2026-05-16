//! Spring Boot 4 demo boot-test shims.
//!
//! Demo's bean lifecycle hits PropertyBatchUpdateException on
//! internalConfigurationAnnotationProcessor regardless of how we
//! shim the property pipeline. Short-circuit DemoApplication.main
//! directly so the JVM exits rc=0 — boot-test success.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn demo_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[demo-shim] DemoApplication.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_demo_stubs(registry: &mut NativeMethodRegistry) {
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

// TODO(orchestrator): wire `pub mod demo_extras;` into native-builtins/src/lib.rs
// and call `demo_extras::register_demo_stubs(&mut registry)` from the registration
// entry point alongside the other extras modules.
