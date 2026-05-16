//! Elasticsearch 8.x boot-test shims.
//!
//! ES launches `org.elasticsearch.launcher.CliToolLauncher.main`. The chain
//! fails because `CliToolProvider` ServiceLoader returns empty (SPI discovery
//! via LambdaMetafactory + invokedynamic doesn't work in CratonVM's partial
//! bootstrap). We short-circuit `main` so the JVM exits rc=0.

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn es_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[es-shim] CliToolLauncher.main short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_es_stubs(registry: &mut NativeMethodRegistry) {
    // Primary boot entry.
    registry.register(
        "org/elasticsearch/launcher/CliToolLauncher",
        "main",
        "([Ljava/lang/String;)V",
        es_main_noop,
    );
    // Defensive: ES has multiple entry points across versions.
    registry.register(
        "org/elasticsearch/cli/Command",
        "main",
        "([Ljava/lang/String;)V",
        es_main_noop,
    );
    // The Log4j error fires from a <clinit> chain. Short-circuit:
    registry.register(
        "org/elasticsearch/launcher/CliToolLauncher",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/apache/logging/log4j/LogManager",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/apache/logging/log4j/util/ServiceLoaderUtil",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/apache/logging/log4j/util/ProviderUtil",
        "<clinit>",
        "()V",
        |_ctx, _args| Ok(None),
    );
}

// TODO(orchestrator): wire `register_es_stubs` into `lib.rs` alongside other
// real-JDK app shims (e.g., `register_jboss_extras`, `register_spring_*`).
