// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Logging shims: SLF4J/Logback, java.util.logging, Log4j and the PrintStream/PrintWriter fallbacks.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

pub(crate) fn osw_wrapped_output(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "out") {
        Value::Object(Some(out)) => Some(out),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(out)) => Some(out),
            _ => None,
        },
    }
}

fn output_stream_write_array(
    ctx: &mut dyn NativeContext,
    out: ObjectRef,
    arr: ObjectRef,
    len: usize,
) -> MethodCallResult {
    ctx.invoke_virtual(
        out,
        "write",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(len as i32),
        ],
    )?;
    Ok(None)
}

pub(crate) fn write_bytes_from_output_stream_writer(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: &[u8],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    if !ctx.write_byte_array_from(arr, 0, bytes) {
        for (idx, byte) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, idx, Value::Int(*byte as i8 as i32));
        }
    }
    if let Some(out) = osw_wrapped_output(ctx, this) {
        output_stream_write_array(ctx, out, arr, bytes.len())?;
    }
    Ok(None)
}

pub(crate) fn native_output_stream_writer_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let output = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, 0, output);
    ctx.set_field_by_name(this, "out", output);
    Ok(None)
}

/// Register PrintStream/PrintWriter fallback natives.
///
/// These override the real JDK bytecode implementations so our synthetic
/// System.out/System.err streams (1-field objects) work correctly in real JDK
/// mode, particularly when System.initPhase1() has not completed successfully.
/// The fallbacks simply write to the host process stdout/stderr via fd_table.
pub(crate) fn register_printstream_fallback_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: PrintStream/PrintWriter natives bridge host stdout/stderr.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/io/PrintStream",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        native_printstream_init_outputstream,
    );
    registry.register(
        "java/io/PrintStream",
        "<init>",
        "(Ljava/io/OutputStream;Z)V",
        native_printstream_init_outputstream,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/String;)V",
        native_println_string,
    );
    registry.register("java/io/PrintStream", "println", "(I)V", native_println_int);
    registry.register(
        "java/io/PrintStream",
        "println",
        "(J)V",
        native_println_long,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(D)V",
        native_println_double,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Z)V",
        native_println_boolean,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(C)V",
        native_println_char,
    );
    registry.register(
        "java/io/PrintStream",
        "println",
        "(F)V",
        native_println_float,
    );
    registry.register("java/io/PrintStream", "println", "()V", native_println_void);
    registry.register(
        "java/io/PrintStream",
        "println",
        "(Ljava/lang/Object;)V",
        native_println_object,
    );
    registry.register(
        "java/io/PrintStream",
        "print",
        "(Ljava/lang/String;)V",
        native_print_string,
    );
    registry.register("java/io/PrintStream", "print", "(I)V", native_print_int);
    registry.register("java/io/PrintStream", "print", "(C)V", native_print_char);
    registry.register("java/io/PrintStream", "print", "(Z)V", native_print_boolean);
    registry.register("java/io/PrintStream", "print", "(J)V", native_print_long);
    registry.register("java/io/PrintStream", "print", "(F)V", native_print_float);
    registry.register("java/io/PrintStream", "print", "(D)V", native_print_double);
    registry.register(
        "java/io/PrintStream",
        "print",
        "(Ljava/lang/Object;)V",
        native_print_object,
    );
    registry.register(
        "java/io/PrintStream",
        "printf",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf,
    );
    registry.register(
        "java/io/PrintStream",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf,
    );
    // PrintStream writer-path entries. JUnit's ConsoleLauncher wraps
    // `System.out` (a PrintStream) in a `PrintWriter`; `PrintWriter.write`
    // delegates `out.write(String,int,int)` straight onto the PrintStream
    // receiver. The real JDK PrintStream only has a package-private
    // `write(String)` and no 3-arg form, so without these natives the
    // launcher aborts with `NoSuchMethodError: PrintStream.write(String,II)V`.
    registry.register(
        "java/io/PrintStream",
        "write",
        "([BII)V",
        native_printstream_write,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(I)V",
        native_printstream_write_int,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(Ljava/lang/String;)V",
        native_printstream_write_string,
    );
    registry.register(
        "java/io/PrintStream",
        "write",
        "(Ljava/lang/String;II)V",
        native_printstream_write_string_range,
    );
    registry.register(
        "java/io/PrintStream",
        "append",
        "(Ljava/lang/CharSequence;)Ljava/io/PrintStream;",
        native_printstream_append,
    );
    // AssertJ's default unordered-iterable assertion is intentionally generic,
    // but a large boxed-Long comparison otherwise spends its entire timeout in
    // the Java-side primitive-array cascade before reaching Long.equals.
    registry.register(
        "org/assertj/core/internal/StandardComparisonStrategy",
        "areEqual",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_assertj_standard_comparison_are_equal,
    );
    registry.register(
        "org/assertj/core/internal/StandardComparisonStrategy",
        "iterableContains",
        "(Ljava/lang/Iterable;Ljava/lang/Object;)Z",
        native_assertj_standard_comparison_iterable_contains,
    );
    registry.register(
        "org/assertj/core/internal/StandardComparisonStrategy",
        "iterablesRemoveFirst",
        "(Ljava/lang/Iterable;Ljava/lang/Object;)V",
        native_assertj_standard_comparison_iterables_remove_first,
    );
    // UUID generator validation performs millions of successful natural-order
    // AssertJ comparisons. Preserve custom-comparator and failure behavior by
    // delegating those cases to AssertJ; the default String / UUID success
    // path only needs Comparable.compareTo and can avoid the assertion stack.
    registry.register(
        "org/assertj/core/api/AbstractComparableAssert",
        "isGreaterThan",
        "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;",
        native_assertj_comparable_is_greater_than,
    );
    registry.register(
        "org/assertj/core/api/AbstractStringAssert",
        "isGreaterThan",
        "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;",
        native_assertj_comparable_is_greater_than,
    );
    registry.register(
        "org/assertj/core/api/AssertionsForClassTypes",
        "assertThat",
        "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;",
        native_assertj_string_assert_that,
    );
    registry.register(
        "org/assertj/core/api/AssertionsForClassTypes",
        "assertThat",
        "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;",
        native_assertj_comparable_assert_that,
    );
    registry.register(
        "org/assertj/core/api/Assertions",
        "assertThat",
        "(Ljava/lang/String;)Lorg/assertj/core/api/AbstractStringAssert;",
        native_assertj_string_assert_that,
    );
    registry.register(
        "org/assertj/core/api/Assertions",
        "assertThat",
        "(Ljava/lang/Comparable;)Lorg/assertj/core/api/AbstractComparableAssert;",
        native_assertj_comparable_assert_that,
    );
    // Hibernate's RFC-9562 strategies update immutable state records through
    // AtomicReference.updateAndGet.  In a real JDK that tiny CAS loop is
    // inlined, while the interpreter otherwise pays for a lambda and several
    // virtual calls for every UUID.  Keep the same immutable-state CAS
    // protocol in the native implementations below.
    registry.register(
        "org/hibernate/id/uuid/UuidVersion6Strategy",
        "generateUuid",
        "(Lorg/hibernate/engine/spi/SharedSessionContractImplementor;)Ljava/util/UUID;",
        native_hibernate_uuid_v6_generate,
    );
    registry.register(
        "org/hibernate/id/uuid/UuidVersion7Strategy",
        "generateUuid",
        "(Lorg/hibernate/engine/spi/SharedSessionContractImplementor;)Ljava/util/UUID;",
        native_hibernate_uuid_v7_generate,
    );
    // Our System.out/err are fd-backed synthetic PrintStreams (slot 0 = fd
    // id); their inherited FilterOutputStream `out` field is never populated.
    // The real-JDK `PrintStream.flush()`/`close()` bytecode dereferences that
    // null `out` (`out.flush()`) → NPE ("Cannot invoke flush on null") for any
    // program that calls `System.out.flush()`. Route both through the fd-aware
    // natives instead: flush drains the fd's buffer, close is a no-op (we must
    // never close the process stdout/stderr). Mirrors the synthetic-mode
    // PrintStream registration and the long-standing fd-stream flush contract.
    registry.register(
        "java/io/PrintStream",
        "flush",
        "()V",
        native_printstream_flush,
    );
    registry.register(
        "java/io/PrintStream",
        "close",
        "()V",
        native_printstream_close,
    );
    // PrintWriter — use dedicated variants that route through the underlying
    // Writer when the backing is non-fd (e.g. StringWriter in ModelNode.toString()).
    registry.register(
        "java/io/PrintWriter",
        "write",
        "(Ljava/lang/String;II)V",
        native_printwriter_write_string_range,
    );
    registry.register(
        "java/io/PrintWriter",
        "write",
        "(Ljava/lang/String;)V",
        native_printwriter_write_string,
    );
    registry.register(
        "java/io/PrintWriter",
        "println",
        "(Ljava/lang/String;)V",
        native_println_string,
    );
    registry.register("java/io/PrintWriter", "println", "()V", native_println_void);
    registry.register("java/io/PrintWriter", "println", "(I)V", native_println_int);
    registry.register(
        "java/io/PrintWriter",
        "println",
        "(Ljava/lang/Object;)V",
        native_println_object,
    );
    // Single-arg `PrintWriter(OutputStream)` — JDK chains to
    // `PrintWriter(OutputStream, boolean)` with `autoFlush=false`.  JUnit's
    // ConsoleLauncher invokes this twice during startup (wrapping stdout
    // and stderr); without an explicit native, real-JDK dispatch fails to
    // resolve the constructor and the launcher silently aborts with zero
    // tests run.
    registry.register(
        "java/io/PrintWriter",
        "<init>",
        "(Ljava/io/OutputStream;)V",
        native_printwriter_init_outputstream,
    );
    registry.set_category(__prev_cat);
}

/// True when `obj`'s concrete class is `java.util.logging.Logger$ConfigurationData`.
///
/// Used by the `java/util/logging/Logger.{get,set}Level` natives to tell a
/// **real-JDK** `Logger` apart from the flat synthetic loggers our own
/// `getLogger` natives mint. A real `Logger` keeps its level in
/// `config.levelObject` (where `config` is a `ConfigurationData`); the
/// synthetic loggers instead stash the name in slot 0 — which is the real
/// `config` field slot — so for them `config` resolves to a `String`, not a
/// `ConfigurationData`. Only the real shape is written/read through; synthetic
/// loggers keep their historic no-op/null level behaviour (their effective
/// level is governed by the process-wide tracing subscriber).
pub(crate) fn jul_logger_config_is_real(ctx: &mut dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .map(|n| n == "java/util/logging/Logger$ConfigurationData")
        .unwrap_or(false)
}

/// GC-safe side table for `java.util.logging.Logger`'s handler list, keyed by
/// `identity_hash_code` (same pattern as `net_phase_e.rs`'s `ss_side_table` /
/// `stream_owner_table`). `addHandler`/`removeHandler`/`getHandlers` are
/// fully native-overridden (never fall through to real bytecode), so they
/// don't need to live in any particular instance field slot -- and MUST NOT,
/// because real-JDK 25's `Logger` has no `handlers` instance field at all
/// (handlers moved inside `Logger$ConfigurationData`, referenced from slot 0
/// / `config`) and slot 2 is actually `name` (a `String`). The old code
/// stored/read the handler `ArrayList` at raw field slot 2, which on a
/// real-bytecode-constructed `Logger` collided with `name`:
/// `ctx.invoke_virtual(nameString, "size", "()I", ...)` then threw
/// `NoSuchMethodError: java/lang/String.size()I` (surfaced from
/// `org.apache.juli.ClassLoaderLogManager.resetLoggers`, which calls
/// `logger.getHandlers()` during webapp/classloader shutdown -- see
/// docs/known-issues/tomcat-08-07/largeclienthello-string-size-nosuchmethod.md).
/// Keying by identity hash and holding the list as a global GC root
/// sidesteps field layout entirely -- correct for both real and synthetic
/// loggers, and immune to future real-JDK field-order changes.
fn jul_logger_handlers_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, usize>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn jul_logger_handlers_get(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(logger);
    let handle = *jul_logger_handlers_table().lock().unwrap().get(&key)?;
    ctx.resolve_global_root(handle)
}

