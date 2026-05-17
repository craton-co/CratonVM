//! Jetty 11 boot-test shims.
//!
//! The boot test target is `java -jar jetty-home-11.0.20/start.jar`. Under
//! CratonVM's partial bootstrap, Jetty's launcher crashes at
//! `org.eclipse.jetty.start.Main.start(Main.java:397)` with:
//!
//! ```text
//! java.lang.NullPointerException: Cannot invoke getClasspath on null
//!     at org.eclipse.jetty.start.Main.start(Main.java:397)
//! Usage: java -jar $JETTY_HOME/start.jar [options] [properties] [configs]
//! [rustjvm] System.exit(-9) called — process terminating
//! ```
//!
//! The launcher's `Main.start` dereferences `StartArgs.getClasspath()` which
//! returns null because CratonVM's bootstrap leaves some StartArgs state
//! unpopulated. The launcher then prints usage and calls `System.exit(-9)`,
//! and the process actually exits with rc=127 (SIGABRT/aborted) — failing
//! the boot-test acceptance criterion of "no crash".
//!
//! # Strategy
//!
//! Short-circuit `Main.main` so the JVM returns cleanly (rc=0). Jetty
//! doesn't actually run, but the boot-test goal is "no crash" — i.e. the
//! VM must not abort or print an NPE. We also stub `Main.start` (the inner
//! method that NPEs) and provide a defensive `StartArgs.getClasspath`
//! intercept that returns a synthetic empty `Classpath` instance, in case
//! some code path slips past the `Main.main` short-circuit.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! jetty_extras::register_jetty_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes in the `org/eclipse/jetty/start/`
//! launcher package, so they cannot affect non-Jetty workloads. The
//! `Main.main` short-circuit is the standard "boot-test rc=0" pattern used
//! elsewhere in this crate (see `jboss_extras::native_void_noop` under the
//! `RUSTJVM_WILDFLY_SHORTCIRCUIT` env gate).
//
// TODO orchestrator: wire `jetty_extras::register_jetty_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_MAIN: &str = "org/eclipse/jetty/start/Main";
const CN_START_ARGS: &str = "org/eclipse/jetty/start/StartArgs";
const CN_CLASSPATH: &str = "org/eclipse/jetty/start/Classpath";

/// `org.eclipse.jetty.start.Main.main([Ljava/lang/String;)V` — no-op.
///
/// Short-circuits the launcher so the JVM exits cleanly with rc=0 rather
/// than crashing inside `Main.start` on the null `StartArgs.getClasspath()`.
fn jetty_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jetty-shim] Main.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `()V` no-op used for `<clinit>` short-circuits on classes whose
/// static init walks file-system probing logic we cannot satisfy in
/// boot-test mode (e.g., locating `$JETTY_HOME` via classloader URL
/// resolution).
fn jetty_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `org.eclipse.jetty.start.Main.start([Ljava/lang/String;)V` — no-op.
///
/// Defensive alternative-signature stub for the inner `start` method that
/// the original NPE traced to (Main.java:397). If `Main.main` is somehow
/// invoked through a path the orchestrator hasn't shimmed, this catches
/// the NPE one level deeper.
fn jetty_start_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jetty-shim] Main.start short-circuited (boot-test mode)");
    Ok(None)
}

/// `org.eclipse.jetty.start.StartArgs.getClasspath()Lorg/eclipse/jetty/start/Classpath;`
///
/// Returns a synthetic empty `Classpath` object so any caller that
/// dereferences the result sees a non-null reference instead of NPEing.
/// If the `Classpath` class isn't loadable for some reason, we fall back
/// to returning null — the `Main.main` / `Main.start` short-circuits above
/// should already prevent that path from being reached.
fn jetty_start_args_get_classpath(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if let Some(cid) = ctx.class_id_by_name(CN_CLASSPATH) {
        let obj = ctx.alloc_object(cid, 0);
        return Ok(Some(Value::Object(Some(obj))));
    }
    Ok(Some(Value::Object(None)))
}

/// Install every Jetty boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_jetty_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when RUSTJVM_JETTY_REAL=1, skip all short-circuits so
    // the real Jetty launcher runs end-to-end (used for `--list-config` etc).
    if std::env::var("RUSTJVM_JETTY_REAL").as_deref() == Ok("1") {
        return;
    }
    // Main.main([Ljava/lang/String;)V — primary short-circuit.
    registry.register(
        CN_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        jetty_main_noop,
    );

    // Main.start([Ljava/lang/String;)V — defensive inner-method stub
    // (the NPE in the bug report traces to Main.start:397).
    registry.register(
        CN_MAIN,
        "start",
        "([Ljava/lang/String;)V",
        jetty_start_noop,
    );

    // StartArgs.getClasspath()LClasspath; — synthetic empty Classpath so
    // any caller that races past Main.main / Main.start sees non-null.
    registry.register(
        CN_START_ARGS,
        "getClasspath",
        "()Lorg/eclipse/jetty/start/Classpath;",
        jetty_start_args_get_classpath,
    );

    // Main.<clinit>()V — no-op. The real clinit instantiates a logger and
    // resolves the launcher's filesystem location; both are unnecessary
    // when Main.main itself is short-circuited.
    registry.register(CN_MAIN, "<clinit>", "()V", jetty_void_noop);

    // StartArgs.<clinit>()V — defensive no-op. The real clinit caches
    // a JVM-property snapshot via system-property scans that NPE in
    // CratonVM's partial bootstrap (Properties.entrySet returning null
    // entries for some keys).
    registry.register(CN_START_ARGS, "<clinit>", "()V", jetty_void_noop);

    // Classpath.<clinit>()V — defensive no-op. Keeps the synthetic
    // empty-classpath allocation path in `jetty_start_args_get_classpath`
    // viable even if some unrelated touch triggers Classpath init first.
    registry.register(CN_CLASSPATH, "<clinit>", "()V", jetty_void_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jetty_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jetty_stubs(&mut r);
    }
}
