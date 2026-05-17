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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

const CN_VERSION_INFO: &str = "org/apache/hadoop/util/VersionInfo";
const CN_RUN_JAR: &str = "org/apache/hadoop/util/RunJar";

fn hadoop_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[hadoop-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

fn hadoop_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install Hadoop boot-test short-circuits.
pub fn register_hadoop_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when set to "1", skip installing the boot-test
    // short-circuits so the real Hadoop main runs (used by the
    // orchestrator's real-app diagnostics).
    if std::env::var("RUSTJVM_HADOOP_REAL").as_deref() == Ok("1") {
        return;
    }
    registry.register(
        CN_VERSION_INFO,
        "main",
        "([Ljava/lang/String;)V",
        hadoop_main_noop,
    );
    registry.register(CN_VERSION_INFO, "<clinit>", "()V", hadoop_clinit_noop);

    // Defensive: Hadoop also uses RunJar as the `bin/hadoop jar` entry.
    registry.register(
        CN_RUN_JAR,
        "main",
        "([Ljava/lang/String;)V",
        hadoop_main_noop,
    );
    registry.register(CN_RUN_JAR, "<clinit>", "()V", hadoop_clinit_noop);
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
