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
///
/// # These are contract §1.4 SHADOWS, and the two classes have opposite verdicts
///
/// Every `java/io/Print*` registration below stands in front of image bytecode
/// that carries a `Code` attribute — 36 of the census's 6,066
/// `bridge_shadows_bytecode` rows are this one function. They were measured
/// triple by triple on 2026-08-11 with `CRATONVM_ENFORCE_NATIVE_SHADOW`
/// scoped to one class at a time, HotSpot 25.0.3 as the control, and the two
/// halves came out differently for a reason that is entirely about the
/// receiver:
///
/// * **`java/io/PrintWriter` (7 triples) is retirable.** Verdict-neutral on a
///   `ByteArrayOutputStream`, on a `StringWriter`, and wrapping `System.out`,
///   and — the arm that matters — verdict-neutral when the receiver was built
///   by the NATIVE constructor while the methods yielded. That works because
///   `native_printwriter_init_outputstream` chains into the real
///   `PrintWriter(OutputStream, boolean)` (see its `invoke_special`), so
///   `lock`/`out`/`charOut`/`textOut` are populated by JDK bytecode whichever
///   construction path ran.
/// * **`java/io/PrintStream` (29 triples) is BLOCKED**, on `System.out` and
///   `System.err` specifically. Over a user-constructed stream whose ctor
///   yielded too, 26 of 26 triples are verdict-neutral; over the VM-minted
///   `System.out`, *every* output triple silently produces nothing. Silently,
///   because real `writeln` calls `ensureOpen()`, which throws
///   `IOException("Stream closed")` on the null `out` the comment further down
///   describes, and `writeln`'s own exception table catches `IOException` and
///   sets `trouble = true`. A suite asserting only on exceptions reads green
///   while the VM prints nothing.
///
/// So the fd-backed `System.out`/`System.err` have to be CONSTRUCTED rather
/// than fabricated before this class's shadows can go, and
/// `native_printstream_init_outputstream` has to chain to a real ctor the way
/// the PrintWriter one does — retiring the methods while it does not
/// reproduces `close()` NPEing on a null `textOut`, which is what the
/// measurement caught. The full per-row table, the field-by-field diff against
/// HotSpot, and the `retired_shadow.rs` patch for the PrintWriter half are in
/// docs/known-issues/jdk-only/W7-22-shadow-retirement-logging-and-time.md.
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
    // The locale-taking overloads. `javap java.io.PrintStream` lists four
    // format entry points, not two; only the two above were registered, so the
    // other two fell through to real `Formatter`-over-`Appendable` bytecode and
    // printed NOTHING — silently, beside a working sibling. See
    // `native_printf_locale`.
    registry.register(
        "java/io/PrintStream",
        "printf",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf_locale,
    );
    registry.register(
        "java/io/PrintStream",
        "format",
        "(Ljava/util/Locale;Ljava/lang/String;[Ljava/lang/Object;)Ljava/io/PrintStream;",
        native_printf_locale,
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
    // HELD BACK from any future §1.4 retirement, by name. JDK 25 does not
    // DECLARE this overload — the census row reads `declared: false` against
    // the image, so there is no bytecode for it to yield to and refusing it
    // would replace a working native with a `NoSuchMethodError`, which is the
    // shape `retired_shadow.rs` holds `Logger.log(Level, Supplier, Throwable)`
    // back for. It is not one of the 29 shadow rows on this class.
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
    // natives instead. Mirrors the synthetic-mode PrintStream registration and
    // the long-standing fd-stream flush contract.
    //
    // Both triples are registered UNCONDITIONALLY, so both natives run for
    // every `PrintStream` in the VM and not only for the console pair this
    // comment describes. `close` used to be a bare no-op on that reading, which
    // meant `new PrintStream(fileOutputStream).close()` delivered no bytes and
    // released no handle; it now performs the receiver test the comment was
    // asserting — a null `out` IS the console — and closes everything else.
    // W7-70-printstream-close-noop.md
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
    //
    // The seven registrations from here to the end of this function are the
    // §1.4 shadows measured RETIRABLE (see this function's doc comment). They
    // are kept as `Bridge` only because the retirement is a re-tag in
    // `native-api/src/retired_shadow.rs`, which the measuring lane did not own;
    // the exact table entries are in
    // docs/known-issues/jdk-only/W7-22-shadow-retirement-logging-and-time.md.
    // Anything ADDED here is a new shadow on a class already adjudicated
    // retirable, so it needs a row in that table too or the class retires
    // half-way — the shape that made `close()` NPE on the PrintStream side.
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
/// largeclienthello-string-size-nosuchmethod-FIXED.md).
/// Keying by identity hash and holding the list as a global GC root
/// sidesteps field layout entirely -- correct for both real and synthetic
/// loggers, and immune to future real-JDK field-order changes.
fn jul_logger_handlers_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<i32, usize>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

pub(crate) fn jul_logger_handlers_get(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(logger);
    let handle = *jul_logger_handlers_table(vm).lock().unwrap().get(&key)?;
    ctx.resolve_global_root(handle)
}

pub(crate) fn jul_logger_handlers_set(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    list: ObjectRef,
) {
    let vm = ctx.vm_identity();
    // Adding a global root may grow the root table and collect. The logger is
    // keyed immediately afterward, so retain it across that allocation.
    let logger_pin = ctx.pin_native_root(logger);
    let handle = ctx.add_global_root(list);
    let logger = ctx.read_native_pin(logger_pin, logger);
    let key = ctx.identity_hash_code(logger);
    jul_logger_handlers_table(vm)
        .lock()
        .unwrap()
        .insert(key, handle);
    ctx.unpin_native_roots(logger_pin);
}

/// GC-safe side table for `java.util.logging.Handler`'s `ErrorManager`, keyed
/// by `identity_hash_code` — same pattern, and for the same reason, as
/// `jul_logger_handlers_table`.
///
/// `java.util.logging.Handler` declares
/// `private volatile ErrorManager errorManager = new ErrorManager();` and
/// every `Handler` in the JDK routes its absorbed `Exception` there through
/// `reportError`. CratonVM's SYNTHETIC `StreamHandler` is a 2-field object
/// (stream=0, formatter=1) with no slot for it, so keying by identity sidesteps
/// the layout the way the handler-list table already does — and, unlike a
/// fixed slot, cannot collide with whatever a real-JDK `Handler` keeps there.
///
/// Compatible mode never reaches this: `Handler.reportError`,
/// `Handler.setErrorManager` and `StreamHandler.flush`/`close` all run real
/// bytecode there, over the real `errorManager` field.
/// W7-64-printstream-trouble-and-errormanager.md
fn jul_handler_error_manager_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<i32, usize>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

pub(crate) fn jul_handler_error_manager_get(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(handler);
    let handle = *jul_handler_error_manager_table(vm)
        .lock()
        .unwrap()
        .get(&key)?;
    ctx.resolve_global_root(handle)
}

pub(crate) fn jul_handler_error_manager_set(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    manager: ObjectRef,
) {
    let vm = ctx.vm_identity();
    // Adding a global root may grow the root table and collect. The handler is
    // keyed immediately afterward, so retain it across that allocation. Same
    // hazard, and same fix, as `jul_logger_handlers_set`.
    let handler_pin = ctx.pin_native_root(handler);
    let handle = ctx.add_global_root(manager);
    let handler = ctx.read_native_pin(handler_pin, handler);
    let key = ctx.identity_hash_code(handler);
    if let Some(previous) = jul_handler_error_manager_table(vm)
        .lock()
        .unwrap()
        .insert(key, handle)
    {
        ctx.remove_global_root(previous);
    }
    ctx.unpin_native_roots(handler_pin);
}

pub(crate) fn jul_logger_handlers_clear(ctx: &mut dyn NativeContext, logger: ObjectRef) {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(logger);
    if let Some(handle) = jul_logger_handlers_table(vm).lock().unwrap().remove(&key) {
        ctx.remove_global_root(handle);
    }
}

/// GC-safe side table for `java.util.logging.Logger`'s parent link, keyed by
/// `identity_hash_code` — same pattern, and the same reason, as
/// `jul_logger_handlers_table`: real-JDK 25 keeps the parent inside
/// `Logger$ConfigurationData` (reachable from slot 0 / `config`), while the
/// flat synthetic loggers our `getLogger` natives mint hold their name there,
/// so no raw slot index is safe for both shapes.
fn jul_logger_parents_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<i32, usize>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

pub(crate) fn jul_logger_parent_get(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(logger);
    let handle = *jul_logger_parents_table(vm).lock().unwrap().get(&key)?;
    ctx.resolve_global_root(handle)
}

pub(crate) fn jul_logger_parent_set(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    parent: Option<ObjectRef>,
) {
    let vm = ctx.vm_identity();
    // Drop the previous parent root first: re-parenting must not leak the old
    // logger as a permanent global root.
    let key = ctx.identity_hash_code(logger);
    if let Some(handle) = jul_logger_parents_table(vm).lock().unwrap().remove(&key) {
        ctx.remove_global_root(handle);
    }
    let Some(parent) = parent else {
        return;
    };
    // Adding a global root may grow the root table and collect. The logger is
    // keyed immediately afterward, so retain it across that allocation
    // (mirrors `jul_logger_handlers_set`).
    let logger_pin = ctx.pin_native_root(logger);
    let handle = ctx.add_global_root(parent);
    let logger = ctx.read_native_pin(logger_pin, logger);
    let key = ctx.identity_hash_code(logger);
    jul_logger_parents_table(vm)
        .lock()
        .unwrap()
        .insert(key, handle);
    ctx.unpin_native_roots(logger_pin);
}

/// GC-safe side table for Logger filters. Real JDK loggers keep a Filter in
/// `Logger$ConfigurationData`, while our compact loggers do not have that
/// shape; sharing neither raw layout is safe.
fn jul_logger_filters_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, usize>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<i32, usize>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

fn jul_logger_filter_names_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<String, usize>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

/// The name-keyed filter table's key. Delegates to
/// [`crate::logmanager::read_jul_logger_name`] — the one function that knows
/// where each of this VM's three JUL Logger layouts keeps its name.
///
/// It used to inline a two-step guess of its own: `get_field_by_name(.., "name")`
/// and then a raw `get_field(logger, LOGGER_FIELD_NAME)` — and the constant that
/// resolved to (`use super::*` → `lib.rs:36361`) is **0**, not the declared 2.
/// Slot 0 of `java/util/logging/Logger` is `config:
/// Ljava/util/logging/Logger$ConfigurationData;` (`class_manager.rs:14191`), so
/// the fallback arm was reading a `ConfigurationData` and calling `read_string`
/// on it. This is the only reader of that convention that is LIVE in every mode
/// — `Logger.setFilter`/`getFilter` are registered by
/// `register_essential_natives_with_shims` (`lib.rs:17592`/`:17608`) and
/// `--dump-native-registry` reports both with `owns_slot: true` in compatible
/// mode — so it is the one that had to move, not merely the shadowed ones.
fn jul_logger_filter_name(ctx: &mut dyn NativeContext, logger: ObjectRef) -> Option<String> {
    let name_obj = crate::logmanager::jul_logger_name_object(&*ctx, logger)?;
    ctx.read_string(name_obj)
}

pub(crate) fn jul_logger_filter_get(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(logger);
    if let Some(handle) = jul_logger_filters_table(vm)
        .lock()
        .unwrap()
        .get(&key)
        .copied()
    {
        return ctx.resolve_global_root(handle);
    }
    let name = jul_logger_filter_name(ctx, logger)?;
    let handle = jul_logger_filter_names_table(vm)
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
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(logger);
    let name = jul_logger_filter_name(ctx, logger);
    let mut table = jul_logger_filters_table(vm).lock().unwrap();
    if let Some(handle) = table.remove(&key) {
        ctx.remove_global_root(handle);
    }
    if let Some(name) = &name {
        if let Some(handle) = jul_logger_filter_names_table(vm)
            .lock()
            .unwrap()
            .remove(name)
        {
            ctx.remove_global_root(handle);
        }
    }
    if let Some(filter) = filter {
        table.insert(key, ctx.add_global_root(filter));
        if let Some(name) = name {
            jul_logger_filter_names_table(vm)
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
/// filehandler-noarg-ctor-handler-field-layout-gap-FIXED.md
/// for the full diagnosis. Keying by identity hash sidesteps field layout
/// entirely, exactly like the `Logger` handler-list/filter tables above,
/// and leaves `Handler`'s real `logLevel`/`filter`/`formatter` fields (which
/// `Handler.setLevel`/`getLevel`/`isLoggable`/`setFormatter`/`getFormatter`
/// in `reflect_annotations.rs` access by NAME, not slot) untouched and
/// correct regardless of how a `FileHandler` was constructed.
#[allow(clippy::type_complexity)]
fn jul_file_handler_state_table(
    vm: usize,
) -> &'static std::sync::Mutex<std::collections::HashMap<i32, (Option<String>, bool)>> {
    static T: OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                &'static std::sync::Mutex<std::collections::HashMap<i32, (Option<String>, bool)>>,
            >,
        >,
    > = OnceLock::new();
    crate::logmanager::per_vm_table(&T, vm)
}

pub(crate) fn jul_file_handler_filename(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Option<String> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(this);
    jul_file_handler_state_table(vm)
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
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(this);
    let mut table = jul_file_handler_state_table(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    table.entry(key).or_insert((None, false)).0 = filename;
}

pub(crate) fn jul_file_handler_is_closed(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(this);
    jul_file_handler_state_table(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|(_, closed)| *closed)
        .unwrap_or(false)
}

pub(crate) fn jul_file_handler_set_closed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    closed: bool,
) {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(this);
    let mut table = jul_file_handler_state_table(vm)
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
            ctx.class_name_arc_of_id(ctx.class_id_of_object(out)).as_deref(),
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
            ctx.class_name_arc_of_id(ctx.class_id_of_object(out)).as_deref(),
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

/// `String.valueOf(obj)` for a `print`/`println` argument, preserving the
/// difference between a NULL REFERENCE and a `toString()` that returns null.
///
/// `print(Object)` is `write(String.valueOf(obj))`, and `String.valueOf` is
/// specified as `obj == null ? "null" : obj.toString()` — it does NOT
/// substitute for a null RESULT. So the two arguments below are different:
///
/// ```text
///   print((Object) null)                            "null"
///   print(new Object(){ public String toString(){ return null; } })  NPE
/// ```
///
/// MEASURED: this VM printed "null" for both, because `invoke_to_string`
/// coerces — correctly, for its other callers (`StringBuilder.append(Object)`
/// really does substitute the text). The distinction has to be made HERE.
fn printstream_value_of(
    ctx: &mut dyn NativeContext,
    arg: Option<&Value>,
) -> Result<String, cratonvm_types::error::MethodCallFailed> {
    match arg {
        Some(Value::Object(Some(obj))) => {
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => Ok(ctx.read_string(s).unwrap_or_default()),
                Some(Value::Object(None)) => Err(
                    cratonvm_types::error::RuntimeError::NullPointerException { message: None }
                        .into(),
                ),
                _ => Ok(invoke_to_string(ctx, *obj)?),
            }
        }
        _ => Ok("null".to_string()),
    }
}

pub(crate) fn native_print_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let text = printstream_value_of(ctx, args.get(1))?;
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
    // `PrintStream(OutputStream out)` is `this(false, requireNonNull(out))`,
    // and the two-argument form the same — so a null sink is an NPE at
    // CONSTRUCTION. MEASURED: this VM built the stream and stored the null,
    // which turns every later `print` into a silent no-op that `checkError()`
    // never reports, because there is no sink to fail.
    let out_val = args.get(1).cloned().unwrap_or(Value::Object(None));
    if matches!(out_val, Value::Object(None)) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    ctx.set_field(this, 0, out_val.clone());
    ctx.set_field_by_name(this, "out", out_val);
    // STORE `autoFlush`. This native serves BOTH the one- and the two-argument
    // constructor, and it never wrote the flag — so `new PrintStream(sink,
    // true)` produced a stream whose `autoFlush` field stayed null and whose
    // autoflush therefore never fired, however faithfully the write path
    // consulted it. MEASURED: HotSpot flushed the sink once per `print` and
    // once per `println`; this VM never flushed at all, so a line protocol
    // written through `new PrintStream(socket.getOutputStream(), true)` sat in
    // the buffer until something else happened to flush it.
    //
    // The one-argument form is `this(false, out)`, so an absent third argument
    // means false — which is also what the field already held.
    let auto_flush = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    ctx.set_field_by_name(this, "autoFlush", Value::Int(i32::from(auto_flush)));
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
                // RECORDED since W7-64. `PrintWriter.format` is
                // `try { ensureOpen(); …formatter.format(…); }
                //  catch (InterruptedIOException x) { …interrupt(); }
                //  catch (IOException x) { trouble = true; }` — the same two
                // clauses as `write`, so the same recording policy.
                // W7-64-printstream-trouble-and-errormanager.md
                let wrote = ctx.invoke_virtual(
                    backing,
                    "write",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(s))],
                );
                // ROUTED, not DELIVERED — the same distinction W7-81 drew in
                // `route_write_through_out`, and the same defect if it is
                // missed. The retry below exists for the case the comment
                // above names: `write(String)` "isn't registered", i.e. a
                // `NoSuchMethodError` — `DelegatedWrite::Refused`. An absorbed
                // `IOException` is not that. HotSpot's `catch` has run, the
                // characters are gone, and re-sending them through the byte
                // overload on the SAME backing is a double write on a sink the
                // JDK already gave up on. `record_write_failure`'s `bool`
                // cannot tell those apart, which is exactly why it must not be
                // the gate on a retry.
                // W7-81-write-route-three-way.md
                let routed = cratonvm_native_api::print_error_state::classify_write_failure(
                    ctx, this, wrote,
                )
                .routed();
                if !routed && !backing_is_writer {
                    let bytes = text.as_bytes();
                    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
                    for (i, b) in bytes.iter().enumerate() {
                        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
                    }
                    let wrote_bytes = ctx.invoke_virtual(
                        backing,
                        "write",
                        "([BII)V",
                        &[
                            Value::Object(Some(arr)),
                            Value::Int(0),
                            Value::Int(bytes.len() as i32),
                        ],
                    );
                    cratonvm_native_api::print_error_state::record_write_failure(
                        ctx,
                        this,
                        wrote_bytes,
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
    //
    // KEPT SWALLOW, NARROWED. `java.io.PrintStream.flush()` is
    // `synchronized (this) { try { ensureOpen(); out.flush(); }
    // catch (IOException x) { trouble = true; } }` — the absorb is the JDK's,
    // and `PrintStream` declares no checked exception, so propagating
    // everything would be a fresh divergence. That `catch` names `IOException`
    // and nothing wider, so an `Error` — a `NoSuchMethodError` from our own
    // dispatch above all — now comes out.
    // W7-57-close-flush-swallow-sweep.md
    //
    // RECORDED since W7-64. The `catch` body is `trouble = true`, and
    // `checkError()` is that field's only reader — absorbing without setting
    // it made the failure *unobservable* rather than merely unthrown, which is
    // strictly worse than the swallow this narrowing removed. Parity, not a
    // behaviour change: HotSpot sets `trouble` at exactly this point.
    // W7-64-printstream-trouble-and-errormanager.md
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        if let Value::Object(Some(out)) = ctx.get_field_by_name(this, "out") {
            let flushed = ctx.invoke_virtual(out, "flush", "()V", &[]);
            cratonvm_native_api::print_error_state::absorb_io_exception_recording(
                &*ctx, this, flushed,
            )?;
            return Ok(None);
        }
        // `out` is not a Java sink, and there are TWO reasons for that which
        // must not be confused:
        //
        //  * the stream was CLOSED — `close()` nulled `out`, so `flush()`'s
        //    opening `ensureOpen()` throws `IOException("Stream closed")` into
        //    its own `catch` and sets `trouble` WITHOUT touching any sink;
        //  * this is the process console, whose `out` was never a Java object.
        //
        // The `closing` latch separates them, and it is only ever latched on
        // the branch that had a real sink. Gating on `closing` ALONE would be
        // wrong: when `close()` propagates an `Error`, HotSpot never reaches
        // its `out = null`, so `closing` is set while `out` is still live and
        // a later `flush()` really does flush the sink and leaves `trouble`
        // clear. That is `CloseFlushSwallowProbe`'s over-correction guard,
        // `printStreamCheckErrorAfterPropagatedCloseError`, and testing the
        // FIELD rather than the latch is what keeps it passing.
        if cratonvm_native_api::print_error_state::is_closing(&*ctx, this) {
            cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
            return Ok(None);
        }
        // The fd path below is `PrintStream.flush()` over the process console.
        // A failing `write`/`flush` on the fd is exactly the `IOException` the
        // JDK's `catch` names, so it records too — see `stream_write`.
        if let Some(fd) = stream_fd(ctx, args) {
            if ctx.fd_table().flush(fd).is_err() {
                cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
            }
        }
        return Ok(None);
    }
    if let Some(fd) = stream_fd(ctx, args) {
        let _ = ctx.fd_table().flush(fd);
    }
    Ok(None)
}

/// `java.io.PrintStream.close()` — flush the sink, then close it.
///
/// This was a bare `Ok(None)` with the comment "Don't actually close
/// stdout/stderr". That is a correct reason for a receiver test the body never
/// performed: the triple is registered unconditionally in BOTH registrars, so
/// the no-op applied to every `PrintStream` in the VM, and
/// `new PrintStream(new FileOutputStream(f)).close()` neither delivered the
/// buffered bytes nor released the file handle. A `try`-with-resources over
/// one saw a clean exit — lost data reported as success, the same fault shape
/// W7-57-close-flush-swallow-sweep.md exists for.
///
/// **The JDK body**, `lib/src.zip` from JDK 25.0.3.9:
///
/// ```java
/// public void close() {
///     synchronized (this) {
///         if (!closing) {
///             closing = true;
///             try {
///                 textOut.close();
///                 out.close();
///             }
///             catch (IOException x) { trouble = true; }
///             textOut = null; charOut = null; out = null;
///         }
///     }
/// }
/// ```
///
/// **What the SINK sees**, measured on HotSpot 25.0.3.9 rather than inferred
/// from that source — because `textOut.close()` does not look like a flush and
/// is one. `charOut` is `new OutputStreamWriter(this, charset)`, so closing the
/// character layer bottoms out in `StreamEncoder.implClose`, whose `out` is
/// `this`: it calls `this.flush()` (which is `out.flush()` on the real sink)
/// and then `this.close()` (a no-op, caught by the `closing` latch). The
/// observable contract on the sink is therefore exactly:
///
/// | case | sink ops | close() throws | `checkError()` |
/// |---|---|---|---|
/// | clean | `[flush, close]` | none | `false` |
/// | sink `flush` throws `IOException` | `[flush, close]` | none | `true` |
/// | sink `flush` throws `Error` | `[flush]` — **close is skipped** | the `Error` | — |
/// | sink `close` throws `IOException` | `[flush, close]` | none | `true` |
/// | sink `close` throws `Error` | `[flush, close]` | the `Error` | `false` |
/// | second `close()` | nothing more | none | unchanged |
///
/// Every row is an assertion in `probes/CloseFlushSwallowProbe.java`.
/// The flush-first-then-close pair with `?` between them reproduces all six,
/// including the one that is easy to get wrong: a propagated `Error` out of the
/// flush must skip the close, which is what the `?` does.
///
/// **`textOut`/`charOut` are deliberately not driven.** They are null on every
/// `PrintStream` this VM constructs (`native_printstream_init_outputstream`
/// sets only `out`, and `ensure_system_streams` allocates a zeroed object), and
/// where a real ctor we do not shadow does populate them the character layer is
/// still empty, because our own `print`/`println`/`write` natives write to
/// `out` directly and never buffer into `textOut`. Closing it as well would
/// drive the sink's `flush` twice. If the `native_osw_init`/`native_bw_init`
/// lane ever makes a real `textOut` load-bearing, this is the site to revisit.
///
/// **The console still cannot be closed**, and now for a reason the code
/// states: `System.out`/`System.err` are fd-backed with a NULL `out`
/// (`ensure_system_streams` never populates it — that is the same invariant
/// `route_write_through_out` and `native_printstream_flush` already branch on),
/// so there is no sink object to close and the fd is flushed instead. A
/// `PrintStream` that WRAPS `System.out` delegates its close to it and lands on
/// that same branch; and `FdTable::close` refuses fd < 3 outright. Three
/// independent guards, none of which is a name test.
///
/// KEPT SWALLOW, NARROWED, RECORDED — the same three-part policy as
/// `native_printstream_flush` above and `native_printwriter_close`. The JDK's
/// `catch` names `IOException` and `close()` declares no checked exception, so
/// an `IOException` is absorbed into `trouble`; an `Error` — a
/// `NoSuchMethodError` out of our own dispatch above all — is not named by that
/// `catch` and comes out. W7-57-close-flush-swallow-sweep.md,
/// W7-64-printstream-trouble-and-errormanager.md,
/// W7-70-printstream-close-noop.md
pub(crate) fn native_printstream_close(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    // `if (!closing)`. Never cleared, so this is both the recursion guard the
    // JDK's comment names and what makes a second close a total no-op.
    if cratonvm_native_api::print_error_state::is_closing(&*ctx, this) {
        return Ok(None);
    }
    // The sink, by the JDK's own field name. A non-object here — including the
    // `Value::Int` fd tag `ensure_system_streams` parks in the legacy
    // synthetic layout's slot 0 — is "no Java sink", i.e. the console.
    // Bound to a local first, so the `&*ctx` read is fully over before the
    // `&mut ctx` dispatch below starts — the nested-reborrow shape that had to
    // be split once already in this file's neighbour.
    let out_field = ctx.get_field_by_name(this, "out");
    let Value::Object(Some(sink)) = out_field else {
        // The process console. HotSpot really would close it; we must not, so
        // the closest useful behaviour is the flush its close would have
        // performed. `closing` is deliberately NOT latched here: it means "this
        // stream's sink has been closed", and nothing on this branch closed
        // one — so a second `System.out.close()` still drains the console
        // rather than silently skipping it.
        if let Some(fd) = stream_fd(ctx, args) {
            if ctx.fd_table().flush(fd).is_err() {
                cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
            }
        }
        return Ok(None);
    };
    cratonvm_native_api::print_error_state::latch_closing(&*ctx, this);
    // `flush()` can move both `sink` (dereferenced by the `close()` below) and
    // `this` (passed to the recorder either side). Pin across and re-derive.
    let sink_pin = ctx.pin_native_root(sink);
    let this_pin = ctx.pin_native_root(this);
    let flushed = ctx.invoke_virtual(sink, "flush", "()V", &[]);
    let sink = ctx.read_native_pin(sink_pin, sink);
    let this = ctx.read_native_pin(this_pin, this);
    cratonvm_native_api::print_error_state::absorb_io_exception_recording(&*ctx, this, flushed)?;
    let closed = ctx.invoke_virtual(sink, "close", "()V", &[]);
    cratonvm_native_api::print_error_state::absorb_io_exception_recording(&*ctx, this, closed)?;
    // `textOut = null; charOut = null; out = null;` — the JDK's last three
    // statements in `close()`, and they are not bookkeeping. `checkError()` is
    // `if (out != null) { flush(); } … return trouble;`, so a closed stream
    // skips the flush ENTIRELY: HotSpot answers `false` after a clean close and
    // never touches the sink. Leaving `out` populated made every `checkError()`
    // on a closed stream re-flush the sink — `flush,close,flush` where the
    // oracle traces `flush,close` (`CloseFlushSwallowProbe`'s
    // `printStreamCloseIoSinkTrace`), and one more flush per call after that.
    //
    // Nulling it is safe HERE and only here: the console branch above returns
    // BEFORE `closing` is latched, so a latched `closing` means this stream had
    // a real Java sink and a null `out` can no longer be mistaken for the
    // console marker. Every path that would read `out` after this point —
    // `write`/`print` via `printstream_refuse_if_closed`, and `flush` above —
    // now refuses on the `closing` latch before looking at the field at all.
    ctx.set_field_by_name(this, "out", Value::Object(None));
    Ok(None)
}

pub(crate) fn native_printstream_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0]=this, args[1]=byte[], args[2]=off, args[3]=len
    //
    // THIS BODY CRASHED THE VM. `write(b, 0, -1)` reached `vec![0u8; len]`
    // with `len = -1 as usize` = 18 446 744 073 709 551 615 and aborted the
    // process with `capacity overflow` — a Rust panic escaping as
    // `internal error: native method panic`, which no Java handler can catch
    // and which killed the probe 72 rows before its end. `write(b, -1, 1)`
    // did not crash; it read `arr[usize::MAX]`, got zero, and PRINTED A NUL
    // BYTE where HotSpot throws.
    //
    // `PrintStream.write(byte[], int, int)` delegates to the stream
    // underneath, and every one of those runs `Objects.checkFromIndexSize(
    // off, len, b.length)` first: a null buffer is an NPE from `b.length`,
    // and an out-of-range window is the PLAIN `IndexOutOfBoundsException`
    // (not the array subclass — measured against HotSpot on all three rows).
    //
    // This is the one place in this campaign where the refusal is not merely
    // about a type: `PrintStream` swallows IOExceptions into `checkError()`,
    // but an NPE and an IndexOutOfBoundsException are ARGUMENT errors and
    // must escape.
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
            )
        }
        _ => return Ok(None),
    };
    let off_i = match args.get(2) {
        Some(Value::Int(o)) => *o,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(l)) => *l,
        _ => 0,
    };
    let arr_len = ctx.array_length(arr) as i32;
    if off_i < 0 || len_i < 0 || off_i.checked_add(len_i).map_or(true, |e| e > arr_len) {
        return Err(cratonvm_types::error::RuntimeError::ioobe(
            cratonvm_types::error::out_of_bounds_message::check_from_index_size(
                i64::from(off_i),
                i64::from(len_i),
                i64::from(arr_len),
            ),
        )
        .into());
    }
    let off = off_i as usize;
    let len = len_i as usize;
    let mut buf = vec![0u8; len];
    for (i, slot) in buf.iter_mut().enumerate() {
        if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
            *slot = b as u8;
        }
    }
    // `PrintStream.write(byte[], int, int)` is the one overload whose bytes
    // are NOT encoded — they are already bytes — but it is also the one whose
    // autoflush is UNCONDITIONAL: its body ends `out.write(buf, off, len);
    // if (autoFlush) out.flush();` with no newline test. Every `print` and
    // `println` reaches it through the internal `OutputStreamWriter`, which is
    // why a `new PrintStream(sink, true)` flushes after a plain `print("a")`
    // on HotSpot and did not here.
    if crate::printstream_refuse_if_closed(ctx, args) {
        return Ok(None);
    }
    // User/Tee streams route through the real underlying stream; canonical
    // synthetic out/err (out==null) write to the fd directly.
    // LOCK-SCOPE (2026-07-21): see `stream_write`.
    let text = String::from_utf8_lossy(&buf);
    if !surefire_forwarding_write(ctx, args, &text, false)
        && !route_write_through_out(ctx, args, &buf)
    {
        if let Some(fd) = stream_fd(ctx, args) {
            let ok = with_stdio_print_lock(|| ctx.fd_table().write_bytes(fd, &buf));
            // RECORDED since W7-64 — see `stream_write` in native-builtins/src/lib.rs.
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                cratonvm_native_api::print_error_state::record_host_io_failure(&*ctx, this, ok);
            }
        } else if let Some(Value::Object(Some(this))) = args.first().copied() {
            // See `stream_write`: no sink and no descriptor is `ensureOpen()`.
            cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
        }
    }
    crate::printstream_autoflush(ctx, args);
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
    if crate::printstream_refuse_if_closed(ctx, args) {
        return Ok(None);
    }
    // LOCK-SCOPE (2026-07-21): see `stream_write`.
    let text = String::from_utf8_lossy(&buf);
    if !surefire_forwarding_write(ctx, args, &text, false)
        && !route_write_through_out(ctx, args, &buf)
    {
        if let Some(fd) = stream_fd(ctx, args) {
            let ok = with_stdio_print_lock(|| ctx.fd_table().write_bytes(fd, &buf));
            // RECORDED since W7-64 — see `stream_write` in native-builtins/src/lib.rs.
            if let Some(Value::Object(Some(this))) = args.first().copied() {
                cratonvm_native_api::print_error_state::record_host_io_failure(&*ctx, this, ok);
            }
        } else if let Some(Value::Object(Some(this))) = args.first().copied() {
            cratonvm_native_api::print_error_state::set_trouble(&*ctx, this);
        }
    }
    // `write(int b)` is the ONE overload whose autoflush is conditional:
    // `if ((b == '\n') && autoFlush) out.flush();`. Its byte-array sibling
    // flushes unconditionally, which is why the two cannot share a hook.
    // MEASURED: `new PrintStream(sink, true).write('\n')` did not flush.
    if b == b'\n' {
        crate::printstream_autoflush(ctx, args);
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

/// The text of a `CharSequence` argument, by `String.valueOf` rules: the string
/// itself when it is one, otherwise whatever `toString()` returns.
///
/// [`NativeContext::read_string`] reads a `java.lang.String` and nothing else,
/// so using it alone on a `CharSequence` parameter silently turns every
/// `StringBuilder`, `StringBuffer` and `CharBuffer` into the four characters
/// `null`.
fn char_sequence_text(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    if let Some(s) = ctx.read_string(obj) {
        return s;
    }
    match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        // A CharSequence whose `toString()` fails or returns null has no text
        // to contribute. Appending the literal "null" here would be the very
        // bug this function exists to fix, so append nothing.
        _ => String::new(),
    }
}

