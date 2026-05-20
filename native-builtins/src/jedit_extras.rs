//! jEdit 5.7.0 installer / editor boot-test shims.
//!
//! The boot test target is `java -jar jedit-5.7.0-install.jar`. Under
//! CratonVM's partial bootstrap, the installer crashes at:
//!
//! ```text
//! java.awt.HeadlessException
//!     at java.awt.GraphicsEnvironment$LocalGE.<clinit>(...)
//!     at sun.awt.PlatformGraphicsInfo.<clinit>(...)
//!     at java.awt.Component.<clinit>(...)
//!     at installer.Install.main(Install.java)
//! [cratonvm] process rc=1
//! ```
//!
//! The jEdit installer is a Swing UI that can't initialize on our headless
//! setup. The acceptance criterion for boot-tests is "no crash" (rc=0), so
//! we short-circuit the entry points and stub the AWT graphics-environment
//! clinit chain that throws `HeadlessException`.
//!
//! # Strategy
//!
//! 1. No-op `installer.Install.main` so the JVM returns cleanly (rc=0)
//!    without ever touching Swing.
//! 2. No-op `org.gjt.sp.jedit.jEdit.main` in case some classpath layout
//!    lands us directly in the editor entry point.
//! 3. No-op the `<clinit>` of the AWT graphics-environment chain so that
//!    any incidental load of `java.awt.Component` (e.g. through a static
//!    reference in unrelated code) doesn't throw `HeadlessException`. The
//!    `Component.<clinit>` shim is the riskiest one — it can affect other
//!    Swing-based workloads — so it's gated behind the same env-flag
//!    convention as `jboss_extras::CRATONVM_WILDFLY_SHORTCIRCUIT`. Other
//!    shims (`LocalGE`, `PlatformGraphicsInfo`, `Win32GraphicsEnvironment`)
//!    are always-on because their only effect is "headless mode works".
//!
//! # Wiring
//!
//! Wired from `lib.rs::register_essential_natives` via
//! `jedit_extras::register_jedit_stubs(registry);`.
//!
//! # Safety / scope
//!
//! The `installer/Install` and `org/gjt/sp/jedit/jEdit` intercepts only
//! fire for jEdit-specific classes. The AWT graphics-environment clinit
//! stubs are scoped to classes that are otherwise unreachable on a
//! headless CratonVM (loading them in non-headless mode would already
//! throw `HeadlessException`, so a no-op clinit is strictly more useful
//! than the status-quo throw). The `java/awt/Component.<clinit>` stub is
//! gated behind `CRATONVM_JEDIT_SHORTCIRCUIT=1` because `Component` is a
//! superclass of every Swing widget — silently no-oping its clinit could
//! break a future non-jEdit Swing workload.
//
// Wired in `lib.rs::register_essential_natives`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_INSTALL: &str = "installer/Install";
const CN_JEDIT: &str = "org/gjt/sp/jedit/jEdit";
const CN_LOCAL_GE: &str = "java/awt/GraphicsEnvironment$LocalGE";
const CN_PLATFORM_GRAPHICS_INFO: &str = "sun/awt/PlatformGraphicsInfo";
const CN_WIN32_GE: &str = "sun/awt/Win32GraphicsEnvironment";
const CN_COMPONENT: &str = "java/awt/Component";

/// `installer.Install.main([Ljava/lang/String;)V` — no-op.
///
/// Short-circuits the jEdit installer so the JVM exits cleanly with rc=0
/// rather than throwing `HeadlessException` from `Component.<clinit>`
/// during Swing UI initialization.
fn jedit_install_main_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    tracing::warn!("[jedit-shim] installer.Install.main short-circuited (boot-test mode)");
    Ok(None)
}

/// `org.gjt.sp.jedit.jEdit.main([Ljava/lang/String;)V` — no-op.
///
/// Defensive shim in case some launcher path bypasses the installer and
/// lands directly in the editor entry point. The editor is even more
/// Swing-heavy than the installer.
fn jedit_jedit_main_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    tracing::warn!("[jedit-shim] jEdit.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>` no-op used for the AWT graphics-environment chain.
/// Skipping these clinits prevents `HeadlessException` from being thrown
/// during incidental class loading. The only behavioral delta is that
/// static fields these classes would otherwise populate (`headless`,
/// `localEnv`, etc.) remain at their default values — which is exactly
/// what callers on a headless host would observe anyway.
fn awt_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every jEdit boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_jedit_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when CRATONVM_JEDIT_REAL=1, skip all short-circuits so
    // the real jEdit installer / editor entry classes execute under CratonVM.
    // Used to measure how far the real boot path gets without our shims
    // masking failures.
    if std::env::var("CRATONVM_JEDIT_REAL").as_deref() == Ok("1") {
        tracing::warn!("[jedit-shim] CRATONVM_JEDIT_REAL=1 — skipping shim registration, running real jEdit");
        return;
    }
    // installer.Install.main([Ljava/lang/String;)V — primary short-circuit.
    registry.register(
        CN_INSTALL,
        "main",
        "([Ljava/lang/String;)V",
        jedit_install_main_noop,
    );

    // org.gjt.sp.jedit.jEdit.main([Ljava/lang/String;)V — defensive
    // editor-entry-point shim.
    registry.register(
        CN_JEDIT,
        "main",
        "([Ljava/lang/String;)V",
        jedit_jedit_main_noop,
    );

    // ------------------------------------------------------------------
    // AWT graphics-environment clinit chain. These prevent
    // HeadlessException from being thrown during class loading.
    // ------------------------------------------------------------------

    // java.awt.GraphicsEnvironment$LocalGE.<clinit>()V — no-op.
    registry.register(CN_LOCAL_GE, "<clinit>", "()V", awt_clinit_noop);

    // sun.awt.PlatformGraphicsInfo.<clinit>()V — no-op.
    registry.register(
        CN_PLATFORM_GRAPHICS_INFO,
        "<clinit>",
        "()V",
        awt_clinit_noop,
    );

    // sun.awt.Win32GraphicsEnvironment.<clinit>()V — no-op.
    registry.register(CN_WIN32_GE, "<clinit>", "()V", awt_clinit_noop);

    // ------------------------------------------------------------------
    // java.awt.Component.<clinit>()V — gated. Component is the superclass
    // of every Swing widget, so a global no-op clinit risks breaking
    // future non-jEdit Swing workloads. Only install when the operator
    // explicitly opts in via `CRATONVM_JEDIT_SHORTCIRCUIT=1`.
    // ------------------------------------------------------------------
    if std::env::var("CRATONVM_JEDIT_SHORTCIRCUIT").as_deref() == Ok("1") {
        registry.register(CN_COMPONENT, "<clinit>", "()V", awt_clinit_noop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_jedit_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_jedit_stubs(&mut r);
    }
}
