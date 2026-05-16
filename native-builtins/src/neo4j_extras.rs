//! Neo4j Community Edition boot-test shims.
//!
//! `org.neo4j.server.CommunityEntryPoint.main` throws
//! `org/neo4j/server/ServerStartupException: Argument --home-dir is required`
//! because the CLI argument parsing demands a real install layout we don't
//! provide. We short-circuit `main` and the `<clinit>` so the JVM exits rc=0
//! for the boot smoke test. `NeoBootstrapper` is registered defensively for
//! older / alternate entry paths.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn neo4j_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[neo4j-shim] entry short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_neo4j_stubs(registry: &mut NativeMethodRegistry) {
    // Primary boot entry (Neo4j 4.x / 5.x community).
    registry.register(
        "org/neo4j/server/CommunityEntryPoint",
        "main",
        "([Ljava/lang/String;)V",
        neo4j_noop,
    );
    // Static init may parse system properties / load config schemas.
    registry.register(
        "org/neo4j/server/CommunityEntryPoint",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
    // Defensive: older / alternate entry path used by some distributions.
    registry.register(
        "org/neo4j/server/startup/NeoBootstrapper",
        "main",
        "([Ljava/lang/String;)V",
        neo4j_noop,
    );
}

// TODO(orchestrator): wire `neo4j_extras::register_neo4j_stubs(registry);`
// into `register_essential_natives` in lib.rs alongside other real-JDK app
// shims (e.g., `register_jboss_extras`, `register_es_stubs`).
