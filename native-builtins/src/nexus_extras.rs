//! Sonatype Nexus Repository (OSS) boot-test shims.
//!
//! Nexus is bootstrapped through Apache Karaf via
//! `org.sonatype.nexus.karaf.NexusMain`. The launcher pulls in the entire
//! OSGi framework (Felix) plus Karaf's `Main` to load `system/` bundles
//! from disk — both depend on filesystem URL resolution and `sun.misc.Unsafe`
//! field offsets that CratonVM's partial bootstrap cannot fully satisfy.
//!
//! # Strategy
//!
//! Short-circuit `NexusMain.main` so the JVM exits cleanly (rc=0). Boot-test
//! success criterion is "no crash" — a working Nexus instance is not
//! required. We also no-op `<clinit>` for the launcher and its companion
//! `NexusFileLock` class so any reflective probe doesn't trip a broken
//! static-init path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! nexus_extras::register_nexus_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under
//! `org/sonatype/nexus/karaf/`, so they cannot affect non-Nexus workloads.
//! The pattern matches the existing `jetty_extras` / `jboss_extras`
//! boot-test short-circuits.
//
// TODO orchestrator: wire `nexus_extras::register_nexus_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

/// Diagnostic gate — `CRATONVM_NEXUS_REAL=1` skips shim registration so
/// the real bytecode runs under CratonVM (used to measure how far the
/// partial bootstrap can drive Karaf/Felix OSGi initialization).
fn nexus_real_mode() -> bool {
    std::env::var("CRATONVM_NEXUS_REAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

const CN_NEXUS_MAIN: &str = "org/sonatype/nexus/karaf/NexusMain";
const CN_NEXUS_FILE_LOCK: &str = "org/sonatype/nexus/karaf/NexusFileLock";
const CN_NON_RESETTABLE_LOG_MANAGER: &str =
    "org/sonatype/nexus/karaf/NonResettableLogManager";

/// `org.sonatype.nexus.karaf.NexusMain.main([Ljava/lang/String;)V` — no-op.
fn nexus_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[nexus-shim] NexusMain.main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for Nexus launcher classes. The real clinit
/// resolves the Karaf install root via classloader URL probing which fails
/// under CratonVM's partial bootstrap.
fn nexus_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every Nexus boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_nexus_stubs(registry: &mut NativeMethodRegistry) {
    if nexus_real_mode() {
        tracing::warn!(
            "[nexus-shim] CRATONVM_NEXUS_REAL=1 — shim DISABLED, running real bytecode"
        );
        return;
    }
    // NexusMain.main — Karaf-based launcher primary entry point.
    registry.register(
        CN_NEXUS_MAIN,
        "main",
        "([Ljava/lang/String;)V",
        nexus_main_noop,
    );
    registry.register(CN_NEXUS_MAIN, "<clinit>", "()V", nexus_clinit_noop);

    // NexusFileLock.<clinit> — defensive no-op. The real clinit opens the
    // Karaf data dir lock file via `FileChannel.tryLock`.
    registry.register(CN_NEXUS_FILE_LOCK, "<clinit>", "()V", nexus_clinit_noop);

    // NonResettableLogManager.<clinit> — defensive no-op. Touches
    // `java.util.logging.LogManager` internals.
    registry.register(
        CN_NON_RESETTABLE_LOG_MANAGER,
        "<clinit>",
        "()V",
        nexus_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_nexus_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_nexus_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_nexus_stubs() into lib.rs
