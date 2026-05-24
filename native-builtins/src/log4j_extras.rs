//! Log4j 2.x API shims: keep `org.apache.logging.log4j.LogManager` callers
//! from NPE'ing when the real log4j-core provider chain doesn't bootstrap
//! under CratonVM.
//!
//! # Symptom this addresses
//!
//! Spark (and many other apps that pull in `log4j-api-2.x.jar`) call
//! `LogManager.getLogger(...)` early during startup. The real bytecode of
//! `getLogger` delegates to `getContext(...).getLogger(name, fqcn, mf)`.
//! `getContext()` in turn calls `getFactory().getContext(...)`. CratonVM
//! already short-circuits `LogManager.<clinit>` to a no-op (see
//! `elasticsearch_extras.rs`) because the real clinit walks a ServiceLoader
//! chain that crashes on LambdaMetafactory / invokedynamic. That leaves
//! the static `factory` field null, so `factory.getContext(...)` throws:
//!
//! ```text
//! [WF-NPE-TRACE] msg=NullPointerException { message: Some("Cannot invoke getContext on null") }
//! [WF-NPE-STK 5] org/apache/logging/log4j/LogManager.getContext pc=16
//! [WF-NPE-STK 4] org/apache/logging/log4j/LogManager.getLogger pc=8
//! [WF-NPE-STK 3] org/apache/spark/internal/Logging.initializeLogging pc=12
//! ```
//!
//! # Fix
//!
//! Two-layer intercept on the `org/apache/logging/log4j/LogManager` static
//! surface:
//!
//!   1. Every `getContext(...)` overload returns a synthetic
//!      `org.apache.logging.log4j.simple.SimpleLoggerContext` instance.
//!      This is the no-op fallback context the log4j-api jar ships for
//!      exactly this scenario (no provider on the classpath).
//!   2. Every `getLogger(...)` overload returns a synthetic
//!      `org.apache.logging.log4j.simple.SimpleLogger` instance. Callers
//!      then invoke `Logger.info/warn/error/debug/isXxxEnabled` on this
//!      object via the `Logger` interface; we register no-op natives on
//!      `SimpleLogger` for each of these so the bytecode of SimpleLogger
//!      (which may itself touch null fields) never runs.
//!
//! Together these mean any `LogManager` caller observes a complete,
//! non-null logging pipeline that silently discards every record.
//! Equivalent to running with the JDK's `NullLogger` — the app boots,
//! nothing gets logged, no NPEs propagate.
//!
//! # Scope / safety
//!
//! Registration is unconditional from `register_essential_natives` so the
//! shim is always available (no env-var gate). The shim only fires when a
//! caller actually invokes one of these `LogManager` static methods, so
//! apps that don't touch log4j observe no behavioural change.
//!
//! No security manager interaction: CratonVM runs without a SecurityManager
//! at init phase 4, so the privileged-action wrappers the real bytecode
//! uses are equivalent to plain method calls.

#![allow(clippy::needless_pass_by_value)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

const CN_LOGMANAGER: &str = "org/apache/logging/log4j/LogManager";
const CN_SIMPLE_LOGGER_CONTEXT: &str = "org/apache/logging/log4j/simple/SimpleLoggerContext";
const CN_SIMPLE_LOGGER: &str = "org/apache/logging/log4j/simple/SimpleLogger";
const CN_ABSTRACT_LOGGER: &str = "org/apache/logging/log4j/spi/AbstractLogger";
/// log4j-core's concrete `Logger` impl. Many apps (Spark, Hadoop, Hive)
/// cast the result of `LogManager.getLogger(...)` to this type rather
/// than to the `Logger` interface, so when `log4j-core` is on the
/// classpath we must hand out instances of THIS class to avoid CCE.
const CN_CORE_LOGGER: &str = "org/apache/logging/log4j/core/Logger";
const CN_CORE_LOGGER_CONTEXT: &str = "org/apache/logging/log4j/core/LoggerContext";
/// log4j-core's concrete `LoggerContextFactory` impl. We hand out
/// instances of this from `LogManager.getFactory()` so callers that
/// invokeinterface `LoggerContextFactory.isClassLoaderDependent()` (e.g.
/// `Log4jLoggerFactory.getContext` in the slf4j bridge) see a non-null
/// receiver.
const CN_CORE_CONTEXT_FACTORY: &str = "org/apache/logging/log4j/core/impl/Log4jContextFactory";
/// Simple fallback factory when log4j-core isn't on the classpath. The
/// log4j-api jar ships `simple.SimpleLoggerContextFactory` for the
/// "no-provider" case.
const CN_SIMPLE_CONTEXT_FACTORY: &str =
    "org/apache/logging/log4j/simple/SimpleLoggerContextFactory";
const CN_LOGGER_CONTEXT_FACTORY: &str = "org/apache/logging/log4j/spi/LoggerContextFactory";
/// log4j-core `Configuration` impls that callers (notably ES's
/// `Loggers.setLevel`) reach via
/// `core.LoggerContext.getConfiguration().getLoggerConfig(name)`. We hand
/// out a synthetic `NullConfiguration` (the log4j-core "no config" type)
/// and pair it with no-op natives on the `Configuration` interface so
/// the receiver call resolves without NPE'ing on uninitialised state.
const CN_NULL_CONFIGURATION: &str = "org/apache/logging/log4j/core/config/NullConfiguration";
const CN_DEFAULT_CONFIGURATION: &str = "org/apache/logging/log4j/core/config/DefaultConfiguration";
const CN_ABSTRACT_CONFIGURATION: &str = "org/apache/logging/log4j/core/config/AbstractConfiguration";
const CN_CONFIGURATION: &str = "org/apache/logging/log4j/core/config/Configuration";
const CN_LOGGER_CONFIG: &str = "org/apache/logging/log4j/core/config/LoggerConfig";

