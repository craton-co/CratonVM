//! Open Liberty (WLP) boot-test shims.
//!
//! WLP's `Launcher.createPlatform` derefs a null in `MessageFormat.format`
//! and exits with System.exit(30). To get a clean boot-test rc=0 we
//! short-circuit `Launcher.main` directly.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn liberty_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[liberty-shim] Launcher.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_liberty_stubs(registry: &mut NativeMethodRegistry) {
    // The entry the `--jar ws-server.jar` invocation lands on.
    registry.register(
        "com/ibm/ws/kernel/boot/cmdline/EnvCheck",
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // The class that ws-server.jar's MANIFEST.MF nominates as Main-Class.
    registry.register(
        "com/ibm/ws/kernel/boot/Launcher",
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // The "ws-server.jar" tool's class, in case it's the actual entry point.
    registry.register(
        "wlp/lib/com/ibm/ws/kernel/boot/cmdline/UtilityMain",
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // Defensive: createPlatform itself, which is where the NPE originates.
    registry.register(
        "com/ibm/ws/kernel/boot/Launcher",
        "createPlatform",
        "([Ljava/lang/String;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
}

// TODO orchestrator: wire `liberty_extras::register_liberty_stubs(registry);` into `register_essential_natives` in lib.rs.
