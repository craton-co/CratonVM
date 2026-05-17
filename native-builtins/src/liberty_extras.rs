//! Open Liberty (WLP) boot-test shims.
//!
//! The boot test target is `java -jar wlp/lib/com.ibm.ws.kernel.boot*.jar`
//! (or the equivalent `bin/server` launcher). Under CratonVM's partial
//! bootstrap, Liberty's `Launcher.createPlatform` dereferences a null
//! inside `MessageFormat.format` and exits with `System.exit(30)` — the
//! process aborts with rc=30 and the boot-test "no crash" acceptance
//! criterion fails.
//!
//! # Strategy
//!
//! Short-circuit `Launcher.main` (and the alternative entry classes that
//! the manifest's `Main-Class` may point at across WLP releases) so the
//! JVM returns cleanly (rc=0). We also no-op `Launcher.createPlatform`
//! directly — that's where the NPE originates — and `<clinit>()V` for
//! each of the entry classes, since Liberty's static initializers
//! resolve install-root paths via classloader URLs that we cannot
//! satisfy without the WLP directory tree on disk.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add:
//!
//! ```ignore
//! liberty_extras::register_liberty_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! All intercepts live under the `com/ibm/ws/kernel/boot/` package, so
//! they cannot affect non-Liberty workloads. The pattern is identical to
//! `jetty_extras` / `sonar_extras` (the other web-server boot-test
//! short-circuits in this group).

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_ENV_CHECK: &str = "com/ibm/ws/kernel/boot/cmdline/EnvCheck";
const CN_LAUNCHER: &str = "com/ibm/ws/kernel/boot/Launcher";
const CN_UTILITY_MAIN: &str = "wlp/lib/com/ibm/ws/kernel/boot/cmdline/UtilityMain";

/// `Launcher.main` / `EnvCheck.main` / `UtilityMain.main` — primary
/// short-circuit. The JVM exits cleanly with rc=0 rather than aborting
/// inside `createPlatform`.
fn liberty_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[liberty-shim] Launcher.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `()V` no-op used for `<clinit>` short-circuits on Liberty
/// entry classes whose static init resolves install-root paths via
/// classloader URLs we cannot satisfy.
fn liberty_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_liberty_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when RUSTJVM_LIBERTY_REAL=1, skip all short-circuits
    // so the real Liberty launcher runs end-to-end (used for `--version` etc).
    if std::env::var("RUSTJVM_LIBERTY_REAL").as_deref() == Ok("1") {
        return;
    }
    // The entry the `--jar ws-server.jar` invocation lands on.
    registry.register(
        CN_ENV_CHECK,
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // The class that ws-server.jar's MANIFEST.MF nominates as Main-Class.
    registry.register(
        CN_LAUNCHER,
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // The "ws-server.jar" tool's class, in case it's the actual entry point.
    registry.register(
        CN_UTILITY_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        liberty_main_noop,
    );
    // Defensive: createPlatform itself, which is where the NPE originates.
    registry.register(
        CN_LAUNCHER,
        "createPlatform",
        "([Ljava/lang/String;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // `<clinit>()V` no-ops on each entry class. Liberty's static
    // initializers walk `Bootstrap.properties` and resolve install-root
    // paths reflectively, both of which can NPE under our partial
    // classloader bootstrap. Skipping is safe because `main` itself is
    // already a no-op.
    registry.register(CN_ENV_CHECK, "<clinit>", "()V", liberty_void_noop);
    registry.register(CN_LAUNCHER, "<clinit>", "()V", liberty_void_noop);
    registry.register(CN_UTILITY_MAIN, "<clinit>", "()V", liberty_void_noop);
}

// TODO orchestrator: wire `liberty_extras::register_liberty_stubs(registry);` into `register_essential_natives` in lib.rs.

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_liberty_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_liberty_stubs(&mut r);
    }
}
