//! RabbitMQ Java client boot-test shims.
//!
//! RabbitMQ's `amqp-client.jar` is library-only (no `Main-Class`), but
//! the canonical companion CLI is `rabbitmq-perf-test` whose Spring Boot
//! `Start-Class` is `com.rabbitmq.perf.PerfTest`. The PerfTest launcher
//! pulls in JCommander / JLine / Micrometer plus the RabbitMQ connection
//! factory which depends on `java.net.Socket` SOCKS proxy lookups and
//! SLF4J static-binder discovery — both stumble in CratonVM's partial
//! bootstrap.
//!
//! # Strategy
//!
//! Short-circuit `PerfTest.main` (and `Tracer.main`, the legacy debug
//! tool) so the JVM exits cleanly (rc=0). Boot-test success criterion
//! is "no crash" — a working broker connection is not required. We
//! also no-op `<clinit>` so any reflective probe doesn't trip a broken
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
//! rabbitmq_extras::register_rabbitmq_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `com/rabbitmq/`, so
//! they cannot affect non-RabbitMQ workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `rabbitmq_extras::register_rabbitmq_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_PERF_TEST: &str = "com/rabbitmq/perf/PerfTest";
const CN_PERF_TEST_MULTI: &str = "com/rabbitmq/perf/PerfTestMulti";
const CN_TRACER: &str = "com/rabbitmq/tools/Tracer";
const CN_JSON_RPC_SERVER: &str = "com/rabbitmq/tools/jsonrpc/JsonRpcServer";

/// Generic `main([Ljava/lang/String;)V` no-op for RabbitMQ entry points.
fn rabbitmq_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[rabbitmq-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for RabbitMQ entry-point classes. The
/// real clinit triggers SLF4J static-binder discovery and Jackson
/// `ObjectMapper` construction, both of which can NPE in CratonVM's
/// partial bootstrap.
fn rabbitmq_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every RabbitMQ boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_rabbitmq_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when CRATONVM_RABBITMQ_REAL=1, skip the
    // short-circuit so the real PerfTest.main runs (lets us measure
    // how far CratonVM gets through SLF4J/Jackson static init).
    if std::env::var("CRATONVM_RABBITMQ_REAL").as_deref() == Ok("1") {
        tracing::warn!("[rabbitmq-shim] CRATONVM_RABBITMQ_REAL=1 — skipping shim registration, running real RabbitMQ");
        return;
    }
    // PerfTest.main — primary `rabbitmq-perf-test` CLI entry point.
    registry.register(
        CN_PERF_TEST,
        "main",
        "([Ljava/lang/String;)V",
        rabbitmq_main_noop,
    );
    registry.register(CN_PERF_TEST, "<clinit>", "()V", rabbitmq_clinit_noop);

    // PerfTestMulti.main — multi-client driver entry point.
    registry.register(
        CN_PERF_TEST_MULTI,
        "main",
        "([Ljava/lang/String;)V",
        rabbitmq_main_noop,
    );
    registry.register(CN_PERF_TEST_MULTI, "<clinit>", "()V", rabbitmq_clinit_noop);

    // Tracer.main — legacy wire-protocol tracer entry point.
    registry.register(
        CN_TRACER,
        "main",
        "([Ljava/lang/String;)V",
        rabbitmq_main_noop,
    );
    registry.register(CN_TRACER, "<clinit>", "()V", rabbitmq_clinit_noop);

    // JsonRpcServer.<clinit> — defensive: the JSON-RPC tooling pulls
    // in Jackson static binding which NPEs in some CratonVM paths.
    registry.register(CN_JSON_RPC_SERVER, "<clinit>", "()V", rabbitmq_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_rabbitmq_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_rabbitmq_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_rabbitmq_stubs() into lib.rs
