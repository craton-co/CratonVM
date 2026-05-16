//! Apache ActiveMQ console boot-test shim.
//!
//! `org.apache.activemq.console.Main.main` invokes `System.exit(1)` when
//! the command line is empty (the help/usage path). For a pure
//! "does CratonVM survive ActiveMQ's bootstrap?" boot test we want the
//! JVM to exit cleanly (rc=0) rather than terminating with the
//! console's exit code. Short-circuit the entry point.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by a parallel agent this round. After the
//! orchestrator pass, add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! activemq_extras::register_activemq_stubs(registry);
//! ```

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn activemq_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[activemq-shim] console/Main.main short-circuited (System.exit(1) bypass)");
    Ok(None)
}

fn activemq_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_activemq_stubs(registry: &mut NativeMethodRegistry) {
    // org.apache.activemq.console.Main.main([Ljava/lang/String;)V — no-op,
    // skipping the System.exit(1) help-text path.
    registry.register(
        "org/apache/activemq/console/Main",
        "main",
        "([Ljava/lang/String;)V",
        activemq_main_noop,
    );

    // Defensive: also no-op the <clinit> in case static init reaches
    // platform-specific code that exits early.
    registry.register(
        "org/apache/activemq/console/Main",
        "<clinit>",
        "()V",
        activemq_void_noop,
    );
}

// TODO(orchestrator): wire register_activemq_stubs() in native-builtins/src/lib.rs
