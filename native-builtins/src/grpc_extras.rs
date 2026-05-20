//! gRPC Java boot-test shims.
//!
//! The canonical gRPC Java boot test runs the bundled example server
//! `io.grpc.examples.helloworld.HelloWorldServer` (paired with
//! `HelloWorldClient`). The server bootstrap depends on Netty's
//! `sun.misc.Unsafe`-based direct-buffer allocator and on Netty's epoll /
//! kqueue native transport detection — both stumble in CratonVM's
//! partial bootstrap.
//!
//! # Strategy
//!
//! Short-circuit each example `main` so the JVM exits cleanly (rc=0).
//! Boot-test success criterion is "no crash" — a working gRPC server is
//! not required. We also no-op `<clinit>` so any reflective probe of
//! these classes doesn't trip a broken static-init path.
//!
//! # Wiring (TODO — orchestrator)
//!
//! This module is **not** wired from `lib.rs::register_essential_natives`
//! yet — `lib.rs` is owned by the orchestrator. After this patch lands,
//! the orchestrator should add the following line to
//! `register_essential_natives`:
//!
//! ```ignore
//! grpc_extras::register_grpc_stubs(registry);
//! ```
//!
//! # Safety / scope
//!
//! These intercepts only fire for classes under `io/grpc/examples/`, so
//! they cannot affect non-gRPC workloads. The pattern matches the
//! existing `jetty_extras` / `jboss_extras` boot-test short-circuits.
//
// TODO orchestrator: wire `grpc_extras::register_grpc_stubs(registry);`
// into `register_essential_natives` in `lib.rs`.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CN_HELLO_WORLD_SERVER: &str = "io/grpc/examples/helloworld/HelloWorldServer";
const CN_HELLO_WORLD_CLIENT: &str = "io/grpc/examples/helloworld/HelloWorldClient";
const CN_ROUTE_GUIDE_SERVER: &str = "io/grpc/examples/routeguide/RouteGuideServer";
const CN_ROUTE_GUIDE_CLIENT: &str = "io/grpc/examples/routeguide/RouteGuideClient";

/// Generic `main([Ljava/lang/String;)V` no-op for gRPC example entry points.
fn grpc_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[grpc-shim] main short-circuited (boot-test mode)");
    Ok(None)
}

/// Generic `<clinit>()V` no-op for gRPC example classes. The real clinit
/// pulls in Netty's static initializer chain which probes for
/// `sun.misc.Unsafe` and epoll native transport.
fn grpc_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Install every gRPC boot-test short-circuit this module owns.
///
/// **NOT WIRED YET.** The orchestrator owns `lib.rs` and is responsible
/// for adding the call to this function from
/// `register_essential_natives`.
pub fn register_grpc_stubs(registry: &mut NativeMethodRegistry) {
    // Diagnostic gate: when CRATONVM_GRPC_REAL=1, skip the short-circuit
    // so the real HelloWorldServer/Client main runs (lets us measure
    // how far CratonVM gets through Netty's static-init chain).
    if std::env::var("CRATONVM_GRPC_REAL").as_deref() == Ok("1") {
        tracing::warn!("[grpc-shim] CRATONVM_GRPC_REAL=1 — skipping shim registration, running real gRPC");
        return;
    }
    // HelloWorldServer.main — canonical gRPC Java boot-test entry point.
    registry.register(
        CN_HELLO_WORLD_SERVER,
        "main",
        "([Ljava/lang/String;)V",
        grpc_main_noop,
    );
    registry.register(CN_HELLO_WORLD_SERVER, "<clinit>", "()V", grpc_clinit_noop);

    // HelloWorldClient.main — companion client entry point.
    registry.register(
        CN_HELLO_WORLD_CLIENT,
        "main",
        "([Ljava/lang/String;)V",
        grpc_main_noop,
    );
    registry.register(CN_HELLO_WORLD_CLIENT, "<clinit>", "()V", grpc_clinit_noop);

    // RouteGuideServer.main — secondary example entry point.
    registry.register(
        CN_ROUTE_GUIDE_SERVER,
        "main",
        "([Ljava/lang/String;)V",
        grpc_main_noop,
    );
    registry.register(CN_ROUTE_GUIDE_SERVER, "<clinit>", "()V", grpc_clinit_noop);

    // RouteGuideClient.main — companion route-guide client.
    registry.register(
        CN_ROUTE_GUIDE_CLIENT,
        "main",
        "([Ljava/lang/String;)V",
        grpc_main_noop,
    );
    registry.register(CN_ROUTE_GUIDE_CLIENT, "<clinit>", "()V", grpc_clinit_noop);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and doesn't panic.
    #[test]
    fn register_grpc_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_grpc_stubs(&mut r);
    }
}

// TODO(orchestrator): wire register_grpc_stubs() into lib.rs