/// `PrintStream.append(CharSequence)` — appends the text and returns `this`.
fn native_printstream_append(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0]=this (PrintStream), args[1]=CharSequence
    //
    // The real method is `write(String.valueOf(csq))`, and `String.valueOf`
    // means `csq.toString()` for anything non-null. This used to be
    // `read_string(obj).unwrap_or("null")`, which recognises a
    // `java.lang.String` and nothing else — so every non-String
    // `CharSequence` appended `null`.
    //
    // Not a corner case: `java.util.Formatter` appends a `StringBuilder` for
    // the numeric conversions (`printInteger`/`printFloat` →
    // `appendJustified(a, sb)`) and a `String` for `%s`. So
    // `PrintStream.printf(Locale, …)` — the overload with no native of its
    // own, hence the only one reaching real `Formatter` bytecode — printed
    // `[x|null|null]` where HotSpot prints `[x|7|01.50]`: `%s` right, every
    // numeric conversion lost. `probes/PrintStreamAppendProbe` pins that down,
    // and it is why `regression-suite` was uniformly red — its classes assert
    // through `printf`.
    let text = match args.get(1) {
        Some(Value::Object(Some(obj))) => char_sequence_text(ctx, *obj),
        // `append((CharSequence) null)` is specified to append "null".
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
                    let written = ctx.invoke_virtual(
                        this,
                        "write",
                        "(Ljava/lang/String;II)V",
                        &[Value::Object(Some(s)), Value::Int(0), Value::Int(len)],
                    );
                    // RECORDED since W7-64 — `PrintWriter.write(String,int,int)`
                    // ends `catch (IOException x) { trouble = true; }`, and
                    // `checkError()` is that field's only reader.
                    // W7-64-printstream-trouble-and-errormanager.md
                    cratonvm_native_api::print_error_state::record_write_failure(
                        ctx, this, written,
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
            let written = ctx.invoke_virtual(out_obj, "write", "(Ljava/lang/String;)V", &[str_val]);
            // RECORDED since W7-64 — see the sibling range overload below.
            //
            // ROUTED since W7-81. This native resolves `out` itself and never
            // reaches `route_write_through_out`, so the three-way answer has to
            // be made here too or this receiver shape keeps the defect the
            // routing helper just lost: a `Refused` call — a `NoSuchMethodError`
            // out of our own dispatch — used to `return Ok(None)` and the text
            // vanished with no fallback at all. Falling through instead reaches
            // the shared path, which has the console fallback. An `Absorbed`
            // `IOException` still returns here, because HotSpot wrote the
            // characters nowhere and re-sending them would be a double write.
            // W7-81-write-route-three-way.md
            let routed =
                cratonvm_native_api::print_error_state::classify_write_failure(ctx, this, written)
                    .routed();
            if routed {
                return Ok(None);
            }
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
            let written = ctx.invoke_virtual(
                out_obj,
                "write",
                "(Ljava/lang/String;II)V",
                &[str_val, off_val, len_val],
            );
            // RECORDED since W7-64. `java.io.PrintWriter.write(String,int,int)`
            // is `synchronized (lock) { try { ensureOpen(); out.write(s, off,
            // len); } catch (InterruptedIOException x) { …interrupt(); }
            // catch (IOException x) { trouble = true; } }` — the absorb was
            // already here, the record was not.
            // W7-64-printstream-trouble-and-errormanager.md
            //
            // ROUTED since W7-81 — see the sibling `write(String)` overload
            // above for why this native needs the three-way answer of its own.
            // W7-81-write-route-three-way.md
            let routed =
                cratonvm_native_api::print_error_state::classify_write_failure(ctx, this, written)
                    .routed();
            if routed {
                return Ok(None);
            }
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
            let written = ctx.invoke_virtual(out_obj, "write", "(I)V", &[ch]);
            // RECORDED since W7-64 — `PrintWriter.write(int)` ends
            // `catch (IOException x) { trouble = true; }`.
            //
            // NOT routed three ways, unlike its two `write(String…)` siblings
            // above, and the difference is deliberate: they have a fallthrough
            // to hand a REFUSED call to (`native_printstream_write_string…`,
            // which owns the console fallback) and this one has none — it is
            // the end of its own path. Giving it one means inventing a
            // `stream_write` call for a single char, which is a different
            // change from the one W7-81 made and needs its own justification.
            // So a `NoSuchMethodError` from `out.write(int)` still loses the
            // character silently here. W7-81-write-route-three-way.md
            cratonvm_native_api::print_error_state::record_write_failure(ctx, this, written);
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

/// Decode a `java.util.logging.Level` argument (or a `Logger`'s stored level
/// slot) to its `intValue()`, tolerating every shape this VM produces.
///
/// There is no single shape to rely on: `java/util/logging/Logger.setLevel` is
/// registered three times across this file and `lib.rs`, and the winner has
/// changed with registration order — one stores the `Level` OBJECT in the level
/// slot, another stores the raw `Value::Int`. A synthetic `Level` additionally
/// has NO field names (`ensure_synthetic_class` mints unnamed slots), so the
/// by-name `value` read that the real-JDK layout needs resolves nothing and the
/// only readable copy is slot 1 (or the level NAME in slot 0).
///
/// Returns `None` when the value carries no level at all (a null/unset slot),
/// so the caller can apply the JDK default rather than a silent 0 — reading an
/// unset slot as level 0 makes EVERY record loggable, which is the exact bug
/// this helper exists to prevent.
fn jul_level_int(ctx: &dyn NativeContext, level: Option<Value>) -> Option<i32> {
    match level? {
        Value::Int(v) => Some(v),
        Value::Long(v) => Some(v as i32),
        Value::Object(Some(l)) => {
            if let Value::Int(v) = ctx.get_field_by_name(l, "value") {
                return Some(v);
            }
            if ctx.object_num_fields(l) > 1 {
                if let Value::Int(v) = ctx.get_field(l, 1) {
                    return Some(v);
                }
            }
            // Last resort: a Level that only carries its name.
            let name_slot = if ctx.object_num_fields(l) > 0 {
                ctx.get_field(l, 0)
            } else {
                Value::Object(None)
            };
            let name = match ctx.get_field_by_name(l, "name") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => match name_slot {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                },
            }?;
            Some(match name.as_str() {
                "OFF" => i32::MAX,
                "SEVERE" => 1000,
                "WARNING" => 900,
                "INFO" => 800,
                "CONFIG" => 700,
                "FINE" => 500,
                "FINER" => 400,
                "FINEST" => 300,
                "ALL" => i32::MIN,
                _ => return None,
            })
        }
        _ => None,
    }
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
            // Side table, not slot 2 — slot 2 is `name` on the declared layout
            // (`class_manager.rs:14191`), and this used to overwrite it with an
            // ArrayList. Same table the winning `addHandler`
            // (`reflect_annotations.rs:213`) uses, so the two agree.
            let handlers = match jul_logger_handlers_get(ctx, *this) {
                Some(list) => list,
                None => {
                    let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
                    cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))])?;
                    // `jul_logger_handlers_set` calls `add_global_root`, which
                    // may grow the root table and collect — the `set_field` it
                    // replaces could not. Pin the list across it and re-read
                    // before handing the address to `native_al_add`.
                    let list_pin = ctx.pin_native_root(list);
                    jul_logger_handlers_set(ctx, *this, list);
                    let list = ctx.read_native_pin(list_pin, list);
                    ctx.unpin_native_roots(list_pin);
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
            ctx.set_field(this, crate::logmanager::LOGGER_FIELD_LEVEL, level);
            // Also publish to the name-keyed explicit-level table. That table
            // is what `logmanager::native_jul_logger_is_loggable` — which wins
            // the `isLoggable` registry slot — consults FIRST, so without this
            // a `setLevel` served by THIS registration was invisible to the
            // `isLoggable` served by that one.
            crate::logmanager::record_jul_logger_level(ctx, this, level);
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
            let arg_val = jul_level_int(ctx, args.get(1).copied()).unwrap_or(800);
            // The level slot holds EITHER a `Level` object or its raw int value:
            // `java/util/logging/Logger.setLevel` is registered twice in this
            // file, and the later registration (which wins) stores
            // `Value::Int(level_val)` where the earlier one stored the Level
            // object. Reading only the object shape meant every `setLevel`
            // silently left this at the INFO default, so `isLoggable` said yes
            // to everything — `logger.setLevel(SEVERE)` did not suppress INFO.
            //
            // The slot is `logmanager::LOGGER_FIELD_LEVEL`, the VM-internal slot
            // the declaration anchors past the 12 real fields — NOT the local
            // `LOGGER_FIELD_LEVEL = 1`, which is `manager: LogManager`.
            let stored = ctx.get_field(this, crate::logmanager::LOGGER_FIELD_LEVEL);
            let current = jul_level_int(ctx, Some(stored)).unwrap_or(800); // default INFO
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
// JDK-ONLY-CLASSIFY: stub — stated for the whole registrar, not adjudicated
// per row. Every one of these was among the 200 registrations the real boot
// made with NO category scope over them, which `--dump-native-registry`
// could not report until `current_category` became an `Option`: the old
// `category_chosen` flag was set by the first `set_category` in boot and
// never cleared, so everything after it claimed to have been chosen.
// `SyntheticStub` is the kind these carried before and after — verified by
// a census A/B — and it is the right one on the merits: the SLF4J binder surface is application bytecode this
// VM stands in front of; nothing here is a VM/OS boundary.
pub fn register_slf4j_binder_stubs_pub(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
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
            let s = try_alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticMDCBinder", 1)?;
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
            let a = try_alloc_concurrent_synthetic(ctx, "org/slf4j/helpers/BasicMDCAdapter", 0)?;
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
                if ctx.class_declares_method(cid, "getMDCAdapterClassStr", "()Ljava/lang/String;") {
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
            let s = try_alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticLoggerBinder", 1)?;
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
                if ctx.class_declares_method(
                    cid,
                    "getLoggerFactory",
                    "()Lorg/slf4j/ILoggerFactory;",
                ) {
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
            let f = try_alloc_concurrent_synthetic(ctx, "org/slf4j/ILoggerFactory", 0)?;
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
            let s = try_alloc_concurrent_synthetic(ctx, "org/slf4j/impl/StaticMarkerBinder", 1)?;
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
            let f = try_alloc_concurrent_synthetic(ctx, chosen, 0)?;
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

    // BasicMDCAdapter — the adapter the binder above returns, and (in SLF4J
    // 2.x) the object EVERY `org.slf4j.MDC` call is routed through.
    //
    // These were constant no-ops / constant nulls, justified as "our MDC stubs
    // implement put/get directly so these don't fire on user paths". They do
    // fire, and as constants they made the adapter contradict the façade
    // sharing its own storage: a `put` was discarded and the matching `get`
    // answered null for the key just written, so `%X{...}`/`%mdc` converters
    // and correlation-id filters always saw an empty diagnostic context.
    // Route them at the same thread-local map the `org/slf4j/MDC` natives use
    // (`mdc_*` helpers), with an argument offset of 1 for the receiver. Note
    // the adapter instance handed out above is `alloc_concurrent_synthetic`'d
    // with zero fields, so real `BasicMDCAdapter` bytecode could not service
    // these calls either — its `inheritableThreadLocal` is null.
    let mdc_adapter = "org/slf4j/helpers/BasicMDCAdapter";
    registry.register(
        mdc_adapter,
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| mdc_put_at(ctx, args, 1),
    );
    registry.register(
        mdc_adapter,
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| mdc_get_at(ctx, args, 1),
    );
    registry.register(
        mdc_adapter,
        "remove",
        "(Ljava/lang/String;)V",
        |ctx, args| mdc_remove_at(ctx, args, 1),
    );
    registry.register(mdc_adapter, "clear", "()V", mdc_clear);
    registry.register(
        mdc_adapter,
        "getCopyOfContextMap",
        "()Ljava/util/Map;",
        mdc_copy_of_context_map,
    );
    registry.register(
        mdc_adapter,
        "setContextMap",
        "(Ljava/util/Map;)V",
        |ctx, args| mdc_set_context_map_at(ctx, args, 1),
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
            let logger = try_alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2)?;
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
    //
    // TRACE/DEBUG answer from `slf4j_threshold` (INFO by default) instead of a
    // hardcoded `false`, so the property that admits TRACE/DEBUG records also
    // opens the guards callers check before building them — otherwise the
    // emitters below would never be reached and the filter would be inert.
    fn slf4j_trace_on(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(if slf4j_threshold(ctx) <= SLF4J_TRACE {
            1
        } else {
            0
        })))
    }
    fn slf4j_debug_on(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(if slf4j_threshold(ctx) <= SLF4J_DEBUG {
            1
        } else {
            0
        })))
    }
    // FIXED wave 4 (2026-07-28): these three were one shared `slf4j_enabled`
    // returning a constant `1`, while the emitters they guard
    // (`slf4j_info_msg`/`slf4j_warn_msg`/`slf4j_error_msg`) DO apply
    // `slf4j_threshold`. With `org.slf4j.simpleLogger.defaultLogLevel=warn` (or
    // `=off`) `isInfoEnabled()` answered true and `info(...)` then emitted
    // nothing — the guard lied about the emitter it guards, which is exactly
    // the disagreement the trace/debug guards above and the whole Log4j guard
    // block (`slf4j_level_enabled`) were changed to avoid. Same threshold
    // source for all five levels now.
    fn slf4j_info_on(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        slf4j_level_enabled(ctx, SLF4J_INFO)
    }
    fn slf4j_warn_on(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        slf4j_level_enabled(ctx, SLF4J_WARN)
    }
    fn slf4j_error_on(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        slf4j_level_enabled(ctx, SLF4J_ERROR)
    }
    registry.register("org/slf4j/Logger", "isTraceEnabled", "()Z", slf4j_trace_on);
    registry.register("org/slf4j/Logger", "isDebugEnabled", "()Z", slf4j_debug_on);
    registry.register("org/slf4j/Logger", "isInfoEnabled", "()Z", slf4j_info_on);
    registry.register("org/slf4j/Logger", "isWarnEnabled", "()Z", slf4j_warn_on);
    registry.register("org/slf4j/Logger", "isErrorEnabled", "()Z", slf4j_error_on);
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
        slf4j_trace_on,
    );
    registry.register(
        "org/slf4j/Logger",
        "isDebugEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_debug_on,
    );
    registry.register(
        "org/slf4j/Logger",
        "isInfoEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_info_on,
    );
    registry.register(
        "org/slf4j/Logger",
        "isWarnEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_warn_on,
    );
    registry.register(
        "org/slf4j/Logger",
        "isErrorEnabled",
        "(Lorg/slf4j/Marker;)Z",
        slf4j_error_on,
    );

    // Round 63: Keycloak — KerberosJdkProvider.isKerberosAvailable() probes the
    // JCE provider list via java.security.Provider.checkInitialized, which
    // throws IllegalStateException in our environment because the security
    // provider isn't initialized at Profile.configure time.
    //
    // KEEP (the constant is the true answer for this VM) — re-derived wave 4,
    // 2026-07-28. `false` here is not "we have no data"; it is a fact about the
    // platform: CratonVM ships no Kerberos/GSS-API support at all. A tree-wide
    // search for `krb5` / `Kerberos` / `GSSCredential` outside this file finds
    // exactly ZERO natives, shims or class stubs (the single hit is a vendored
    // rustls doc comment), so `sun.security.krb5` / `javax.security.auth
    // .kerberos` cannot function and any answer other than `false` would send
    // Keycloak down a code path that must then fail. Implementing the probe
    // faithfully (initialising the security providers so `checkInitialized`
    // stops throwing) would arrive at the same `false` by a longer route.
    // Revisit only if a Kerberos provider is ever added.
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
    //
    // TRACE/DEBUG go through the same `slf4j_log_msg` formatter as the levels
    // above them, behind `slf4j_threshold`: at the default INFO threshold the
    // record is dropped by the filter (matching `isTraceEnabled`/
    // `isDebugEnabled` above), and lowering the threshold actually delivers it.
    // This registration runs after `register_slf4j_natives`' own trace/debug
    // block (last-registration-wins), so it is the one that decides.
    let lg = "org/slf4j/Logger";
    for sig in [
        "(Ljava/lang/String;)V",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
    ] {
        // The VARARGS descriptor must SPREAD its `Object[]` across the `{}`
        // placeholders; every other descriptor passes its arguments
        // positionally already. See `slf4j_spread_varargs` -- and note that
        // this loop, not `register_slf4j_natives`' block, is the registration
        // that decides (last-registration-wins, per the comment above).
        let varargs = sig == "(Ljava/lang/String;[Ljava/lang/Object;)V";
        if varargs {
            registry.register(lg, "trace", sig, slf4j_trace_msg_varargs);
            registry.register(lg, "debug", sig, slf4j_debug_msg_varargs);
            registry.register(lg, "info", sig, slf4j_log_msg_varargs);
            registry.register(lg, "warn", sig, slf4j_log_msg_varargs);
            registry.register(lg, "error", sig, slf4j_log_msg_varargs);
            continue;
        }
        registry.register(lg, "trace", sig, slf4j_trace_msg);
        registry.register(lg, "debug", sig, slf4j_debug_msg);
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
    // logback-loggercontext-listenerlist-final-field-corruption-FIXED.md).
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
    // FIXED 2026-07-27 (wave-2 inline-constant stub removal): `getName()`
    // (constant `"default"`) and `setName(String)` (no-op) were the last two
    // survivors of the synthetic-`LoggerContext` era documented above. With
    // `<init>`, `reset`, `start`, `stop` and `getLogger` all back on real
    // bytecode, these two were a pure contradiction of the object they sat on:
    // real `ContextBase.<init>` already names the context `"default"`, so the
    // constant added nothing, while `setName` swallowed every rename —
    // `<contextName>` in a logback.xml, Spring Boot's
    // `LoggingSystemProperties`, and the `%contextName` pattern converter all
    // silently kept reporting `"default"`, and `ContextBase`'s own
    // "already given a name" `IllegalStateException` could never fire. Real
    // `ContextBase.getName`/`setName` are a plain field read/write over state
    // the real constructor now initialises. Guarded by
    // `logback_context_construction_and_state_are_not_native_overridden`.
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
    registry.set_category(__prev_cat);
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
            let logger = try_alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2)?;
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
            // JDK-ONLY-LAYOUT: the class name comes from `mirror_class_name`,
            // not from raw slot 0.
            //
            // This read used to be `match get_field(mirror, 0) {
            // Value::Object(Some(n)) => n, _ => "unknown" }` — it expected a
            // name String at slot 0, which no mirror this VM builds has ever
            // held: `get_or_create_class_mirror` puts `Int(class_id)` there and
            // the name at whatever index `java.lang.Class` declares `name` at.
            // So the match fell through on EVERY call and every logger obtained
            // through `getLogger(Foo.class)` was named "unknown". Silent, and
            // the wrong-field read is the whole family this marker names.
            let name_val = match args.first() {
                Some(Value::Object(Some(class_mirror))) => {
                    match crate::lang_class::mirror_class_name(ctx, *class_mirror) {
                        Some(n) => {
                            let s = ctx.create_string(&n.replace('/', "."));
                            Value::Object(Some(s))
                        }
                        None => {
                            let s = ctx.create_string("unknown");
                            Value::Object(Some(s))
                        }
                    }
                }
                _ => {
                    let s = ctx.create_string("unknown");
                    Value::Object(Some(s))
                }
            };
            let logger = try_alloc_concurrent_synthetic(ctx, "org/slf4j/Logger", 2)?;
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
            let factory = try_alloc_concurrent_synthetic(ctx, "org/slf4j/ILoggerFactory", 0)?;
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
    // trace(String) — same formatter as debug() below, gated on
    // `slf4j_threshold` (TRACE sits below the default INFO threshold, so the
    // filter drops the record instead of the native silently discarding it).
    registry.register(lg, "trace", "(Ljava/lang/String;)V", slf4j_trace_msg);
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        slf4j_trace_msg,
    );
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
        slf4j_trace_msg,
    );
    // VARARGS form: the array must be SPREAD across the placeholders, not
    // rendered as one argument. See `slf4j_spread_varargs`.
    registry.register(
        lg,
        "trace",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_trace_msg_varargs,
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
    // VARARGS form: the array must be SPREAD across the placeholders, not
    // rendered as one argument. See `slf4j_spread_varargs`.
    registry.register(
        lg,
        "debug",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg_varargs,
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
    // VARARGS form: the array must be SPREAD across the placeholders, not
    // rendered as one argument. See `slf4j_spread_varargs`.
    registry.register(
        lg,
        "info",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg_varargs,
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
    // VARARGS form: the array must be SPREAD across the placeholders, not
    // rendered as one argument. See `slf4j_spread_varargs`.
    registry.register(
        lg,
        "warn",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg_varargs,
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
    // VARARGS form: the array must be SPREAD across the placeholders, not
    // rendered as one argument. See `slf4j_spread_varargs`.
    registry.register(
        lg,
        "error",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_log_msg_varargs,
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

    // MDC (Mapped Diagnostic Context) — backed by thread-local HashMap.
    // The bodies live in the `mdc_*` helpers below so the
    // `org.slf4j.helpers.BasicMDCAdapter` instance methods (registered in
    // `register_slf4j_binder_stubs_pub`) can share the SAME thread-local map
    // instead of being no-ops that silently disagree with this façade.
    let mdc = "org/slf4j/MDC";
    registry.register(
        mdc,
        "put",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| mdc_put_at(ctx, args, 0),
    );
    registry.register(
        mdc,
        "get",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| mdc_get_at(ctx, args, 0),
    );
    registry.register(mdc, "remove", "(Ljava/lang/String;)V", |ctx, args| {
        mdc_remove_at(ctx, args, 0)
    });
    registry.register(mdc, "clear", "()V", mdc_clear);
    registry.register(
        mdc,
        "getCopyOfContextMap",
        "()Ljava/util/Map;",
        mdc_copy_of_context_map,
    );
    registry.register(mdc, "setContextMap", "(Ljava/util/Map;)V", |ctx, args| {
        mdc_set_context_map_at(ctx, args, 0)
    });

    // Marker — Spring Boot sometimes uses markers
    let marker = "org/slf4j/MarkerFactory";
    registry.register(
        marker,
        "getMarker",
        "(Ljava/lang/String;)Lorg/slf4j/Marker;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let m = try_alloc_concurrent_synthetic(ctx, "org/slf4j/Marker", 1)?;
            ctx.set_field(m, 0, name);
            Ok(Some(Value::Object(Some(m))))
        },
    );

    // SLF4J 1.7 static-binder stubs (getSingleton / adapter / factory).
    // Implementation is shared with the real-JDK boot path — see
    // `register_slf4j_binder_stubs_pub` for the full rationale.
    register_slf4j_binder_stubs_pub(registry);

    // --- java.util.logging (JUL) — standard JDK logging ---
    //
    // ONE slot map: the one `ClassManager::synthetic_stub_fields` declares for
    // `java/util/logging/Logger` (`class_manager.rs:14191`, real-JDK order) and
    // `logmanager.rs` names — `LOGGER_FIELD_NAME` 2, `LOGGER_FIELD_PARENT` 8,
    // `LOGGER_FIELD_LEVEL` 12 (VM-internal, anchored past the 12 real fields).
    //
    // These bodies used to ask for a 2- or 3-slot Logger and write the name at
    // 0 and the level at 1, which on the declared layout is the name String
    // into `config: Logger$ConfigurationData` and an `Int` into
    // `manager: LogManager` — a wrong-typed write into a reference slot, and
    // exactly the defect `logmanager.rs:161`'s comment records as already fixed
    // there ("this used to sit on `manager`"). The width was never the hazard:
    // `Logger` HAS a declaration, so `try_alloc_concurrent_synthetic`'s closing
    // `num_fields.max(real)` clamped 3 up to 13 and the writes landed in bounds.
    // The field IDENTITY was.
    //
    // These are not all dead. `getGlobal`, `setLevel`, `getLevel`, `getName` and
    // `config(String)V` are the LAST registration for their triples in
    // synthetic-JDK mode: `register_slf4j_natives` runs at `lib.rs:24029`, and
    // `logmanager::register_logmanager_natives` (`lib.rs:24372`, the final say)
    // registers neither `getGlobal` nor `getLevel`/`setLevel`/`getName`/`config`.
    let jul_logger = "java/util/logging/Logger";
    registry.register(
        jul_logger,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/logging/Logger",
                crate::logmanager::LOGGER_NUM_FIELDS,
            )?;
            ctx.set_field(logger, crate::logmanager::LOGGER_FIELD_NAME, name);
            // No explicit level — `jul_level_int` answers `None` for an unset
            // slot and every caller applies the JDK default (INFO). Storing
            // `Int(800)` here instead would be an Int in a slot the declaration
            // types `Ljava/lang/Object;`.
            ctx.set_field(
                logger,
                crate::logmanager::LOGGER_FIELD_LEVEL,
                Value::Object(None),
            );
            Ok(Some(Value::Object(Some(logger))))
        },
    );
    registry.register(
        jul_logger,
        "getGlobal",
        "()Ljava/util/logging/Logger;",
        |ctx, _| {
            let name = ctx.create_string("global");
            let logger = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/logging/Logger",
                crate::logmanager::LOGGER_NUM_FIELDS,
            )?;
            ctx.set_field(
                logger,
                crate::logmanager::LOGGER_FIELD_NAME,
                Value::Object(Some(name)),
            );
            ctx.set_field(
                logger,
                crate::logmanager::LOGGER_FIELD_LEVEL,
                Value::Object(None),
            );
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
            // `as_int()` alone silently answers 800 (INFO) whenever the level slot
            // holds a `Level` OBJECT rather than a raw int — which is what the
            // OTHER two `setLevel` registrations store. That default made
            // `setLevel(SEVERE)` suppress nothing. Decode every shape instead.
            let stored = ctx.get_field(this, crate::logmanager::LOGGER_FIELD_LEVEL);
            let logger_level = jul_level_int(ctx, Some(stored)).unwrap_or(800);
            let check_level = jul_level_int(ctx, args.get(1).copied()).unwrap_or(800);
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
            let level = args.get(1).copied().unwrap_or(Value::Object(None));
            // `None` here means `setLevel(null)` — "inherit from the parent",
            // which must CLEAR the explicit level rather than pin it at INFO.
            let decoded = jul_level_int(ctx, Some(level));
            // Store the `Level` REFERENCE, at the declared VM-internal slot.
            // Both halves changed together and neither alone would be right:
            // the old `set_field(this, 1, Value::Int(..))` put an Int into
            // `manager: Ljava/util/logging/LogManager;`, and slot 12 is typed
            // `Ljava/lang/Object;`, so moving the Int there would just relocate
            // the wrong-tag write. `jul_level_int` decodes the object shape, and
            // this now matches what `lib.rs:17605` (`register_essential_natives_
            // with_shims`, the real-JDK-mode owner of this triple) already
            // stores — so the two modes stop disagreeing about where the level
            // lives.
            ctx.set_field(this, crate::logmanager::LOGGER_FIELD_LEVEL, level);
            // Publish to the name-keyed explicit-level table too: the
            // `isLoggable` that actually wins the registry slot
            // (`logmanager::native_jul_logger_is_loggable`, re-registered last
            // by `register_logmanager_natives`) reads that table first, and
            // without this entry it fell back to the INFO default and reported
            // every level loggable.
            let recorded = match decoded {
                Some(v) => Value::Int(v),
                None => Value::Object(None),
            };
            crate::logmanager::record_jul_logger_level(ctx, this, recorded);
            Ok(None)
        },
    );
    registry.register(
        jul_logger,
        "getLevel",
        "()Ljava/util/logging/Level;",
        |ctx, _| {
            // NOT a slot bug — this ignores the receiver entirely and mints a
            // fresh INFO `Level` per call, so no Logger slot is read and the two
            // slots it writes are `Level`'s own (`name` 0, `value` 1), which
            // match the real class. Left as-is deliberately: making it read
            // `LOGGER_FIELD_LEVEL` (which `setLevel` above now writes, and which
            // `lib.rs:17632` already reads in real-JDK mode) would start
            // returning `null` for a logger with no explicit level — the real
            // JDK contract, but a behaviour change that needs a run this lane
            // could not do. Recorded in E41's note; it is the remaining
            // divergence between this registrar and the essential one.
            let level = try_alloc_concurrent_synthetic(ctx, "java/util/logging/Level", 2)?;
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
            // `jul_logger_name_object`, not `get_field(this, 0)`. This is the
            // LAST registration for the triple, so it owns the slot -- and it
            // was answering the raw contents of slot 0, which is this file's
            // own 2/3-field layout but neither of the other two JUL Logger
            // layouts the VM produces. On the 13-field `logmanager` Logger that
            // `Logger.getLogger(name)` actually returns, slot 0 is not the name
            // and in the synthetic-JDK build is not even an object.
            Ok(Some(
                match crate::logmanager::jul_logger_name_object(&*ctx, this) {
                    Some(name) => Value::Object(Some(name)),
                    None => Value::Object(Some(ctx.create_string(""))),
                },
            ))
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
            // The GC-safe side table, not slot 2. The old comment cited "Logger
            // comment in phases_late.rs: field 2 = handlers ArrayList" — a peer
            // site, not the declaration, and the declaration
            // (`class_manager.rs:14191`) says slot 2 is `name: Ljava/lang/String;`.
            // The `object_num_fields(this) > 2` guard that went with it was
            // measuring the legacy 2-field shim shape, which no producer in this
            // file mints any more.
            let handlers = match jul_logger_handlers_get(ctx, this) {
                Some(lst) => lst,
                None => {
                    let lst = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
                    cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(lst))])
                        .ok();
                    // `jul_logger_handlers_set` calls `add_global_root`, which
                    // may grow the root table and collect — the `set_field` it
                    // replaces could not. Pin across it and re-read.
                    let lst_pin = ctx.pin_native_root(lst);
                    jul_logger_handlers_set(ctx, this, lst);
                    let lst = ctx.read_native_pin(lst_pin, lst);
                    ctx.unpin_native_roots(lst_pin);
                    lst
                }
            };
            cratonvm_native_collections::native_al_add(
                ctx,
                &[Value::Object(Some(handlers)), handler],
            )
            .ok();
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
            // Side table — see `addHandler` above.
            if let Some(handlers) = jul_logger_handlers_get(ctx, this) {
                cratonvm_native_collections::native_al_remove_obj(
                    ctx,
                    &[Value::Object(Some(handlers)), handler],
                )
                .ok();
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
            let lvl = try_alloc_concurrent_synthetic(ctx, "java/util/logging/Level", 2)?;
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
            let mgr = try_alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0)?;
            Ok(Some(Value::Object(Some(mgr))))
        },
    );
    registry.register(
        lm,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        |ctx, args| {
            // Shadowed by `logmanager.rs:5927` (registered last, and
            // `--dump-native-registry` reports it `owns_slot: true`), but kept on
            // the one declared slot map so a registration-order change cannot
            // resurrect a second one.
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let logger = try_alloc_concurrent_synthetic(
                ctx,
                "java/util/logging/Logger",
                crate::logmanager::LOGGER_NUM_FIELDS,
            )?;
            ctx.set_field(logger, crate::logmanager::LOGGER_FIELD_NAME, name);
            ctx.set_field(
                logger,
                crate::logmanager::LOGGER_FIELD_LEVEL,
                Value::Object(None),
            );
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
            // JDK-ONLY-LAYOUT: same wrong-field read as the slf4j shim above,
            // and the same fix — slot 0 of a Class mirror is the VM's ClassId
            // Int (or, on a real layout, `cachedConstructor`), never a name.
            let name_val = match args.first() {
                Some(Value::Object(Some(mirror))) => {
                    match crate::lang_class::mirror_class_name(ctx, *mirror) {
                        Some(n) => {
                            let s = ctx.create_string(&n.replace('/', "."));
                            Value::Object(Some(s))
                        }
                        None => {
                            let s = ctx.create_string("unknown");
                            Value::Object(Some(s))
                        }
                    }
                }
                _ => {
                    let s = ctx.create_string("unknown");
                    Value::Object(Some(s))
                }
            };
            let logger = try_alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2)?;
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
            let logger = try_alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2)?;
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
            let logger = try_alloc_concurrent_synthetic(ctx, "org/apache/logging/log4j/Logger", 2)?;
            ctx.set_field(logger, 0, Value::Object(Some(name)));
            ctx.set_field(logger, 1, Value::Int(2));
            Ok(Some(Value::Object(Some(logger))))
        },
    );

    register_log4j_stacklocator_bridge(registry);

    let log4j_lg = "org/apache/logging/log4j/Logger";
    // Log4j2 trace — same formatter as the debug/info/warn/error
    // registrations below, behind `slf4j_threshold` (INFO by default, so a
    // TRACE record is dropped by the filter rather than by the native).
    registry.register(log4j_lg, "trace", "(Ljava/lang/String;)V", slf4j_trace_msg);
    registry.register(
        log4j_lg,
        "trace",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_trace_msg_varargs,
    );
    // Each level goes through its own threshold-gated emitter (the same ones
    // the SLF4J `Logger` block above uses), so the `is*Enabled` guards further
    // down can be answered from that threshold without the guard and the
    // emitter ever disagreeing.
    registry.register(log4j_lg, "debug", "(Ljava/lang/String;)V", slf4j_debug_msg);
    registry.register(
        log4j_lg,
        "debug",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_debug_msg_varargs,
    );
    registry.register(log4j_lg, "info", "(Ljava/lang/String;)V", slf4j_info_msg);
    registry.register(
        log4j_lg,
        "info",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_info_msg_varargs,
    );
    registry.register(log4j_lg, "warn", "(Ljava/lang/String;)V", slf4j_warn_msg);
    registry.register(
        log4j_lg,
        "warn",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_warn_msg_varargs,
    );
    registry.register(log4j_lg, "error", "(Ljava/lang/String;)V", slf4j_error_msg);
    registry.register(
        log4j_lg,
        "error",
        "(Ljava/lang/String;[Ljava/lang/Object;)V",
        slf4j_error_msg_varargs,
    );
    registry.register(
        log4j_lg,
        "error",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        slf4j_error_msg,
    );
    // Log4j FATAL has no SLF4J counterpart; it sits above ERROR, so only
    // `defaultLogLevel=off` suppresses it — that is what `slf4j_error_msg` does.
    registry.register(log4j_lg, "fatal", "(Ljava/lang/String;)V", slf4j_error_msg);
    registry.register(log4j_lg, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    // Same threshold source as the `trace` registration above, so the guard
    // callers check and the emitter they guard cannot disagree.
    registry.register(log4j_lg, "isTraceEnabled", "()Z", |ctx, _| {
        Ok(Some(Value::Int(if slf4j_threshold(ctx) <= SLF4J_TRACE {
            1
        } else {
            0
        })))
    });
    // Previously constant `true` on the grounds that the emitters applied no
    // threshold — which made `org.slf4j.simpleLogger.defaultLogLevel` inert for
    // Log4j callers and forced every framework down its "logging is on" path.
    // Both sides moved together: the emitters above are now the level-gated
    // ones, so these guards can read the same threshold and stay truthful.
    registry.register(log4j_lg, "isDebugEnabled", "()Z", |ctx, _| {
        slf4j_level_enabled(ctx, SLF4J_DEBUG)
    });
    registry.register(log4j_lg, "isInfoEnabled", "()Z", |ctx, _| {
        slf4j_level_enabled(ctx, SLF4J_INFO)
    });
    registry.register(log4j_lg, "isWarnEnabled", "()Z", |ctx, _| {
        slf4j_level_enabled(ctx, SLF4J_WARN)
    });
    registry.register(log4j_lg, "isErrorEnabled", "()Z", |ctx, _| {
        slf4j_level_enabled(ctx, SLF4J_ERROR)
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

// ---------------------------------------------------------------------------
// SLF4J MDC — one implementation, two surfaces
// ---------------------------------------------------------------------------
//
// `org.slf4j.MDC` (static façade) and `org.slf4j.helpers.BasicMDCAdapter`
// (instance) are the two entry points SLF4J callers reach, and SLF4J 2.x routes
// EVERY façade call through the adapter. The adapter's methods used to be
// registered as constant no-ops / constant nulls "because our MDC stubs
// implement put/get directly" — but that made the pair contradict itself: a
// `put` through the adapter was discarded and the matching `get` answered null
// for a key that had just been written, so `%X{...}` / `%mdc` pattern
// converters and every correlation-id filter saw an empty context.
//
// The adapter instance the binder hands out is allocated by
// `alloc_concurrent_synthetic` with zero fields (no `<init>` runs), so its real
// `inheritableThreadLocal` field is null and real `BasicMDCAdapter` bytecode
// cannot service these calls either — the natives must stay, and they must be
// real. `off` is the index of the first REAL argument: 0 for the static façade,
// 1 for the adapter, whose `args[0]` is the receiver.

fn mdc_put_at(ctx: &mut dyn NativeContext, args: &[Value], off: usize) -> MethodCallResult {
    let key = match args.get(off) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    let value = match args.get(off + 1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    MDC_MAP.with(|m| m.borrow_mut().insert(key, value));
    Ok(None)
}

fn mdc_get_at(ctx: &mut dyn NativeContext, args: &[Value], off: usize) -> MethodCallResult {
    let key = match args.get(off) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let value = MDC_MAP.with(|m| m.borrow().get(&key).cloned());
    match value {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn mdc_remove_at(ctx: &mut dyn NativeContext, args: &[Value], off: usize) -> MethodCallResult {
    let key = match args.get(off) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(None),
    };
    MDC_MAP.with(|m| m.borrow_mut().remove(&key));
    Ok(None)
}

fn mdc_clear(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    MDC_MAP.with(|m| m.borrow_mut().clear());
    Ok(None)
}

fn mdc_copy_of_context_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
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
    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
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
}

fn mdc_set_context_map_at(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    off: usize,
) -> MethodCallResult {
    // Replace the thread-local MDC map with the entries from the provided Map.
    // Step 1: clear current MDC.
    MDC_MAP.with(|m| m.borrow_mut().clear());
    // Step 2: if arg is non-null, iterate keys and copy entries.
    let map = match args.get(off) {
        Some(Value::Object(Some(m))) => *m,
        _ => return Ok(None),
    };
    // Get key set
    let key_set =
        match cratonvm_native_collections::native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        {
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
}

#[cfg(test)]
mod logback_construction_registration_tests {
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
}

/// SLF4J level codes, as stored in the shim `Logger`'s `SLF4J_LEVEL` slot and
/// as compared by the `is*Enabled` natives: 0=TRACE … 4=ERROR, 5=OFF.
const SLF4J_TRACE: i32 = 0;
const SLF4J_DEBUG: i32 = 1;
const SLF4J_INFO: i32 = 2;
const SLF4J_WARN: i32 = 3;
const SLF4J_ERROR: i32 = 4;
const SLF4J_OFF: i32 = 5;

/// Process-wide threshold below which the SLF4J/Log4j shims drop a record.
///
/// The shims advertise INFO by default — that is exactly what their
/// `is*Enabled` natives answer — and slf4j-simple's own
/// `org.slf4j.simpleLogger.defaultLogLevel` property moves it. Consulting the
/// property here is what makes TRACE/DEBUG suppression a *filter* decision
/// rather than a hardcoded discard in the `trace`/`debug` natives: with
/// `-Dorg.slf4j.simpleLogger.defaultLogLevel=trace` a TRACE record now reaches
/// the same formatter and console sink every other level uses.
///
/// The per-logger level slot is deliberately NOT consulted: an
/// allocated-but-never-stamped slot decodes as `Value::Int(0)` (an all-zero
/// `Value` cell is `Int(0)`, i.e. indistinguishable from an explicit TRACE),
/// and a receiver that reached these interface natives from a real Logback
/// logger carries an unrelated field there. The `is*Enabled` natives that own
/// the per-logger slot keep reading it as before.
fn slf4j_threshold(ctx: &mut dyn NativeContext) -> i32 {
    match ctx.get_system_property("org.slf4j.simpleLogger.defaultLogLevel") {
        Some(level) => match level.trim().to_ascii_lowercase().as_str() {
            "trace" | "all" | "finest" => SLF4J_TRACE,
            "debug" | "fine" => SLF4J_DEBUG,
            "warn" | "warning" => SLF4J_WARN,
            "error" | "severe" | "fatal" => SLF4J_ERROR,
            "off" | "none" => SLF4J_OFF,
            // "info" and anything unparseable: slf4j-simple's own default.
            _ => SLF4J_INFO,
        },
        None => SLF4J_INFO,
    }
}

/// `trace(...)` for the SLF4J and Log4j2 shim loggers: the same formatter the
/// neighbouring `debug(...)` registrations use (`{}` parameter substitution
/// included), one level lower, delivered only when the threshold filter admits
/// TRACE.
fn slf4j_trace_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if slf4j_threshold(ctx) > SLF4J_TRACE {
        return Ok(None);
    }
    slf4j_log_msg(ctx, args)
}

/// `debug(...)` routed through the same threshold filter as `slf4j_trace_msg`.
/// Below the default INFO threshold, so this stays quiet unless the property
/// lowers it — but the record is now dropped by the filter, not by a native
/// that silently discards its arguments.
fn slf4j_debug_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if slf4j_threshold(ctx) > SLF4J_DEBUG {
        return Ok(None);
    }
    slf4j_log_msg(ctx, args)
}

/// `info(...)` behind the same threshold filter as `slf4j_debug_msg`, so the
/// `isInfoEnabled()` guard above it stays truthful once the level property
/// raises the threshold (`...defaultLogLevel=warn|error|off`).
fn slf4j_info_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if slf4j_threshold(ctx) > SLF4J_INFO {
        return Ok(None);
    }
    slf4j_log_msg(ctx, args)
}

/// `warn(...)` behind the threshold filter; see `slf4j_info_msg`.
fn slf4j_warn_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if slf4j_threshold(ctx) > SLF4J_WARN {
        return Ok(None);
    }
    slf4j_log_msg(ctx, args)
}

/// `error(...)` (and Log4j's `fatal(...)`, which has no separate SLF4J level)
/// behind the threshold filter; only `defaultLogLevel=off` suppresses these.
fn slf4j_error_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if slf4j_threshold(ctx) > SLF4J_ERROR {
        return Ok(None);
    }
    slf4j_log_msg(ctx, args)
}