pub(crate) fn jul_logger_handlers_set(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    list: ObjectRef,
) {
    // Adding a global root may grow the root table and collect. The logger is
    // keyed immediately afterward, so retain it across that allocation.
    let logger_pin = ctx.pin_native_root(logger);
    let handle = ctx.add_global_root(list);
    let logger = ctx.read_native_pin(logger_pin, logger);
    let key = ctx.identity_hash_code(logger);
    jul_logger_handlers_table()
        .lock()
        .unwrap()
        .insert(key, handle);
    ctx.unpin_native_roots(logger_pin);
}

pub(crate) fn jul_logger_handlers_clear(ctx: &mut dyn NativeContext, logger: ObjectRef) {
    let key = ctx.identity_hash_code(logger);
    if let Some(handle) = jul_logger_handlers_table().lock().unwrap().remove(&key) {
        ctx.remove_global_root(handle);
    }
}

/// GC-safe side table for Logger filters. Real JDK loggers keep a Filter in
/// `Logger$ConfigurationData`, while our compact loggers do not have that
/// shape; sharing neither raw layout is safe.
fn jul_logger_filters_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, usize>>> = OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn jul_logger_filter_names_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn jul_logger_filter_name(ctx: &mut dyn NativeContext, logger: ObjectRef) -> Option<String> {
    match ctx.get_field_by_name(logger, "name") {
        Value::Object(Some(name)) => ctx.read_string(name),
        _ => match ctx.get_field(logger, LOGGER_FIELD_NAME) {
            Value::Object(Some(name)) => ctx.read_string(name),
            _ => None,
        },
    }
}

pub(crate) fn jul_logger_filter_get(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    let key = ctx.identity_hash_code(logger);
    if let Some(handle) = jul_logger_filters_table()
        .lock()
        .unwrap()
        .get(&key)
        .copied()
    {
        return ctx.resolve_global_root(handle);
    }
    let name = jul_logger_filter_name(ctx, logger)?;
    let handle = jul_logger_filter_names_table()
        .lock()
        .unwrap()
        .get(&name)
        .copied()?;
    ctx.resolve_global_root(handle)
}

pub(crate) fn jul_logger_filter_set(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    filter: Option<ObjectRef>,
) {
    let key = ctx.identity_hash_code(logger);
    let name = jul_logger_filter_name(ctx, logger);
    let mut table = jul_logger_filters_table().lock().unwrap();
    if let Some(handle) = table.remove(&key) {
        ctx.remove_global_root(handle);
    }
    if let Some(name) = &name {
        if let Some(handle) = jul_logger_filter_names_table().lock().unwrap().remove(name) {
            ctx.remove_global_root(handle);
        }
    }
    if let Some(filter) = filter {
        table.insert(key, ctx.add_global_root(filter));
        if let Some(name) = name {
            jul_logger_filter_names_table()
                .lock()
                .unwrap()
                .insert(name, ctx.add_global_root(filter));
        }
    }
}

/// GC-safe side table for `java.util.logging.FileHandler`'s own bookkeeping
/// (a resolved output filename plus a closed flag), keyed by
/// `identity_hash_code` -- same pattern as `jul_logger_handlers_table`/
/// `jul_logger_filters_table` above.
///
/// `FileHandler`'s `<init>`/`publish`/`flush`/`close` are fully
/// native-overridden (this module never falls through to real bytecode for
/// them), so this state doesn't need to live in any particular instance
/// field slot -- and MUST NOT, for the same reason documented on
/// `jul_logger_handlers_table`: `FileHandler` is loaded from the real
/// `java.base` module, so `new_object_initialized`/`ctx.set_field(this, N,
/// ...)` allocates and indexes the REAL declared-field array (`Handler`'s
/// inherited `manager`/`filter`/`formatter`/`logLevel`/`errorManager`/
/// `encoding`, then `StreamHandler`'s and `FileHandler`'s own real fields).
/// A prior fix attempt wrote the resolved filename to raw slot 0 (aliasing
/// `Handler.manager`) and a closed flag to slot 2 -- see
/// docs/known-issues/springboot/filehandler-noarg-ctor-handler-field-layout-gap.md
/// for the full diagnosis. Keying by identity hash sidesteps field layout
/// entirely, exactly like the `Logger` handler-list/filter tables above,
/// and leaves `Handler`'s real `logLevel`/`filter`/`formatter` fields (which
/// `Handler.setLevel`/`getLevel`/`isLoggable`/`setFormatter`/`getFormatter`
/// in `reflect_annotations.rs` access by NAME, not slot) untouched and
/// correct regardless of how a `FileHandler` was constructed.
fn jul_file_handler_state_table() -> &'static std::sync::Mutex<std::collections::HashMap<i32, (Option<String>, bool)>>
{
    static T: OnceLock<std::sync::Mutex<std::collections::HashMap<i32, (Option<String>, bool)>>> =
        OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn jul_file_handler_filename(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    let key = ctx.identity_hash_code(this);
    jul_file_handler_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .and_then(|(filename, _)| filename.clone())
}

pub(crate) fn jul_file_handler_set_filename(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    filename: Option<String>,
) {
    let key = ctx.identity_hash_code(this);
    let mut table = jul_file_handler_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.entry(key).or_insert((None, false)).0 = filename;
}

pub(crate) fn jul_file_handler_is_closed(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let key = ctx.identity_hash_code(this);
    jul_file_handler_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|(_, closed)| *closed)
        .unwrap_or(false)
}

pub(crate) fn jul_file_handler_set_closed(ctx: &mut dyn NativeContext, this: ObjectRef, closed: bool) {
    let key = ctx.identity_hash_code(this);
    let mut table = jul_file_handler_state_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.entry(key).or_insert((None, false)).1 = closed;
}

pub(crate) fn emit_framework_log(ctx: &mut dyn NativeContext, text: &str) {
    ctx.record_printed_line(text.to_string());
    // `NativeContext::get_system_stream` is the process's canonical fd-backed
    // stream. `System.setOut` intentionally leaves that canonical stream in
    // place and records the Java-level replacement in the override table, so
    // native-originated logs must prefer the override just as GETSTATIC does.
    let canonical = ctx.get_system_stream("out");
    if let Some(out) = system_overridden_stream_resolved(ctx, "out").or(canonical) {
        // DISPATCH (2026-07-21): an override stream installed via
        // `System.setOut` may be a delegating subclass whose `println`
        // override is real bytecode routing to a dynamically-looked-up
        // target (WildFly: `org.jboss.stdio.StdioContext$DelegatingPrintStream`
        // — its sink is NOT reachable by any field walk, so the old direct
        // `stream_writeln` call dropped every native-originated boot log
        // line). Give the receiver's own Java `println` a chance first; the
        // canonical fd-backed stream keeps the direct native fast path.
        let is_canonical = ctx
            .get_system_stream("out")
            .map(|c| std::ptr::eq(out.as_ptr(), c.as_ptr()))
            .unwrap_or(false)
            || ctx
                .get_system_stream("err")
                .map(|c| std::ptr::eq(out.as_ptr(), c.as_ptr()))
                .unwrap_or(false);
        let depth = EMIT_FRAMEWORK_LOG_DEPTH.with(|d| d.get());
        // Guard the Java dispatch on the receiver actually being a live,
        // classed object — a dead/stale ref reads as `java/lang/Object`
        // (zeroed header) and the `println` dispatch would raise a bogus
        // `NoSuchMethodError` into whatever Java frame invoked the logging
        // native (observed killing the WildFly boot thread outright).
        let receiver_classed = matches!(
            ctx.class_name_of_id(ctx.class_id_of_object(out)).as_deref(),
            Some(n) if n != "java/lang/Object"
        );
        // WildFly's `org.jboss.stdio` override streams REDIRECT stdout back
        // INTO the logging framework (delegating stream → JUL "stdout" logger
        // → jboss-logmanager). Dispatching a framework log RECORD into them is
        // circular by construction: on real HotSpot these records flow
        // logger → ConsoleHandler → the fd saved BEFORE the stdio swap, never
        // through the live `System.out`. The dispatch below therefore
        // black-holed every WildFly boot log line (WFLYSRV0049/0025 included —
        // boot "completed" invisibly, startup-marker written but no console
        // output). Route framework records straight to the canonical fd-backed
        // stream for these redirect streams.
        let is_stdio_redirect = matches!(
            ctx.class_name_of_id(ctx.class_id_of_object(out)).as_deref(),
            Some(n) if n.starts_with("org/jboss/stdio/")
        );
        if !is_canonical && receiver_classed && !is_stdio_redirect && depth < 2 {
            EMIT_FRAMEWORK_LOG_DEPTH.with(|d| d.set(depth + 1));
            // GC-safety: `create_string` can trigger a moving collection;
            // pin `out` across it and re-read the (possibly relocated) ref
            // before dispatching (Family-1 pin/refresh idiom).
            let pin = ctx.pin_native_root(out);
            let s = ctx.create_string(text);
            let out_fixed = ctx.read_native_pin(pin, out);
            let dispatched = ctx
                .invoke_virtual(
                    out_fixed,
                    "println",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(s))],
                )
                .is_ok();
            // The invoke itself may have moved the stream; refresh before
            // the fallback writeln uses it.
            let out_after = ctx.read_native_pin(pin, out_fixed);
            ctx.unpin_native_roots(pin);
            EMIT_FRAMEWORK_LOG_DEPTH.with(|d| d.set(depth));
            if dispatched {
                return;
            }
            stream_writeln(ctx, &[Value::Object(Some(out_after))], text);
            return;
        }
        let sink = if is_stdio_redirect {
            canonical.unwrap_or(out)
        } else {
            out
        };
        stream_writeln(ctx, &[Value::Object(Some(sink))], text);
    }
}

pub(crate) fn native_print_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_char(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ch = match args.get(1) {
        Some(Value::Int(v)) => char::from_u32(*v as u32).unwrap_or('\0'),
        _ => '\0',
    };
    let text = ch.to_string();
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_boolean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // CompactValue tag-erasure: a long pushed via `CompactValue::long(v)` is
    // stored with the Double tag (no embedded marker — see
    // `types/src/compact_value.rs::pub fn long`). When `invokevirtual` pops
    // args with `pop_unchecked()` the slot decodes back as
    // `Value::Double(<denormal>)`, not `Value::Long`. Treat a Double
    // argument here as the raw long bits so `PrintStream.print(J)V`
    // prints the correct value instead of silently falling through to 0
    // (the visible symptom of the `System.currentTimeMillis()` "always
    // returns 0 delta" bug in `bench/nbody.java`).
    let val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Float(f)) => f.to_bits() as i64,
        _ => 0,
    };
    let text = val.to_string();
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    };
    let text = format_float(val);
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let text = format_double(val);
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

pub(crate) fn native_print_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => invoke_to_string(ctx, *obj)?,
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