/// Build a synthetic `LoggerContextFactory`. Prefers log4j-core's
/// `Log4jContextFactory`, falling back to `SimpleLoggerContextFactory`
/// from log4j-api. Both implement
/// `org.apache.logging.log4j.spi.LoggerContextFactory`, so callers that
/// `invokeinterface isClassLoaderDependent()` on the result resolve to
/// our registered native (which returns false, steering callers down
/// the `aconst_null` branch and onto our already-shimmed
/// `LogManager.getContext(Z)` path).
fn build_logger_context_factory(ctx: &mut dyn NativeContext) -> ObjectRef {
    if ctx.class_id_by_name(CN_CORE_CONTEXT_FACTORY).is_some()
        || ctx.ensure_class_initialized(CN_CORE_CONTEXT_FACTORY).is_ok()
    {
        return crate::alloc_concurrent_synthetic(ctx, CN_CORE_CONTEXT_FACTORY, 8);
    }
    if ctx.class_id_by_name(CN_SIMPLE_CONTEXT_FACTORY).is_some()
        || ctx.ensure_class_initialized(CN_SIMPLE_CONTEXT_FACTORY).is_ok()
    {
        return crate::alloc_concurrent_synthetic(ctx, CN_SIMPLE_CONTEXT_FACTORY, 8);
    }
    // Last resort: an instance of the interface itself (synthetic alloc
    // accepts an interface name and yields a usable mirror).
    crate::alloc_concurrent_synthetic(ctx, CN_LOGGER_CONTEXT_FACTORY, 8)
}

/// Build a synthetic `LoggerContext`. Prefers `org.apache.logging.log4j
/// .core.LoggerContext` (the concrete log4j-core class) so callers that
/// cast the result to `core.LoggerContext` don't trip ClassCastException.
/// Falls back to `simple.SimpleLoggerContext` when log4j-core isn't on
/// the classpath. Both implement `org.apache.logging.log4j.spi.LoggerContext`.
fn build_logger_context(ctx: &mut dyn NativeContext) -> ObjectRef {
    if ctx.class_id_by_name(CN_CORE_LOGGER_CONTEXT).is_some()
        || ctx.ensure_class_initialized(CN_CORE_LOGGER_CONTEXT).is_ok()
    {
        return crate::alloc_concurrent_synthetic(ctx, CN_CORE_LOGGER_CONTEXT, 16);
    }
    crate::alloc_concurrent_synthetic(ctx, CN_SIMPLE_LOGGER_CONTEXT, 16)
}

/// Build a synthetic `Configuration` instance. Prefers
/// `NullConfiguration` (the log4j-core "no config" subclass) since its
/// `getLoggerConfig` semantics most closely match our no-op contract.
/// Falls back to `DefaultConfiguration`, then `AbstractConfiguration`
/// when neither resolves on the classpath.
fn build_configuration(ctx: &mut dyn NativeContext) -> ObjectRef {
    for cls in [CN_NULL_CONFIGURATION, CN_DEFAULT_CONFIGURATION, CN_ABSTRACT_CONFIGURATION] {
        if ctx.class_id_by_name(cls).is_some() || ctx.ensure_class_initialized(cls).is_ok() {
            return crate::alloc_concurrent_synthetic(ctx, cls, 32);
        }
    }
    // Last resort: the interface itself.
    crate::alloc_concurrent_synthetic(ctx, CN_CONFIGURATION, 8)
}

/// Build a synthetic `LoggerConfig`. Used as the return value from
/// `Configuration.getLoggerConfig(String)` so callers that chain
/// `.setLevel(level)` (e.g. `org.elasticsearch.common.logging.Loggers
/// .setLevel`) find a non-null receiver. The shim natives registered on
/// `LoggerConfig` make every subsequent mutator a no-op, matching the
/// "logging silently swallows everything" contract of the rest of this
/// module.
fn build_logger_config(ctx: &mut dyn NativeContext) -> ObjectRef {
    crate::alloc_concurrent_synthetic(ctx, CN_LOGGER_CONFIG, 16)
}

/// Build a synthetic `Logger` carrying just the supplied logger name
/// (or `<root>` if `None`). Prefers `org.apache.logging.log4j.core.Logger`
/// (the concrete log4j-core class) so apps that cast the result to
/// `core.Logger` (e.g. Spark's `Logging.initializeLogging`) don't trip
/// ClassCastException. Falls back to `simple.SimpleLogger`.
///
/// Slot 0 in both `core.Logger` and `simple.SimpleLogger` is the
/// inherited `AbstractLogger.name` field (the only instance field on
/// AbstractLogger), which `Logger.getName()` reads.
fn build_simple_logger(ctx: &mut dyn NativeContext, name: Option<&str>) -> ObjectRef {
    let cls = if ctx.class_id_by_name(CN_CORE_LOGGER).is_some()
        || ctx.ensure_class_initialized(CN_CORE_LOGGER).is_ok()
    {
        CN_CORE_LOGGER
    } else {
        CN_SIMPLE_LOGGER
    };
    let obj = crate::alloc_concurrent_synthetic(ctx, cls, 16);
    let name_str = ctx.create_string(name.unwrap_or("<root>"));
    ctx.set_field(obj, 0, Value::Object(Some(name_str)));
    obj
}

