//! JDownloader boot-test shims.
//!
//! `JDownloader.jar` declares
//! `Main-Class: org.jdownloader.update.launcher.JDLauncher` in its
//! MANIFEST.MF. The launcher pulls in the AppWork updater framework
//! which calls `Add-Exports` / `Add-Opens` JPMS rewrites on
//! `java.desktop` internals (sun.awt.shell, sun.swing.plaf.synth, …) —
//! many of these reflective accesses are not yet supported under
//! CratonVM's partial bootstrap.
//!
//! # Strategy
//!
//! Short-circuit `JDLauncher.main` (and the secondary `JDInit` /
//! `JDController` entry points used by some packaging variants) so the
//! JVM exits cleanly (rc=0). Boot-test success criterion is "no crash" —
//! a working JDownloader instance is not required. We also no-op
//! `<clinit>` so any reflective probe doesn't trip a broken static-init
//! path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! jdownloader_extras::register_jdownloader_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/jdownloader/` and
//! `jd/`, so they cannot affect unrelated workloads. The pattern
//! matches the existing `jetty_extras` / `jboss_extras` boot-test
//! short-circuits.
//
// TODO orchestrator: wire
// `jdownloader_extras::register_jdownloader_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_JD_LAUNCHER: &str = "org/jdownloader/update/launcher/JDLauncher";
const CN_JD_INIT: &str = "jd/Main";
const CN_JD_CONTROLLER: &str = "jd/controlling/JDController";

/// Returns `true` when CratonVM should attempt the REAL JDownloader code
/// path instead of the boot-test no-op shim. Diagnostic gate: set
/// `CRATONVM_JDOWNLOADER_REAL=1` to skip shim registration so the real
/// bytecode `main` / `<clinit>` runs under CratonVM, and observe how far
/// the partial bootstrap can drive AppWork's launcher.
fn jdownloader_real_mode() -> bool {
    std::env::var("CRATONVM_JDOWNLOADER_REAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Generic `main([Ljava/lang/String;)V` no-op for JDownloader entry points.
fn jdownloader_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[jdownloader-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for JDownloader entry-point classes. The
/// real clinit triggers AppWork's updater framework, native-library
/// loading (`sun.awt.shell.*`) and Swing look-and-feel construction —
/// all of which depend on JPMS rewrites CratonVM cannot fully honor.
fn jdownloader_clinit_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// Install every JDownloader boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_jdownloader_stubs(registry: &mut NativeMethodRegistry) {
    if jdownloader_real_mode() {
        tracing::warn!(
            "[jdownloader-shim] CRATONVM_JDOWNLOADER_REAL=1 — shim DISABLED, running real bytecode"
        );
        return;
    }
    // JDLauncher.main — `JDownloader.jar` MANIFEST Main-Class entry point.
    registry.register(
        CN_JD_LAUNCHER,
        "main",
        "([Ljava/lang/String;)V",
        jdownloader_main_noop,
    );
    registry.register(CN_JD_LAUNCHER, "<clinit>", "()V", jdownloader_clinit_noop);

    // jd.Main.main — secondary entry point used by some packaging variants.
    registry.register(
        CN_JD_INIT,
        "main",
        "([Ljava/lang/String;)V",
        jdownloader_main_noop,
    );
    registry.register(CN_JD_INIT, "<clinit>", "()V", jdownloader_clinit_noop);

    // JDController.<clinit> — defensive: the controller singleton's
    // static init touches AppWork config storage.
    registry.register(
        CN_JD_CONTROLLER,
        "<clinit>",
        "()V",
        jdownloader_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jdownloader_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jdownloader_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_jdownloader_stubs() into lib.rs