/// `PrintWriter.format(String, Object[])` / `printf(String, Object[])`.
///
/// Delegates formatting to `String.format` (the same `java.util.Formatter`
/// path used by `String.format`), then writes the produced text through to
/// the PrintWriter's underlying `Writer` if one is attached (field 0 in our
/// synthetic layout), so code like `new PrintWriter(sw).format(...)` ends up
/// with formatted text in the `StringWriter`.
///
/// Writing is performed via `invoke_virtual` on the backing writer, which
/// dispatches to whatever concrete subtype was wired up (StringWriter,
/// BufferedWriter, OutputStreamWriter, etc.). If no backing writer is
/// attached we still record the formatted line for test inspection.
/// `PrintWriter(OutputStream out)` — single-arg constructor.
///
/// JDK semantics: delegates to `PrintWriter(OutputStream, boolean)` with
/// `autoFlush=false`, which wraps the stream in a `BufferedWriter` over an
/// `OutputStreamWriter` and stores it as the underlying `Writer`.
///
/// JUnit's ConsoleLauncher wraps stdout and stderr with this constructor
/// twice during startup; previously CratonVM logged a WARN and silently
/// continued, leading to a downstream NPE inside the launcher and zero
/// tests executed.
///
/// Implementation strategy: delegate to the real two-arg JDK constructor
/// via `invoke_special` so that the JDK's own `out`/`lock`/etc. fields get
/// populated correctly.  As a defensive fallback (in case the two-arg ctor
/// is itself unavailable), we also set the synthetic `field 0 = stream`
/// convention used by `native_printwriter_printf` and the wrapping branch
/// of `stream_fd`.
fn native_printstream_init_outputstream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let out_val = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.set_field(this, 0, out_val.clone());
    ctx.set_field_by_name(this, "out", out_val);
    if let Ok(Some(Value::Object(Some(lock_ref)))) = ctx.new_object("java/lang/Object") {
        ctx.set_field_by_name(this, "lock", Value::Object(Some(lock_ref)));
    }
    Ok(None)
}

fn native_printwriter_init_outputstream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let out_val = args.get(1).cloned().unwrap_or(Value::Object(None));

    // Set the synthetic `field 0 = stream` slot up front so the stream_fd
    // wrapper-walk works even if invoke_special below silently bails.
    ctx.set_field(this, 0, out_val.clone());
    // Best-effort: assign the JDK-named `out` field on the receiver so
    // real-JDK `print*` bytecode paths find their underlying Writer.
    ctx.set_field_by_name(this, "out", out_val.clone());

    // PrintWriter inherits `protected Object lock` from `Writer`.  Real
    // JDK methods like `flush()`, `write(...)` etc. do `synchronized (lock)`;
    // without a lock object the bytecode NPEs on monitorenter.  Allocate a
    // bare Object and install it under the canonical field name (we
    // tolerate `set_field_by_name` silently failing on unknown layouts).
    if let Ok(Some(Value::Object(Some(lock_ref)))) = ctx.new_object("java/lang/Object") {
        ctx.set_field_by_name(this, "lock", Value::Object(Some(lock_ref)));
    }

    // Try to chain to the real two-arg JDK constructor (autoFlush=false).
    // We ignore the result: the synthetic field-0 fallback above is enough
    // for our native print/println intercepts, and this path is best-effort.
    let _ = ctx.invoke_special(
        "java/io/PrintWriter",
        "<init>",
        "(Ljava/io/OutputStream;Z)V",
        &[Value::Object(Some(this)), out_val, Value::Int(0)],
    );
    Ok(None)
}

pub(crate) fn native_printwriter_printf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this_opt = match args.first() {
        Some(Value::Object(obj)) => *obj,
        _ => None,
    };
    let fmt_args = &args[1..];
    let result = native_string_format(ctx, fmt_args)?;
    if let Some(Value::Object(Some(str_ref))) = result {
        let text = ctx.read_string(str_ref).unwrap_or_default();
        ctx.record_printed_line(text.clone());

        // Flush the formatted text through to the underlying Writer, if any.
        // Synthetic PrintWriter layout: field 0 = backing Writer/OutputStream.
        if let Some(this) = this_opt {
            if let Value::Object(Some(backing)) = ctx.get_field(this, 0) {
                // If `backing` is a CHAR `java/io/Writer` (the JDK
                // `PrintWriter` sink, e.g. BufferedWriter/OutputStreamWriter)
                // it has NO byte `write([BII)V` — only `write(String)` /
                // `write([CII)V`.  Never fall through to the byte path for a
                // Writer, or it raises `NoSuchMethodError:
                // java/io/BufferedWriter.write([BII)V`.
                let backing_is_writer = matches!(sink_is_writer(ctx, backing), Some(true));
                let s = ctx.create_string(&text);
                // Prefer Writer.write(String) which is the canonical PrintWriter
                // sink. If that isn't registered we fall through to the
                // OutputStream byte path so BAOS-backed writers still work.
                let wrote_string = ctx
                    .invoke_virtual(
                        backing,
                        "write",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(s))],
                    )
                    .is_ok();
                if !wrote_string && !backing_is_writer {
                    let bytes = text.as_bytes();
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
                    for (i, b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
                    }
                    let _ = ctx.invoke_virtual(
                        backing,
                        "write",
                        "([BII)V",
                        &[
                            Value::Object(Some(arr)),
                            Value::Int(0),
                            Value::Int(bytes.len() as i32),
                        ],
                    );
                }
            }
        }
    }
    Ok(Some(Value::Object(this_opt)))
}

pub(crate) fn native_printstream_flush(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // User/Tee streams: propagate flush() to the real underlying stream so a
    // redirected file (e.g. DaCapo stdout.log) is durable before its digest is
    // read. Canonical synthetic out/err (out==null) flush the fd directly.
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Value::Object(Some(out)) = ctx.get_field_by_name(*this, "out") {
            let _ = ctx.invoke_virtual(out, "flush", "()V", &[]);
            return Ok(None);
        }
    }
    if let Some(fd) = stream_fd(ctx, args) {
        let _ = ctx.fd_table().flush(fd);
    }
    Ok(None)
}

pub(crate) fn native_printstream_close(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Don't actually close stdout/stderr
    Ok(None)
}

pub(crate) fn native_printstream_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this, args[1]=byte[], args[2]=off, args[3]=len
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(o)) => *o as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(l)) => *l as usize,
        _ => 0,
    };
    let mut buf = vec![0u8; len];
    for (i, slot) in buf.iter_mut().enumerate() {
        if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
            *slot = b as u8;
        }
    }
    // User/Tee streams route through the real underlying stream; canonical
    // synthetic out/err (out==null) write to the fd directly.
    // LOCK-SCOPE (2026-07-21): see `stream_write`.
    let text = String::from_utf8_lossy(&buf);
    if !surefire_forwarding_write(ctx, args, &text, false)
        && !route_write_through_out(ctx, args, &buf)
    {
        if let Some(fd) = stream_fd(ctx, args) {
            with_stdio_print_lock(|| {
                let _ = ctx.fd_table().write_bytes(fd, &buf);
            });
        }
    }
    Ok(None)
}

pub(crate) fn native_printstream_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this, args[1]=int. Java PrintStream.write(int) writes the low
    // eight bits of the argument to the underlying byte stream.
    let b = match args.get(1) {
        Some(Value::Int(v)) => (*v & 0xff) as u8,
        _ => 0,
    };
    let buf = [b];
    // LOCK-SCOPE (2026-07-21): see `stream_write`.
    let text = String::from_utf8_lossy(&buf);
    if !surefire_forwarding_write(ctx, args, &text, false)
        && !route_write_through_out(ctx, args, &buf)
    {
        if let Some(fd) = stream_fd(ctx, args) {
            with_stdio_print_lock(|| {
                let _ = ctx.fd_table().write_bytes(fd, &buf);
            });
        }
    }
    Ok(None)
}

/// `PrintStream.write(String)` — the package-private writer-path entry used by
/// `print(String)`. Writes the whole string to the underlying stream.
pub(crate) fn native_printstream_write_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this (PrintStream), args[1]=String
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        _ => return Ok(None),
    };
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

/// `PrintStream.append(CharSequence)` — appends the text and returns `this`.
fn native_printstream_append(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0]=this (PrintStream), args[1]=CharSequence
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        _ => "null".to_string(),
    };
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(args.first().copied())
}

/// `PrintStream.write(String, int, int)` — writes `str.substring(off, off+len)`
/// to the underlying stream. This is the writer-path 3-arg entry that JUnit's
/// console output reaches via `Writer.write(String,int,int)` when a system
/// `PrintStream` is used through the character-writer chain.
pub(crate) fn native_printstream_write_string_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this (PrintStream), args[1]=String, args[2]=off, args[3]=len
    let full = match args.get(1) {
        Some(Value::Object(Some(obj))) => {
            ctx.read_string(*obj).unwrap_or_else(|| "null".to_string())
        }
        Some(Value::Object(None)) => "null".to_string(),
        _ => return Ok(None),
    };
    let off = match args.get(2) {
        Some(Value::Int(o)) => (*o).max(0) as usize,
        _ => 0,
    };
    let len = match args.get(3) {
        Some(Value::Int(l)) => (*l).max(0) as usize,
        _ => 0,
    };
    // Slice on UTF-16 code units to match Java String.substring semantics.
    let units: Vec<u16> = full.encode_utf16().collect();
    let end = off.saturating_add(len).min(units.len());
    let start = off.min(end);
    let text = String::from_utf16_lossy(&units[start..end]);
    ctx.record_printed_line(text.clone());
    stream_write(ctx, args, &text);
    Ok(None)
}

/// `PrintWriter.write(String)V` — routes through the underlying `Writer out`
/// for real-JDK `PrintWriter(Writer)` constructions such as `ModelNode.toString()`
/// wrapping a `StringWriter`.  Falls back to the fd path for PrintStream-backed
/// writers (e.g. JUnit's `PrintWriter(System.out)`).
fn native_printwriter_write_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        // Real JDK's `PrintWriter.write(String)` body is `write(s, 0,
        // s.length())` — a VIRTUAL call back on `this`. A user subclass that
        // overrides `write(String,int,int)` (e.g. Spring's
        // `MockHttpServletResponse`'s private `ResponsePrintWriter`, which
        // auto-flushes and tracks commit state on every write) depends on
        // that dispatch. Writing straight to the backing `out` object below
        // skips `this` entirely: the char data still reaches the backing
        // Writer, but any subclass side effect the override exists to provide
        // (here, forcing the buffered `OutputStreamWriter`/`StreamEncoder`
        // bytes out to the real sink) never runs — silently losing the
        // content once the request stops touching the response any further
        // (e.g. a `View.render()`/`@ExceptionHandler` write with no later
        // explicit flush). Detect the subclass case first and re-dispatch
        // through `this`, so ordinary virtual method resolution finds the
        // override; when there isn't one, `write(String,int,int)` falls
        // through to `native_printwriter_write_string_range` below —
        // functionally identical to the fast path already taken here for a
        // plain `java.io.PrintWriter` receiver, just one indirection deeper.
        let this_cid = ctx.class_id_of_object(this);
        let this_cname = ctx.class_name_of_id(this_cid);
        let is_plain_printwriter = this_cname.as_deref() == Some("java/io/PrintWriter");
        if !is_plain_printwriter {
            if let Some(Value::Object(Some(s))) = args.get(1).copied() {
                if let Some(text) = ctx.read_string(s) {
                    let len = text.encode_utf16().count() as i32;
                    let _ = ctx.invoke_virtual(
                        this,
                        "write",
                        "(Ljava/lang/String;II)V",
                        &[Value::Object(Some(s)), Value::Int(0), Value::Int(len)],
                    );
                    return Ok(None);
                }
            }
            // Null/non-String arg: real `write(String)` would NPE inside
            // `s.length()` before ever reaching a writer — fall through to
            // the pre-existing behaviour below rather than invent new null
            // semantics here.
        }
        if let Some(out_obj) = printwriter_get_backing_writer(ctx, this) {
            // Pass the EXISTING Java String arg directly to out.write(String).
            // Do NOT call write_string_to_writer (which does ctx.create_string → GC hazard:
            // create_string allocates, potentially triggering a compacting GC that moves
            // `out_obj` before it is passed to invoke_virtual).
            let str_val = args.get(1).cloned().unwrap_or(Value::Object(None));
            let _ = ctx.invoke_virtual(out_obj, "write", "(Ljava/lang/String;)V", &[str_val]);
            return Ok(None);
        }
    }
    native_printstream_write_string(ctx, args)
}

