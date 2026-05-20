//! Apache Hadoop boot-test shims.
//!
//! Hadoop's `bin/hadoop version` entry point is
//! `org/apache/hadoop/util/VersionInfo.main`. The `bin/hadoop jar`
//! dispatcher uses `org/apache/hadoop/util/RunJar.main`. Both pull in
//! Hadoop configuration / FileSystem bootstrap that CratonVM cannot
//! fully drive today.
//!
//! # Strategy
//!
//! Short-circuit `main` and `<clinit>` on both entry classes so the
//! JVM returns rc=0 without exercising the Hadoop bootstrap chain.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

#[allow(dead_code)]
const CN_VERSION_INFO: &str = "org/apache/hadoop/util/VersionInfo";
#[allow(dead_code)]
const CN_RUN_JAR: &str = "org/apache/hadoop/util/RunJar";

#[allow(dead_code)]
fn hadoop_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[hadoop-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

#[allow(dead_code)]
fn hadoop_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install Hadoop boot-test short-circuits.
pub fn register_hadoop_stubs(registry: &mut NativeMethodRegistry) {
    // DISABLED per "no synthetic stubs" policy. All four registrations
    // here were fake-main / fake-<clinit> no-ops for VersionInfo and RunJar,
    // masking the real failure path. The orchestrator wants the first
    // real-bytecode failure surfaced, then dispatches follow-up fix agents.
    let _ = registry;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_hadoop_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_hadoop_stubs(&mut r);
    }
}