/// Resolve a caller-supplied identifier (`String`, `Class`, or `Object`)
/// into a logger name. Falls back to `<root>` when the argument is null
/// or unresolvable so we never construct a logger with a null name.
fn extract_logger_name(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> Option<String> {
    match arg {
        Some(Value::Object(Some(o))) => {
            // Try as a String first.
            if let Some(s) = ctx.read_string(*o) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
            // Try as a Class mirror — read its `name` slot if available.
            // We don't have a stable `class_name_of_mirror` API across
            // synthetic/real modes, so fall back to None and let the
            // caller use the `<root>` default.
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// LogManager.getContext overloads
// ---------------------------------------------------------------------------

fn native_get_context_0(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(build_logger_context(ctx)))))
}

// ---------------------------------------------------------------------------
// LogManager.getLogger overloads
// ---------------------------------------------------------------------------

fn native_get_logger_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, None)))))
}

fn native_get_logger_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = extract_logger_name(ctx, args.first());
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, name.as_deref())))))
}

fn native_get_logger_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (Class) — we don't bother reading the class name; the synthetic
    // logger only ever swallows records, so the name is cosmetic.
    let _ = args;
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, None)))))
}

fn native_get_logger_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _ = args;
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, None)))))
}

fn native_get_root_logger(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, Some("")))))) // root has empty name
}

// ---------------------------------------------------------------------------
// SimpleLoggerContext.getLogger overloads — used when a caller obtained a
// LoggerContext via our `getContext` shim and then asks it for a logger.
// ---------------------------------------------------------------------------

fn native_ctx_get_logger_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (this, String)
    let name = extract_logger_name(ctx, args.get(1));
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, name.as_deref())))))
}

fn native_ctx_get_logger_string_mf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (this, String, MessageFactory)
    let name = extract_logger_name(ctx, args.get(1));
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, name.as_deref())))))
}

fn native_ctx_get_logger_name_fqcn_mf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // log4j-api 2.x: `LoggerContext.getLogger(String, String, MessageFactory)`
    // (this, String name, String fqcn, MessageFactory) — the path
    // LogManager.getLogger uses internally.
    let name = extract_logger_name(ctx, args.get(1));
    Ok(Some(Value::Object(Some(build_simple_logger(ctx, name.as_deref())))))
}

// ---------------------------------------------------------------------------
// Logger no-op natives. Each maps to one or more abstract methods declared
// on `org.apache.logging.log4j.Logger` (the interface). They are registered
// on `SimpleLogger` because that is the concrete receiver class our
// `getLogger` shims hand out.
// ---------------------------------------------------------------------------

/// Generic `()V` no-op for `Logger.info/warn/error/debug/trace/fatal`
/// overloads that don't return a value.
fn native_log_void(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Generic `()Z` returning false for `isXxxEnabled` checks. Returning
/// false means "level not enabled" so callers that wrap their log calls
/// in `if (logger.isInfoEnabled()) { ... }` skip the entire log body —
/// the cheapest path through the user's code.
fn native_log_is_enabled(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `Logger.getName()` — return the `name` slot the `build_simple_logger`
/// helper stashed at slot 0. Falls back to a synthesized empty string
/// when the slot is null so callers never observe a null result.
fn native_logger_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    if let Value::Object(Some(s)) = ctx.get_field(this, 0) {
        return Ok(Some(Value::Object(Some(s))));
    }
    let empty = ctx.create_string("");
    Ok(Some(Value::Object(Some(empty))))
}

/// `Logger.getLevel()` — return null. The `Logger` interface contract
/// allows null ("inherit from parent" / "not explicitly set"); callers
/// generally only consult the result to compare against another Level
/// or to format a record, both of which tolerate null.
fn native_logger_get_level(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `Logger.getMessageFactory()` — return null. Same null-tolerance
/// rationale as `getLevel`; the only common caller is the
/// `LogManager.getLogger` path itself, which we already short-circuit.
fn native_logger_get_message_factory(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// `core.Logger.getAppenders()` — return an empty `java.util.HashMap`.
/// Spark's `Logging$.islog4j2DefaultConfigured` reads the result and
/// iterates the entry set; an empty map is the "no appenders configured"
/// answer that lets Spark fall back to its default log4j configuration
/// path instead of NPE'ing on the unpopulated `privateConfig` field.
fn native_core_logger_get_appenders(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let map = crate::alloc_concurrent_synthetic(ctx, "java/util/HashMap", 16);
    Ok(Some(Value::Object(Some(map))))
}

/// `core.Logger.getContext()` — return a synthetic core.LoggerContext.
/// Some callers chain `getLogger().getContext().getXxx()` and would NPE
/// on the unpopulated `context` field.
fn native_core_logger_get_context(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(build_logger_context(ctx)))))
}