/// `PrintWriter.write(String,II)V` — routes through the underlying Writer;
/// falls back to the fd path for PrintStream-backed writers.
fn native_printwriter_write_string_range(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        if let Some(out_obj) = printwriter_get_backing_writer(ctx, this) {
            // Pass the original String + range args directly — no allocation, no GC hazard.
            let str_val = args.get(1).cloned().unwrap_or(Value::Object(None));
            let off_val = args.get(2).cloned().unwrap_or(Value::Int(0));
            let len_val = args.get(3).cloned().unwrap_or(Value::Int(0));
            let _ = ctx.invoke_virtual(
                out_obj,
                "write",
                "(Ljava/lang/String;II)V",
                &[str_val, off_val, len_val],
            );
            return Ok(None);
        }
    }
    native_printstream_write_string_range(ctx, args)
}

/// `PrintWriter.write(int c)V` — routes through the underlying `Writer out`
/// so single-char writes (e.g. JSON-quoting `"` from `ModelNode.toString()`)
/// reach the Writer.  No-op for fd-backed streams (those are handled by
/// `print*`/`println*` natives).
pub(crate) fn native_printwriter_write_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        if let Some(out_obj) = printwriter_get_backing_writer(ctx, this) {
            let ch = args.get(1).cloned().unwrap_or(Value::Int(0));
            let _ = ctx.invoke_virtual(out_obj, "write", "(I)V", &[ch]);
            return Ok(None);
        }
    }
    Ok(None)
}

pub(crate) fn native_juli_filehandler_clean(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let read_string_field =
        |ctx: &mut dyn NativeContext, name: &str| match ctx.get_field_by_name(this, name) {
            Value::Object(Some(value)) => ctx.read_string(value),
            _ => None,
        };
    let (Some(directory), Some(prefix), Some(suffix)) = (
        read_string_field(ctx, "directory"),
        read_string_field(ctx, "prefix"),
        read_string_field(ctx, "suffix"),
    ) else {
        return Ok(None);
    };
    let max_days = match ctx.get_field_by_name(this, "maxDays") {
        Value::Object(Some(value)) => match ctx.invoke_virtual(value, "intValue", "()I", &[])? {
            Some(Value::Int(days)) if days >= 0 => days as i64,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    // Tomcat rotates once per LocalDate and removes files strictly older than
    // maxDays. Its regular cleaner uses an executor; run this tiny, bounded
    // deletion synchronously so VM executor timing cannot leave stale files.
    let expired = juli_utc_date_days_ago(max_days + 1);
    let path = std::path::Path::new(&directory).join(format!("{prefix}{expired}{suffix}"));
    let _ = std::fs::remove_file(path);
    Ok(None)
}

// ===========================================================================
// Phase 31: java.util.logging + java.util.Locale
// ===========================================================================

pub(crate) fn register_log4j_stacklocator_bridge(registry: &mut NativeMethodRegistry) {
    fn log4j_stack_locator_caller(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        first_string: usize,
    ) -> MethodCallResult {
        let fqcn = match args.get(first_string) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let pkg = match args.get(first_string + 1) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        };
        let fqcn_internal = fqcn.replace('.', "/");
        let pkg_internal = pkg.replace('.', "/");
        let trace = ctx.capture_stack_trace(0);
        let mut seen_fqcn = false;
        for frame in trace.iter().rev() {
            if !seen_fqcn {
                if frame.class_name.as_ref() == fqcn_internal {
                    seen_fqcn = true;
                }
                continue;
            }
            if frame.class_name.as_ref() == fqcn_internal {
                continue;
            }
            if pkg_internal.is_empty() || frame.class_name.starts_with(&pkg_internal) {
                if let Some(cid) = ctx.class_id_by_name(&frame.class_name) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
            }
        }
        for frame in trace.iter().rev() {
            let class_name = frame.class_name.as_ref();
            if class_name == "org/apache/logging/log4j/util/StackLocator"
                || class_name == "org/apache/logging/log4j/util/StackLocatorUtil"
                || class_name.starts_with("java/lang/StackWalker")
                || class_name.starts_with("java/lang/StackStreamFactory")
                || class_name.starts_with("jdk/internal/reflect/")
                || class_name.starts_with("sun/reflect/")
            {
                continue;
            }
            if pkg_internal.is_empty() || class_name.starts_with(&pkg_internal) {
                if let Some(cid) = ctx.class_id_by_name(class_name) {
                    return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
                }
            }
        }
        let fallback = ctx
            .class_id_by_name("java/lang/Object")
            .map(|cid| Value::Object(Some(ctx.get_class_mirror(cid))))
            .unwrap_or(Value::Object(None));
        Ok(Some(fallback))
    }
    registry.register(
        "org/apache/logging/log4j/util/StackLocator",
        "getCallerClass",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| log4j_stack_locator_caller(ctx, args, 1),
    );
    registry.register(
        "org/apache/logging/log4j/util/StackLocatorUtil",
        "getCallerClass",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| log4j_stack_locator_caller(ctx, args, 0),
    );
}

pub(crate) fn register_logging_natives(registry: &mut NativeMethodRegistry) {
    let logger = "java/util/logging/Logger";
    let level = "java/util/logging/Level";

    // Logger = 2-field synthetic (name, level)
    registry.register(
        logger,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_logger_get,
    );
    registry.register(
        logger,
        "getGlobal",
        "()Ljava/util/logging/Logger;",
        native_logger_get_global,
    );
    registry.register(
        logger,
        "getName",
        "()Ljava/lang/String;",
        native_logger_get_name,
    );
    registry.register(
        logger,
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        |ctx, args| {
            let Some(Value::Object(Some(this))) = args.first() else {
                return Ok(None);
            };
            let Some(Value::Object(Some(handler))) = args.get(1) else {
                return Ok(None);
            };
            let handlers = match ctx.get_field(*this, 2) {
                Value::Object(Some(list)) => list,
                _ => {
                    let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
                    cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))])?;
                    ctx.set_field(*this, 2, Value::Object(Some(list)));
                    list
                }
            };
            let _ = cratonvm_native_collections::native_al_add(
                ctx,
                &[Value::Object(Some(handlers)), Value::Object(Some(*handler))],
            );
            Ok(None)
        },
    );
    // Level-aware logging methods. SEVERE=1000, WARNING=900, INFO=800,
    // CONFIG=700, FINE=500, FINER=400, FINEST=300. A message is logged if the
    // logger's current level <= the method's level.
    registry.register(logger, "info", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 800)
    });
    registry.register(logger, "warning", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 900)
    });
    registry.register(logger, "severe", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 1000)
    });
    registry.register(logger, "fine", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 500)
    });
    registry.register(logger, "finer", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 400)
    });
    registry.register(logger, "finest", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 300)
    });
    registry.register(logger, "config", "(Ljava/lang/String;)V", |ctx, args| {
        native_logger_log_if(ctx, args, 700)
    });
    registry.register(
        logger,
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let level = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, LOGGER_FIELD_LEVEL, level);
            Ok(None)
        },
    );
    registry.register(
        logger,
        "getLevel",
        "()Ljava/util/logging/Level;",
        native_logger_get_level,
    );
    registry.register(
        logger,
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(1))),
            };
            let arg_val = match args.get(1) {
                Some(Value::Object(Some(l))) => ctx.get_field(*l, 1).as_int().unwrap_or(800),
                _ => 800,
            };
            let current = match ctx.get_field(this, LOGGER_FIELD_LEVEL) {
                Value::Object(Some(l)) => ctx.get_field(l, 1).as_int().unwrap_or(800),
                _ => 800, // default INFO
            };
            // A message is loggable if its level >= logger's current level
            Ok(Some(Value::Int(if arg_val >= current { 1 } else { 0 })))
        },
    );
    registry.register(
        logger,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        native_logger_log_level,
    );

    registry.register(level, "<clinit>", "()V", native_level_clinit);
    registry.register(level, "<init>", "(Ljava/lang/String;I)V", native_level_init);
    registry.register(
        level,
        "<init>",
        "(Ljava/lang/String;ILjava/lang/String;)V",
        native_level_init,
    );
    // Level constants
    registry.register(
        level,
        "ALL",
        "()Ljava/util/logging/Level;",
        native_level_all,
    );
    registry.register(
        level,
        "SEVERE",
        "()Ljava/util/logging/Level;",
        native_level_severe,
    );
    registry.register(
        level,
        "WARNING",
        "()Ljava/util/logging/Level;",
        native_level_warning,
    );
    registry.register(
        level,
        "INFO",
        "()Ljava/util/logging/Level;",
        native_level_info,
    );
    registry.register(
        level,
        "CONFIG",
        "()Ljava/util/logging/Level;",
        native_level_config,
    );
    registry.register(
        level,
        "FINE",
        "()Ljava/util/logging/Level;",
        native_level_fine,
    );
    registry.register(
        level,
        "FINER",
        "()Ljava/util/logging/Level;",
        native_level_finer,
    );
    registry.register(
        level,
        "FINEST",
        "()Ljava/util/logging/Level;",
        native_level_finest,
    );
    registry.register(
        level,
        "OFF",
        "()Ljava/util/logging/Level;",
        native_level_off,
    );
    registry.register(
        level,
        "getName",
        "()Ljava/lang/String;",
        native_level_get_name,
    );
    registry.register(level, "intValue", "()I", native_level_int_value);
    registry.register(
        level,
        "toString",
        "()Ljava/lang/String;",
        native_level_get_name,
    );
}

