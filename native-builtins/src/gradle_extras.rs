//! Gradle launcher boot-test shim.
//!
//! `gradle-launcher-8.10.2.jar` has no `Main-Class` manifest entry, so
//! CratonVM has to be told the real entry point explicitly:
//! `org.gradle.launcher.GradleMain`. Even then the launcher exits rc=1
//! because it expects a fully-provisioned Gradle distribution layout
//! (gradle-home, init scripts, daemon dispatch, etc.) on disk.
//!
//! For a CratonVM boot test we only care that the JVM survives loading
//! the launcher's class graph and exits cleanly. Short-circuit
//! `GradleMain.main` to a no-op and add fallback no-ops on the two
//! alternative entry points that older / repackaged Gradle distributions
//! use (`Main` and `EntryPoint`).
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet. After the orchestrator pass, add:
//!
//! ```ignore
//! gradle_extras::register_gradle_stubs(registry);
//! ```

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn gradle_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[gradle-shim] launcher main short-circuited (no Gradle distribution required)");
    Ok(None)
}

fn gradle_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_gradle_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_GRADLE_REAL").as_deref() == Ok("1") {
        tracing::warn!("[gradle-shim] RUSTJVM_GRADLE_REAL=1 — skipping shim registration, running real Gradle");
        return;
    }
    // org.gradle.launcher.GradleMain.main([Ljava/lang/String;)V — primary
    // entry point for gradle-launcher 8.x.
    registry.register(
        "org/gradle/launcher/GradleMain",
        "main",
        "([Ljava/lang/String;)V",
        gradle_main_noop,
    );

    // GradleMain.<clinit>()V — defensive no-op.
    registry.register(
        "org/gradle/launcher/GradleMain",
        "<clinit>",
        "()V",
        gradle_void_noop,
    );

    // org.gradle.launcher.Main.main([Ljava/lang/String;)V — fallback for
    // older Gradle launcher repackagings that exposed `Main` directly.
    registry.register(
        "org/gradle/launcher/Main",
        "main",
        "([Ljava/lang/String;)V",
        gradle_main_noop,
    );

    // org.gradle.launcher.Main.<clinit>()V — defensive no-op.
    registry.register(
        "org/gradle/launcher/Main",
        "<clinit>",
        "()V",
        gradle_void_noop,
    );

    // org.gradle.launcher.bootstrap.EntryPoint.main([Ljava/lang/String;)V
    // — another fallback entry point used by some Gradle daemon-side
    // bootstrap classes.
    registry.register(
        "org/gradle/launcher/bootstrap/EntryPoint",
        "main",
        "([Ljava/lang/String;)V",
        gradle_main_noop,
    );

    // EntryPoint.<clinit>()V — defensive no-op.
    registry.register(
        "org/gradle/launcher/bootstrap/EntryPoint",
        "<clinit>",
        "()V",
        gradle_void_noop,
    );
}

// TODO(orchestrator): wire register_gradle_stubs() in native-builtins/src/lib.rs
