use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn logback_context_construction_and_state_are_not_native_overridden() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    register_slf4j_natives(&mut registry);

    for (class_name, method_name, descriptor) in [
        ("ch/qos/logback/classic/LoggerContext", "<init>", "()V"),
        (
            "ch/qos/logback/core/ContextBase",
            "getObject",
            "(Ljava/lang/String;)Ljava/lang/Object;",
        ),
        (
            "ch/qos/logback/core/ContextBase",
            "putObject",
            "(Ljava/lang/String;Ljava/lang/Object;)V",
        ),
        (
            "ch/qos/logback/classic/LoggerContext",
            "getStatusManager",
            "()Lch/qos/logback/core/status/StatusManager;",
        ),
        (
            "ch/qos/logback/classic/LoggerContext",
            "getTurboFilterList",
            "()Lch/qos/logback/classic/spi/TurboFilterList;",
        ),
        // FIXED 2026-07-26 (loggingapplicationlistenertests-logbacklogsystemtests-reset-noop):
        // `reset()` (and `start`/`stop`/`isStarted`) were stale no-ops
        // that silently skipped `Logger.recursiveReset()` /
        // `detachAndStopAllAppenders()`, leaking every previously
        // attached ConsoleAppender across `@Test` methods. Guard
        // against reintroducing any of these.
        ("ch/qos/logback/classic/LoggerContext", "reset", "()V"),
        ("ch/qos/logback/classic/LoggerContext", "stop", "()V"),
        ("ch/qos/logback/classic/LoggerContext", "start", "()V"),
        ("ch/qos/logback/classic/LoggerContext", "isStarted", "()Z"),
        // FIXED 2026-07-27 (wave-2 inline-constant stub removal):
        // `getName()` returned a constant `"default"` and `setName` was a
        // no-op, so `<contextName>`, Spring Boot's
        // `LoggingSystemProperties` and `%contextName` could never observe
        // a renamed context. Real `ContextBase` owns both.
        (
            "ch/qos/logback/classic/LoggerContext",
            "getName",
            "()Ljava/lang/String;",
        ),
        (
            "ch/qos/logback/classic/LoggerContext",
            "setName",
            "(Ljava/lang/String;)V",
        ),
        // FIXED 2026-07-17 (conditionevaluationreport-capturedoutput-empty-cluster):
        // LoggerContext.getLogger used to fabricate a throwaway synthetic
        // Logger, and every Logger/Log instance method below was a
        // native no-op — so log output never reached a real
        // ConsoleAppender/System.out, and Spring Boot's CapturedOutput
        // always saw the empty string. Guard against reintroducing any
        // of these.
        (
            "ch/qos/logback/classic/LoggerContext",
            "getLogger",
            "(Ljava/lang/String;)Lch/qos/logback/classic/Logger;",
        ),
        (
            "ch/qos/logback/classic/LoggerContext",
            "getLogger",
            "(Ljava/lang/Class;)Lch/qos/logback/classic/Logger;",
        ),
        (
            "ch/qos/logback/classic/Logger",
            "addAppender",
            "(Lch/qos/logback/core/Appender;)V",
        ),
        (
            "ch/qos/logback/classic/Logger",
            "info",
            "(Ljava/lang/String;)V",
        ),
        (
            "ch/qos/logback/classic/Logger",
            "debug",
            "(Ljava/lang/String;)V",
        ),
        (
            "org/apache/commons/logging/LogFactory",
            "getLog",
            "(Ljava/lang/Class;)Lorg/apache/commons/logging/Log;",
        ),
        (
            "org/apache/commons/logging/LogFactory",
            "getLog",
            "(Ljava/lang/String;)Lorg/apache/commons/logging/Log;",
        ),
        (
            "org/apache/commons/logging/Log",
            "info",
            "(Ljava/lang/Object;)V",
        ),
        (
            "org/apache/commons/logging/Log",
            "debug",
            "(Ljava/lang/Object;)V",
        ),
    ] {
        assert!(
            registry
                .find(class_name, method_name, descriptor)
                .is_none(),
            "{class_name}.{method_name}{descriptor} must use real Logback/commons-logging bytecode"
        );
    }
}