/// SLF4J 1.7 static-binder pattern. SLF4J 1.7 wires its API to a logging
/// backend via three companion classes that the impl JAR (slf4j-log4j12,
/// logback-classic, slf4j-simple, ...) provides on the classpath:
///
///   org.slf4j.impl.StaticLoggerBinder   — supplies ILoggerFactory
///   org.slf4j.impl.StaticMDCBinder      — supplies MDCAdapter
///   org.slf4j.impl.StaticMarkerBinder   — supplies IMarkerFactory
///
/// Each is loaded from MDC.<clinit> / LoggerFactory.<clinit> via something
/// like `StaticMDCBinder.getSingleton().getMDCA()`. When the impl JAR
/// isn't visible to the VM's class loader (Spring Boot fat-jar nested
/// BOOT-INF/lib/ visibility issue, or the user simply didn't add one),
/// those <clinit>s blow up with a NoSuchMethodError that surfaces as a
/// linkage error during JIT dispatch.
///
/// The adapter/factory the binders return here is never consulted by the
/// SLF4J API call sites we model (the MDC / Logger / Marker natives all
/// short-circuit before delegating) — the binders just have to exist so
/// that <clinit> can complete.
///
/// Registered both from `register_slf4j_natives` (synthetic-jdk path) and
/// directly from the real-JDK boot in `vm/src/vm/vm_init.rs` so Spring
/// Boot 2.x fat-jars without an SLF4J impl on the classpath survive
/// boot. (Spring Boot 3.x ships SLF4J 2.x which uses the
/// `META-INF/services/org.slf4j.spi.SLF4JServiceProvider` discovery
/// mechanism instead, so this stub doesn't fire — and is harmless if the
/// classes happen to be present, because last-writer-wins on the native
/// registry just leaves the real bytecode dispatch in place.)
pub fn register_slf4j_binder_stubs_pub(registry: &mut NativeMethodRegistry) {
    // FIX (log4j2loggingsystemtests-correlationid-mdc-binder-shadowed): same
    // bug class as `micrometer-metrics-logbackcondition-wrong-binder-20260724`
    // below (`StaticLoggerBinder`), just never applied here too — these three
    // natives used to apply UNCONDITIONALLY, shadowing a REAL
    // `org/slf4j/impl/StaticMDCBinder` class whenever one is genuinely on the
    // classpath (e.g. `@ConfigureClasspathToPreferLog4j2`'s `ClassPathOverrides`
    // pulling in `log4j-slf4j-impl` on a `ModifiedClassPathClassLoader`, whose
    // real `getMDCA()` returns a real `Log4jMDCAdapter` bridging straight into
    // `org.apache.logging.log4j.ThreadContext`). With the stub always winning,
    // `org.slf4j.MDC.put`/`setContextMap` never reached `ThreadContext` at all,
    // so Log4j2's `%correlationId` pattern converter never saw the MDC values
    // an app set via the SLF4J facade
    // (`Log4J2LoggingSystemTests.correlationLoggingTo*WhenExpectCorrelationIdTrueAndMdcContent`/
    // `WhenHasCorrelationPattern`, all real-JDK-A/B-confirmed CratonVM-only).
    // Same fix shape as `StaticLoggerBinder` below: prefer the real
    // bytecode via the `*_bytecode_only` primitives when a genuine
    // (non-bridge) declaration is present; only fabricate the synthetic
    // `BasicMDCAdapter` placeholder when no real implementation exists.
    registry.register(
        "org/slf4j/impl/StaticMDCBinder",
        "getSingleton",
        "()Lorg/slf4j/impl/StaticMDCBinder;",
        |ctx, _| {
            if let Some(cid) = ctx.class_id_by_name("org/slf4j/impl/StaticMDCBinder") {
                if ctx.class_declares_method(
                    cid,
                    "getSingleton",
                    "()Lorg/slf4j/impl/StaticMDCBinder;",
                ) {
                    if let Ok(Some(real)) = ctx.invoke_special_bytecode_only(
                        "org/slf4j/impl/StaticMDCBinder",
                        "getSingleton",
                        "()Lorg/slf4j/impl/StaticMDCBinder;",
                        &[],
                    ) {
                        return Ok(Some(real));
                    }
                }
            }
            let s = alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticMDCBinder", 1);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    registry.register(
        "org/slf4j/impl/StaticMDCBinder",
        "getMDCA",
        "()Lorg/slf4j/spi/MDCAdapter;",
        |ctx, args| {
            if let Ok(this) = obj_arg(args, 0) {
                let cid = ctx.class_id_of_object(this);
                if ctx.class_declares_method(cid, "getMDCA", "()Lorg/slf4j/spi/MDCAdapter;") {
                    if let Ok(Some(real)) = ctx.invoke_virtual_bytecode_only(
                        this,
                        "getMDCA",
                        "()Lorg/slf4j/spi/MDCAdapter;",
                        &[],
                    ) {
                        return Ok(Some(real));
                    }
                }
            }
            let a = alloc_concurrent_synthetic(ctx, "org/slf4j/helpers/BasicMDCAdapter", 0);
            Ok(Some(Value::Object(Some(a))))
        },
    );
    registry.register(
        "org/slf4j/impl/StaticMDCBinder",
        "getMDCAdapterClassStr",
        "()Ljava/lang/String;",
        |ctx, args| {
            if let Ok(this) = obj_arg(args, 0) {
                let cid = ctx.class_id_of_object(this);
                if ctx.class_declares_method(cid, "getMDCAdapterClassStr", "()Ljava/lang/String;")
                {
                    if let Ok(Some(real)) = ctx.invoke_virtual_bytecode_only(
                        this,
                        "getMDCAdapterClassStr",
                        "()Ljava/lang/String;",
                        &[],
                    ) {
                        return Ok(Some(real));
                    }
                }
            }
            let s = ctx.create_string("org.slf4j.helpers.BasicMDCAdapter");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // BUG (micrometer-metrics-logbackcondition-wrong-binder-20260724): both
    // natives below used to apply UNCONDITIONALLY — registered natives shadow
    // ANY class of this name at every interpreter dispatch site (WP0.1
    // native-override-priority), regardless of whether a REAL binder jar
    // (log4j-slf4j-impl, slf4j-log4j12, ...) is actually on the classpath
    // with its own genuine `getSingleton`/`getLoggerFactory` bytecode. That
    // silently discarded a real, non-Logback binding: Spring Boot's
    // `@ConfigureClasspathToPreferLog4j2` (`ClassPathOverrides` installing
    // log4j-slf4j-impl ahead of logback-classic on a `ModifiedClassPathClassLoader`)
    // makes `org/slf4j/impl/StaticLoggerBinder` resolve to log4j's REAL class
    // — whose real `getLoggerFactory()` legitimately returns a Log4j-backed
    // `ILoggerFactory` — but this fallback kept fabricating a Logback
    // `LoggerContext` anyway (logback-classic remains on the classpath, just
    // not what THIS StaticLoggerBinder should bind to), so
    // `LogbackLoggingCondition`/`LogbackMetricsAutoConfiguration` wrongly saw
    // Logback as active and registered a `logbackMetrics` bean the test
    // asserts must be absent (`LogbackMetricsAutoConfigurationWithLog4j2AndLogbackTests
    // .doesNotConfigureLogbackMetrics`), and separately broke
    // `Log4J2MetricsWithLog4jLoggerContextAutoConfigurationTests`. Now: run
    // the receiver/class's own real bytecode via the `*_bytecode_only`
    // primitives (bypassing native re-dispatch to avoid self-recursion) when
    // `class_declares_method` confirms a genuine (non-bridge) declaration is
    // present — i.e. a real binder jar was actually found. Only fabricate the
    // synthetic Logback-preferring placeholder below when no real
    // implementation exists anywhere (the "impl JAR isn't visible" case this
    // stub was written for — see the module doc above).
    registry.register(
        "org/slf4j/impl/StaticLoggerBinder",
        "getSingleton",
        "()Lorg/slf4j/impl/StaticLoggerBinder;",
        |ctx, _| {
            if let Some(cid) = ctx.class_id_by_name("org/slf4j/impl/StaticLoggerBinder") {
                if ctx.class_declares_method(
                    cid,
                    "getSingleton",
                    "()Lorg/slf4j/impl/StaticLoggerBinder;",
                ) {
                    if let Ok(Some(real)) = ctx.invoke_special_bytecode_only(
                        "org/slf4j/impl/StaticLoggerBinder",
                        "getSingleton",
                        "()Lorg/slf4j/impl/StaticLoggerBinder;",
                        &[],
                    ) {
                        return Ok(Some(real));
                    }
                }
            }
            let s = alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticLoggerBinder", 1);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    // Spring Boot's LogbackLoggingSystem checks
    // `LoggerFactory.getILoggerFactory()` returns a
    // `ch.qos.logback.classic.LoggerContext`. Real-JDK LoggerFactory
    // bytecode calls `StaticLoggerBinder.getSingleton().getLoggerFactory()`
    // — so this native is the final arbiter of the returned type.
    // Prefer allocating a real-classed LoggerContext when logback-classic
    // is on the classpath; fall back to the synthetic ILoggerFactory
    // for apps that don't ship logback (the slf4j-API <clinit> just
    // needs a non-null return; nothing else inspects this object's class).
    registry.register(
        "org/slf4j/impl/StaticLoggerBinder",
        "getLoggerFactory",
        "()Lorg/slf4j/ILoggerFactory;",
        |ctx, args| {
            // Prefer the RECEIVER's own real bytecode (see the BUG note
            // above `getSingleton`) — `getSingleton()` may have handed back a
            // genuinely-real binder instance (a real jar was found), in
            // which case its own `getLoggerFactory()` already knows the
            // correct backend and must not be second-guessed here.
            if let Ok(this) = obj_arg(args, 0) {
                let cid = ctx.class_id_of_object(this);
                if ctx.class_declares_method(cid, "getLoggerFactory", "()Lorg/slf4j/ILoggerFactory;")
                {
                    if let Ok(Some(real)) = ctx.invoke_virtual_bytecode_only(
                        this,
                        "getLoggerFactory",
                        "()Lorg/slf4j/ILoggerFactory;",
                        &[],
                    ) {
                        return Ok(Some(real));
                    }
                }
            }
            if ctx
                .ensure_class_initialized("ch/qos/logback/classic/LoggerContext")
                .is_ok()
            {
                if let Some(Value::Object(Some(context))) =
                    ctx.new_object_initialized("ch/qos/logback/classic/LoggerContext", "()V", &[])?
                {
                    return Ok(Some(Value::Object(Some(context))));
                }
            }
            let f = alloc_concurrent_synthetic(ctx, "org/slf4j/ILoggerFactory", 0);
            Ok(Some(Value::Object(Some(f))))
        },
    );
    registry.register(
        "org/slf4j/impl/StaticLoggerBinder",
        "getLoggerFactoryClassStr",
        "()Ljava/lang/String;",
        |ctx, _| {
            let name = if ctx
                .ensure_class_initialized("ch/qos/logback/classic/LoggerContext")
                .is_ok()
            {
                "ch.qos.logback.classic.LoggerContext"
            } else {
                "org.slf4j.helpers.NOPLoggerFactory"
            };
            let s = ctx.create_string(name);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    registry.register(
        "org/slf4j/impl/StaticMarkerBinder",
        "getSingleton",
        "()Lorg/slf4j/impl/StaticMarkerBinder;",
        |ctx, _| {
            let s = alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticMarkerBinder", 1);
            Ok(Some(Value::Object(Some(s))))
        },
    );
    registry.register(
        "org/slf4j/impl/StaticMarkerBinder",
        "getMarkerFactory",
        "()Lorg/slf4j/IMarkerFactory;",
        |ctx, _| {
            // Allocate AND run the real MarkerFactory.<init> so its inline
            // `markerMap = new ConcurrentHashMap<>()` initializer fires.
            // Without this, getMarker(name) NPEs on markerMap.get(name)
            // (observed during Kafka 4.2.0 boot: kafka/utils/Logging$.<clinit>).
            //
            // Kafka ships log4j-slf4j-impl whose `StaticLoggerBinder.<init>`
            // performs `checkcast Log4jMarkerFactory` on whatever this
            // returns. If we hand back a `BasicMarkerFactory`, that
            // checkcast throws ClassCastException, the slf4j clinit is
            // swallowed, and downstream Kafka.main() exits with code 1.
            // Prefer the log4j-specific subclass when it's on the
            // classpath; fall back to BasicMarkerFactory otherwise.
            let preferred = "org/apache/logging/slf4j/Log4jMarkerFactory";
            let fallback = "org/slf4j/helpers/BasicMarkerFactory";
            let chosen = if ctx.ensure_class_initialized(preferred).is_ok() {
                preferred
            } else {
                fallback
            };
            let f = alloc_concurrent_synthetic(ctx, chosen, 0);
            let _ = ctx.invoke_special(chosen, "<init>", "()V", &[Value::Object(Some(f))]);
            Ok(Some(Value::Object(Some(f))))
        },
    );
    registry.register(
        "org/slf4j/impl/StaticMarkerBinder",
        "getMarkerFactoryClassStr",
        "()Ljava/lang/String;",
        |ctx, _| {
            let preferred = "org/apache/logging/slf4j/Log4jMarkerFactory";
            let chosen = if ctx.ensure_class_initialized(preferred).is_ok() {
                "org.apache.logging.slf4j.Log4jMarkerFactory"
            } else {
                "org.slf4j.helpers.BasicMarkerFactory"
            };
            let s = ctx.create_string(chosen);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // BasicMDCAdapter no-op surface — the adapter the binder above returns.
    // Our MDC stubs implement put/get/... directly so these don't fire on
    // user paths, but newer SLF4J façades sometimes route through the
    // adapter; keeping these no-ops avoids surprise NSME later.
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "remove",
        "(Ljava/lang/String;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "clear",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "getCopyOfContextMap",
        "()Ljava/util/Map;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        "org/slf4j/helpers/BasicMDCAdapter",
        "setContextMap",
        "(Ljava/util/Map;)V",
        |_ctx, _args| Ok(None),
    );

    // SB2-NPE: Spring Boot 2.x fat-jars hit
    // `LogAdapter$Slf4jLog.<init>(Logger)` with a null logger because the
    // bytecode flow `LoggerFactory.getLogger(name) -> getILoggerFactory()
    // -> ILoggerFactory.getLogger(name)` lands on the synthetic
    // ILoggerFactory we returned above, which has no `getLogger` native
    // and so returns null (default-Object reference) → NPE on
    // `logger.getName()` at LogAdapter.java:279.
    //
    // Mirror the synthetic-jdk `LoggerFactory.getLogger(String)` stub on
    // the real-JDK path: register `ILoggerFactory.getLogger(String)` to
    // hand back a synthetic Logger whose `name` field is the requested
    // string, plus the small surface (`getName`, `is*Enabled`, `trace/
    // debug/info/warn/error`) that LogAdapter and friends invoke.
    //
    // We use BOTH `set_field_by_name("name", ...)` (matches the real
    // `org.slf4j.helpers.NamedLoggerBase.name` slot when the synthetic
    // Logger object happens to share that layout) and a slot-0 fallback
    // (matches our synthetic-jdk `SLF4J_NAME = 0` invariant) so the
    // accessor below can find the name regardless of which path
    // allocated the Logger.
    registry.register(
        "org/slf4j/ILoggerFactory",
        "getLogger",
        "(Ljava/lang/String;)Lorg/slf4j/Logger;",
        |ctx, args| {
            let name = args.get(1).copied().unwrap_or(Value::Object(None));
            let logger = alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2);
            // Best-effort dual-write: name-by-name (real layout) +
            // name-at-slot-0 (synthetic layout).
            ctx.set_field_by_name(logger, "name", name);
            ctx.set_field(logger, 0, name);
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    // Logger.getName() — read by-name first, fall back to slot 0. The
    // synthetic-jdk `register_slf4j_natives` registers a slot-0-reading
    // version that is replayed AFTER this fn (last-writer-wins is
    // harmless because both versions return the same value when both
    // writes happened).
    registry.register(
        "org/slf4j/Logger",
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Prefer real-layout `name` field; fall back to synthetic
            // slot 0; final fallback is empty string so callers like
            // `Slf4jLog.<init>` never see null.
            let v = match ctx.get_field_by_name(this, "name") {
                Value::Object(Some(s)) => Value::Object(Some(s)),
                _ => match ctx.get_field(this, 0) {
                    Value::Object(Some(s)) => Value::Object(Some(s)),
                    _ => Value::Object(Some(ctx.create_string(""))),
                },
            };
            Ok(Some(v))
        },
    );

    // Retain trace/debug suppression, but keep the production log levels
    // live. Spring's commons-logging and SLF4J adapters guard their warning
    // paths with these checks; returning false here silently erased records
    // instead of delivering them to the Java console stream.
    fn slf4j_false(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(0)))
    }
    fn slf4j_enabled(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(1)))
    }
    registry.register("org/slf4j/Logger", "isTraceEnabled", "()Z", slf4j_false);
    registry.register("org/slf4j/Logger", "isDebugEnabled", "()Z", slf4j_false);
    registry.register("org/slf4j/Logger", "isInfoEnabled", "()Z", slf4j_enabled);
    registry.register("org/slf4j/Logger", "isWarnEnabled", "()Z", slf4j_enabled);
    registry.register("org/slf4j/Logger", "isErrorEnabled", "()Z", slf4j_enabled);
    // Marker-aware variants: SLF4J `Logger` interface declares
    // `is{Trace,Debug,Info,Warn,Error}Enabled(Marker)`. Kafka (kafka.Kafka via
    // Scala) routes log calls through these overloads on first startup; the
    // synthetic Log objects returned by our `LoggerFactory.getLogger` natives
    // otherwise dispatch to the abstract interface decl and throw
    // `AbstractMethodError: org/slf4j/Logger.isErrorEnabled(Lorg/slf4j/Marker;)Z`.
    registry.register(
        "org/slf4j/Logger",
        "isTraceEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_false,
    );
    registry.register(
        "org/slf4j/Logger",
        "isDebugEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_false,
    );
    registry.register(
        "org/slf4j/Logger",
        "isInfoEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_enabled,
    );
    registry.register(
        "org/slf4j/Logger",
        "isWarnEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_enabled,
    );
    registry.register(
        "org/slf4j/Logger",
        "isErrorEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_enabled,
    );

    // Round 63: Keycloak — KerberosJdkProvider.isKerberosAvailable() probes the
    // JCE provider list via java.security.Provider.checkInitialized, which
    // throws IllegalStateException in our environment because the security
    // provider isn't initialized at Profile.configure time. We have no
    // Kerberos support anyway, so return false unconditionally and let
    // Profile.configure() advance past the KerberosJdkProvider check.
    registry.register(
        "org/keycloak/common/util/KerberosJdkProvider",
        "isKerberosAvailable",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))),
    );

    // Round 82 stub REMOVED (keycloak-quarkus-boot gap #3): the no-op stub of
    // `PropertyMappers$MappersConfig.sanitizeDisabledMappers` was added to dodge a
    // `PropertyException("Duplicated mapper for key 'kc.file'")` (the
    // MultivaluedHashMap was seen accumulating duplicate mappers because the
    // sanitize step runs from both initConfig() and
    // PersistedConfigSource.runWithDisabled). But `sanitizeDisabledMappers` is
    // ALSO where Keycloak configures the feature `Profile`: it calls
    // `DisabledMappersInterceptor.runWithDisabled(Runnable)` → a runnable that
    // (via `lambda$sanitizeDisabledMappers$3`) calls
    // `Environment.getCurrentOrCreateFeatureProfile()` → `Profile.configure(...)`,
    // which sets the static `Profile.CURRENT`. No-op'ing the method left
    // `Profile.CURRENT` null, so the real Keycloak (Quarkus) server boot NPE'd
    // later at `Profile.isFeatureEnabled` ("Cannot read field 'features' because
    // the object is null") during CLI config validation
    // (PropertyMapper.isRequired → InfinispanUtils.isRemoteInfinispan). Run the
    // real bytecode so Profile is configured; if the duplicate-mapper
    // accumulation recurs it must be fixed at its source (the map/collection
    // layer), not by skipping this method.

    // Preserve lightweight native fallback logging without discarding its
    // observable console output. The full Logback pipeline is not available
    // on every supported classpath, but Spring's OutputCaptureExtension must
    // see INFO/WARN/ERROR records exactly as it sees direct System.out writes.
    fn slf4j_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    let lg = "org/slf4j/Logger";
    for sig in [
        "(Ljava/lang/String;)V",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ] {
        registry.register(lg, "trace", sig, slf4j_noop);
        registry.register(lg, "debug", sig, slf4j_noop);
        registry.register(lg, "info", sig, slf4j_log_msg);
        registry.register(lg, "warn", sig, slf4j_log_msg);
        registry.register(lg, "error", sig, slf4j_log_msg);
    }

    // Logback LoggerContext bridge — paired with the
    // `StaticLoggerBinder.getLoggerFactory` native above. When logback
    // is on the fat-jar classpath we return a real-classed
    // `LoggerContext` instance so Spring Boot's
    // `LoggingSystemFactory.LogbackLoggingSystem.beforeInitialize()`
    // class check passes. The instance is initialized through Logback's real
    // constructor; `getLogger(String)` is now left to Logback's real
    // bytecode (see below) instead of a native override.
    //
    // FIXED 2026-07-17 (conditionevaluationreport-capturedoutput-empty-cluster):
    // `LoggerContext.getLogger(String)`/`getLogger(Class)` used to be
    // natively overridden to fabricate a throwaway 2-field synthetic
    // `ch.qos.logback.classic.Logger` (name + level only), completely
    // bypassing the real object the caller's `LoggerContext` already
    // constructed (its real `root` Logger, its real `loggerCache`, its real
    // parent-chain walk). This ran unconditionally in real-JDK mode even
    // when `ch/qos/logback/classic/LoggerContext` was fully real (per the
    // `StaticLoggerBinder.getLoggerFactory` fix below, which already
    // prefers `ctx.new_object_initialized` when the real class is
    // loadable) — an overlay/real-class-layout mismatch in the same family
    // as the already-fixed `loggerContextListenerList` corruption (see
    // docs/internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md).
    // Every `ch/qos/logback/classic/Logger` instance method below
    // (`addAppender`, `info`/`warn`/`error`/etc., `filterAndLog_*`) was
    // ALSO natively stubbed to a no-op, so even a caller holding a real
    // `LoggerContext.root` reference could never attach a `ConsoleAppender`
    // or have a log record actually reach one: `BasicConfigurator.configure()`
    // would call `context.getLogger("ROOT")`, get a disposable synthetic
    // Logger back, call `addAppender` on it (a no-op), and the REAL root
    // logger inside `LoggerContext` never received the appender — so
    // `OutputCaptureExtension`'s `CapturedOutput` (and every other consumer
    // of Logback-routed log output) always saw the empty string, regardless
    // of `System.out`/`System.err` redirection state. Removed the
    // `getLogger` overrides here and every `ch/qos/logback/classic/Logger`
    // no-op below so real Logback bytecode drives logger creation,
    // appender attachment, and the whole `filterAndLog` → appender chain.
    let lb_ctx = "ch/qos/logback/classic/LoggerContext";
    registry.register(lb_ctx, "getName", "()Ljava/lang/String;", |ctx, _| {
        Ok(Some(Value::Object(Some(ctx.create_string("default")))))
    });
    registry.register(lb_ctx, "setName", "(Ljava/lang/String;)V", |_, _| Ok(None));
    // FIX (loggingapplicationlistenertests-logbacklogsystemtests-reset-noop):
    // `start`/`stop`/`reset`/`isStarted` were STILL natively stubbed here
    // from the same pre-fix era the `getLogger` comment above documents —
    // `reset()`'s no-op meant `LoggerContext.reset()` (called by
    // `LogbackLoggingSystem.stopAndReset` between every test-method-scoped
    // re-`initialize()`) never ran `Logger.recursiveReset()` /
    // `detachAndStopAllAppenders()`, so EVERY previously-configured
    // ConsoleAppender (including Logback's own auto-bootstrap default one)
    // stayed permanently attached to the root logger — each subsequent test
    // method's log calls fired through every prior test's appenders too
    // (duplicate/leaked output across `@Test` methods in the same class,
    // e.g. `LoggingApplicationListenerTests`, `LogbackLoggingSystemTests`,
    // `SpringBootJoranConfiguratorTests`). Confirmed via a minimal direct
    // Logback repro (`ctx.reset()` leaving `root.iteratorForAppenders()`
    // non-empty) and real-JDK A/B. `ContextBase`'s real `start`/`stop`/
    // `reset`/`isStarted` are simple, safe real bytecode (a boolean field
    // flip, a few real method calls) — the field-layout risk this stub
    // family originally guarded against was `<init>`-shaped, and `<init>`
    // has been real bytecode since the `getLogger` fix above (guarded by
    // `logback_context_construction_and_state_are_not_native_overridden`).

    // `ch/qos/logback/classic/Logger` (getLogger, addAppender, info/warn/
    // error/etc., filterAndLog_*) is intentionally NOT natively overridden
    // here anymore — see the FIXED note above the `getLogger` removal. A
    // concurrent session's alternative fix (keep the stub, route info/warn/
    // error through `emit_framework_log`, `setLevel`/`isDebugEnabled` still
    // hardcoded) is superseded here for the reasons noted above the
    // `LogMessage.toString()` native. Real Logback bytecode now owns logger
    // creation, appender attachment, and the filterAndLog → appender
    // dispatch chain.
}

/// Spring Boot 3.2 logback bridge — registered unconditionally in real-JDK
/// mode by `vm_init.rs`.
///
/// STALE, REMOVED 2026-07-24 (Cluster C logging-bootstrap batch):
/// `DefaultLogbackConfiguration.apply(LogbackConfigurator)` used to be
/// forced to a no-op here because `ch/qos/logback/classic/LoggerContext`
/// was served via `alloc_concurrent_synthetic` (bypassing logback's real
/// `<init>`), so `apply()`'s first `monitorenter` NPE'd on a null field.
/// That premise no longer holds: `LoggerContext` construction is real
/// bytecode now (confirmed by both the
/// `logback_context_construction_and_state_are_not_native_overridden`
/// regression test below and direct testing — `new LoggerContext()` +
/// `getConfigurationLock()` return a genuinely working `ReentrantLock`).
/// With this no-op left in place, `apply()` silently never installed a
/// `ConsoleAppender`, root level, or pattern layout at all — the actual
/// root cause of `DefaultLogbackConfigurationTests`' failures and (via
/// `LoggingApplicationListener`'s bootstrap path, which calls `apply()`)
/// `LoggingApplicationListenerTests`' captured-output-always-empty
/// failures. Letting real bytecode run instead: `defaults()` +
/// `consoleAppender()` + `config.root(...)` verified directly to work and
/// correctly preserve `LoggerContext` properties/state, both individually
/// and through `apply()`'s own try/finally.
pub fn register_spring_boot_logback_apply(_registry: &mut NativeMethodRegistry) {}

pub(crate) fn register_slf4j_natives(registry: &mut NativeMethodRegistry) {
    let lf = "org/slf4j/LoggerFactory";

    // LoggerFactory.getLogger(String) → Logger
    registry.register(
        lf,
        "getLogger",
        "(Ljava/lang/String;)Lorg/slf4j/Logger;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2);
            ctx.set_field(logger, SLF4J_NAME, name);
            ctx.set_field(logger, SLF4J_LEVEL, Value::Int(1)); // default: DEBUG
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    // LoggerFactory.getLogger(Class) → Logger
    registry.register(
        lf,
        "getLogger",
        "(Ljava/lang/Class;)Lorg/slf4j/Logger;",
        |ctx, args| {
            // Extract class name from the Class mirror
            let name_val = match args.first() {
                Some(Value::Object(Some(class_mirror))) => match ctx.get_field(*class_mirror, 0) {
                    Value::Object(Some(n)) => Value::Object(Some(n)),
                    _ => {
                        let s = ctx.create_string("unknown");
                        Value::Object(Some(s))
                    }
                },
                _ => {
                    let s = ctx.create_string("unknown");
                    Value::Object(Some(s))
                }
            };
            let logger = alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2);
            ctx.set_field(logger, SLF4J_NAME, name_val);
            ctx.set_field(logger, SLF4J_LEVEL, Value::Int(1)); // DEBUG
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    // LoggerFactory.getILoggerFactory() → ILoggerFactory
    registry.register(
        lf,
        "getILoggerFactory",
        "()Lorg/slf4j/ILoggerFactory;",
        |ctx, _| {
            let factory = alloc_concurrent_synthetic(ctx, "org/slf4j/ILoggerFactory", 0);
            Ok(Some(Value::Object(Some(factory))))
        },
    );

    let lg = "org/slf4j/Logger";

    // Logger.getName() → String
    registry.register(lg, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SLF4J_NAME)))
    });

    // Logging methods: trace, debug, info, warn, error
    // trace(String) — instance method; SLF4J TRACE level is below the
    // default threshold so trace() intentionally discards its arguments.
    // NEW-6: documented with the _with_this form so the intent is clear.
    registry.register(lg, "trace", "(Ljava/lang/String;)V", native_noop_with_this);
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        native_noop_with_this,
    );

    // debug(String)
    registry.register(lg, "debug", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        lg,
        "debug",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "debug",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "debug",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );

    // info(String)
    registry.register(lg, "info", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        lg,
        "info",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "info",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "info",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );

    // warn(String)
    registry.register(lg, "warn", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        lg,
        "warn",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "warn",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "warn",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "warn",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        slf4j_log_msg,
    );

    // error(String)
    registry.register(lg, "error", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        lg,
        "error",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "error",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "error",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        lg,
        "error",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        slf4j_log_msg,
    );

    // Level checks — consult the stored level (0=TRACE, 1=DEBUG, 2=INFO, 3=WARN, 4=ERROR)
    registry.register(lg, "isTraceEnabled", "()Z", |ctx, args| {
        let level = match args.first() {
            Some(Value::Object(Some(this))) => {
                ctx.get_field(*this, SLF4J_LEVEL).as_int().unwrap_or(1)
            }
            _ => 1,
        };
        Ok(Some(Value::Int(if level <= 0 { 1 } else { 0 })))
    });
    registry.register(lg, "isDebugEnabled", "()Z", |ctx, args| {
        let level = match args.first() {
            Some(Value::Object(Some(this))) => {
                ctx.get_field(*this, SLF4J_LEVEL).as_int().unwrap_or(1)
            }
            _ => 1,
        };
        Ok(Some(Value::Int(if level <= 1 { 1 } else { 0 })))
    });
    registry.register(lg, "isInfoEnabled", "()Z", |ctx, args| {
        let level = match args.first() {
            Some(Value::Object(Some(this))) => {
                ctx.get_field(*this, SLF4J_LEVEL).as_int().unwrap_or(1)
            }
            _ => 1,
        };
        Ok(Some(Value::Int(if level <= 2 { 1 } else { 0 })))
    });
    registry.register(lg, "isWarnEnabled", "()Z", |ctx, args| {
        let level = match args.first() {
            Some(Value::Object(Some(this))) => {
                ctx.get_field(*this, SLF4J_LEVEL).as_int().unwrap_or(1)
            }
            _ => 1,
        };
        Ok(Some(Value::Int(if level <= 3 { 1 } else { 0 })))
    });
    registry.register(lg, "isErrorEnabled", "()Z", |ctx, args| {
        let level = match args.first() {
            Some(Value::Object(Some(this))) => {
                ctx.get_field(*this, SLF4J_LEVEL).as_int().unwrap_or(1)
            }
            _ => 1,
        };
        Ok(Some(Value::Int(if level <= 4 { 1 } else { 0 })))
    });

    // MDC (Mapped Diagnostic Context) — backed by thread-local HashMap
    let mdc = "org/slf4j/MDC";
    registry.register(
        mdc,
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let key = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(None),
            };
            let value = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(None),
            };
            MDC_MAP.with(|m| m.borrow_mut().insert(key, value));
            Ok(None)
        },
    );
    registry.register(
        mdc,
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let key = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let value = MDC_MAP.with(|m| m.borrow().get(&key).cloned());
            match value {
                Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    registry.register(mdc, "remove", "(Ljava/lang/String;)V", |ctx, args| {
        let key = match args.first() {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => return Ok(None),
        };
        MDC_MAP.with(|m| m.borrow_mut().remove(&key));
        Ok(None)
    });
    registry.register(mdc, "clear", "()V", |_ctx, _args| {
        MDC_MAP.with(|m| m.borrow_mut().clear());
        Ok(None)
    });
    registry.register(
        mdc,
        "getCopyOfContextMap",
        "()Ljava/util/Map;",
        |ctx, _args| {
            // Snapshot the thread-local MDC map into a fresh HashMap.
            let snapshot: Vec<(String, String)> = MDC_MAP.with(|m| {
                m.borrow()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            });
            if snapshot.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let map = alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3);
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
            for (k, v) in snapshot {
                let key_obj = ctx.create_string(&k);
                let val_obj = ctx.create_string(&v);
                cratonvm_native_collections::native_map_put_pub(
                    ctx,
                    &[
                        Value::Object(Some(map)),
                        Value::Object(Some(key_obj)),
                        Value::Object(Some(val_obj)),
                    ],
                )
                .ok();
            }
            Ok(Some(Value::Object(Some(map))))
        },
    );
    registry.register(mdc, "setContextMap", "(Ljava/util/Map;)V", |ctx, args| {
        // Replace the thread-local MDC map with the entries from the provided Map.
        // Step 1: clear current MDC.
        MDC_MAP.with(|m| m.borrow_mut().clear());
        // Step 2: if arg is non-null, iterate keys and copy entries.
        let map = match args.first() {
            Some(Value::Object(Some(m))) => *m,
            _ => return Ok(None),
        };
        // Get key set
        let key_set = match cratonvm_native_collections::native_map_key_set_pub(
            ctx,
            &[Value::Object(Some(map))],
        ) {
            Ok(Some(Value::Object(Some(s)))) => s,
            _ => return Ok(None),
        };
        // Convert set to array via toArray (HashSet has 1-field backing array structure).
        // Iterate the set's backing storage instead by getting size first.
        let size_val =
            cratonvm_native_collections::native_map_size_pub(ctx, &[Value::Object(Some(map))]);
        let size = match size_val {
            Ok(Some(Value::Int(n))) => n,
            _ => 0,
        };
        if size == 0 {
            return Ok(None);
        }
        // The key_set is a HashSet — convert to array by trying its toArray method.
        let arr_result = ctx.invoke_virtual(key_set, "toArray", "()[Ljava/lang/Object;", &[]);
        let arr = match arr_result {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => return Ok(None),
        };
        let arr_len = ctx.array_length(arr);
        for i in 0..arr_len {
            let key_val = ctx.get_array_element(arr, i);
            let key_obj = match key_val {
                Value::Object(Some(o)) => o,
                _ => continue,
            };
            let key_str = ctx.read_string(key_obj).unwrap_or_default();
            // Get value via map.get(key)
            let val_result = cratonvm_native_collections::native_map_get_pub(
                ctx,
                &[Value::Object(Some(map)), Value::Object(Some(key_obj))],
            );
            let val_str = match val_result {
                Ok(Some(Value::Object(Some(v)))) => ctx.read_string(v).unwrap_or_default(),
                _ => String::new(),
            };
            MDC_MAP.with(|m| m.borrow_mut().insert(key_str, val_str));
        }
        Ok(None)
    });

    // Marker — Spring Boot sometimes uses markers
    let marker = "org/slf4j/MarkerFactory";
    registry.register(
        marker,
        "getMarker",
        "(Ljava/lang/String;)Lorg/slf4j/Marker;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let m = alloc_concurrent_synthetic(ctx, "org/slf4j/Marker", 1);
            ctx.set_field(m, 0, name);
            Ok(Some(Value::Object(Some(m))))
        },
    );

    // SLF4J 1.7 static-binder stubs (getSingleton / adapter / factory).
    // Implementation is shared with the real-JDK boot path — see
    // `register_slf4j_binder_stubs_pub` for the full rationale.
    register_slf4j_binder_stubs_pub(registry);

    // --- java.util.logging (JUL) — standard JDK logging ---
    let jul_logger = "java/util/logging/Logger";
    registry.register(
        jul_logger,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = alloc_concurrent_synthetic(ctx, "java/util/logging/Logger", 3);
            ctx.set_field(logger, 0, name);
            ctx.set_field(logger, 1, Value::Int(800)); // INFO level
            Ok(Some(Value::Object(Some(logger))))
        },
    );
    registry.register(
        jul_logger,
        "getGlobal",
        "()Ljava/util/logging/Logger;",
        |ctx, _| {
            let name = ctx.create_string("global");
            let logger = alloc_concurrent_synthetic(ctx, "java/util/logging/Logger", 3);
            ctx.set_field(logger, 0, Value::Object(Some(name)));
            ctx.set_field(logger, 1, Value::Int(800));
            Ok(Some(Value::Object(Some(logger))))
        },
    );
    registry.register(jul_logger, "info", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "warning", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "severe", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "fine", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "finer", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "finest", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(jul_logger, "config", "(Ljava/lang/String;)V", jul_log_msg);
    registry.register(
        jul_logger,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(msg))) = args.get(2) {
                if let Some(s) = ctx.read_string(*msg) {
                    ctx.record_printed_line(format!("[JUL] {}", s));
                }
            }
            Ok(None)
        },
    );
    registry.register(
        jul_logger,
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let logger_level = ctx.get_field(this, 1).as_int().unwrap_or(800);
            let check_level = match args.get(1) {
                Some(Value::Object(Some(lvl))) => ctx.get_field(*lvl, 1).as_int().unwrap_or(800),
                _ => 800,
            };
            Ok(Some(Value::Int(if check_level >= logger_level {
                1
            } else {
                0
            })))
        },
    );
    registry.register(
        jul_logger,
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let level_val = match args.get(1) {
                Some(Value::Object(Some(lvl))) => ctx.get_field(*lvl, 1).as_int().unwrap_or(800),
                _ => 800,
            };
            ctx.set_field(this, 1, Value::Int(level_val));
            Ok(None)
        },
    );
    registry.register(
        jul_logger,
        "getLevel",
        "()Ljava/util/logging/Level;",
        |ctx, _| {
            let level = alloc_concurrent_synthetic(ctx, "java/util/logging/Level", 2);
            let name = ctx.create_string("INFO");
            ctx.set_field(level, 0, Value::Object(Some(name)));
            ctx.set_field(level, 1, Value::Int(800));
            Ok(Some(Value::Object(Some(level))))
        },
    );
    registry.register(
        jul_logger,
        "getName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    registry.register(
        jul_logger,
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let handler = args.get(1).copied().unwrap_or(Value::Object(None));
            // Logger comment in phases_late.rs: field 2 = handlers ArrayList. Some allocations
            // only create 2 fields, so guard with object_num_fields.
            if ctx.object_num_fields(this) > 2 {
                // Lazily initialise the handlers ArrayList if absent.
                let handlers = match ctx.get_field(this, 2) {
                    Value::Object(Some(lst)) => lst,
                    _ => {
                        let lst = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
                        cratonvm_native_collections::native_al_init(
                            ctx,
                            &[Value::Object(Some(lst))],
                        )
                        .ok();
                        ctx.set_field(this, 2, Value::Object(Some(lst)));
                        lst
                    }
                };
                cratonvm_native_collections::native_al_add(
                    ctx,
                    &[Value::Object(Some(handlers)), handler],
                )
                .ok();
            }
            Ok(None)
        },
    );
    registry.register(
        jul_logger,
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let handler = args.get(1).copied().unwrap_or(Value::Object(None));
            if ctx.object_num_fields(this) > 2 {
                if let Value::Object(Some(handlers)) = ctx.get_field(this, 2) {
                    cratonvm_native_collections::native_al_remove_obj(
                        ctx,
                        &[Value::Object(Some(handlers)), handler],
                    )
                    .ok();
                }
            }
            Ok(None)
        },
    );

    // java.util.logging.Level
    let level = "java/util/logging/Level";
    registry.register(
        level,
        "parse",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let lvl = alloc_concurrent_synthetic(ctx, "java/util/logging/Level", 2);
            ctx.set_field(lvl, 0, name);
            ctx.set_field(lvl, 1, Value::Int(800));
            Ok(Some(Value::Object(Some(lvl))))
        },
    );

    // java.util.logging.LogManager
    let lm = "java/util/logging/LogManager";
    registry.register(
        lm,
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        |ctx, _| {
            let mgr = alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0);
            Ok(Some(Value::Object(Some(mgr))))
        },
    );
    registry.register(
        lm,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = alloc_concurrent_synthetic(ctx, "java/util/logging/Logger", 2);
            ctx.set_field(logger, 0, name);
            ctx.set_field(logger, 1, Value::Int(800));
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    // --- Apache Commons Logging ---
    //
    // FIXED 2026-07-17 (conditionevaluationreport-capturedoutput-empty-cluster):
    // this duplicated (and, per the registry's last-write-wins semantics,
    // was shadowed by) `register_essential_natives`'s now-removed
    // `LogFactory.getLog`/`Log.*` overrides — see the longer note there.
    // Removed here too so nothing re-registers the fake `Log`.

    // --- Log4j2 (org.apache.logging.log4j) ---
    let log4j_lm = "org/apache/logging/log4j/LogManager";
    registry.register(
        log4j_lm,
        "getLogger",
        "(Ljava/lang/Class;)Lorg/apache/logging/log4j/Logger;",
        |ctx, args| {
            let name_val = match args.first() {
                Some(Value::Object(Some(mirror))) => match ctx.get_field(*mirror, 0) {
                    Value::Object(Some(n)) => Value::Object(Some(n)),
                    _ => {
                        let s = ctx.create_string("unknown");
                        Value::Object(Some(s))
                    }
                },
                _ => {
                    let s = ctx.create_string("unknown");
                    Value::Object(Some(s))
                }
            };
            let logger = alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2);
            ctx.set_field(logger, 0, name_val);
            ctx.set_field(logger, 1, Value::Int(2)); // INFO
            Ok(Some(Value::Object(Some(logger))))
        },
    );
    registry.register(
        log4j_lm,
        "getLogger",
        "(Ljava/lang/String;)Lorg/apache/logging/log4j/Logger;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2);
            ctx.set_field(logger, 0, name);
            ctx.set_field(logger, 1, Value::Int(2)); // INFO
            Ok(Some(Value::Object(Some(logger))))
        },
    );
    registry.register(
        log4j_lm,
        "getRootLogger",
        "()Lorg/apache/logging/log4j/Logger;",
        |ctx, _| {
            let name = ctx.create_string("ROOT");
            let logger = alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2);
            ctx.set_field(logger, 0, Value::Object(Some(name)));
            ctx.set_field(logger, 1, Value::Int(2));
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    register_log4j_stacklocator_bridge(registry);

    let log4j_lg = "org/apache/logging/log4j/Logger";
    // Log4j2 trace — instance method, below default threshold. NEW-6.
    registry.register(
        log4j_lg,
        "trace",
        "(Ljava/lang/String;)V",
        native_noop_with_this,
    );
    registry.register(
        log4j_lg,
        "trace",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        native_noop_with_this,
    );
    registry.register(log4j_lg, "debug", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        log4j_lg,
        "debug",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(log4j_lg, "info", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        log4j_lg,
        "info",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(log4j_lg, "warn", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        log4j_lg,
        "warn",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(log4j_lg, "error", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(
        log4j_lg,
        "error",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg,
    );
    registry.register(
        log4j_lg,
        "error",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        slf4j_log_msg,
    );
    registry.register(log4j_lg, "fatal", "(Ljava/lang/String;)V", slf4j_log_msg);
    registry.register(log4j_lg, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    registry.register(log4j_lg, "isTraceEnabled", "()Z", |_, _| {
        Ok(Some(Value::Int(0)))
    });
    registry.register(log4j_lg, "isDebugEnabled", "()Z", |_, _| {
        Ok(Some(Value::Int(1)))
    });
    registry.register(log4j_lg, "isInfoEnabled", "()Z", |_, _| {
        Ok(Some(Value::Int(1)))
    });
    registry.register(log4j_lg, "isWarnEnabled", "()Z", |_, _| {
        Ok(Some(Value::Int(1)))
    });
    registry.register(log4j_lg, "isErrorEnabled", "()Z", |_, _| {
        Ok(Some(Value::Int(1)))
    });

    // --- Logback (ch.qos.logback) ---
    //
    // FIXED 2026-07-17 (conditionevaluationreport-capturedoutput-empty-cluster):
    // this used to duplicate (and conflict with) `register_slf4j_binder_stubs_pub`'s
    // now-removed `LoggerContext.getLogger`/`ch/qos/logback/classic/Logger`
    // overrides with a SECOND, differently-shaped 2-field synthetic Logger
    // (name + level) whose `debug`/`info`/`warn`/`error` routed through
    // `slf4j_log_msg` (a fake "[LOG] name - message" formatter) instead of
    // real Logback `filterAndLog` → appender dispatch. Neither synthetic
    // Logger ever actually reached a `ConsoleAppender`/`OutputStreamAppender`,
    // so any log line asserted on via Spring Boot's `CapturedOutput` (or any
    // other real-appender consumer) was silently lost. See the longer note
    // above `register_slf4j_binder_stubs_pub`'s (removed) `getLogger`
    // overrides. Real Logback bytecode now owns `LoggerContext.getLogger`
    // and every `ch/qos/logback/classic/Logger` instance method.
}

#[cfg(test)]
mod logback_construction_registration_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

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
}

fn slf4j_log_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Logger), args[1] = format string, args[2..] = params
    let logger_name = match args.first() {
        Some(Value::Object(Some(this))) => match ctx.get_field(*this, SLF4J_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    };

    let format_str = match args.get(1) {
        Some(Value::Object(Some(msg))) => ctx.read_string(*msg).unwrap_or_default(),
        _ => return Ok(None),
    };

    // Substitute {} placeholders with parameter values
    let mut result = format_str.clone();
    let mut param_idx = 2;
    while let Some(pos) = result.find("{}") {
        let replacement = match args.get(param_idx) {
            Some(Value::Object(Some(obj))) => ctx
                .read_string(*obj)
                .unwrap_or_else(|| format!("Object@{:x}", param_idx)),
            Some(Value::Object(None)) => "null".to_string(),
            Some(Value::Int(v)) => v.to_string(),
            Some(Value::Long(v)) => v.to_string(),
            Some(Value::Float(v)) => format!("{}", v),
            Some(Value::Double(v)) => format!("{}", v),
            _ => break,
        };
        result = format!("{}{}{}", &result[..pos], replacement, &result[pos + 2..]);
        param_idx += 1;
    }

    // Format as: [LEVEL] logger - message
    let short_name = logger_name.rsplit('.').next().unwrap_or(&logger_name);
    let formatted = format!("[LOG] {} - {}", short_name, result);
    emit_framework_log(ctx, &formatted);
    Ok(None)
}

/// java.util.logging (JUL) log message handler.
pub(crate) fn jul_log_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Logger), args[1] = message string
    let logger_name = match args.first() {
        Some(Value::Object(Some(this))) => match ctx.get_field(*this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    if let Some(Value::Object(Some(msg))) = args.get(1) {
        if let Some(s) = ctx.read_string(*msg) {
            let short = logger_name.rsplit('.').next().unwrap_or(&logger_name);
            ctx.record_printed_line(format!("[JUL] {} - {}", short, s));
        }
    }
    // JUL handlers are observable application state. The old synthetic logger
    // bridge only wrote to stderr, so `Logger.addHandler(new AsyncFileHandler)`
    // silently lost every record. Build a normal LogRecord and fan it out to
    // the logger's handler list, matching Logger.log's essential contract.
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(message)))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    let handlers = match ctx.get_field(*logger, 2) {
        Value::Object(Some(list)) => list,
        _ => return Ok(None),
    };
    let level_class = match ctx.ensure_class_initialized("java/util/logging/Level") {
        Ok(class) => class,
        Err(_) => return Ok(None),
    };
    let Some(info_index) = ctx.static_field_index_by_name(level_class, "INFO") else {
        return Ok(None);
    };
    let level = ctx.get_static_field(level_class, info_index);
    let record = match ctx.new_object_initialized(
        "java/util/logging/LogRecord",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        &[level, Value::Object(Some(*message))],
    )? {
        Some(Value::Object(Some(record))) => record,
        _ => return Ok(None),
    };
    let size = match ctx.invoke_virtual(handlers, "size", "()I", &[])? {
        Some(Value::Int(size)) if size > 0 => size as usize,
        _ => return Ok(None),
    };
    for index in 0..size {
        let handler = match ctx.invoke_virtual(
            handlers,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(index as i32)],
        )? {
            Some(Value::Object(Some(handler))) => handler,
            _ => continue,
        };
        let _ = ctx.invoke_virtual(
            handler,
            "publish",
            "(Ljava/util/logging/LogRecord;)V",
            &[Value::Object(Some(record))],
        );
    }
    Ok(None)
}