/// `is<Level>Enabled()` for the shim loggers, answered from the one threshold
/// source the emitters use — a guard can then never disagree with the emitter
/// it guards.
fn slf4j_level_enabled(ctx: &mut dyn NativeContext, level: i32) -> MethodCallResult {
    Ok(Some(Value::Int(if slf4j_threshold(ctx) <= level {
        1
    } else {
        0
    })))
}

/// Render one `{}` argument the way SLF4J does: `String.valueOf(arg)`, i.e.
/// the argument's own `toString()`.
///
/// This used to be `read_string(obj).unwrap_or_else(|| format!("Object@{:x}",
/// param_idx))` — a String argument printed its text and EVERYTHING ELSE
/// printed the literal text `Object@` followed by its **argument index**. Not
/// an identity hash, not a class name: three different objects logged in the
/// same run all render as `Object@2` because they were all the first `{}`.
///
/// The cost is not cosmetic. Every diagnostic whose payload is not already a
/// String loses its payload, and those are the ones worth logging — a `Path`,
/// a policy object, a config value. Testcontainers' startup log on this VM
/// read `Image pull policy will be performed by: Object@2` and `you must set
/// 'testcontainers.reuse.enable=true' in a file located at Object@2`, which is
/// how a Docker-transport investigation lost its evidence trail (2026-08-14).
///
/// Falls back to `Object.toString()`'s own shape (`ClassName@hash`) if the
/// virtual dispatch cannot answer, so a broken `toString()` degrades to
/// HotSpot's default rather than to a placeholder.
fn slf4j_render_arg(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    if let Some(s) = ctx.read_string(obj) {
        return s;
    }
    let pin = ctx.pin_native_root(obj);
    let via_to_string = match ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    };
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    via_to_string.unwrap_or_else(|| {
        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(obj))
            .unwrap_or_else(|| "java/lang/Object".to_string())
            .replace('/', ".");
        format!("{cls}@{:x}", ctx.identity_hash_code(obj))
    })
}

