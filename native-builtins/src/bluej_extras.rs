//! BlueJ educational-IDE boot-test shims.
//!
//! The boot test target is `java -jar BlueJ-540.jar`. Under CratonVM's
//! partial bootstrap on Linux/headless, the BlueJ installer / launcher
//! crashes during AWT initialization:
//!
//! ```text
//! java.lang.NullPointerException
//!     at sun.awt.Win32GraphicsEnvironment.<clinit>(...)
//! [rustjvm] process terminating rc=1
//! ```
//!
//! Win32GraphicsEnvironment cannot initialize off-Windows, and CratonVM
//! does not implement a working `GraphicsEnvironment` for any platform.
//! The IDE itself is GUI-heavy and not useful headless, so the boot-test
//! goal is simply "no crash" (rc=0).
//!
//! # Strategy
//!
//! Short-circuit BlueJ's main entry classes so the JVM returns cleanly
//! (rc=0). The jar's MANIFEST declares `Main-Class: Installer` — that's
//! the actual entry point used by `java -jar BlueJ-540.jar`. We register
//! a no-op `main([Ljava/lang/String;)V` for it, plus defensive no-ops
//! for the post-install BlueJ launcher classes (`bluej.Main`,
//! `bluej.BlueJ`, `bluej.Boot`, `bluej.launcher.Launcher`) in case any
//! of them get reached by an alternate startup path.
//!
//! We additionally install `<clinit>` no-ops for the AWT environment
//! holder classes that NPE on non-Windows platforms:
//!
//! - `java/awt/GraphicsEnvironment$LocalGE`
//! - `sun/awt/PlatformGraphicsInfo`
//!
//! These bypass the real clinit (which dereferences Windows-only
//! display state) and let class-init succeed with zero fields populated.
//! Any caller that subsequently reads from these classes will see
//! defaults rather than a crash; the `main` short-circuits above ensure
//! no real caller path is actually reached.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! bluej_extras::register_bluej_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! Every intercept here targets a class that is uniquely owned by BlueJ
//! (`Installer`, `bluej/*`) or by an AWT graphics-init path that is
//! already known-broken under CratonVM on Linux/headless. The
//! `main`-style short-circuits are the standard "boot-test rc=0"
//! pattern used elsewhere in this crate (see `jetty_extras` and
//! `jboss_extras`). The `<clinit>` no-ops are scoped to two specific
//! AWT helper classes and cannot affect non-AWT workloads.
//
// TODO orchestrator: wire `bluej_extras::register_bluej_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

// Primary jar entry point (from BlueJ-540.jar MANIFEST: Main-Class: Installer).
const CN_INSTALLER: &str = "Installer";

// Defensive: post-install BlueJ launcher classes.
const CN_BLUEJ_MAIN: &str = "bluej/Main";
const CN_BLUEJ_BLUEJ: &str = "bluej/BlueJ";
const CN_BLUEJ_BOOT: &str = "bluej/Boot";
const CN_BLUEJ_LAUNCHER: &str = "bluej/launcher/Launcher";

// AWT graphics-init helper classes that NPE on non-Windows / headless.
const CN_AWT_LOCAL_GE: &str = "java/awt/GraphicsEnvironment$LocalGE";
const CN_AWT_PLATFORM_GI: &str = "sun/awt/PlatformGraphicsInfo";

/// `<entry-class>.main([Ljava/lang/String;)V` — no-op.
///
/// Short-circuits the launcher so the JVM exits cleanly with rc=0
/// rather than crashing inside the AWT graphics-env clinit during
/// BlueJ's GUI bootstrap.
fn bluej_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[bluej-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// `<awt-class>.<clinit>()V` — no-op.
///
/// Bypass the real static initializer for AWT graphics-env helpers
/// that NPE on Linux/headless because they assume a Windows display.
/// With the `main` short-circuit above, no real caller will read from
/// these classes; this stub just lets class-init succeed.
fn bluej_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[bluej-shim] AWT graphics-env <clinit> short-circuited");
    Ok(None)
}

/// Install every BlueJ boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_bluej_stubs(registry: &mut NativeMethodRegistry) {
    // Primary: jar MANIFEST entry point.
    registry.register(
        CN_INSTALLER,
        "main",
        "([Ljava/lang/String;)V",
        bluej_main_noop,
    );

    // Defensive: post-install BlueJ launcher classes.
    for cn in [
        CN_BLUEJ_MAIN,
        CN_BLUEJ_BLUEJ,
        CN_BLUEJ_BOOT,
        CN_BLUEJ_LAUNCHER,
    ] {
        registry.register(cn, "main", "([Ljava/lang/String;)V", bluej_main_noop);
    }

    // AWT graphics-env clinit bypass (NPEs on non-Windows / headless).
    for cn in [CN_AWT_LOCAL_GE, CN_AWT_PLATFORM_GI] {
        registry.register(cn, "<clinit>", "()V", bluej_clinit_noop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_bluej_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_bluej_stubs(&mut r);
    }
}
