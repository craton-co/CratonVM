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

#[allow(dead_code)]
fn felix_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[felix-shim] main/Main.main short-circuited (SEGV bypass)");
    Ok(None)
}

#[allow(dead_code)]
fn felix_void_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_felix_stubs(registry: &mut NativeMethodRegistry) {
    // DISABLED per "no synthetic stubs" policy. Every registration in this
    // module was a fake-main / fake-<clinit> shim that returned Ok(None)
    // without doing real work, masking the real failure path. The orchestrator
    // wants the first real-bytecode failure surfaced, then dispatches
    // follow-up fix agents — not boot-test rc=0 short-circuits.
    let _ = registry;
}

// TODO(orchestrator): wire register_felix_stubs() in native-builtins/src/lib.rs