/// Spread an SLF4J VARARGS call's `Object[]` into positional arguments.
///
/// `info(String, Object...)` arrives here as `[this, format, theArray]`, so a
/// handler that reads `args[2..]` sees ONE argument -- the array -- renders its
/// `toString()` into the first `{}`, and leaves every later placeholder
/// unsubstituted. Testcontainers' Ryuk diagnostic is the canonical face:
///
/// ```text
/// Can not connect to Ryuk at [Ljava.lang.Object;@2795a:{}
/// ```
///
/// where HotSpot prints `localhost:60499`. That is the same class of defect as
/// the 2026-08-14 `Object@<argument index>` fix (see
/// `wrongcredentialstest-...`, section 5) and was left behind by it: that one
/// corrected HOW an argument is rendered, this one corrects WHICH arguments
/// there are. Every multi-placeholder SLF4J call in every workload was losing
/// all but its first argument.
///
/// SLF4J's own `MessageFormatter.arrayFormat` is the spec being matched. The
/// non-varargs 1- and 2-argument overloads are unaffected: javac routes a call
/// whose sole argument IS an `Object[]` to `info(String, Object)`, so an array
/// arriving on the varargs descriptor is always a spread.
fn slf4j_spread_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = args.iter().take(2).copied().collect();
    match args.get(2) {
        Some(Value::Object(Some(arr))) => {
            let n = ctx.array_length(*arr);
            for i in 0..n {
                out.push(ctx.get_array_element(*arr, i));
            }
        }
        // `info(fmt, (Object[]) null)` -- SLF4J renders no arguments at all.
        Some(Value::Object(None)) => {}
        _ => out.extend(args.iter().skip(2).copied()),
    }
    out
}

