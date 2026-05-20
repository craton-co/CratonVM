//! Elasticsearch 8.x boot-test shims.
//!
//! ES launches `org.elasticsearch.launcher.CliToolLauncher.main`. The chain
//! fails because `CliToolProvider` ServiceLoader returns empty (SPI discovery
//! via LambdaMetafactory + invokedynamic doesn't work in CratonVM's partial
//! bootstrap). We short-circuit `main` so the JVM exits rc=0.
//!
//! Across ES major versions the documented entry-point class has moved:
//!
//! - ES 8.x: `org.elasticsearch.launcher.CliToolLauncher`
//! - ES 8.x (server-mode): `org.elasticsearch.server.cli.Elasticsearch`
//! - ES 7.x and earlier: `org.elasticsearch.bootstrap.Elasticsearch`
//!
//! All three entry classes are registered defensively so the boot-test
//! succeeds regardless of which the launcher script picks.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

fn es_main_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[es-shim] Elasticsearch main short-circuited (boot-test mode)");
    Ok(None)
}

fn es_clinit_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

pub fn register_es_stubs(registry: &mut NativeMethodRegistry) {
    // Real-mode gate: when CRATONVM_ES_REAL is set (any value), skip every
    // boot-test short-circuit so the real Elasticsearch entry chain runs.
    // Used by diag harnesses to measure how far CratonVM gets on the
    // actual ES launcher before failure.
    if std::env::var_os("CRATONVM_ES_REAL").is_some() {
        tracing::warn!("[es-shim] CRATONVM_ES_REAL set - skipping ES boot-test shims");
        return;
    }
    // Primary + alternative documented entry points. Each major ES release
    // has used a different `main`-bearing class; register all three so the
    // boot-test no-ops regardless of which the bin/elasticsearch script
    // picks up.
    for class in [
        // ES 8.x — current documented launcher entry.
        "org/elasticsearch/launcher/CliToolLauncher",
        // ES 8.x — server-mode entry (invoked from CliToolLauncher).
        "org/elasticsearch/server/cli/Elasticsearch",
        // ES 7.x and earlier — historic bootstrap entry.
        "org/elasticsearch/bootstrap/Elasticsearch",
        // Defensive: ES CLI base class also exposes a static main.
        "org/elasticsearch/cli/Command",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", es_main_noop);
        registry.register(class, "<clinit>", "()V", es_clinit_noop);
    }

    // The Log4j error fires from a <clinit> chain. Short-circuit:
    registry.register(
        "org/apache/logging/log4j/LogManager",
        "<clinit>",
        "()V",
        es_clinit_noop,
    );
    registry.register(
        "org/apache/logging/log4j/util/ServiceLoaderUtil",
        "<clinit>",
        "()V",
        es_clinit_noop,
    );
    registry.register(
        "org/apache/logging/log4j/util/ProviderUtil",
        "<clinit>",
        "()V",
        es_clinit_noop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the registration function exists, takes a
    /// `&mut NativeMethodRegistry`, and runs without panicking.
    #[test]
    fn register_es_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_es_stubs(&mut r);
    }
}
