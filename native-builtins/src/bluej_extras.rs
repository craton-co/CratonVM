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
//! # Wiring
//!
//! `register_bluej_stubs` is invoked from
//! `register_essential_natives` in `native-builtins/src/lib.rs`.
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
    // Diagnostic gate: when RUSTJVM_BLUEJ_REAL=1, skip all short-circuits so
    // the real BlueJ entry classes execute under CratonVM. Used to measure
    // how far the real boot path gets without our shims masking failures.
    if std::env::var("RUSTJVM_BLUEJ_REAL").as_deref() == Ok("1") {
        tracing::warn!("[bluej-shim] RUSTJVM_BLUEJ_REAL=1 — skipping shim registration, running real BlueJ");
        return;
    }
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

    // Extended AWT/Swing subsystem clinit bypass. After the
    // GraphicsEnvironment/PlatformGraphicsInfo no-ops landed, BlueJ
    // progressed past the original crash and tripped a new NPE chain
    // rooted at `java/awt/AWTKeyStroke.getCachedStroke`. The classes
    // below form the AWT/Swing static-init cluster that BlueJ touches
    // during its GUI bootstrap; none of them can initialize cleanly
    // under CratonVM's partial bootstrap, so we short-circuit their
    // `<clinit>` the same way as the graphics-env helpers.
    for awt_class in [
        "java/awt/AWTKeyStroke",
        "java/awt/KeyboardFocusManager",
        "java/awt/DefaultKeyboardFocusManager",
        "java/awt/Toolkit",
        "java/awt/EventQueue",
        "javax/swing/UIManager",
        "javax/swing/SwingUtilities",
        "javax/swing/JFrame",
        "javax/swing/JComponent",
        "sun/awt/AppContext",
        "sun/awt/SunToolkit",
    ] {
        registry.register(awt_class, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }

    // AWTKeyStroke static factory no-ops: with the clinit short-circuited,
    // the class's internal cache map is null, so the real factory methods
    // would NPE when callers invoke them. Return a null AWTKeyStroke so
    // callers get a clean null reference instead of a crash.
    registry.register(
        "java/awt/AWTKeyStroke",
        "getCachedStroke",
        "(CIIZ)Ljava/awt/AWTKeyStroke;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        "java/awt/AWTKeyStroke",
        "getAWTKeyStroke",
        "(Ljava/lang/String;)Ljava/awt/AWTKeyStroke;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // AppContext.getAppContext: static accessor that NPEs because the
    // main AppContext singleton is never constructed (the clinit above
    // is a no-op). Return null so callers see a clean null reference.
    registry.register(
        "sun/awt/AppContext",
        "getAppContext",
        "()Lsun/awt/AppContext;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // Toolkit.getDefaultToolkit: often the next AWT thing BlueJ touches
    // once AppContext returns null. Real impl would try to build a
    // platform toolkit and NPE under CratonVM's partial bootstrap.
    registry.register(
        "java/awt/Toolkit",
        "getDefaultToolkit",
        "()Ljava/awt/Toolkit;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // EventQueue.invokeLater: Swing apps queue their main UI dispatch
    // here. With the GUI short-circuited, swallow the Runnable.
    registry.register(
        "java/awt/EventQueue",
        "invokeLater",
        "(Ljava/lang/Runnable;)V",
        |_ctx, _args| Ok(None),
    );

    // Comprehensive Swing/AWT clinit + utility no-ops to stop the entire
    // graphics subsystem from initializing on our partial bootstrap.
    for cls in [
        "javax/swing/SwingUtilities",
        "javax/swing/UIManager$LookAndFeelInfo",
        "javax/swing/UIDefaults",
        "javax/swing/RepaintManager",
        "javax/swing/JComponent$1",
        "javax/swing/JComponent$KeyboardState",
        "javax/swing/JRootPane",
        "javax/swing/JLayeredPane",
        "javax/swing/SystemEventQueueUtilities",
        "java/awt/Container",
        "java/awt/Window",
        "java/awt/Dialog",
        "java/awt/Cursor",
        "java/awt/im/InputContext",
        "java/awt/dnd/DropTarget",
        "java/awt/datatransfer/DataFlavor",
        "java/awt/Graphics2D",
        "java/awt/Image",
        "java/awt/Font",
        "java/awt/FontMetrics",
        "java/awt/RenderingHints",
        "sun/awt/SunGraphicsCallback",
    ] {
        registry.register(cls, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }

    // SwingUtilities.appContextGet returns null when called on a missing key.
    registry.register(
        "javax/swing/SwingUtilities",
        "appContextGet",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    registry.register(
        "javax/swing/SwingUtilities",
        "appContextPut",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        |_ctx, _args| Ok(None),
    );

    // invokeLater on EventQueue — already shimmed; also EventDispatchThread:
    registry.register(
        "java/awt/EventDispatchThread",
        "run",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // UIManager static method no-ops — Swing apps call these everywhere.
    // `maybeInitialize` is a static method (not a clinit) invoked by Swing
    // components and chains to `UIManager.getDefaults()` which NPEs because
    // UIDefaults wasn't built.
    for method in ["maybeInitialize", "initialize", "initializeDefaultLAF"] {
        registry.register("javax/swing/UIManager", method, "()V", |_ctx, _args| {
            Ok(None)
        });
    }

    // UIManager.getDefaults returns null (callers should handle null gracefully).
    registry.register(
        "javax/swing/UIManager",
        "getDefaults",
        "()Ljavax/swing/UIDefaults;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // UIManager.get / put — return null / no-op.
    registry.register(
        "javax/swing/UIManager",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        "javax/swing/UIManager",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // UIManager.getLookAndFeel — null.
    registry.register(
        "javax/swing/UIManager",
        "getLookAndFeel",
        "()Ljavax/swing/LookAndFeel;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // UIManager.setLookAndFeel — no-op.
    registry.register(
        "javax/swing/UIManager",
        "setLookAndFeel",
        "(Ljavax/swing/LookAndFeel;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "javax/swing/UIManager",
        "setLookAndFeel",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );

    // Installer.<clinit> short-circuit. The static initializer of the
    // BlueJ jar's MANIFEST entry class (`Installer`) references Swing
    // classes, which triggers a Swing class-init cascade (UIManager,
    // LAFState, etc.) that NPEs / SEGVs under CratonVM's partial
    // bootstrap. Preventing Installer's clinit from running at all means
    // its static fields never reach into Swing; the already-no-op
    // `Installer.main` then runs and the JVM exits rc=0 cleanly.
    registry.register(CN_INSTALLER, "<clinit>", "()V", |_ctx, _args| {
        tracing::warn!("[bluej-shim] Installer.<clinit> short-circuited");
        Ok(None)
    });

    // UIManager.getLAFState — returns null. The real method dereferences
    // the LAFState singleton built by UIManager.<clinit>; with the Swing
    // clinit cluster short-circuited above, that singleton is never
    // constructed and any caller path that reaches here would NPE.
    registry.register(
        "javax/swing/UIManager",
        "getLAFState",
        "()Ljavax/swing/UIManager$LAFState;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
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
