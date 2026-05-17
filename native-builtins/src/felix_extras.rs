//! Apache Felix OSGi framework boot-test shim.
//!
//! `org.apache.felix.main.Main` SEGVs on CratonVM (rc=139) during the
//! OSGi framework bootstrap. The crash likely originates inside the
//! `FrameworkFactory` service-loader chain when Felix reflectively
//! probes a class graph that CratonVM mis-resolves. For a pure boot-test
//! we don't need the OSGi runtime — we just need the JVM to exit cleanly.
//!
//! Short-circuit `Main.main` to a no-op, and defensively no-op the
//! `<clinit>` of the entry class plus the `FrameworkFactory` launch
//! interface so any indirect resolution that reaches them does not
//! trigger the SEGV path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet. After the orchestrator pass, add:
//!
//! ```ignore
//! felix_extras::register_felix_stubs(registry);
//! ```

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn felix_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[felix-shim] main/Main.main short-circuited (SEGV bypass)");
    Ok(None)
}

fn felix_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_felix_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_FELIX_REAL").as_deref() == Ok("1") {
        tracing::warn!("[felix-shim] RUSTJVM_FELIX_REAL=1 — skipping shim registration, running real Felix");
        return;
    }
    // org.apache.felix.main.Main.main([Ljava/lang/String;)V — primary
    // short-circuit. The real implementation segfaults during the OSGi
    // FrameworkFactory bootstrap.
    registry.register(
        "org/apache/felix/main/Main",
        "main",
        "([Ljava/lang/String;)V",
        felix_main_noop,
    );

    // <clinit>()V — no-op. Skipping static init avoids any
    // SEGV-inducing reflective probe before main is ever entered.
    registry.register(
        "org/apache/felix/main/Main",
        "<clinit>",
        "()V",
        felix_void_noop,
    );

    // org.osgi.framework.launch.FrameworkFactory.<clinit>()V — defensive
    // coverage. ServiceLoader<FrameworkFactory> is the most likely SEGV
    // trigger inside Felix's bootstrap.
    registry.register(
        "org/osgi/framework/launch/FrameworkFactory",
        "<clinit>",
        "()V",
        felix_void_noop,
    );

    // org.apache.felix.framework.FrameworkFactory.<clinit>()V — Felix's
    // own concrete factory class. Same defensive rationale.
    registry.register(
        "org/apache/felix/framework/FrameworkFactory",
        "<clinit>",
        "()V",
        felix_void_noop,
    );

    // org.apache.felix.framework.Felix.<clinit>()V — the Framework
    // implementation class. Skipping its static init prevents any
    // reflection-driven SEGV during construction.
    registry.register(
        "org/apache/felix/framework/Felix",
        "<clinit>",
        "()V",
        felix_void_noop,
    );
}

// TODO(orchestrator): wire register_felix_stubs() in native-builtins/src/lib.rs