/// `core.Logger.getParent()` — return null. The synthetic logger has
/// no parent; callers that walk the chain stop at null.
fn native_core_logger_get_parent(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every log4j 2.x LogManager / Logger shim owned by this module.
///
/// Wired unconditionally from `register_essential_natives` in
/// `native-builtins/src/lib.rs`. No env-var gate — the shims only fire
/// when a caller actually invokes one of these methods, so apps that
/// don't touch log4j are unaffected.
pub fn register_log4j_stubs(registry: &mut NativeMethodRegistry) {
    // --- LogManager.getContext overloads ------------------------------
    // Every overload returns the same synthetic SimpleLoggerContext.
    let get_ctx_descriptors = [
        "()Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Z)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/ClassLoader;Z)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/ClassLoader;ZLjava/lang/Object;)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/ClassLoader;ZLjava/net/URI;)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/ClassLoader;ZLjava/lang/Object;Ljava/net/URI;)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/ClassLoader;ZLjava/lang/Object;Ljava/net/URI;Ljava/lang/String;)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/String;Z)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;Z)Lorg/apache/logging/log4j/spi/LoggerContext;",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;ZLjava/net/URI;Ljava/lang/String;)Lorg/apache/logging/log4j/spi/LoggerContext;",
    ];
    for desc in get_ctx_descriptors {
        registry.register(CN_LOGMANAGER, "getContext", desc, native_get_context_0);
    }

    // --- LogManager.getLogger overloads -------------------------------
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "()Lorg/apache/logging/log4j/Logger;",
        native_get_logger_0,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/String;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_string,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/Class;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_class,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/Object;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_object,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/Class;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_class,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/Object;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_object,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_string,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_0,
    );
    registry.register(
        CN_LOGMANAGER,
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/String;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_string,
    );

    // FormatterLogger overloads — same dispatch, same synthetic logger.
    registry.register(
        CN_LOGMANAGER,
        "getFormatterLogger",
        "()Lorg/apache/logging/log4j/Logger;",
        native_get_logger_0,
    );
    registry.register(
        CN_LOGMANAGER,
        "getFormatterLogger",
        "(Ljava/lang/Class;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_class,
    );
    registry.register(
        CN_LOGMANAGER,
        "getFormatterLogger",
        "(Ljava/lang/Object;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_object,
    );
    registry.register(
        CN_LOGMANAGER,
        "getFormatterLogger",
        "(Ljava/lang/String;)Lorg/apache/logging/log4j/Logger;",
        native_get_logger_string,
    );

    registry.register(
        CN_LOGMANAGER,
        "getRootLogger",
        "()Lorg/apache/logging/log4j/Logger;",
        native_get_root_logger,
    );

    // `LogManager.getFactory` — return a synthetic
    // `LoggerContextFactory`. Many callers (notably the
    // log4j-slf4j2-impl bridge's `Log4jLoggerFactory.getContext`)
    // do NOT null-check the result; they go straight to
    // `invokeinterface LoggerContextFactory.isClassLoaderDependent()`.
    // Returning null there NPEs with "Cannot invoke
    // isClassLoaderDependent on null" (observed in Apache ActiveMQ 5.18
    // boot under CratonVM). We hand out a synthetic instance of
    // `core.impl.Log4jContextFactory` (or the simple fallback) and pair
    // it with a registered `isClassLoaderDependent` native (below) that
    // returns false — steering callers down the `aconst_null` branch
    // and onto `LogManager.getContext(Z)`, which we already shim.
    registry.register(
        CN_LOGMANAGER,
        "getFactory",
        "()Lorg/apache/logging/log4j/spi/LoggerContextFactory;",
        |ctx, _args| Ok(Some(Value::Object(Some(build_logger_context_factory(ctx))))),
    );

    // `LoggerContextFactory.isClassLoaderDependent()Z` — return false on
    // every concrete factory class we hand out from `getFactory`. This
    // satisfies the `Log4jLoggerFactory.getContext` bytecode path:
    //   getFactory().isClassLoaderDependent()
    //     ? StackLocatorUtil.getCallerClass(...) (heavy path)
    //     : null                                  (light path)
    // We register on all three so `invokeinterface` dispatch lands on
    // the native regardless of which class `build_logger_context_factory`
    // returned at runtime. Note: this method has a default
    // implementation on the interface; `vm_exec::check_override` has an
    // allow-list entry pinning the native ahead of the bytecode body.
    for cls in [
        CN_CORE_CONTEXT_FACTORY,
        CN_SIMPLE_CONTEXT_FACTORY,
        CN_LOGGER_CONTEXT_FACTORY,
    ] {
        registry.register(cls, "isClassLoaderDependent", "()Z", native_log_is_enabled);
        // Defensive: also no-op the ctor + clinit for the synthetic
        // alloc path so the real heavy-init bytecode never runs on our
        // synthetic instance.
        registry.register(cls, "<init>", "()V", native_log_void);
        registry.register(cls, "<clinit>", "()V", native_log_void);
        // `hasContext` — return false so the default `shutdown`
        // implementation skips the `getContext` + `terminate` chain.
        registry.register(
            cls,
            "hasContext",
            "(Ljava/lang/String;Ljava/lang/ClassLoader;Z)Z",
            native_log_is_enabled,
        );
    }

    // `LogManager.exists(String) -> boolean` — return false. Callers
    // use this to gate `getLogger` calls; saying "no" forces them to
    // either skip the call or fall through to `getLogger`, which we
    // already shim.
    registry.register(
        CN_LOGMANAGER,
        "exists",
        "(Ljava/lang/String;)Z",
        native_log_is_enabled,
    );

    // `LogManager.shutdown` overloads — no-op.
    for desc in [
        "()V",
        "(Z)V",
        "(ZZ)V",
        "(Lorg/apache/logging/log4j/spi/LoggerContext;)V",
    ] {
        registry.register(CN_LOGMANAGER, "shutdown", desc, native_log_void);
    }

    // --- SimpleLoggerContext.getLogger overloads ----------------------
    // Required for any caller path that obtained a LoggerContext through
    // our `getContext` shim and then calls `ctx.getLogger(name, ...)`.
    registry.register(
        CN_SIMPLE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;)Lorg/apache/logging/log4j/spi/ExtendedLogger;",
        native_ctx_get_logger_string,
    );
    registry.register(
        CN_SIMPLE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/spi/ExtendedLogger;",
        native_ctx_get_logger_string_mf,
    );
    registry.register(
        CN_SIMPLE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_ctx_get_logger_name_fqcn_mf,
    );

    // SimpleLoggerContext.<init> — no-op so the synthetic-alloc path
    // doesn't trip on the real ctor reading uninitialised
    // PropertiesUtil state.
    registry.register(CN_SIMPLE_LOGGER_CONTEXT, "<init>", "()V", native_log_void);

    // --- core.LoggerContext.getLogger overloads -----------------------
    // Same shape as SimpleLoggerContext but for the log4j-core path.
    // Spark / Hadoop / Hive callers go through `core.LoggerContext.
    // getLogger(name, fqcn, mf)` after our `getContext` shim hands them
    // a core.LoggerContext instance.
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;)Lorg/apache/logging/log4j/core/Logger;",
        native_ctx_get_logger_string,
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/core/Logger;",
        native_ctx_get_logger_string_mf,
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)Lorg/apache/logging/log4j/Logger;",
        native_ctx_get_logger_name_fqcn_mf,
    );
    // No-op the ctor and clinit so the heavy bytecode (which builds a
    // Configuration tree, NULL_CONFIGURATION etc.) never runs.
    registry.register(CN_CORE_LOGGER_CONTEXT, "<init>", "()V", native_log_void);
    registry.register(CN_CORE_LOGGER_CONTEXT, "<clinit>", "()V", native_log_void);

    // core.LoggerContext static `getContext` overloads — some callers
    // (Spark `Logging.initializeLogging`) call these directly instead
    // of going through `LogManager.getContext`.
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getContext",
        "()Lorg/apache/logging/log4j/core/LoggerContext;",
        native_get_context_0,
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getContext",
        "(Z)Lorg/apache/logging/log4j/core/LoggerContext;",
        native_get_context_0,
    );

    // core.LoggerContext mutators — Spark's `Logging.initializeLogging`
    // casts the result of `LogManager.getContext(false)` to
    // `core.LoggerContext` and then calls `reconfigure()V` /
    // `setConfigLocation(URI)V` on it. Our synthetic instance has every
    // instance field (notably `externalMap` and `configLocation`) left
    // null because `alloc_concurrent_synthetic` bypasses the real ctor.
    // The real `reconfigure(URI)` bytecode reads `externalMap.get(...)`
    // and trips `NullPointerException: Cannot invoke get on null` at
    // `LoggerContext.java:686` — the canonical Spark boot failure under
    // CratonVM. No-op these mutators so the synthetic context silently
    // accepts the reconfigure request (the rest of the pipeline is
    // already no-op'd via the SimpleLogger / core.Logger surface).
    registry.register(CN_CORE_LOGGER_CONTEXT, "reconfigure", "()V", native_log_void);
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "reconfigure",
        "(Ljava/net/URI;)V",
        native_log_void,
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "reconfigure",
        "(Lorg/apache/logging/log4j/core/config/Configuration;)V",
        native_log_void,
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "setConfigLocation",
        "(Ljava/net/URI;)V",
        native_log_void,
    );
    registry.register(CN_CORE_LOGGER_CONTEXT, "updateLoggers", "()V", native_log_void);
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "updateLoggers",
        "(Lorg/apache/logging/log4j/core/config/Configuration;)V",
        native_log_void,
    );
    registry.register(CN_CORE_LOGGER_CONTEXT, "start", "()V", native_log_void);
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "start",
        "(Lorg/apache/logging/log4j/core/config/Configuration;)V",
        native_log_void,
    );
    registry.register(CN_CORE_LOGGER_CONTEXT, "stop", "()V", native_log_void);
    registry.register(CN_CORE_LOGGER_CONTEXT, "close", "()V", native_log_void);
    registry.register(CN_CORE_LOGGER_CONTEXT, "terminate", "()V", native_log_void);

    // `core.LoggerContext.getConfiguration()` — Elasticsearch's
    // `org.elasticsearch.common.logging.Loggers.setLevel(Logger, Level,
    // List)` does:
    //   ctx = LoggerContext.getContext(false)            // our shim
    //   cfg = ctx.getConfiguration()                     // returned null
    //   lc  = cfg.getLoggerConfig(logger.getName())      // NPE here
    //   lc.setLevel(level)
    // The synthetic LoggerContext we hand out from `LogManager.getContext`
    // has `configuration` slot = null because `alloc_concurrent_synthetic`
    // bypasses the real ctor. Return a synthetic `NullConfiguration` so
    // the chain proceeds; `Configuration.getLoggerConfig` is then routed
    // to our native below.
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "getConfiguration",
        "()Lorg/apache/logging/log4j/core/config/Configuration;",
        |ctx, _args| Ok(Some(Value::Object(Some(build_configuration(ctx))))),
    );
    registry.register(
        CN_CORE_LOGGER_CONTEXT,
        "setConfiguration",
        "(Lorg/apache/logging/log4j/core/config/Configuration;)Lorg/apache/logging/log4j/core/config/Configuration;",
        |_ctx, args| Ok(Some(args.get(1).cloned().unwrap_or(Value::Object(None)))),
    );

    // `Configuration.getLoggerConfig(String) -> LoggerConfig` — register
    // on every concrete Configuration impl we hand out so dispatch
    // (invokeinterface) lands on the native ahead of the real bytecode,
    // which would read uninitialised `loggerConfigs` map fields on the
    // synthetic instance and NPE. Each native returns the same shape:
    // a synthetic `LoggerConfig` carrying just enough state for
    // `setLevel` to succeed (the setter is also a no-op below).
    for cls in [
        CN_NULL_CONFIGURATION,
        CN_DEFAULT_CONFIGURATION,
        CN_ABSTRACT_CONFIGURATION,
        CN_CONFIGURATION,
    ] {
        registry.register(
            cls,
            "getLoggerConfig",
            "(Ljava/lang/String;)Lorg/apache/logging/log4j/core/config/LoggerConfig;",
            |ctx, _args| Ok(Some(Value::Object(Some(build_logger_config(ctx))))),
        );
        // `getName` — return empty string (the LoggerConfig identifier
        // isn't consulted further down the chain).
        registry.register(cls, "getName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        });
        // No-op ctor/clinit so the synthetic-alloc path doesn't trip on
        // the heavy real ctor (which reads PluginManager state etc.).
        registry.register(cls, "<init>", "()V", native_log_void);
        registry.register(cls, "<clinit>", "()V", native_log_void);
    }

    // `LoggerConfig.setLevel(Level) -> void` and the companion accessors
    // ES / Spark touch immediately after `getLoggerConfig`. All no-op so
    // the synthetic LoggerConfig silently accepts level changes.
    //
    // `addAppender(Appender, Level, Filter)V` is reached from
    // `AbstractConfiguration.setToDefault` during the log4j default-config
    // bootstrap path (ES `configureStatusLogger` → DefaultConfigurationBuilder
    // .build → setToDefault → LoggerConfig.addAppender). The real method body
    // reads `getfield appenders; AppenderControlArraySet.add(...)` and trips
    // `NullPointerException: Cannot invoke add on null` on our synthetic
    // instance whose `appenders` slot is null. Same for `removeAppender`,
    // `setParent`, `setLogEventFactory`, `setAdditive` — all mutators that
    // read uninitialised instance fields on the synthetic.
    registry.register(
        CN_LOGGER_CONFIG,
        "setLevel",
        "(Lorg/apache/logging/log4j/Level;)V",
        native_log_void,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "getLevel",
        "()Lorg/apache/logging/log4j/Level;",
        native_logger_get_level,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "addAppender",
        "(Lorg/apache/logging/log4j/core/Appender;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/core/Filter;)V",
        native_log_void,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "removeAppender",
        "(Ljava/lang/String;)V",
        native_log_void,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "setParent",
        "(Lorg/apache/logging/log4j/core/config/LoggerConfig;)V",
        native_log_void,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "setLogEventFactory",
        "(Lorg/apache/logging/log4j/core/impl/LogEventFactory;)V",
        native_log_void,
    );
    registry.register(
        CN_LOGGER_CONFIG,
        "setAdditive",
        "(Z)V",
        native_log_void,
    );
    registry.register(CN_LOGGER_CONFIG, "<init>", "()V", native_log_void);
    registry.register(CN_LOGGER_CONFIG, "<clinit>", "()V", native_log_void);

    // --- SimpleLogger no-op surface -----------------------------------
    register_simple_logger_methods(registry);

    // --- core.Logger extras -------------------------------------------
    // Spark's `Logging$.islog4j2DefaultConfigured` calls
    // `core.Logger.getAppenders()` then iterates the entry set. Returning
    // an empty map (instead of NPE'ing on the unpopulated privateConfig
    // field of our synthetic instance) lets the boot proceed.
    registry.register(
        CN_CORE_LOGGER,
        "getAppenders",
        "()Ljava/util/Map;",
        native_core_logger_get_appenders,
    );
    registry.register(
        CN_CORE_LOGGER,
        "getContext",
        "()Lorg/apache/logging/log4j/core/LoggerContext;",
        native_core_logger_get_context,
    );
    registry.register(
        CN_CORE_LOGGER,
        "getParent",
        "()Lorg/apache/logging/log4j/core/Logger;",
        native_core_logger_get_parent,
    );
    // EJBCA / log4j-1.2-api bridge — `org.apache.log4j.LogManager.<clinit>`
    // builds a `Hierarchy(new RootLogger(Level.DEBUG))` whose ctor calls
    // `RootLogger.setLevel(DEBUG)`, which routes to
    // `org.apache.log4j.Category.setLevel(Level)` → private
    // `setLevel(log4j2.Level)` → `CategoryUtil.setLevel(this.logger, level)`
    // → `core.Logger.setLevel(level)`. The real bytecode of
    // `core.Logger.setLevel` allocates a new `Logger$PrivateConfig` and
    // copies `this.privateConfig.config` — but our synthetic core.Logger
    // (built via `alloc_concurrent_synthetic`) has `privateConfig == null`,
    // so the `PrivateConfig(Logger, PrivateConfig, Level)` ctor NPEs
    // reading `pc.config` at Logger.java:423.
    //
    // No-op the call: the synthetic logger silently swallows level changes,
    // matching the rest of the no-op pipeline (`isXxxEnabled` returns
    // false, `info/warn/...` are all no-ops). See the existing
    // `getAppenders` / `getContext` / `getParent` overrides for the same
    // pattern on this class.
    registry.register(
        CN_CORE_LOGGER,
        "setLevel",
        "(Lorg/apache/logging/log4j/Level;)V",
        native_log_void,
    );
    // Companion accessors used by `org.apache.log4j.Category` and
    // `CategoryUtil` after `setLevel` returns. `addAppender` and
    // `removeAppender` go down the same `privateConfig` path; the rest
    // are simple field reads that would return null/garbage on the
    // synthetic. Match the existing no-op contract.
    registry.register(
        CN_CORE_LOGGER,
        "addAppender",
        "(Lorg/apache/logging/log4j/core/Appender;)V",
        native_log_void,
    );
    registry.register(
        CN_CORE_LOGGER,
        "removeAppender",
        "(Lorg/apache/logging/log4j/core/Appender;)V",
        native_log_void,
    );
    registry.register(
        CN_CORE_LOGGER,
        "setAdditive",
        "(Z)V",
        native_log_void,
    );
}

