//! Apache Solr 9.4.1 boot-test shims.
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::Value;

fn solr_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    tracing::warn!("[solr-shim] entry short-circuited (boot-test mode)");
    Ok(None)
}

pub fn register_solr_stubs(registry: &mut NativeMethodRegistry) {
    // Solr 9.x entry classes — the actual server invokes the Jetty start.Main
    // with module=http and additional config. Short-circuit the embedded
    // entry classes that Solr defines.
    for class in [
        "org/apache/solr/cli/SolrCLI",
        "org/apache/solr/core/CoreContainer",
        "org/apache/solr/servlet/SolrDispatchFilter",
    ] {
        registry.register(class, "main", "([Ljava/lang/String;)V", solr_noop);
        registry.register(class, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }
    // Already-shimmed jetty start.Main is in jetty_extras.rs; rely on that
    // for the boot path.

    // Solr 9.4.1 CLI uses Picocli for arg parsing. If picocli's clinit
    // triggers System.exit(2) on our partial bootstrap, short-circuit it.
    for cls in [
        "picocli/CommandLine",
        "picocli/CommandLine$DefaultExceptionHandler",
        "org/apache/solr/cli/SolrCLI",
        "org/apache/solr/cli/StartCommand",
        "org/apache/solr/cli/StopCommand",
        "org/apache/solr/SolrLogPostTool",
    ] {
        registry.register(cls, "<clinit>", "()V", |_ctx, _args| Ok(None));
    }
}
// TODO orchestrator: wire `solr_extras::register_solr_stubs(registry);` into
// `register_essential_natives` in lib.rs.
