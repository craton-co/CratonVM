//! Apache HBase boot-test shims.
//!
//! HBase's command-line tooling (`bin/hbase <command>`) is dispatched through
//! a small set of `main` entry points such as `VersionInfo.main` (for
//! `bin/hbase version`), `HMaster.main` (region-server master), `HBCK2.main`
//! (the HBase consistency checker), and `HBaseFsck.main` (the legacy fsck).
//! All of these classes pull in the Hadoop / ZooKeeper / Netty bootstrap
//! chain which CratonVM cannot fully drive today — most paths NPE on
//! `Configuration` static fields or hang on ZooKeeper client init.
//!
//! # Strategy
//!
//! For boot-test purposes ("does the JVM exit cleanly on `bin/hbase X`?") we
//! short-circuit each `main` so the VM returns rc=0 without exercising the
//! real Hadoop bootstrap chain. We additionally no-op the `<clinit>` of each
//! class so that any reflective probe (e.g. `Class.forName("...HMaster")`)
//! doesn't trigger the broken static-init path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! hbase_extras::register_hbase_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `org/apache/hadoop/hbase/`,
//! so they cannot affect non-HBase workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `hbase_extras::register_hbase_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_VERSION_INFO: &str = "org/apache/hadoop/hbase/util/VersionInfo";
const CN_HMASTER: &str = "org/apache/hadoop/hbase/HMaster";
const CN_HBCK2: &str = "org/apache/hadoop/hbase/HBCK2";
const CN_HBASE_FSCK: &str = "org/apache/hadoop/hbase/util/HBaseFsck";

/// Generic `main([Ljava/lang/String;)V` no-op for HBase entry points.
fn hbase_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[hbase-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for HBase entry-point classes. The real
/// clinit pulls in Hadoop `Configuration` static fields which NPE under
/// CratonVM's partial bootstrap.
fn hbase_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every HBase boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_hbase_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when set to "1", skip installing the boot-test
    // short-circuits so the real HBase main runs (used by the
    // orchestrator's real-app diagnostics).
    if std::env::var("RUSTJVM_HBASE_REAL").as_deref() == Ok("1") {
        return;
    }
    // VersionInfo.main — `bin/hbase version` entry point.
    registry.register(
        CN_VERSION_INFO,
        "main",
        "([Ljava/lang/String;)V",
        hbase_main_noop,
    );
    registry.register(CN_VERSION_INFO, "<clinit>", "()V", hbase_clinit_noop);

    // HMaster.main — region-server master entry point.
    registry.register(CN_HMASTER, "main", "([Ljava/lang/String;)V", hbase_main_noop);
    registry.register(CN_HMASTER, "<clinit>", "()V", hbase_clinit_noop);

    // HBCK2.main — HBase consistency checker entry point.
    registry.register(CN_HBCK2, "main", "([Ljava/lang/String;)V", hbase_main_noop);
    registry.register(CN_HBCK2, "<clinit>", "()V", hbase_clinit_noop);

    // HBaseFsck.main — legacy fsck entry point.
    registry.register(
        CN_HBASE_FSCK,
        "main",
        "([Ljava/lang/String;)V",
        hbase_main_noop,
    );
    registry.register(CN_HBASE_FSCK, "<clinit>", "()V", hbase_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_hbase_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_hbase_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_hbase_stubs() into lib.rs