/// One spreading wrapper per level-gated emitter. Every `(String, Object[])`
/// registration must use the `_varargs` form -- there are two registrars for
/// `org/slf4j/Logger` in this file and the LAST one wins, so fixing only one
/// of them changes nothing.
macro_rules! slf4j_varargs_wrapper {
    ($name:ident, $inner:ident) => {
        fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let spread = slf4j_spread_varargs(ctx, args);
            $inner(ctx, &spread)
        }
    };
}

slf4j_varargs_wrapper!(slf4j_log_msg_varargs, slf4j_log_msg);
slf4j_varargs_wrapper!(slf4j_trace_msg_varargs, slf4j_trace_msg);
slf4j_varargs_wrapper!(slf4j_debug_msg_varargs, slf4j_debug_msg);
slf4j_varargs_wrapper!(slf4j_info_msg_varargs, slf4j_info_msg);
slf4j_varargs_wrapper!(slf4j_warn_msg_varargs, slf4j_warn_msg);
slf4j_varargs_wrapper!(slf4j_error_msg_varargs, slf4j_error_msg);

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

    // Render every object argument BEFORE substituting, because rendering one
    // of them re-enters Java (`toString()`) and a collection during that call
    // moves the others. Pin them all for the duration.
    let mut pins: Vec<Option<(usize, ObjectRef)>> = Vec::new();
    let mut base_pin: Option<usize> = None;
    for a in args.iter().skip(2) {
        match a {
            Value::Object(Some(o)) => {
                let p = ctx.pin_native_root(*o);
                if base_pin.is_none() {
                    base_pin = Some(p);
                }
                pins.push(Some((p, *o)));
            }
            _ => pins.push(None),
        }
    }
    let mut rendered: Vec<Option<String>> = Vec::with_capacity(pins.len());
    for i in 0..pins.len() {
        let Some((p, o)) = pins[i] else {
            rendered.push(None);
            continue;
        };
        let o = ctx.read_native_pin(p, o);
        pins[i] = Some((p, o));
        let text = slf4j_render_arg(ctx, o);
        // `slf4j_render_arg` may have collected; refresh every other pin.
        for j in 0..pins.len() {
            if let Some((pj, oj)) = pins[j] {
                pins[j] = Some((pj, ctx.read_native_pin(pj, oj)));
            }
        }
        rendered.push(Some(text));
    }
    if let Some(bp) = base_pin {
        ctx.unpin_native_roots(bp);
    }

    // Substitute {} placeholders with parameter values
    let mut result = format_str.clone();
    let mut param_idx = 2;
    while let Some(pos) = result.find("{}") {
        let replacement = match args.get(param_idx) {
            Some(Value::Object(Some(_))) => match rendered.get(param_idx - 2) {
                Some(Some(t)) => t.clone(),
                _ => "null".to_string(),
            },
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
///
/// Slot map: NONE of its own. Both reads below used to be raw slot indices from
/// the legacy 2/3-field shim layout — `get_field(this, 0)` for the name and
/// `get_field(logger, 2)` for the handler list — against a class that
/// `ClassManager::synthetic_stub_fields` declares in **real-JDK order**
/// (`class_manager.rs:14191`: `config` 0, `manager` 1, `name` **2**). On the
/// 13-slot Logger that `Logger.getLogger(name)` actually returns, slot 0 is the
/// (unset) `config` and slot 2 is the **name String** — so the name came back
/// empty and the "handler list" was a `java.lang.String` that the fan-out loop
/// below then called `size()` / `get(I)` on. This is not dead code: it is the
/// registered body for `Logger.config(String)V` in
/// [`register_slf4j_natives`], and nothing re-registers that triple after it.
pub(crate) fn jul_log_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Logger), args[1] = message string
    let logger_name = match args.first() {
        Some(Value::Object(Some(this))) => crate::logmanager::read_jul_logger_name(&*ctx, *this),
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
    // The GC-safe side table, not a raw slot: `jul_logger_handlers_set` is what
    // the `addHandler` that actually owns the registry slot
    // (`reflect_annotations.rs:213`, re-registered last) writes into, and slot 2
    // on the declared layout is the logger's NAME.
    let handlers = match jul_logger_handlers_get(ctx, *logger) {
        Some(list) => list,
        None => return Ok(None),
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
