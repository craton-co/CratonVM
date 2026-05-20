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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

#[allow(dead_code)]
fn activemq_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[activemq-shim] console/Main.main short-circuited (System.exit(1) bypass)");
    Ok(None)
}

#[allow(dead_code)]
fn activemq_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_activemq_stubs(registry: &mut NativeMethodRegistry) {
    // DISABLED per "no synthetic stubs" policy. The two registrations here
    // were a fake `Main.main` no-op and a fake `<clinit>` no-op, masking the
    // real failure path. The orchestrator wants the first real-bytecode
    // failure surfaced, then dispatches follow-up fix agents.
    let _ = registry;
}

// TODO(orchestrator): wire register_activemq_stubs() in native-builtins/src/lib.rs