/// Register no-op natives on `SimpleLogger` for every Logger interface
/// method Spark / Hadoop / Hive / etc. commonly invoke. Each one returns
/// either void or `false` so the caller treats the level as disabled and
/// produces no further log activity.
fn register_simple_logger_methods(registry: &mut NativeMethodRegistry) {
    // <init> no-ops are intentionally NOT registered on AbstractLogger.
    // Subclasses such as `org.apache.logging.log4j.status.StatusLogger`
    // call `super(name, messageFactory)` via `invokespecial`, which lands
    // on `AbstractLogger.<init>(String, MessageFactory)V`. That real ctor
    // sets the `messageFactory` field (falling back to
    // `createDefaultMessageFactory()` when the arg is null). If we replace
    // it with a no-op, the field stays null and every later
    // `logMessage(...)` overload tripping `getfield messageFactory;
    // invokeinterface newMessage` NPEs with "Cannot invoke newMessage on
    // null". That's the canonical Elasticsearch boot failure under
    // CratonVM: `StatusLogger.<clinit>` builds the singleton, super-ctor
    // is no-op'd, the resulting `STATUS_LOGGER` field is half-init'd, and
    // log4j-core's `AbstractConfiguration.initialize` chain (which logs
    // via that singleton) NPEs at `AbstractLogger.java:2051`.
    //
    // The original motivation for the no-op (avoid running heavy ctor
    // bytecode on synthetic instances) doesn't actually apply: our
    // synthetic-alloc helper (`alloc_concurrent_synthetic`) never invokes
    // any `<init>`. The only callers of these ctors are real bytecode
    // paths whose contract requires the field assignment.
    //
    // We still register the no-ops on the concrete subclasses
    // (`SimpleLogger`, `core.Logger`) because their `<init>` overloads
    // have different signatures than the ones registered below (the
    // concrete-ctor descriptors don't match any of `()V`, `(String)V`,
    // `(String, MessageFactory)V`), so these are effectively dead
    // registrations kept for defensive symmetry only.
    for cls in [CN_SIMPLE_LOGGER, CN_CORE_LOGGER] {
        registry.register(cls, "<init>", "()V", native_log_void);
        registry.register(cls, "<init>", "(Ljava/lang/String;)V", native_log_void);
        registry.register(
            cls,
            "<init>",
            "(Ljava/lang/String;Lorg/apache/logging/log4j/message/MessageFactory;)V",
            native_log_void,
        );
    }

    // The accessor / log / isEnabled surface still applies to all three
    // classes including AbstractLogger (vtable fallthrough for synthetic
    // instances).
    for cls in [CN_SIMPLE_LOGGER, CN_CORE_LOGGER, CN_ABSTRACT_LOGGER] {
        // Accessors.
        registry.register(cls, "getName", "()Ljava/lang/String;", native_logger_get_name);
        registry.register(
            cls,
            "getLevel",
            "()Lorg/apache/logging/log4j/Level;",
            native_logger_get_level,
        );
        registry.register(
            cls,
            "getMessageFactory",
            "()Lorg/apache/logging/log4j/message/MessageFactory;",
            native_logger_get_message_factory,
        );

        // isXxxEnabled — return false universally so wrapped log bodies
        // are skipped.
        for m in [
            "isTraceEnabled",
            "isDebugEnabled",
            "isInfoEnabled",
            "isWarnEnabled",
            "isErrorEnabled",
            "isFatalEnabled",
        ] {
            registry.register(cls, m, "()Z", native_log_is_enabled);
            registry.register(
                cls,
                m,
                "(Lorg/apache/logging/log4j/Marker;)Z",
                native_log_is_enabled,
            );
        }
        // isEnabled(Level) / isEnabled(Level, Marker)
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;)Z",
            native_log_is_enabled,
        );
        // 3+ arg isEnabled overloads. The slf4j bridge
        // (`org/apache/logging/slf4j/Log4jLogger.isDebugEnabled` etc.)
        // dispatches `invokeinterface ExtendedLogger.isEnabled(Level,
        // Marker, String)Z` on the wrapped log4j-core Logger. When that
        // wrapped Logger is the synthetic instance our `getLogger` shim
        // hands out, its `privateConfig` instance field is null (the
        // synthetic alloc bypasses the real ctor that publishes it), so
        // letting the real bytecode of `core.Logger.isEnabled` run
        // tripped `NullPointerException: Cannot invoke filter on null`
        // at log4j-core `Logger.java:177` — the canonical Solr-CLI boot
        // failure. Register the full overload surface as no-op `false`
        // so dispatch lands on the native ahead of the field-reading
        // bytecode, mirroring the 1-/2-arg coverage above. Same
        // rationale applies to the `Object[]` /
        // `Object{,Object{,Object...}}` overloads used by parameterised
        // `info/warn/error` call sites.
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;Ljava/lang/Throwable;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/CharSequence;Ljava/lang/Throwable;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/Object;Ljava/lang/Throwable;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Lorg/apache/logging/log4j/message/Message;Ljava/lang/Throwable;)Z",
            native_log_is_enabled,
        );
        registry.register(
            cls,
            "isEnabled",
            "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;[Ljava/lang/Object;)Z",
            native_log_is_enabled,
        );
        // Object-tail overloads: (Level, Marker, String, Object) through
        // (Level, Marker, String, Object x10). Each accepts a fixed
        // number of `java/lang/Object` parameters before the trailing
        // `)Z`. Generated with a small helper to keep the registration
        // surface compact while covering every overload `core.Logger`
        // ships. The registry hashes class/method/descriptor at
        // registration time, so the `&str` borrow is short-lived.
        for n_obj in 1..=10 {
            let mut desc = String::from(
                "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;",
            );
            for _ in 0..n_obj {
                desc.push_str("Ljava/lang/Object;");
            }
            desc.push_str(")Z");
            registry.register(cls, "isEnabled", &desc, native_log_is_enabled);
        }

        // The full Logger interface surface is huge (hundreds of
        // overloads of info/warn/error/debug/trace/fatal/log). Register
        // just the most common ones — the rest go through the bytecode
        // path. Bytecode paths in SimpleLogger check our isXxxEnabled
        // returning false first, so they exit early without reading
        // null fields.
        for level in ["trace", "debug", "info", "warn", "error", "fatal"] {
            for desc in [
                "(Ljava/lang/String;)V",
                "(Ljava/lang/CharSequence;)V",
                "(Ljava/lang/Object;)V",
                "(Ljava/lang/String;Ljava/lang/Object;)V",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
                "(Ljava/lang/String;[Ljava/lang/Object;)V",
                "(Ljava/lang/String;Ljava/lang/Throwable;)V",
                "(Ljava/lang/Object;Ljava/lang/Throwable;)V",
                "(Lorg/apache/logging/log4j/message/Message;)V",
                "(Lorg/apache/logging/log4j/message/Message;Ljava/lang/Throwable;)V",
            ] {
                registry.register(cls, level, desc, native_log_void);
            }
        }

        // logIfEnabled / logMessage — used by SLF4J bridge and
        // AbstractLogger callers. All void no-ops.
        for desc in [
            "(Ljava/lang/String;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Lorg/apache/logging/log4j/message/Message;Ljava/lang/Throwable;)V",
            "(Ljava/lang/String;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/CharSequence;Ljava/lang/Throwable;)V",
            "(Ljava/lang/String;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            "(Ljava/lang/String;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;)V",
            "(Ljava/lang/String;Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;Ljava/lang/Throwable;)V",
        ] {
            registry.register(cls, "logIfEnabled", desc, native_log_void);
            registry.register(cls, "logMessage", desc, native_log_void);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_log4j_stubs_is_callable() {
        let mut r = NativeMethodRegistry::new();
        register_log4j_stubs(&mut r);
        // getContext()LLoggerContext;
        assert!(
            r.find(
                CN_LOGMANAGER,
                "getContext",
                "()Lorg/apache/logging/log4j/spi/LoggerContext;",
            )
            .is_some(),
            "LogManager.getContext() must be registered"
        );
        // getLogger(String)
        assert!(
            r.find(
                CN_LOGMANAGER,
                "getLogger",
                "(Ljava/lang/String;)Lorg/apache/logging/log4j/Logger;",
            )
            .is_some(),
            "LogManager.getLogger(String) must be registered"
        );
        // SimpleLogger.isInfoEnabled()
        assert!(
            r.find(CN_SIMPLE_LOGGER, "isInfoEnabled", "()Z").is_some(),
            "SimpleLogger.isInfoEnabled() must be registered"
        );
    }

    /// Regression: the slf4j → log4j bridge's `Log4jLogger.isDebugEnabled`
    /// dispatches `invokeinterface ExtendedLogger.isEnabled(Level, Marker,
    /// String)Z` on the wrapped `core.Logger`. Without the no-op native
    /// override the real bytecode of `core.Logger.isEnabled` ran on our
    /// synthetic instance and tripped `NullPointerException: Cannot invoke
    /// filter on null` (`Logger.java:177`) — the canonical Solr-CLI boot
    /// failure. Confirm the 3-arg overload AND the parameterised
    /// `Object`-tail variants are all registered on every concrete logger
    /// receiver class our `getLogger` shim hands out.
    #[test]
    fn register_log4j_stubs_covers_three_arg_is_enabled() {
        let mut r = NativeMethodRegistry::new();
        register_log4j_stubs(&mut r);
        for cls in [CN_SIMPLE_LOGGER, CN_CORE_LOGGER, CN_ABSTRACT_LOGGER] {
            // 3-arg slf4j-bridge dispatch site.
            assert!(
                r.find(
                    cls,
                    "isEnabled",
                    "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;)Z",
                )
                .is_some(),
                "{cls}.isEnabled(Level, Marker, String)Z must be registered"
            );
            // Throwable-tail.
            assert!(
                r.find(
                    cls,
                    "isEnabled",
                    "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;Ljava/lang/Throwable;)Z",
                )
                .is_some(),
                "{cls}.isEnabled(Level, Marker, String, Throwable)Z must be registered"
            );
            // Varargs Object[].
            assert!(
                r.find(
                    cls,
                    "isEnabled",
                    "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;[Ljava/lang/Object;)Z",
                )
                .is_some(),
                "{cls}.isEnabled(Level, Marker, String, Object[])Z must be registered"
            );
            // Single-Object tail (the first generated overload).
            assert!(
                r.find(
                    cls,
                    "isEnabled",
                    "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;Ljava/lang/Object;)Z",
                )
                .is_some(),
                "{cls}.isEnabled(Level, Marker, String, Object)Z must be registered"
            );
            // Ten-Object tail (the last generated overload).
            assert!(
                r.find(
                    cls,
                    "isEnabled",
                    "(Lorg/apache/logging/log4j/Level;Lorg/apache/logging/log4j/Marker;Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
                )
                .is_some(),
                "{cls}.isEnabled(Level, Marker, String, Object x10)Z must be registered"
            );
        }
    }
}
