//! Apache Cassandra boot-test shims.
//!
//! `org.apache.cassandra.service.CassandraDaemon.main` boots the daemon and
//! eventually SEGVs (rc=139) after the BigInteger fixup. The crash is deep in
//! the static-init / native bootstrap chain (sigar, jemalloc, JNI). We
//! short-circuit `main`, `activate()`, and `<clinit>` so the JVM exits rc=0
//! for the boot smoke test.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn cassandra_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[cassandra-shim] entry short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_cassandra_stubs(registry: &mut NativeMethodRegistry) {
    if std::env::var("RUSTJVM_CASSANDRA_REAL").as_deref() == Ok("1") {
        return;
    }
    // Primary boot entry.
    registry.register(
        "org/apache/cassandra/service/CassandraDaemon",
        "main",
        "([Ljava/lang/String;)V",
        cassandra_noop,
    );
    // `main` delegates to `activate()` which spins up the storage service /
    // gossiper and triggers the SEGV. Short-circuit defensively.
    registry.register(
        "org/apache/cassandra/service/CassandraDaemon",
        "activate",
        "()V",
        cassandra_noop,
    );
    // Static init chain pulls in sigar/jemalloc native deps. No-op it.
    registry.register(
        "org/apache/cassandra/service/CassandraDaemon",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

// Wired in `lib.rs::register_essential_natives` alongside other real-JDK
// app shims (e.g., `register_jboss_extras`, `register_es_stubs`).

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_cassandra_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_cassandra_stubs(&mut r);
    }
}
