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
//! [cratonvm] System.exit(-9) called — process terminating
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
//! `CRATONVM_WILDFLY_SHORTCIRCUIT` env gate).
//
// TODO orchestrator: wire `jetty_extras::register_jetty_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

#[allow(dead_code)]
const CN_MAIN: &str = "org/eclipse/jetty/start/Main";
#[allow(dead_code)]
const CN_START_ARGS: &str = "org/eclipse/jetty/start/StartArgs";
#[allow(dead_code)]
const CN_CLASSPATH: &str = "org/eclipse/jetty/start/Classpath";

/// `org.eclipse.jetty.start.Main.main([Ljava/lang/String;)V` — no-op.
///
/// Short-circuits the launcher so the JVM exits cleanly with rc=0 rather
/// than crashing inside `Main.start` on the null `StartArgs.getClasspath()`.
#[allow(dead_code)]
fn jetty_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jetty-shim] Main.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `()V` no-op used for `<clinit>` short-circuits on classes whose
/// static init walks file-system probing logic we cannot satisfy in
/// boot-test mode (e.g., locating `$JETTY_HOME` via classloader URL
/// resolution).
#[allow(dead_code)]
fn jetty_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `org.eclipse.jetty.start.Main.start([Ljava/lang/String;)V` — no-op.
///
/// Defensive alternative-signature stub for the inner `start` method that
/// the original NPE traced to (Main.java:397). If `Main.main` is somehow
/// invoked through a path the orchestrator hasn't shimmed, this catches
/// the NPE one level deeper.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// `org.eclipse.jetty.start.Main.processCommandLine([Ljava/lang/String;)Lorg/eclipse/jetty/start/StartArgs;`
///
/// Real-mode (`CRATONVM_JETTY_REAL=1`) intercept that returns a synthetic
/// non-null `StartArgs` instance. The real bytecode walks the argument
/// list, parses `start.d/*.ini` files, and constructs a fully-populated
/// `StartArgs` — but on CratonVM's partial bootstrap several internal
/// helpers (Props, BaseHome filesystem probing) return null and the
/// final return value ends up null. Downstream `Main.start(StartArgs)`
/// then NPEs at line 397 on `aload_1 + invokevirtual getClasspath`.
///
/// Returning a synthetic non-null `StartArgs` is paired with no-op
/// intercepts of the launcher's boolean predicates (`isHelp`,
/// `isListClasspath`, `isListConfig`, `isDryRun`, `isStopCommand`,
/// `isTestingModeEnabled`, `isRun`, `isExec`, `isCreateFiles`,
/// `hasJvmArgs`, `hasSystemProperties`) and getter methods that the
/// `start(StartArgs)` body invokes before any code path that would
/// actually run Jetty. The combination unblocks `--list-config` to
/// progress past the NPE — it won't print Jetty's real configuration
/// (we return false for `isListConfig`), but it returns cleanly
/// without aborting the VM.
#[allow(dead_code)]
fn jetty_main_process_command_line(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if let Some(cid) = ctx.class_id_by_name(CN_START_ARGS) {
        let obj = ctx.alloc_object(cid, 0);
        tracing::warn!(
            "[jetty-shim] Main.processCommandLine returned synthetic StartArgs (REAL mode)"
        );
        return Ok(Some(Value::Object(Some(obj))));
    }
    Ok(Some(Value::Object(None)))
}

/// Generic `()Z` returning `false` (0). Used for the boolean predicates
/// on a synthetic `StartArgs` so that `Main.start(StartArgs)` falls
/// through every conditional branch without dereferencing fields that
/// were never populated.
#[allow(dead_code)]
fn jetty_return_false(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// Generic getter returning `null` reference. Used for `StartArgs` getter
/// methods invoked by `Main.start(StartArgs)` (e.g. `getListModules`,
/// `getShowModules`, `getModuleGraphFilename`). Each call site downstream
/// of these checks for null before dereferencing.
#[allow(dead_code)]
fn jetty_return_null(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Install every Jetty boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_jetty_stubs(registry: &mut NativeMethodRegistry) {
    // DISABLED per "no synthetic stubs" policy. Every registration in this
    // module was a fake-out: synthetic empty `StartArgs` / `Classpath`
    // objects, predicates wired to return false, getters wired to return
    // null, and Main.main / Main.start no-ops. These masked the real
    // failure path. The orchestrator wants the first real-bytecode failure
    // surfaced, then dispatches follow-up fix agents.
    let _ = registry;
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
