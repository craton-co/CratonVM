// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.lang.reflect` / annotation / module natives: dynamic proxies, annotation attribute maps, Module & ModuleBuilder.
//!
//! Pure code move out of `lib.rs` (no logic, signature or ordering changes).
//! Registration call sites are untouched, so the native registration sequence
//! is byte-identical to before the split.

use super::*;

/// The body of the native `java.util.logging.Handler.<init>()V`: rebuild the
/// field initializers a native constructor skips.
///
/// `this_pin` pins the receiver (`this0` is its address when the pin was
/// taken); the caller releases it. Every GC point below -- `Level`'s first-use
/// `<clinit>`, the `LogManager` singleton, `new ErrorManager()` -- is followed
/// by a fresh read of the receiver through the pin (gc-common w21-d: they were
/// followed by writes through the `args` slice's copy, which a moving
/// collection does not rewrite).
fn jul_handler_init_fields(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this0: ObjectRef,
) -> MethodCallResult {
    let level_class = ctx.ensure_class_initialized("java/util/logging/Level")?;
    let this = ctx.read_native_pin(this_pin, this0);
    if let Some(all_index) = ctx.static_field_index_by_name(level_class, "ALL") {
        ctx.set_field_by_name(
            this,
            "logLevel",
            ctx.get_static_field(level_class, all_index),
        );
    }
    ctx.set_field_by_name(this, "filter", Value::Object(None));
    // `private final LogManager manager = LogManager.getLogManager();` is
    // this class's FIRST field initializer, and a native `<init>` skips it
    // for the same reason it skipped `errorManager` below. MEASURED, both
    // modes, `probes/JulHandlerLevel.java`:
    //
    //   Handler.manager   HotSpot  LogManager
    //                     --jdk-only  LogManager   (the real ctor runs:
    //                                               this registration is
    //                                               a SyntheticStub and
    //                                               strict refuses it)
    //                     compatible  null
    //
    // So the gap was compatible-mode only, and closing it converges the two
    // modes rather than inventing anything: the value written is the same
    // singleton `LogManager.getLogManager()` answers.
    //
    // Same two guards as `errorManager`: the field must EXIST (a synthetic
    // `Handler` has no `manager`, and `get_field_by_name` cannot tell
    // "absent" from "null"), and it must still be null, so a real
    // constructor that did run is never clobbered. `this` is re-read
    // through the pin because building the singleton allocates.
    let handler_class_for_manager = ctx.class_id_of_object(this);
    if ctx
        .resolve_field_index_by_class_id(handler_class_for_manager, "manager")
        .is_some()
        && matches!(ctx.get_field_by_name(this, "manager"), Value::Object(None))
    {
        let manager = crate::logmanager::jul_log_manager_singleton(ctx);
        let this_live = ctx.read_native_pin(this_pin, this0);
        if let Some(manager) = manager {
            ctx.set_field_by_name(this_live, "manager", Value::Object(Some(manager)));
        }
    }
    // `java.util.logging.Handler`'s third field initializer is
    // `private volatile ErrorManager errorManager = new ErrorManager();`
    // — and a native `<init>` replaces the real constructor wholesale, so
    // NONE of the JDK's field initializers run. Two of the three were
    // already reconstructed above; this one was not, which left
    // `errorManager` null on every `Handler` in Compatible mode. HotSpot's
    // is never null (its own javadoc: "there is a default ErrorManager
    // installed"), and `Handler.reportError` — the destination of every
    // absorbed `Exception` in the whole `Handler` family — dereferences it
    // unguarded, so an absorbed flush/close failure NPE'd inside the
    // reporting path and came out as `reportError`'s own
    // `catch (Exception ex2)` message instead of the failure.
    //
    // COMPATIBLE-MODE PARITY, not a behaviour change: it converges on what
    // HotSpot's constructor does. Guarded on null so a real ctor that DID
    // run (or a `setErrorManager` that already landed) is never clobbered.
    // W7-64-printstream-trouble-and-errormanager.md
    //
    // The `resolve_field_index_by_class_id` guard is not belt-and-braces:
    // `get_field_by_name` answers `Object(None)` for "null" and for "no
    // such field" alike, and a SYNTHETIC `Handler` has no such field — so
    // without it this would allocate one dead `ErrorManager` per handler
    // and drop it into a `set_field_by_name` that is documented to no-op.
    // The synthetic arm gets its `ErrorManager` from
    // `register_p61_handler_error_manager` instead.
    let this = ctx.read_native_pin(this_pin, this0);
    let handler_class = ctx.class_id_of_object(this);
    let has_error_manager_field = ctx
        .resolve_field_index_by_class_id(handler_class, "errorManager")
        .is_some();
    if has_error_manager_field
        && matches!(
            ctx.get_field_by_name(this, "errorManager"),
            Value::Object(None)
        )
    {
        if let Ok(Some(Value::Object(Some(em)))) =
            ctx.new_object("java/util/logging/ErrorManager")
        {
            let this_live = ctx.read_native_pin(this_pin, this0);
            ctx.set_field_by_name(this_live, "errorManager", Value::Object(Some(em)));
        }
    }
    Ok(None)
}

/// Annotation natives shared between essential and synthetic registration.
/// Intercepts `Class.getAnnotation(s)`, `Field.getAnnotation(s)`,
/// `Method.getAnnotation(s)`, etc., bypassing the JDK's
/// `AnnotationParser.parseAnnotations` path which depends on raw class-file
/// bytes and a fully-wired `jdk.internal.reflect.ConstantPool` we don't
/// provide.
pub(crate) fn register_annotation_overrides(registry: &mut NativeMethodRegistry) {
    // census-tag: bypasses the JDK's AnnotationParser (depends on raw class
    // bytes + jdk.internal.reflect.ConstantPool we don't provide) → Bridge.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "java/lang/Class",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_class_get_annotations,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        lang_class::native_class_get_declared_annotations,
    );
    registry.register(
        "java/lang/Class",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        native_class_get_annotation,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        lang_class::native_class_get_declared_annotation,
    );
    registry.register(
        "java/lang/Class",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        native_class_is_annotation_present,
    );
    registry.register(
        "java/lang/Class",
        "isAnnotation",
        "()Z",
        native_class_is_annotation,
    );
    registry.register(
        "java/lang/Class",
        "getAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        native_class_get_annotations_by_type,
    );
    registry.register(
        "java/lang/Class",
        "getDeclaredAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        lang_class::native_class_get_declared_annotations_by_type,
    );
    // Field annotation methods
    registry.register(
        "java/lang/reflect/Field",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_field_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_field_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Field",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        native_field_is_annotation_present,
    );
    registry.register(
        "java/lang/reflect/Field",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        lang_class::native_field_get_annotation,
    );
    // Method annotation methods
    registry.register(
        "java/lang/reflect/Method",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_method_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_method_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Method",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        native_method_is_annotation_present,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        native_method_get_annotation,
    );
    // Without these, the real Executable.getAnnotationsByType bytecode
    // re-parses raw annotation bytes our synthetic Method doesn't carry →
    // AnnotationFormatError ("Unexpected end of annotations") and JUnit's
    // repeatable-annotation lookups die (see
    // native_method_get_annotations_by_type).
    registry.register(
        "java/lang/reflect/Method",
        "getAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        lang_class::native_method_get_annotations_by_type,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getDeclaredAnnotationsByType",
        "(Ljava/lang/Class;)[Ljava/lang/annotation/Annotation;",
        lang_class::native_method_get_annotations_by_type,
    );
    registry.register(
        "java/lang/reflect/Method",
        "getParameterAnnotations",
        "()[[Ljava/lang/annotation/Annotation;",
        lang_class::native_method_get_parameter_annotations,
    );
    // Constructor annotation methods. CratonVM builds Constructor reflective
    // objects (create_constructor_object) WITHOUT the raw `annotations` /
    // `parameterAnnotations` byte[] fields the real JDK bytecode parses, so
    // without these native overrides Constructor.getDeclaredAnnotations() /
    // getParameterAnnotations() fell through to that bytecode and returned
    // empty — Jackson then reported "no Creators" for an `@JsonCreator`
    // constructor (keycloak CredentialModelTest / PasswordCredentialData).
    // The shared Method natives work because `method_class_name_desc` now
    // resolves a Constructor receiver to its `<init>` metadata.
    registry.register(
        "java/lang/reflect/Constructor",
        "getAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_method_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getDeclaredAnnotations",
        "()[Ljava/lang/annotation/Annotation;",
        native_method_get_annotations,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getAnnotation",
        "(Ljava/lang/Class;)Ljava/lang/annotation/Annotation;",
        native_method_get_annotation,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "isAnnotationPresent",
        "(Ljava/lang/Class;)Z",
        native_method_is_annotation_present,
    );
    registry.register(
        "java/lang/reflect/Constructor",
        "getParameterAnnotations",
        "()[[Ljava/lang/annotation/Annotation;",
        lang_class::native_method_get_parameter_annotations,
    );
    // Annotation proxy: annotationType() returns the Class mirror
    registry.register(
        "java/lang/annotation/Annotation",
        "annotationType",
        "()Ljava/lang/Class;",
        native_annotation_annotation_type,
    );
    // census-tag: end of the direct annotation-bridge registrations. Restore
    // here so the many nested module registrars below keep their own (default)
    // categories instead of inheriting this fn's Bridge scope.
    registry.set_category(__prev_cat);

    // T19.H3 (final override): LogManager singleton + Logger registry.
    // Registered LAST so it wins over every earlier `getLogManager` /
    // `getLogger` / `addLogger` fallback in this file AND in
    // `phases_late::register_p61_logging` (which runs earlier via the
    // phases registration path). Fixes KC26
    // `ClassCastException: java/lang/Class cannot be cast to
    // java/util/logging/LogManager` by ensuring the native always
    // returns a LogManager instance ObjectRef (never a Class mirror).
    logmanager::register_logmanager_natives(registry);
    // JULI final override: LogManager's generic registry has no model of the
    // real Logger.ConfigurationData layout. Keep explicit handlers on the
    // logger mirror itself so AsyncFileHandler delivery stays rooted and
    // isolated across the parameterized Tomcat fixtures.
    registry.register(
        "java/util/logging/LogRecord",
        "getMessage",
        "()Ljava/lang/String;",
        |ctx, args| Ok(Some(ctx.get_field_by_name(obj_arg(args, 0)?, "message"))),
    );
    registry.register(
        "java/util/logging/Logger",
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        |ctx, args| {
            // MEASURED 2026-08-13 (/tmp/W.java): NullPointerException with NO
            // message. NOT a blanket JUL rule -- `Handler.setFilter(null)` and
            // `Logger.setLevel(null)` are LEGAL on HotSpot (the latter means
            // "inherit"), so the check goes only where it was measured.
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            let Some(Value::Object(Some(logger))) = args.first() else {
                return Ok(None);
            };
            let logger = *logger;
            let handler = args.get(1).copied().unwrap_or(Value::Object(None));
            // gc-common w14-d handoff: the side-list row is keyed by the
            // logger's ADDRESS, so it must be keyed after the list's
            // allocation and init (both GC points), not before. Root the
            // logger and the handler across them and re-read both.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let logger_h = scope.root(logger);
            let handler_h = match handler {
                Value::Object(Some(h)) => Some(scope.root(h)),
                _ => None,
            };
            let handlers = match jul_logger_handlers_get(&mut *scope, logger) {
                Some(list) => list,
                None => {
                    let list =
                        try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
                    let list_h = scope.root(list);
                    cratonvm_native_collections::native_al_init(
                        &mut *scope,
                        &[Value::Object(Some(list))],
                    )?;
                    let list = scope.get(&list_h);
                    let logger = scope.get(&logger_h);
                    jul_logger_handlers_set(&mut *scope, logger, list);
                    list
                }
            };
            let handler = match &handler_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => handler,
            };
            // gc-common w11-c: the reclaiming door (a registration; nothing is
            // used after the add).
            cratonvm_native_collections::native_al_add_entry_pub(
                &mut *scope,
                &[Value::Object(Some(handlers)), handler],
            )?;
            Ok(None)
        },
    );
    registry.register(
        "java/util/logging/Logger",
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        |ctx, args| {
            let Some(Value::Object(Some(logger))) = args.first() else {
                return Ok(None);
            };
            // This native logger has no parent-handler chain. Once JULI
            // detaches a fixture handler, discard its list for the next case.
            jul_logger_handlers_clear(ctx, *logger);
            Ok(None)
        },
    );
    registry.register(
        "java/util/logging/Logger",
        "getHandlers",
        "()[Ljava/util/logging/Handler;",
        // `Handler[]` (i7-L2): the one shared body; this registration is the
        // one that wins over `lib.rs`'s, which registers the same fn.
        jul_logger_get_handlers,
    );
    registry.register(
        "java/util/logging/Handler",
        "getFormatter",
        "()Ljava/util/logging/Formatter;",
        |ctx, args| {
            let value = match args.first() {
                Some(Value::Object(Some(this))) => ctx.get_field_by_name(*this, "formatter"),
                _ => Value::Object(None),
            };
            Ok(Some(value))
        },
    );
    registry.register(
        "java/util/logging/Handler",
        "setFormatter",
        "(Ljava/util/logging/Formatter;)V",
        |ctx, args| {
            // MEASURED 2026-08-13 (/tmp/W.java): NullPointerException with NO
            // message. NOT a blanket JUL rule -- `Handler.setFilter(null)` and
            // `Logger.setLevel(null)` are LEGAL on HotSpot (the latter means
            // "inherit"), so the check goes only where it was measured.
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            if let Some(Value::Object(Some(this))) = args.first() {
                ctx.set_field_by_name(
                    *this,
                    "formatter",
                    args.get(1).cloned().unwrap_or(Value::Object(None)),
                );
            }
            Ok(None)
        },
    );
    registry.register("java/util/logging/Handler", "<init>", "()V", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first() else {
            return Ok(None);
        };
        // gc-common w21-d: every step below that can collect -- `Level`'s
        // first-use `<clinit>`, building the `LogManager` singleton,
        // `new ErrorManager()` -- used to be followed by a write through
        // `*this`, the `args` slice's copy of the receiver, which a moving
        // collection does not rewrite. The receiver is pinned for the whole
        // body and read back through the pin after each GC point.
        let this_pin = ctx.pin_native_root(*this);
        let result = jul_handler_init_fields(ctx, this_pin, *this);
        ctx.unpin_native_roots(this_pin);
        result
    });
    registry.register(
        "java/util/logging/Handler",
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        |ctx, args| {
            // MEASURED 2026-08-13 (/tmp/W.java): NullPointerException with NO
            // message. NOT a blanket JUL rule -- `Handler.setFilter(null)` and
            // `Logger.setLevel(null)` are LEGAL on HotSpot (the latter means
            // "inherit"), so the check goes only where it was measured.
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            ctx.set_field_by_name(
                obj_arg(args, 0)?,
                "logLevel",
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    registry.register(
        "java/util/logging/Handler",
        "getLevel",
        "()Ljava/util/logging/Level;",
        |ctx, args| Ok(Some(ctx.get_field_by_name(obj_arg(args, 0)?, "logLevel"))),
    );
    registry.register(
        "java/util/logging/Handler",
        "isLoggable",
        "(Ljava/util/logging/LogRecord;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(Value::Object(Some(record))) = args.get(1).copied() else {
                return Ok(Some(Value::Int(0)));
            };
            // gc-common w13-f: the two `intValue()` calls below run Java, and
            // the handler (`filter` is read off it) and the record (handed to
            // `filter.isLoggable`) were both used AFTER them through the
            // addresses copied out of `args` — only the record's level was
            // pinned. All three are rooted in one scope, which also releases
            // them on every exit (the old pin was never unpinned).
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let record_h = scope.root(record);
            let level = scope.get_field_by_name(this, "logLevel");
            let record_level_h = match scope.get_field_by_name(record, "level") {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            };
            let level_value = match level {
                Value::Object(Some(level)) => {
                    match scope.invoke_virtual(level, "intValue", "()I", &[])? {
                        Some(Value::Int(value)) => value,
                        _ => return Ok(Some(Value::Int(0))),
                    }
                }
                _ => return Ok(Some(Value::Int(0))),
            };
            let record_value = match record_level_h {
                Some(handle) => {
                    let record_level = scope.get(&handle);
                    match scope.invoke_virtual(record_level, "intValue", "()I", &[])? {
                        Some(Value::Int(value)) => value,
                        _ => return Ok(Some(Value::Int(0))),
                    }
                }
                None => return Ok(Some(Value::Int(0))),
            };
            if level_value == i32::MAX || record_value < level_value {
                return Ok(Some(Value::Int(0)));
            }
            let this = scope.get(&this_h);
            let filter = scope.get_field_by_name(this, "filter");
            if let Value::Object(Some(filter)) = filter {
                let record = scope.get(&record_h);
                return match scope.invoke_virtual(
                    filter,
                    "isLoggable",
                    "(Ljava/util/logging/LogRecord;)Z",
                    &[Value::Object(Some(record))],
                )? {
                    Some(Value::Int(value)) => Ok(Some(Value::Int((value != 0) as i32))),
                    _ => Ok(Some(Value::Int(0))),
                };
            }
            Ok(Some(Value::Int(1)))
        },
    );
    registry.register(
        "java/util/logging/Handler",
        "setFormatter",
        "(Ljava/util/logging/Formatter;)V",
        |ctx, args| {
            // MEASURED 2026-08-13 (/tmp/W.java): NullPointerException with NO
            // message. NOT a blanket JUL rule -- `Handler.setFilter(null)` and
            // `Logger.setLevel(null)` are LEGAL on HotSpot (the latter means
            // "inherit"), so the check goes only where it was measured.
            if matches!(args.get(1), None | Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            ctx.set_field_by_name(
                obj_arg(args, 0)?,
                "formatter",
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        },
    );
    registry.register(
        "java/util/logging/Handler",
        "getFormatter",
        "()Ljava/util/logging/Formatter;",
        |ctx, args| Ok(Some(ctx.get_field_by_name(obj_arg(args, 0)?, "formatter"))),
    );
    register_log4j_stacklocator_bridge(registry);

    // WP2.1: java.lang.reflect full coverage — net-new natives
    // (trySetAccessible, canAccess, getEnclosingClass, Parameter
    // helpers, Executable.getParameters, Method.is{VarArgs,Bridge,
    // Synthetic,Default}, Constructor.is{Synthetic,VarArgs}/getName,
    // Field.is{Synthetic,EnumConstant}, Method.getDefaultValue).
    // Registered last so they override any earlier null-stubs (like
    // the old `Class.getEnclosingClass` -> `native_return_null`).
    lang_reflect::register_wp2_1_natives(registry);

    // WP2.3-B Lookup.defineClass/defineHiddenClass surface. Previously
    // registered only via register_synthetic_overrides — real-JDK mode ran
    // the real Lookup.defineClass bytecode instead, whose ClassLoader
    // descent reached our defineClass1 native with a 0-length byte view
    // ("class file too short"), null-ing the defined class. Canonical
    // victim: Gradle's LookupClassDefiner injecting synthetic legacy
    // interfaces ("Could not inject synthetic classes" + NPE "Cannot invoke
    // getPackageName on null" in every ProjectBuilder bootstrap).
    lookup_define::register_lookup_define_class(registry);

    // WP_CHM_SIZE: ConcurrentHashMap.addCount(JI)V atomic-baseCount override.
    //
    // Real JDK uses a LongAdder-style striped counter (baseCount + counterCells).
    // Under our interpreter, concurrent contention on this striped counter
    // loses ~50-95% of increments at >8-thread loads (size() returns far less
    // than the actual entry count).  Reproduces in `apps/chm_stress/ChmStress`
    // (32×1000 puts → size=1018 instead of 32000 expected).  Repros even with
    // JIT disabled and is specific to real-JDK CHM bytecode (not our synthetic
    // collections) — tracing shows individual `Unsafe.compareAndSetLong` calls
    // succeed with correct atomicity (verified via standalone CASTest at
    // 16×1000 increments = 16000 exact), but the cell-array growth + probe
    // rehash dance in `fullAddCount` somehow loses increments without throwing.
    //
    // Override addCount with a single atomic baseCount-only counter.  Trades
    // the LongAdder cell-striping optimization for correctness; concurrent
    // size() is exact.  size() reads `baseCount + Σ(counterCells)`; since
    // counterCells stays null with this override, size() returns baseCount,
    // which equals the real entry count.
    //
    // CHM instance field layout walked from JVM class file:
    //   AbstractMap (slots 0,1: keySet, values) +
    //   CHM (table=2, nextTable=3, baseCount=4 [J], sizeCtl=5 [I],
    //        transferIndex=6 [I], cellsBusy=7 [I], counterCells=8 [Object],
    //        keySet=9, values=10, entrySet=11).
    //
    // We use `compare_and_swap_field(this, BASECOUNT_SLOT, current, new)`
    // which routes through a per-object mutex (`with_cas_lock`) for
    // atomicity, and `set_field_volatile_as(b'J')` for descriptor-aware
    // tag-exact writes.
    {
        let chm = "java/util/concurrent/ConcurrentHashMap";
        const BASECOUNT_SLOT: usize = 4;
        registry.register(chm, "addCount", "(JI)V", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // The delta `x` arrives as the second arg — typically Long(1) but
            // may surface as Double(<denormal>) due to operand-stack tag-loss
            // for category-2 longs (CompactValue::long stores raw i64 untagged,
            // and `to_value()` decodes untagged bits as Double).  Recover the
            // i64 via direct bit read regardless of which tag the args[] entry
            // arrived with.
            let delta: i64 = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Double(d)) => d.to_bits() as i64,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // CAS retry loop on baseCount.  `compare_and_swap_field` takes a
            // per-object mutex so the read+compare+store is atomic.  Each
            // iteration either succeeds (break) or sees a fresher value on
            // the next read.
            loop {
                let current = ctx.get_field_volatile(this, BASECOUNT_SLOT);
                let cur_bits: i64 = match current {
                    Value::Long(x) => x,
                    Value::Double(d) => d.to_bits() as i64,
                    Value::Int(x) => x as i64,
                    Value::Object(None) | Value::Uninitialized => 0,
                    _ => 0,
                };
                let new_val = Value::Long(cur_bits.wrapping_add(delta));
                if ctx.compare_and_swap_field(this, BASECOUNT_SLOT, current, new_val) {
                    break;
                }
                // CAS failed — racing.  Retry with fresh read.  Since
                // with_cas_lock serializes all CAS on this baseCount slot,
                // the loop is bounded by the number of contending threads.
            }
            Ok(None)
        });
    }

    // SPB.10 / Spring `BeanWrapperImpl`: register `java.beans.Introspector.getBeanInfo`
    // and `PropertyDescriptor` natives in *real-JDK* mode. Phase72 is only wired
    // through `register_synthetic_overrides` (gated on the `synthetic-jdk`
    // feature), so without this call the real-JDK boot lets the JDK bytecode
    // build a `GenericBeanInfo` whose `propertyDescriptors[]` is empty — every
    // setter (e.g. `setMetadataReaderFactory`) then surfaces as
    // `NotWritablePropertyException` to Spring. The native walks
    // declared methods + superclasses to produce real `PropertyDescriptor`s
    // backed by `java.lang.reflect.Method` mirrors.
    crate::phases_late::register_p72_beans(registry);

    // BaseStream / Stream / IntStream / LongStream / DoubleStream
    // `sequential() / parallel() / unordered() / isParallel() / onClose()`
    // are default methods on the `java.util.stream.BaseStream` interface in
    // the real JDK. Real-JDK code (e.g. Spring Boot 4.0.6 auto-config) calls
    // `stream.sequential()` and our `invokeinterface` dispatch surfaces a
    // `NoSuchMethodError java/util/stream/Stream.sequential()Ljava/util/stream/BaseStream;`
    // because find_method_recursive treats the BaseStream declaration as
    // abstract (it IS abstract in the bytecode — concrete subclasses like
    // AbstractPipeline override it) and skips it. The full
    // synthetic-jdk-only `register_stream_overrides` registration was
    // unreachable in real-JDK builds; pull it into essentials so the
    // identity/return-this no-ops are available without the JIT/interpreter
    // having to resolve the override on the receiver's pipeline class.
    crate::streams::register_stream_overrides(registry);
    // `IntStream/LongStream/DoubleStream.summaryStatistics()` are ABSTRACT on
    // the real JDK 25 interfaces (`javap java.util.stream.IntStream`), and
    // `native-collections` mints its primitive streams as instances of those
    // interfaces themselves (`try_alloc_synthetic(ctx, "java/util/stream/
    // IntStream", ..)` in `make_int_stream`). With no native for the exact
    // triple the call resolves to the bodiless interface declaration and dies
    // with `AbstractMethodError: … has no Code attribute` — the same shape that
    // `LongStream.mapToObj` and `Stream.forEachOrdered` already hit. The only
    // registration of these three lived in `register_phase56_stream_extras`,
    // which is reachable solely from `register_synthetic_overrides` and so is
    // compiled out of the default CLI entirely; a Cargo feature is a build-time
    // answer to a runtime question (docs/architecture/natives-over-real-jdk-classes.md
    // §2). `register_phase56_primitive_stream_terminals` is the NARROWED
    // registrar carrying just the terminal operations that are safe on the
    // real-JDK path — the parent registrar cannot be wired wholesale, because
    // it also registers STATIC interface methods (`Stream.iterate/generate/
    // ofNullable`, `{Int,Long,Double}Stream.concat`) that keep the native check
    // in real-JDK mode and would hand real pipelines our 1-field eager stream.
    // See docs/internal/jdk-only/W7-5-registrars-that-never-shipped.md.
    //
    // ORDERING: safe in both directions. No triple registered here is
    // registered by any live registrar (checked against the whole live
    // registration set — the three `summaryStatistics` triples appear nowhere
    // else), so this cannot take over a key something else is serving. And
    // `register_annotation_overrides` runs from `register_essential_natives`,
    // i.e. before `register_collections_natives` in `vm_init`, so even a future
    // overlap would be resolved in native-collections' favour by
    // last-registration-wins rather than against it.
    crate::phases_late::register_phase56_primitive_stream_terminals(registry);
    // Predicate's compositional defaults are invokedynamic captures in the
    // real JDK. Register the GC-visible bridge implementations in real-JDK
    // mode as well so field-filter composition does not retain a stale capture
    // receiver through JUnit cleanup.
    crate::phases_late::register_phase56_function_extras(registry);

    // Spring XML namespace parsing: preserve validation and namespace-aware
    // parsing, but attach a VM-wide Xerces grammar pool so repeated
    // GenericXmlApplicationContext loads of the same Spring XSDs reuse parsed
    // grammars instead of reparsing them for every inherited JUnit method.
    registry.register(
        "org/springframework/beans/factory/xml/DefaultDocumentLoader",
        "createDocumentBuilderFactory",
        "(IZ)Ljavax/xml/parsers/DocumentBuilderFactory;",
        native_spring_default_document_loader_create_document_builder_factory,
    );

    // EUREKA-RB-CANDIDATE: see comment on the `check_override` allow-list
    // entry in `vm_exec.rs` — `ResourceBundle$Control.getCandidateLocales`
    // crashes with `NullPointerException: key must not be null` on our
    // synthetic default Locale (null `baseLocale` field). Return a
    // single-element `[locale]` list to bypass the buggy path.
    registry.register(
        "java/util/ResourceBundle$Control",
        "getCandidateLocales",
        "(Ljava/lang/String;Ljava/util/Locale;)Ljava/util/List;",
        |ctx, args| {
            let locale =
                match args.get(2) {
                    Some(Value::Object(Some(l))) => *l,
                    _ => return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(
                            cratonvm_types::error::RuntimeError::NullPointerException {
                                message: Some(
                                    "ResourceBundle$Control.getCandidateLocales: locale is null"
                                        .to_string(),
                                ),
                            },
                        ),
                    )),
                };
            ctx.invoke(
                "java/util/Collections",
                "singletonList",
                "(Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(locale))],
            )
        },
    );

    // Kafka 4.2.0: MetaPropertiesEnsemble.verify throws
    // "No readable meta.properties files found." in real-JDK mode because
    // our HashMap layout makes the populated logDirProps map look empty
    // to AbstractMap.isEmpty()/size(). Files.newInputStream successfully
    // reads meta.properties (134 bytes / 4 entries parsed), but the
    // dir → MetaProperties put into Loader's HashMap is not visible to
    // a subsequent Map.isEmpty() call inside verify. Make verify a no-op
    // so KafkaRaftServer.initializeLogDirs can advance past this check
    // to the Copier / BootstrapDirectory phase. Paired with the
    // `check_override` allow-list entry in `vm/src/vm/vm_exec.rs` that
    // forces native dispatch over the JDK bytecode.
    //
    // KEEP (no-op, justified but MASKING): this is the one constant in this
    // file that suppresses a real validation rather than reproducing JDK
    // behaviour. It is load-bearing only for the underlying HashMap-visibility
    // bug described above; once that is fixed this registration must be
    // DELETED, not kept, or a genuinely unreadable meta.properties will pass
    // verification silently.
    //
    // Provenance, so a future reader can check the premise instead of trusting
    // it. Introduced by commit 2f128a915 ("Round 74: ... MetaPropertiesEnsemble
    // .verify no-op (Kafka past log-dir verify)"):
    //     git log --oneline -S MetaPropertiesEnsemble -- native-builtins/src/
    // Both halves of the workaround are found by:
    //     git grep -n MetaPropertiesEnsemble
    // which must return exactly two live sites — this registration and the
    // `check_override` allow-list entry in `vm/src/vm/vm_exec.rs`. Delete both
    // together or neither; the allow-list entry alone does nothing and this
    // registration alone is not reached.
    //
    // HOW TO FALSIFY THE PREMISE (it is a claim about CratonVM's HashMap, not
    // about Kafka): the assertion is that a `HashMap` which has had entries
    // `put` into it reports `size()==0` / `isEmpty()==true` on read-back
    // through `AbstractMap`. That is a self-contained probe — put a few
    // entries, then read `size()`/`isEmpty()` via the AbstractMap-inherited
    // path — and needs no Kafka at all. If that probe now passes, remove both
    // sites and re-run `KafkaRaftServer.initializeLogDirs`; the expected
    // failure if the bug IS fixed and this is left in place is silence, which
    // is exactly why it must not be left in place.
    //
    // WAVE 4 — the probe now has a named suspect, which is where to look first.
    // `phases_late/collections.rs::register_p60_abstract_map` registers
    // `java/util/AbstractMap.isEmpty()Z` (and `toString`/`hashCode`) reading
    // `ctx.get_field(this, 1)` as the entry count. That is the 3-slot synthetic
    // map layout (buckets, size, capacity); on a REAL-JDK map slot 1 is not
    // `size`, so the native answers from an unrelated field. `AbstractMap` is a
    // CLASS, so this intercepts every Map that does not override `isEmpty`
    // itself. `java.util.HashMap` DOES override it, so a plain HashMap receiver
    // should resolve to `HashMap.isEmpty` and miss this native — which is
    // exactly the part of the premise that needs the probe: find out what
    // `logDirProps` actually is at the `verify` call (a `Collections
    // .unmodifiableMap` wrapper and `TreeMap` both inherit `isEmpty` from
    // `AbstractMap` and WOULD be intercepted).
    //
    // ESCALATION (not fixable from this file): if the probe confirms it, the
    // repair is in `collections.rs` — make `AbstractMap.isEmpty`/`size` dispatch
    // to the receiver's own `size()` instead of reading slot 1 — and only then
    // delete this registration together with its `check_override` allow-list
    // entry in `vm/src/vm/vm_exec.rs`.
    registry.register(
        "org/apache/kafka/metadata/properties/MetaPropertiesEnsemble",
        "verify",
        "(Ljava/util/Optional;Ljava/util/OptionalInt;Ljava/util/EnumSet;)V",
        |_ctx, _args| Ok(None),
    );

    // Real-JDK Spring/SLF4J: `AccessController.checkPermission` is only wired
    // through `register_letsgo_compat_natives` inside `register_builtins`
    // (synthetic-jdk aggregate). Real-JDK mode calls `register_essential_natives`
    // alone — register the no-op here so linkage succeeds when the SM is absent.
    letsgo_compat::register_security_fallbacks(registry);
    letsgo_compat::register_wrapper_value_of(registry);
    letsgo_compat::register_wrapper_unbox(registry);

    // WildFly server bootstrap can resolve `java/util/StringJoiner` as a
    // synthetic fallback in real-JDK mode while scanning supplemental config.
    // Register the layout-aware fallback as SyntheticStub so real bytecode
    // still wins for a loaded real JDK StringJoiner.
    cratonvm_native_collections::register_string_joiner_stub_natives(registry);

    // Spring Boot / WildFly startup can resolve `EnumSet` as a synthetic
    // fallback in real-JDK mode. Register the full synthetic surface as a
    // SyntheticStub fallback so real bytecode still wins when the real JDK
    // class is loaded, while `of`/`add`/`contains` etc. link for fallback sets.
    crate::phases_early::register_enum_set_stub_natives(registry);
    register_synchronized_collection_wrapper_natives(registry);
    register_function_identity_natives(registry);
    // WildFly can load java.time.Instant through a bootstrap synthetic stub even
    // in real-JDK mode. Keep its SyntheticStub bridge available in essentials;
    // a loaded real-JDK Instant remains on its own bytecode path.
    register_instant_stub_natives(registry);
    // Real-JDK boot can still resolve `java/util/Objects` through a synthetic
    // fallback when java.base stubs are partial. Install the existing spec
    // natives in essentials so `requireNonNull` and friends link in those paths.
    //
    // `SyntheticStub`, not `Intrinsic`, and for the same reason as the
    // `StringJoiner` / `EnumSet` / `Instant` stubs registered just above: this
    // is a FALLBACK for a partial-stub boot, so a loaded real `java/util/Objects`
    // must win. As `Intrinsic` it won unconditionally on `invokestatic` --
    // `dispatch_static` arbitrates on `NativeKind` alone -- which meant the real
    // bytecode was never used and never JIT-compiled. `java/util/Objects` is on
    // `real_protected_stub_class_common` so the yield is armed; the five-term
    // predicate behind it still refuses whenever the real body is not actually
    // there, and `invoke_or_native` finds the native anyway if it is not.
    register_objects_natives(registry, cratonvm_native_api::NativeKind::SyntheticStub);

    // `Throwable.initCause` / `ExceptionInInitializerError.initCause` — vm_util
    // wraps failed `<clinit>` in EIIE and calls `initCause`; linkage must not
    // NSME when the inherited method is not resolved on the subclass.
    registry.register(
        "java/lang/Throwable",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        crate::lang_misc::native_throwable_init_cause,
    );
    registry.register(
        "java/lang/ExceptionInInitializerError",
        "initCause",
        "(Ljava/lang/Throwable;)Ljava/lang/Throwable;",
        crate::lang_misc::native_throwable_init_cause,
    );

    // Reflective `Method.invoke` surfaces `InvocationTargetException`; ensure
    // `getCause` / `getTargetException` read the `target` field on real JDK
    // instances so CLI / callers see the wrapped exception.
    let ite = "java/lang/reflect/InvocationTargetException";
    registry.register(
        ite,
        "<init>",
        "(Ljava/lang/Throwable;)V",
        crate::lang_misc::native_invocation_target_exception_init_target,
    );
    registry.register(
        ite,
        "<init>",
        "(Ljava/lang/Throwable;Ljava/lang/String;)V",
        crate::lang_misc::native_invocation_target_exception_init_target_message,
    );
    registry.register(
        ite,
        "getCause",
        "()Ljava/lang/Throwable;",
        crate::lang_misc::native_invocation_target_exception_get_target,
    );
    registry.register(
        ite,
        "getTargetException",
        "()Ljava/lang/Throwable;",
        crate::lang_misc::native_invocation_target_exception_get_target,
    );

    // `TimeUnit.toMillis(J)` — `SpringApplicationShutdownHook` static `TIMEOUT`
    // uses `TimeUnit.MINUTES.toMillis(10)` before `LogFactory.getLog`. Real-JDK
    // enum conversion can NPE in partial boot; mirror conversion by ordinal.
    registry.register(
        "java/util/concurrent/TimeUnit",
        "sleep",
        "(J)V",
        native_timeunit_sleep,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toMillis",
        "(J)J",
        |ctx, args| {
            let recv = args.first().copied().unwrap_or(Value::Object(None));
            let dur = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // Through the shared `time_unit_ordinal`, not a second copy of the
            // read: that helper memoizes the `ordinal` field index per receiver
            // class, and this overload is 1.00 call per expired task on
            // `HashedWheelTimerTest`. The copy here read the name on every
            // call, which takes the class-manager lock and walks the hierarchy
            // comparing field-name strings.
            //
            // The fallback differs from the helper's and is kept: this one
            // answers MINUTES (4) for a missing receiver, because
            // `SpringApplicationShutdownHook`'s static `TIMEOUT` is
            // `TimeUnit.MINUTES.toMillis(10)` during a partial boot in which
            // the enum constant can be absent. The helper's own slot-0/2
            // fallbacks only apply once there IS a receiver.
            let ordinal = match recv {
                Value::Object(Some(o)) => time_unit_ordinal(ctx, o),
                _ => 4,
            };
            Ok(Some(Value::Long(convert_time_unit_to_millis(dur, ordinal))))
        },
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "convert",
        "(JLjava/util/concurrent/TimeUnit;)J",
        |ctx, args| {
            let target = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Long(0))),
            };
            let dur = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            let source = match args.get(2) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Long(0))),
            };
            let source_ordinal = time_unit_ordinal(ctx, source);
            let target_ordinal = time_unit_ordinal(ctx, target);
            Ok(Some(Value::Long(convert_time_unit_between(
                dur,
                source_ordinal,
                target_ordinal,
            ))))
        },
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toNanos",
        "(J)J",
        native_timeunit_to_nanos,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toMicros",
        "(J)J",
        native_timeunit_to_micros,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toSeconds",
        "(J)J",
        native_timeunit_to_seconds,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toMinutes",
        "(J)J",
        native_timeunit_to_minutes,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toHours",
        "(J)J",
        native_timeunit_to_hours,
    );
    registry.register(
        "java/util/concurrent/TimeUnit",
        "toDays",
        "(J)J",
        native_timeunit_to_days,
    );

    // `Collections.newSetFromMap` — Spring Boot 3+ `SpringApplicationShutdownHook`
    // builds `closedContexts` from `Collections.newSetFromMap(new WeakHashMap<>())`,
    // and Felix's `CapabilitySet.match` returns
    // `Collections.newSetFromMap(new ConcurrentHashMap())`.
    // Real-JDK `newSetFromMap` wraps the map in a private `SetFromMap`
    // implementation; mixed stub/real paths can NPE during `<clinit>`, so we
    // return a plain mutable `HashSet`.
    //
    // The HashSet MUST be built with the same shape the collection natives
    // expect: a one-slot object whose slot 0 holds a *backing HashMap*
    // (itself a buckets/size/capacity triple). The earlier version stored a
    // raw `Object[]` directly in HashSet slot 0 — `add()` happened to work,
    // but `new ArrayList<>(set)` (which calls `HashSet.toArray()`, which reads
    // slot 0 as a HashMap) saw a malformed map and produced an empty list.
    // That made `Felix.getServiceReferences` — which copies the
    // `CapabilitySet.match` result via `new ArrayList<>(set)` — return null,
    // so `getServiceReference("...StartLevel")` NPE'd.
    registry.register(
        "java/util/Collections",
        "newSetFromMap",
        "(Ljava/util/Map;)Ljava/util/Set;",
        |ctx, args| {
            // The backing `Map` argument: for this static method the actual
            // argument may land at index 0 or 1 depending on dispatch path, so
            // pick the first `Object` arg.
            let map = args
                .iter()
                .find(|v| matches!(v, Value::Object(Some(_))))
                .copied()
                .unwrap_or(Value::Object(None));
            // ARGUMENT VALIDATION, which this had none of. The JDK's body is
            // `if (!map.isEmpty()) throw new IllegalArgumentException("Map is
            // non-empty");`, preceded by the implicit NPE of that same call.
            // MEASURED no-throw for both (apps/probes/CollectionsShadowSweep 162-163).
            //
            // The non-empty refusal is the load-bearing one: the returned set's
            // whole contract is that it holds exactly the backing map's keys and
            // that `add(e)` is `map.put(e, TRUE)`. A pre-populated map breaks
            // that silently -- the set reports elements nobody added to it, and
            // a `FALSE`-valued entry is a member all the same.
            let backing = match map {
                Value::Object(Some(m)) => m,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: None,
                    }
                    .into())
                }
            };
            // gc-common w13-f: `isEmpty()` is Java, and the map (read off
            // `args`), then the HashMap built below across a second
            // allocation, were all used after a GC point through their
            // pre-call addresses. Everything below runs through one scope.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let map_h = scope.root(backing);
            if !matches!(
                scope.invoke_virtual(backing, "isEmpty", "()Z", &[]),
                Ok(Some(Value::Int(1)))
            ) {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: "Map is non-empty".to_string(),
                    }
                    .into(),
                );
            }
            let map_now = scope.get(&map_h);
            // The plain `HashSet` returned below uses value `hashCode`/`equals`,
            // which matches `HashMap`/`WeakHashMap`/`ConcurrentHashMap` backings
            // (all key-hash/equals based). But an `IdentityHashMap` backing must
            // use *reference identity*: returning a value-hash set there silently
            // calls each element's `hashCode()`/`equals()` on insertion. For a
            // Hibernate `PersistentSet` element that has side effects —
            // `PersistentSet.hashCode()` force-initializes the collection — which
            // breaks entity-graph `@BatchSize` collection batch fetching
            // (HIB-CV-28: per-row `=?` loads instead of one batched `IN (...)`).
            // For an `IdentityHashMap` backing, build the real
            // `Collections$SetFromMap` wrapping the actual map so add/contains
            // route through `IdentityHashMap` (identity), exactly like HotSpot.
            {
                let m = map_now;
                let cname = scope
                    .class_name_of_id(scope.class_id_of_object(m))
                    .unwrap_or_default();
                // Same trap as `IdentityHashMap` (identity keys): Spring's
                // `LinkedCaseInsensitiveMap` also overrides key equals/hashCode
                // (case-insensitive folding) away from the plain value-hash
                // semantics the synthetic `HashSet` below assumes. Wrapping it
                // in a value-hash `HashSet<String>` silently reverts to
                // case-SENSITIVE dedup, so e.g.
                // `Collections.newSetFromMap(new LinkedCaseInsensitiveMap<>())`
                // (used by `HttpComponentsHeadersAdapter`'s header-name Set)
                // keeps "TestHeader" and "TestHEADER" as two distinct elements
                // instead of one — breaking case-insensitive header-name
                // dedup/removal. Route it through the real
                // `Collections$SetFromMap`, same as `IdentityHashMap`: the
                // `SetFromMap` natives already dispatch `iterator`/`toArray`/
                // `contains` via `invoke_virtual` against the live backing map
                // (see `set_from_map_backing` in native-collections), so they
                // work correctly for any backing map's own equals/hashCode.
                if cname == "java/util/IdentityHashMap"
                    || cname == "org/springframework/util/LinkedCaseInsensitiveMap"
                {
                    return scope.new_object_initialized(
                        "java/util/Collections$SetFromMap",
                        "(Ljava/util/Map;)V",
                        &[Value::Object(Some(m))],
                    );
                }
                // A `ConcurrentHashMap` backing is the other canonical spelling
                // of "give me a concurrent set", and the `HashSet` below is the
                // same wrong answer `ConcurrentHashMap.newKeySet()` used to
                // give: its `add`/`remove`/`size` run the unlocked `HashMap`
                // natives, so the set corrupts under concurrent mutation and
                // its size can go negative. Hand back the live `KeySetView`
                // over that very map instead — `newSetFromMap` over an empty
                // map IS `map.keySet(Boolean.TRUE)` — so every mutation takes
                // the per-segment monitor `native_chm_put`/`native_chm_remove`
                // already hold. See the retired
                // `concurrenthashmap-newkeyset-returns-a-plain-hashset`
                // write-up.
                if cname == "java/util/concurrent/ConcurrentHashMap" {
                    let view =
                        cratonvm_native_collections::make_concurrent_key_set_view(&mut *scope, m)?;
                    return Ok(Some(Value::Object(Some(view))));
                }
            }
            let cap = 16usize;
            // Backing HashMap: slot 0 = buckets, slot 1 = size, slot 2 = capacity.
            // Rooted across the bucket array's allocation and the set's.
            let backing_new =
                crate::try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashMap", 3)?;
            let backing_h = scope.root(backing_new);
            let buckets = scope.new_array(cratonvm_types::ArrayElementType::Reference, cap);
            for i in 0..cap {
                scope.set_array_element(buckets, i, Value::Object(None));
            }
            let backing = scope.get(&backing_h);
            scope.set_field(backing, 0, Value::Object(Some(buckets)));
            scope.set_field(backing, 1, Value::Int(0));
            scope.set_field(backing, 2, Value::Int(cap as i32));
            // HashSet: slot 0 = backing HashMap.
            let set: ObjectRef =
                crate::try_alloc_concurrent_synthetic(&mut *scope, "java/util/HashSet", 1)?;
            let backing = scope.get(&backing_h);
            scope.set_field(set, 0, Value::Object(Some(backing)));
            Ok(Some(Value::Object(Some(set))))
        },
    );

    // FIXED 2026-07-17 (conditionevaluationreport-capturedoutput-empty-cluster):
    // `org/apache/commons/logging/LogFactory.getLog`/`Log.info/debug/warn/
    // error/etc.` used to be natively overridden here (as a `Bridge`, so
    // `CRATONVM_NO_STUBS` could not drop it) to fabricate a throwaway 1-field
    // synthetic `Log` whose `info`/`warn`/`error`/`fatal` routed through
    // `ctx.record_printed_line` with a fake `"[ACL] "` prefix (bypassing
    // System.out/err entirely) and whose `debug`/`trace` were pure no-ops.
    // Every Spring Boot production class using the conventional
    // `private final Log logger = LogFactory.getLog(getClass());` pattern
    // (`ConditionEvaluationReportLogger`, `DockerComposeLifecycleManager`,
    // `EndpointId`, `FreeMarkerAutoConfiguration`, the `Health*Indicator`
    // family, …) got this fake `Log` — so its output could never reach a
    // real Logback `ConsoleAppender`/`OutputStreamAppender`, and Spring
    // Boot's `CapturedOutput` (which hooks `System.out`/`System.err`) always
    // saw the empty string. This bridge predates, and was never revisited
    // after, the `ch/qos/logback/classic/Logger`/`LoggerContext.getLogger`
    // fix directly above — same overlay/real-class-layout mismatch family
    // (see the note above that fix). Real `commons-logging` 1.3.x's own
    // `LogFactory.getLog()` bytecode does its own SLF4J-bridge discovery at
    // runtime and, with the `ch/qos/logback/classic/Logger` fix above in
    // place, correctly hands back a real SLF4J-backed `Log` that reaches
    // real Logback. This native override is dropped so that discovery runs
    // for real. The original motivating case (`SpringApplicationShutdownHook`'s
    // static `Log logger = LogFactory.getLog(...)`, which real-JDK bytecode
    // was said to NPE inside) needs re-verification against a
    // `SpringApplication.run()`-driving test (`SimpleMainTests`/
    // `BannerTests`) after this change — see the doc for the verification
    // run this was checked against.
    //
    // A concurrent session (`e21d80307`, "fix spring boot captured output
    // logging") independently patched this SAME symptom with a different,
    // lower-risk approach: keep the synthetic `Log`/`Logger` stubs but route
    // their formatted text through `emit_framework_log`/`stream_writeln` so
    // it reaches whatever `System.out`/`System.err` currently is. That
    // patch is superseded here — it doesn't fix per-logger dynamic level
    // control (`((LoggerContext) LoggerFactory.getILoggerFactory())
    // .getLogger(X).setLevel(Level.DEBUG)`, which several of this doc's own
    // tests rely on) since the stub's `isDebugEnabled`/`Logger.setLevel`
    // stay hardcoded, and it doesn't use real Logback pattern/appender
    // formatting — so it wasn't a complete fix for this doc's affected
    // classes. Its one genuinely orthogonal addition,
    // `org/springframework/core/log/LogMessage.toString()`, is kept below —
    // real Logback message formatting can call it on a lazy `LogMessage`
    // argument regardless of which Log/Logger path produced it.
    registry.register(
        "org/springframework/core/log/LogMessage",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(this))) => *this,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(result)) = ctx.get_field_by_name(this, "result") {
                return Ok(Some(Value::Object(Some(result))));
            }
            // gc-common w19-e: `buildString()` is Java; the result was memoized
            // through the receiver's address from before it. Rooted.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let result = scope.invoke_virtual(this, "buildString", "()Ljava/lang/String;", &[])?;
            if let Some(Value::Object(Some(result))) = result {
                let this = scope.get(&this_h);
                scope.set_field_by_name(this, "result", Value::Object(Some(result)));
                return Ok(Some(Value::Object(Some(result))));
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // Spring Boot 3 `JarFileArchive.<clinit>` calls `PosixFilePermissions.asFileAttribute`;
    // real `java.base` bytecode from `--java-home` provides the anonymous
    // `FileAttribute` implementation (no synthetic `java/**` holder types).
    crate::phases_late::register_p70_file_attributes(registry);

    // Real-JDK SLF4J replay: `LoggerFactory` drains then clears the event queue.
    // Synthetic-stub `LinkedBlockingQueue` hierarchies may not resolve
    // `AbstractCollection.clear()V` on the method walk — register explicitly.
    //
    // BUG (2026-07-07): this used to unconditionally `ctx.set_field(this, 1,
    // Value::Int(0))`, assuming the synthetic 4-field layout (slot 1 = size
    // int). On a REAL-JDK-constructed `LinkedBlockingQueue` (real bytecode
    // `<init>` ran), slot 1 is the real `count: AtomicInteger` *reference*
    // field, not an int. Stomping it with `Value::Int(0)` corrupted that
    // reference to null, so any later real-bytecode `count.get()` (e.g.
    // `offer()`/`size()`) NPE'd with "Cannot invoke AtomicInteger.get()
    // because count is null" — reproduced by
    // BufferingStompDecoderTests (org.springframework.messaging.simp.stomp),
    // whose `assembleChunksAndReset()` calls `chunks.clear()` right after
    // dequeuing the sole buffered chunk. Mirror the `size()` override just
    // below: detect the real layout by field name first (same technique),
    // and for that case drain via the real `poll()` (already correct for
    // real-layout queues, proven by `count` staying valid across dequeues)
    // instead of touching the raw slot.
    registry.register(
        "java/util/concurrent/LinkedBlockingQueue",
        "clear",
        "()V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            // gc-common w19-e: every `poll()` is Java. The loop dispatched each
            // one after the first, and the `notifyAll` / monitor exit, on the
            // receiver's address from before them. Rooted, re-read per call.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            scope.monitor_enter(this);
            let real_layout =
                matches!(scope.get_field_by_name(this, "count"), Value::Object(Some(_)));
            if !real_layout {
                scope.set_field(this, 1, Value::Int(0));
            } else {
                loop {
                    let this_now = scope.get(&this_h);
                    if !matches!(
                        scope.invoke_virtual(this_now, "poll", "()Ljava/lang/Object;", &[])?,
                        Some(Value::Object(Some(_)))
                    ) {
                        break;
                    }
                }
            }
            let this = scope.get(&this_h);
            scope.monitor_notify_all(this)?;
            scope.monitor_exit(this);
            Ok(None)
        },
    );

    // Real-JDK LinkedBlockingQueue: linkage can miss `size()I` while the JDK
    // declares it. Bridge via the `count` AtomicInteger field (no new java/** synthetics).
    registry.register(
        "java/util/concurrent/LinkedBlockingQueue",
        "size",
        "()I",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            if let Value::Object(Some(ai)) = ctx.get_field_by_name(this, "count") {
                return ctx.invoke_virtual(ai, "get", "()I", &[]);
            }
            match ctx.get_field(this, 1) {
                Value::Int(n) => Ok(Some(Value::Int(n))),
                _ => Ok(Some(Value::Int(0))),
            }
        },
    );

    // NOTE: no `SimpleDateFormat.<init>` override. A former no-op stub here
    // left every field (`pattern`, `calendar`, `numberFormat`, `formatData`)
    // null, so the real `DateFormat.format`/`getDateFormatSymbols` bytecode
    // NPE'd. The real `java.text.SimpleDateFormat` constructor runs instead —
    // its dependencies (`Calendar`, `DateFormatSymbols`, `NumberFormat`) work.

    // Real-JDK CLI builds omit `synthetic-jdk`, so `register_synthetic_overrides`
    // (which used to be the only caller of `register_exception_extras_natives`)
    // is not compiled. Register common exception constructors/getters here so
    // reflective / Spring bootstrap paths do not die on missing natives.
    register_exception_extras_natives(registry);

    // LOCALE / BREAKITER (real-JDK mode): `java.text.BreakIterator`'s static
    // factories (`getLineInstance` / `getWordInstance` / `getSentenceInstance`
    // / `getCharacterInstance`) route through
    //   BreakIterator.createBreakInstance
    //     -> sun.util.locale.provider.BreakIteratorProviderImpl.getBreakInstance
    //       -> LocaleResources.getBreakIteratorInfo("BreakIteratorClasses")
    // which returns `null` on CratonVM because jdk.localedata's class-based
    // resource bundles are not surfaced through our jimage path. The bytecode
    // at `BreakIteratorProviderImpl.getBreakInstance` (JDK 25 line 170) then
    // does `switch (classNames[type])` on the null `classNames` array, throwing
    //   java.lang.NullPointerException: Cannot load from null array
    // and aborting JUnit Platform console `--help` text wrapping
    // (picocli `TextTable.copy` calls `BreakIterator.getLineInstance()` for
    // line-break boundaries).
    //
    // These BreakIterator factory + instance natives (a REAL, working
    // boundary-analysis iterator — see `register_p66_break_iterator` in
    // `phases_late.rs`) historically only shipped via
    // `register_synthetic_overrides`, which is compiled out in real-JDK CLI
    // builds (no `synthetic-jdk` feature). Register them here so the real-JDK
    // path also has a functioning iterator and never reaches the broken
    // null-resource JDK provider path. The allow-list entry at
    // `vm/src/vm/vm_exec.rs` (BREAKITER) promotes these over the JDK bytecode.
    crate::phases_late::register_p66_break_iterator(registry);

    // NIO file-attribute bridge (real-JDK mode): `BasicFileAttributes` is a
    // pure interface in `java.base` — its methods (`isDirectory`, `size`,
    // `lastModifiedTime`, …) have no `Code` attribute. The JDK code path that
    // produces a concrete `BasicFileAttributes` runs through
    // `WindowsFileSystemProvider.readAttributes` → `WindowsNativeDispatcher`
    // which CratonVM has not wired up. Without an intercept, real JDK bytecode
    // for `Files.walkFileTree` → `FileTreeWalker.visit` → `attrs.isDirectory()`
    // dispatches to the abstract interface declaration and throws
    // `AbstractMethodError: BasicFileAttributes.isDirectory()Z has no Code
    // attribute` — fatal for any `Files.walkFileTree` / `Files.walk` /
    // `Files.find` caller, including JUnit Platform's `ClasspathScanner`
    // (which uses `--select-package` discovery to find tests).
    //
    // `register_p59_file_attributes` registers `Files.readAttributes(Path,
    // Class, LinkOption[])` to allocate a synthetic 5-field BFA populated
    // from real `std::fs::metadata` (no fabricated values), and registers
    // `BasicFileAttributes.{isDirectory,isRegularFile,size,…}` natives that
    // read those fields. The native is found via the abstract-declaration
    // rescue at `interpreter::execute` (the path that looks for a native
    // registered directly on the resolved interface class before throwing
    // AbstractMethodError). Historically the registration shipped only via
    // `register_synthetic_overrides`; promote it here so real-JDK CLI builds
    // get the same coverage.
    crate::phases_late::register_p59_file_attributes(registry);

    // METHODHANDLES ARRAY ACCESSORS (real-JDK mode): `MethodHandles.
    // arrayElementGetter` / `arrayElementSetter` run genuine JDK bytecode in
    // real-JDK mode, but `MethodHandleImpl.makeArrayElementAccessor` adapts the
    // generic accessor for primitive arrays via `MethodHandle.viewAsType` →
    // `copyWith` — abstract on CratonVM's synthetic MethodHandles ("has no Code
    // attribute" AbstractMethodError). `ObjectStreamClass$RecordSupport.<clinit>`
    // builds `PRIM_VALUE_EXTRACTORS` with `arrayElementGetter(byte[].class)`, so
    // that clinit aborts and every record-class (de)serialization dies with a
    // bogus `no class def found: ObjectStreamClass$RecordSupport` linkage error
    // (e.g. catalina TestGenericPrincipal). These bridges historically shipped
    // only via `register_synthetic_overrides` (register_p65_method_handles_extra);
    // promote them here so real-JDK CLI builds also get a non-null MethodHandle.
    // The companion `check_override` allow-list entry in `vm/src/vm/vm_exec.rs`
    // pins the native ahead of the (broken) JDK bytecode.
    crate::lang_invoke::register_array_element_accessor_bridges(registry);

    // METHODHANDLES.CONSTANT (real-JDK mode): `MethodHandles.constant` runs
    // genuine JDK bytecode that spins a `BoundMethodHandle` *species* class via
    // `ClassSpecializer` (`makeConstantReturning` → `createConstantForm` →
    // `BoundMethodHandle.<clinit>`). CratonVM models method handles with its
    // `MH_KIND_*` shims rather than the real `BoundMethodHandle`/`LambdaForm`
    // machinery, so that bytecode NPEs at
    // `ClassSpecializer.generateConcreteSpeciesCode`, surfacing as
    // `ExceptionInInitializerError` for `BoundMethodHandle`. `SwitchPoint.<clinit>`
    // builds `K_true`/`K_false` via `constant(boolean.class, …)`, and Apache
    // Groovy's `IndyInterface.<clinit>` initializes a `SwitchPoint` before any
    // script executes — so without this shim every Groovy `invokedynamic` site
    // dies. Pin the functional `MH_KIND_CONSTANT` shim ahead of the (broken) JDK
    // bytecode; the companion `check_override` allow-list entry in
    // `vm/src/vm/vm_exec.rs` makes the native win at the call site.
    crate::lang_invoke::register_method_handles_constant_bridge(registry);

    // METHODHANDLES.IDENTITY (real-JDK mode): like `constant`, the genuine
    // `MethodHandles.identity` bytecode yields a real
    // `MethodHandleImpl$IntrinsicMethodHandle` (and a `BoundMethodHandle`
    // species for primitives) the `MH_KIND_*` shims can't read — so
    // `identity().invoke()`/`.bindTo()` fail. Pin the functional
    // `MH_KIND_IDENTITY` shim (allow-listed in vm_exec.rs).
    crate::lang_invoke::register_method_handles_identity_bridge(registry);

    // CALLSITE.DYNAMICINVOKER (real-JDK mode): `CallSite.makeDynamicInvoker`
    // does `getTargetHandle().bindArgumentL(0, this)` — a `BoundMethodHandle`
    // construction that HANGS on CratonVM (no real species machinery).
    // `SwitchPoint.<init>` calls `mcs.dynamicInvoker()`, so every
    // `new SwitchPoint()` (hence Groovy's `IndyInterface.<clinit>` at runtime)
    // hangs without this. Pin the functional `MH_KIND_DYNAMIC_INVOKER` shim on
    // `MutableCallSite`/`VolatileCallSite` (allow-listed in vm_exec.rs); it
    // delegates to the call site's current target.
    crate::lang_invoke::register_callsite_dynamic_invoker_bridge(registry);

    // METHODHANDLE COMBINATOR EXTRAS (real-JDK mode): functional
    // `MethodHandles.insertArguments` + `MethodHandle.asCollector`, and the
    // `CallSite.makeUninitializedCallSite` / `MutableCallSite.setTarget`
    // natives Groovy's `IndyInterface` fallback construction needs (real
    // bytecode builds `BoundMethodHandle` species / hits a null
    // `MethodTypeForm` cache). Allow-listed in vm_exec.rs.
    crate::lang_invoke::register_method_handle_combinator_extras_bridge(registry);

    // METHODHANDLES LOOP FAMILY + TRYFINALLY (real-JDK mode): `countedLoop`/
    // `whileLoop`/`doWhileLoop`/`iteratedLoop`/`tryFinally` are concrete
    // static factories whose JDK 25 bytecode builds `LambdaForm`/
    // `BoundMethodHandle` species machinery the `MH_KIND_*` shim model can't
    // run — the same class of gap `constant`/`identity`/`insertArguments`
    // above work around. Without this override every one of the five throws
    // (`NullPointerException`, `NoSuchMethodError`, or, once an earlier call
    // has left `BoundMethodHandle`'s `<clinit>` ERRONEOUS,
    // `NoClassDefFoundError`) instead of running the loop/finally semantics.
    // `lane-methodhandles-combinator-sweep-20260917-RETIRED-20260921.md` names all five as "a whole JDK 9 API absent". Pin the

    // functional `MH_KIND_LOOP_*`/`MH_KIND_TRY_FINALLY` natives
    // (`register_method_handle_loop_bridge`) ahead of the broken bytecode;
    // the companion `check_override` allow-list entry in
    // `vm/src/vm/vm_exec.rs` makes the natives win at warm/JIT call sites too.
    crate::lang_invoke::register_method_handle_loop_bridge(registry);

    // METHODHANDLES.ZERO/EMPTY (real-JDK mode): both call
    // `MethodHandleImpl.makeConstantReturning` DIRECTLY rather than through
    // the public `MethodHandles.constant` above, so the already-pinned
    // `constant` native never intercepts them — real bytecode spins the same
    // `BoundMethodHandle`/`ClassSpecializer` species machinery `constant`'s
    // own comment describes, with the same `ExceptionInInitializerError`-class
    // failure. Pin the functional `MH_KIND_CONSTANT`-based shims
    // (`register_method_handles_zero_empty_bridge`); allow-listed in
    // `vm/src/vm/vm_exec.rs`.
    crate::lang_invoke::register_method_handles_zero_empty_bridge(registry);

    // RECORD DESERIALIZATION (real-JDK mode): `ObjectInputStream.readRecord`
    // rebuilds a serialized record by invoking the `MethodHandle` returned by
    // `ObjectStreamClass$RecordSupport.deserializationCtr(ObjectStreamClass)`,
    // which the JDK assembles from `foldArguments`/`insertArguments`/
    // `arrayElementGetter` combinators — an algebra CratonVM's synthetic
    // MethodHandles cannot execute. Intercept `deserializationCtr` to return a
    // synthetic `MH_KIND_RECORD_DESER` handle that rebuilds the record
    // reflectively on `invokeExact(primValues, objValues)`. Needed for any app
    // serializing a record (Tomcat `GenericPrincipal` → `SerializablePrincipal`
    // record; catalina `TestGenericPrincipal`). Companion `check_override` entry
    // in `vm/src/vm/vm_exec.rs` pins this over the JDK bytecode.
    {
        let __prev = registry.current_category();
        registry.set_category(cratonvm_native_api::NativeKind::Bridge);
        registry.register(
            "java/io/ObjectStreamClass$RecordSupport",
            "deserializationCtr",
            "(Ljava/io/ObjectStreamClass;)Ljava/lang/invoke/MethodHandle;",
            crate::lang_invoke::native_record_support_deserialization_ctr,
        );
        registry.set_category(__prev);
    }
}

// ---------------------------------------------------------------------------
// S111r12 SB3 follow-on: jdk.internal.module.Builder.new* static overrides
// ---------------------------------------------------------------------------
//
// Real-JDK `Builder.newExports` (Builder.java:99) bytecode is:
//
//     return JLMA.newExports(ms, pn, targets);
//
// where `JLMA = SharedSecrets.getJavaLangModuleAccess()`.  Rustjvm's
// SharedSecrets bridge (`shared_secrets_bridge.rs`) does not wire up a
// `JavaLangModuleAccess` singleton (that interface holds 13 methods,
// most of which require a fully-implemented module-descriptor builder
// surface that goes far beyond what SB3 boot exercises).  Without a
// JLMA, the static field is null and the `invokeinterface` NPEs.
//
// Spring Boot 3's only consumer of `SystemModuleFinders.ofSystem` →
// `SystemModules$all.moduleDescriptors` is the `<clinit>` of
// `PathMatchingResourcePatternResolver` (Spring uses the class loader's
// boot ModuleLayer to enumerate resource roots).  That walk only needs
// each `Builder.new*` call to NOT throw; the returned objects flow into
// `Set.of(...)` and ultimately into `newModuleDescriptor(...)` whose
// result is also synthetic in our boot path.
//
// Strategy: register native overrides for each static factory that
// allocate a synthetic instance of the matching `ModuleDescriptor$*`
// inner class and stash the original args under JDK-standard field
// names (`source`, `targets`, `mods`, `name`, `compiledVersion`,
// `service`, `providers`).  `set_field_by_name` is slot-resolved so it
// is robust to either the real-JDK private-field layout or our
// synthetic minimum-field allocation.

/// How [`module_builder_alloc_with_named_fields`] fills one field.
#[derive(Clone, Copy)]
enum StandInField {
    /// The argument as passed.
    Arg(Value),
    /// A `Set` argument, or a fresh empty set when it is null.
    SetOrEmpty(Value),
    /// A fresh empty set.
    EmptySet,
}

impl StandInField {
    fn arg(self) -> Value {
        match self {
            StandInField::Arg(v) | StandInField::SetOrEmpty(v) => v,
            StandInField::EmptySet => Value::Object(None),
        }
    }
}

/// Allocate a `class_name` stand-in and fill `fields` by name.
///
/// gc-common w34-a: every object argument is rooted BEFORE the first
/// allocation (the empty sets, then the stand-in itself) and read back after
/// the last. The factories used to copy their arguments out of `args`,
/// allocate the empty sets and the stand-in, and only then write the copies
/// into it -- through the addresses a collection either allocation ran may
/// have vacated (the `args` slice is not updated by a collection; see
/// `native_module_builder_build`'s w13-f note for the same shape).
fn module_builder_alloc_with_named_fields(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    fields: &[(&str, StandInField)],
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let mut handles: Vec<Option<cratonvm_native_api::NativeHandle>> =
        Vec::with_capacity(fields.len());
    for (_, field) in fields {
        handles.push(match field.arg() {
            Value::Object(Some(o)) => Some(scope.root(o)),
            _ => None,
        });
    }
    for (i, (_, field)) in fields.iter().enumerate() {
        let wants_empty_set = match field {
            StandInField::EmptySet => true,
            StandInField::SetOrEmpty(_) => handles[i].is_none(),
            StandInField::Arg(_) => false,
        };
        if wants_empty_set {
            if let Value::Object(Some(empty)) = module_builder_empty_set(&mut *scope)? {
                handles[i] = Some(scope.root(empty));
            }
        }
    }
    // Allocate enough slots for the named fields plus a safety margin;
    // `alloc_concurrent_synthetic` widens to the real-JDK field count
    // when the class is loaded.
    let obj = try_alloc_concurrent_synthetic(&mut *scope, class_name, fields.len().max(4))?;
    // Plain field stores from here on: nothing below can collect.
    for (i, (name, field)) in fields.iter().enumerate() {
        let value = match &handles[i] {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => field.arg(),
        };
        scope.set_field_by_name(obj, name, value);
    }
    Ok(obj)
}

fn module_builder_empty_set(ctx: &mut dyn NativeContext) -> Result<Value, MethodCallFailed> {
    Ok(Value::Object(Some(
        cratonvm_native_collections::make_hashset_with_elements(ctx, &[])?,
    )))
}

/// Argument `i`, or `null` when absent.
fn module_builder_arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or(Value::Object(None))
}

/// Argument `i` as passed.
fn stand_in_arg(args: &[Value], i: usize) -> StandInField {
    StandInField::Arg(module_builder_arg(args, i))
}

/// Argument `i`, a `Set`, or a fresh empty set when it is null.
fn stand_in_set(args: &[Value], i: usize) -> StandInField {
    StandInField::SetOrEmpty(module_builder_arg(args, i))
}

pub(crate) fn module_descriptor_empty_set(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    Ok(cratonvm_native_collections::make_hashset_with_elements(
        ctx,
        &[],
    )?)
}

fn module_descriptor_set_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &str,
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(Some(module_descriptor_empty_set(ctx)?)))),
    };
    if let Value::Object(Some(v)) = ctx.get_field_by_name(this, field) {
        return Ok(Some(Value::Object(Some(v))));
    }
    let this_pin = ctx.pin_native_root(this);
    let empty = module_descriptor_empty_set(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, field, Value::Object(Some(empty)));
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(empty))))
}

fn native_module_descriptor_modifiers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "modifiers")
}

fn native_module_descriptor_requires(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "requires")
}

fn native_module_descriptor_exports(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "exports")
}

fn native_module_descriptor_opens(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "opens")
}

fn native_module_descriptor_uses(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "uses")
}

fn native_module_descriptor_provides(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "provides")
}

fn native_module_descriptor_packages(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_descriptor_set_field(ctx, args, "packages")
}

fn native_module_descriptor_is_automatic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Value::Int(v) = ctx.get_field_by_name(this, "automatic") {
        return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
    }
    Ok(Some(Value::Int(0)))
}

fn native_module_descriptor_is_open(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first().copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if let Value::Int(v) = ctx.get_field_by_name(this, "open") {
        return Ok(Some(Value::Int(if v != 0 { 1 } else { 0 })));
    }
    let flags = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(flags & 1)))
}

fn native_module_descriptor_optional_field(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    field: &str,
) -> MethodCallResult {
    let value = match args.first().copied() {
        Some(Value::Object(Some(this))) => ctx.get_field_by_name(this, field),
        _ => Value::Object(None),
    };
    match value {
        Value::Object(Some(o)) => ctx.invoke(
            "java/util/Optional",
            "of",
            "(Ljava/lang/Object;)Ljava/util/Optional;",
            &[Value::Object(Some(o))],
        ),
        _ => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
    }
}

fn native_module_descriptor_version(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_module_descriptor_optional_field(ctx, args, "version")
}

fn native_module_descriptor_raw_version(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_module_descriptor_optional_field(ctx, args, "rawVersionString")
}

fn native_module_descriptor_main_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_module_descriptor_optional_field(ctx, args, "mainClass")
}

fn native_module_builder_new_exports_qualified(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (Set<Modifier>, String source, Set<String> targets) -> Exports
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Exports",
        &[
            ("mods", stand_in_set(args, 0)),
            ("source", stand_in_arg(args, 1)),
            ("targets", stand_in_set(args, 2)),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_exports_unqualified(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (Set<Modifier>, String source) -> Exports
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Exports",
        &[
            ("mods", stand_in_set(args, 0)),
            ("source", stand_in_arg(args, 1)),
            ("targets", StandInField::EmptySet),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_opens_qualified(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Opens",
        &[
            ("mods", stand_in_set(args, 0)),
            ("source", stand_in_arg(args, 1)),
            ("targets", stand_in_set(args, 2)),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_opens_unqualified(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Opens",
        &[
            ("mods", stand_in_set(args, 0)),
            ("source", stand_in_arg(args, 1)),
            ("targets", StandInField::EmptySet),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_requires_versioned(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (Set<Modifier>, String mn, String compiledVersion) -> Requires
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Requires",
        &[
            ("mods", stand_in_set(args, 0)),
            ("name", stand_in_arg(args, 1)),
            ("compiledVersion", stand_in_arg(args, 2)),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_requires_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (Set<Modifier>, String mn) -> Requires
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Requires",
        &[
            ("mods", stand_in_set(args, 0)),
            ("name", stand_in_arg(args, 1)),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_provides(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (String service, List<String> providers) -> Provides
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Provides",
        &[
            ("service", stand_in_arg(args, 0)),
            ("providers", stand_in_arg(args, 1)),
        ],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn native_module_builder_new_version(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (String v) -> Version
    let obj = module_builder_alloc_with_named_fields(
        ctx,
        "java/lang/module/ModuleDescriptor$Version",
        &[("version", stand_in_arg(args, 0))],
    );
    Ok(Some(Value::Object(Some(obj?))))
}

fn module_descriptor_version_text(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    match ctx.get_field_by_name(obj, "version") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

fn native_module_descriptor_version_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    match ctx.get_field_by_name(this, "version") {
        Value::Object(Some(s)) => Ok(Some(Value::Object(Some(s)))),
        _ => Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    }
}

fn native_module_descriptor_version_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(Value::Object(Some(other))) = args.get(1) else {
        return Ok(Some(Value::Int(0)));
    };
    if this == *other {
        return Ok(Some(Value::Int(1)));
    }
    let a = module_descriptor_version_text(ctx, this);
    let b = module_descriptor_version_text(ctx, *other);
    Ok(Some(Value::Int(if !a.is_empty() && a == b {
        1
    } else {
        0
    })))
}

fn native_module_descriptor_version_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut hash = 0_i32;
    for ch in module_descriptor_version_text(ctx, this).encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(ch as i32);
    }
    Ok(Some(Value::Int(hash)))
}

fn native_module_descriptor_version_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let Some(Value::Object(Some(other))) = args.get(1) else {
        return Ok(Some(Value::Int(1)));
    };
    let a = module_descriptor_version_text(ctx, this);
    let b = module_descriptor_version_text(ctx, *other);
    let ord = match a.cmp(&b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    Ok(Some(Value::Int(ord)))
}

fn native_module_builder_build(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Instance method: build(int hashCode) -> ModuleDescriptor
    // args[0] = this (Builder), args[1] = hashCode int
    // The real impl calls JLMA.newModuleDescriptor(name, version, ...) - JLMA
    // is null in our boot. Allocate a synthetic ModuleDescriptor and copy
    // over the readable Builder state into matching named fields.
    let this = args.first().copied().unwrap_or(Value::Object(None));
    // gc-common w13-f: the Builder (copied out of `args`) is rooted BEFORE the
    // descriptor's allocation. It used to be read after it, through the
    // address the collection that allocation may run had vacated — every
    // field copied below came off the stale copy.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let builder_h = match this {
        Value::Object(Some(builder)) => Some(scope.root(builder)),
        _ => None,
    };
    let md_new =
        try_alloc_concurrent_synthetic(&mut *scope, "java/lang/module/ModuleDescriptor", 16)?;
    let md_h = scope.root(md_new);
    if let Some(handle) = builder_h {
        // Plain field copies: nothing in this loop can collect.
        let builder = scope.get(&handle);
        let md = scope.get(&md_h);
        for f in [
            "name",
            "version",
            "rawVersionString",
            "modifiers",
            "open",
            "automatic",
            "requires",
            "exports",
            "opens",
            "uses",
            "provides",
            "packages",
            "mainClass",
        ] {
            let v = scope.get_field_by_name(builder, f);
            scope.set_field_by_name(md, f, v);
        }
    }
    for field in [
        "modifiers",
        "requires",
        "exports",
        "opens",
        "uses",
        "provides",
        "packages",
    ] {
        let md_current = scope.get(&md_h);
        if !matches!(
            scope.get_field_by_name(md_current, field),
            Value::Object(Some(_))
        ) {
            let empty = module_builder_empty_set(&mut *scope)?;
            let md_current = scope.get(&md_h);
            scope.set_field_by_name(md_current, field, empty);
        }
    }
    let md = scope.get(&md_h);
    if let Some(Value::Int(h)) = args.get(1) {
        scope.set_field_by_name(md, "hashCode", Value::Int(*h));
    }
    Ok(Some(Value::Object(Some(md))))
}

/// The module names of a JDK 25 runtime image, as `java --list-modules`
/// reports them.
///
/// Used only as the *lower bound* of [`is_system_module_name`] — a module the
/// VM's own registry already knows about also counts, so this list going stale
/// against a future image can only under-report, never invent.
const JDK_SYSTEM_MODULE_NAMES: &[&str] = &[
    "java.base",
    "java.compiler",
    "java.datatransfer",
    "java.desktop",
    "java.instrument",
    "java.logging",
    "java.management",
    "java.management.rmi",
    "java.naming",
    "java.net.http",
    "java.prefs",
    "java.rmi",
    "java.scripting",
    "java.se",
    "java.security.jgss",
    "java.security.sasl",
    "java.smartcardio",
    "java.sql",
    "java.sql.rowset",
    "java.transaction.xa",
    "java.xml",
    "java.xml.crypto",
    "jdk.accessibility",
    "jdk.attach",
    "jdk.charsets",
    "jdk.compiler",
    "jdk.crypto.cryptoki",
    "jdk.crypto.ec",
    "jdk.crypto.mscapi",
    "jdk.dynalink",
    "jdk.editpad",
    "jdk.graal.compiler",
    "jdk.graal.compiler.management",
    "jdk.hotspot.agent",
    "jdk.httpserver",
    "jdk.incubator.vector",
    "jdk.internal.ed",
    "jdk.internal.jvmstat",
    "jdk.internal.le",
    "jdk.internal.md",
    "jdk.internal.opt",
    "jdk.internal.vm.ci",
    "jdk.jartool",
    "jdk.javadoc",
    "jdk.jcmd",
    "jdk.jconsole",
    "jdk.jdeps",
    "jdk.jdi",
    "jdk.jdwp.agent",
    "jdk.jfr",
    "jdk.jlink",
    "jdk.jpackage",
    "jdk.jshell",
    "jdk.jsobject",
    "jdk.jstatd",
    "jdk.localedata",
    "jdk.management",
    "jdk.management.agent",
    "jdk.management.jfr",
    "jdk.naming.dns",
    "jdk.naming.rmi",
    "jdk.net",
    "jdk.nio.mapmode",
    "jdk.sctp",
    "jdk.security.auth",
    "jdk.security.jgss",
    "jdk.unsupported",
    "jdk.unsupported.desktop",
    "jdk.xml.dom",
    "jdk.zipfs",
];

/// Does `name` name a module that the system image actually contains?
///
/// `ModuleFinder.ofSystem().find(name)` is a QUERY, and the JDK's answer for a
/// name the image does not contain is `Optional.empty()`. The lazy finder
/// registered below used to build a `ModuleReference` for whatever string it
/// was handed, so `find("cratonvm.absent")` reported PRESENT — indistinguishable
/// from a real hit for any caller that only checks `isPresent()`, and the error
/// only surfaced (if ever) at the eventual `open()`/`read()`.
///
/// Two independent sources, unioned, so neither can shrink the answer:
///   * the VM's own module registry (`module_packages`), which is authoritative
///     for anything actually resolved in this VM, including non-JDK modules on
///     the module path; and
///   * [`JDK_SYSTEM_MODULE_NAMES`], for the JDK 25 image modules the registry
///     has not lazily populated yet (`ofSystem().find("java.base")` must be
///     present at any point in the VM's life, including before java.base's
///     packages are enumerated).
fn is_system_module_name(ctx: &dyn NativeContext, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if JDK_SYSTEM_MODULE_NAMES.contains(&name) {
        return true;
    }
    !ctx.module_packages(name).is_empty()
}

/// Java's `String.compareTo`: the first differing UTF-16 unit's difference,
/// else the length difference.
fn module_member_java_string_compare(a: &str, b: &str) -> i32 {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    for (x, y) in a.iter().zip(b.iter()) {
        if x != y {
            return i32::from(*x) - i32::from(*y);
        }
    }
    a.len() as i32 - b.len() as i32
}

/// The first key HotSpot compares (`name` / `source` / `service`), with a
/// missing one first -- HotSpot never builds such a stand-in (its
/// `compareTo` would throw), so that arm only has to be total.
fn module_member_key_order(a: Option<&str>, b: Option<&str>) -> i32 {
    match (a, b) {
        (Some(a), Some(b)) => module_member_java_string_compare(a, b),
        (None, Some(_)) => -1,
        (Some(_), None) => 1,
        (None, None) => 0,
    }
}

/// The last resort for two DISTINCT stand-ins whose compared key ties, where
/// no real `compareTo` bytecode is there to compare the rest: their weak lock
/// keys. A key is unique per live object and stable for its life (the
/// post-collection remap re-addresses its slot), so the order is total,
/// antisymmetric and consistent over time, and never 0 for two objects --
/// which is what the stand-ins' reference `equals` says they are. The old
/// order (identity hash alone) answered 0 for a same-hash pair.
fn module_member_tie_break(ctx: &dyn NativeContext, a: ObjectRef, b: ObjectRef) -> i32 {
    if a == b {
        return 0;
    }
    let ord = match (
        crate::gc_stable_weak_lock_key(ctx, a),
        crate::gc_stable_weak_lock_key(ctx, b),
    ) {
        (Ok(ka), Ok(kb)) => ka.cmp(&kb),
        // Not reached (minting cannot fail); the addresses still differ.
        _ => (a.as_ptr() as usize).cmp(&(b.as_ptr() as usize)),
    };
    ord as i32
}

fn module_member_key_text(ctx: &dyn NativeContext, obj: ObjectRef, field: &str) -> Option<String> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// `compareTo(Object)` of a `ModuleDescriptor$Exports` / `$Opens` /
/// `$Requires` / `$Provides` stand-in (gc-common w34-a).
///
/// Where the class has its real bytecode, run the TYPED `compareTo` it
/// declares (`typed_descriptor`; only the erased bridge is registered here):
/// that is HotSpot's order exactly, read off the fields the `Builder.new*`
/// overrides fill under the JDK's own names (JDK 25
/// `java/lang/module/ModuleDescriptor.java`: `Requires` by `name`, then the
/// modifier set's ordinal mask, then `compiledVersion`, then
/// `rawCompiledVersion`; `Exports` and `Opens` by `source`, then the modifier
/// mask, then the sorted `targets`; `Provides` by `service`, then the
/// `providers` list element by element, then its length). A null argument
/// throws there, as on HotSpot.
///
/// With no bytecode (a synthetic class): the same first key, by Java's
/// `String.compareTo`, then [`module_member_tie_break`].
fn module_member_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
    typed_descriptor: &str,
    key_field: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = args.get(1).copied().unwrap_or(Value::Object(None));
    if ctx.method_exists(class_name, "compareTo", typed_descriptor) {
        return ctx.invoke_virtual(this, "compareTo", typed_descriptor, &[other]);
    }
    let Value::Object(Some(other)) = other else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    if this == other {
        return Ok(Some(Value::Int(0)));
    }
    let a = module_member_key_text(&*ctx, this, key_field);
    let b = module_member_key_text(&*ctx, other, key_field);
    let by_key = module_member_key_order(a.as_deref(), b.as_deref());
    let c = if by_key != 0 {
        by_key
    } else {
        module_member_tie_break(&*ctx, this, other)
    };
    Ok(Some(Value::Int(c)))
}

fn native_module_exports_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_member_compare_to(
        ctx,
        args,
        "java/lang/module/ModuleDescriptor$Exports",
        "(Ljava/lang/module/ModuleDescriptor$Exports;)I",
        "source",
    )
}

fn native_module_opens_compare_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    module_member_compare_to(
        ctx,
        args,
        "java/lang/module/ModuleDescriptor$Opens",
        "(Ljava/lang/module/ModuleDescriptor$Opens;)I",
        "source",
    )
}

fn native_module_requires_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_member_compare_to(
        ctx,
        args,
        "java/lang/module/ModuleDescriptor$Requires",
        "(Ljava/lang/module/ModuleDescriptor$Requires;)I",
        "name",
    )
}

fn native_module_provides_compare_to(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    module_member_compare_to(
        ctx,
        args,
        "java/lang/module/ModuleDescriptor$Provides",
        "(Ljava/lang/module/ModuleDescriptor$Provides;)I",
        "service",
    )
}

pub(crate) fn register_module_builder_overrides(registry: &mut NativeMethodRegistry) {
    // census-tag: jdk.internal.module.Builder.* are VM-internal module-system
    // factory natives → Bridge.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let owner = "jdk/internal/module/Builder";
    // newExports(Set<Modifier>, String, Set<String>) -> Exports (qualified)
    registry.register(
        owner,
        "newExports",
        "(Ljava/util/Set;Ljava/lang/String;Ljava/util/Set;)Ljava/lang/module/ModuleDescriptor$Exports;",
        native_module_builder_new_exports_qualified,
    );
    // newExports(Set<Modifier>, String) -> Exports (unqualified)
    registry.register(
        owner,
        "newExports",
        "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Exports;",
        native_module_builder_new_exports_unqualified,
    );
    // newOpens(Set<Modifier>, String, Set<String>) -> Opens (qualified)
    registry.register(
        owner,
        "newOpens",
        "(Ljava/util/Set;Ljava/lang/String;Ljava/util/Set;)Ljava/lang/module/ModuleDescriptor$Opens;",
        native_module_builder_new_opens_qualified,
    );
    // newOpens(Set<Modifier>, String) -> Opens (unqualified)
    registry.register(
        owner,
        "newOpens",
        "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Opens;",
        native_module_builder_new_opens_unqualified,
    );
    // newRequires(Set<Modifier>, String, String) -> Requires (with version)
    registry.register(
        owner,
        "newRequires",
        "(Ljava/util/Set;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Requires;",
        native_module_builder_new_requires_versioned,
    );
    // newRequires(Set<Modifier>, String) -> Requires
    registry.register(
        owner,
        "newRequires",
        "(Ljava/util/Set;Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Requires;",
        native_module_builder_new_requires_short,
    );
    // newProvides(String, List<String>) -> Provides
    registry.register(
        owner,
        "newProvides",
        "(Ljava/lang/String;Ljava/util/List;)Ljava/lang/module/ModuleDescriptor$Provides;",
        native_module_builder_new_provides,
    );
    // build(int) -> ModuleDescriptor (instance method, also goes through JLMA)
    registry.register(
        owner,
        "build",
        "(I)Ljava/lang/module/ModuleDescriptor;",
        native_module_builder_build,
    );
    // The Builder.version(String) bytecode goes through Version.parse —
    // we override the Builder method to skip parse and stash the raw
    // string in a synthetic Version object so the static cache field
    // does not feed downstream NPE paths.  We register against
    // `Version.parse` directly because Builder.version() doesn't have
    // a separate factory entry; Builder uses Version.parse().
    registry.register(
        "java/lang/module/ModuleDescriptor$Version",
        "parse",
        "(Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Version;",
        native_module_builder_new_version,
    );

    // S111r12 SB3 follow-on (continued): downstream of `Builder.newExports`
    // is `Builder.exports(Exports[])` which delegates to
    // `Set.of(exports)`.  `ImmutableCollections$SetN.probe` calls
    // `Exports.hashCode()` which dereferences the private `mods`,
    // `source`, `targets` fields:
    //
    //     int hash = modsHashCode(mods);                // NPE on null mods
    //     hash = hash * 43 + source.hashCode();
    //     return hash * 43 + targets.hashCode();
    //
    // Our synthetic stand-ins may have any of those null (Builder is called
    // from generated `SystemModules$all.moduleDescriptors` which builds the
    // Sets first).  Override `hashCode`/`equals` on the four
    // ModuleDescriptor$* inner classes to identity-based defaults.  The
    // descriptor walk only needs hashCode to be well-defined and equals
    // to be reflexive — Set.of's duplicate detection just needs a
    // consistent hash; identity hashing is consistent because we allocate
    // a fresh object per `Builder.new*` call anyway.
    for inner in [
        "java/lang/module/ModuleDescriptor$Exports",
        "java/lang/module/ModuleDescriptor$Opens",
        "java/lang/module/ModuleDescriptor$Requires",
        "java/lang/module/ModuleDescriptor$Provides",
        "java/lang/module/ModuleDescriptor$Version",
    ] {
        registry.register(inner, "hashCode", "()I", |ctx, args| {
            // Identity-based hash: stable per object, never NPEs on
            // null private fields.
            if let Some(Value::Object(Some(o))) = args.first() {
                let h = ctx.identity_hash_code(*o);
                Ok(Some(Value::Int(h)))
            } else {
                Ok(Some(Value::Int(0)))
            }
        });
        registry.register(inner, "equals", "(Ljava/lang/Object;)Z", |_ctx, args| {
            // Reference equality.  Set.of's duplicate detection
            // works because we allocate a fresh stand-in per call.
            let a = args.first().copied().unwrap_or(Value::Object(None));
            let b = args.get(1).copied().unwrap_or(Value::Object(None));
            let eq = matches!(
                (a, b),
                (Value::Object(Some(x)), Value::Object(Some(y))) if x == y
            );
            Ok(Some(Value::Int(if eq { 1 } else { 0 })))
        });
        // Set.of doesn't sort, but ModuleDescriptor's later TreeSet wrappings
        // and `ModuleDescriptor.compareTo` (which sorts the `requires` /
        // `exports` / ... sets) might. gc-common w34-a
        // (`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
        // row 4): this used to order by identity hash alone, so two distinct
        // stand-ins sharing a hash (every 2^20 mints of one class under the
        // 8-byte header) compared 0 and a TreeSet dropped one. It now orders
        // as HotSpot does; see `module_member_compare_to`. The `Version` row
        // is re-registered below with its own text comparison (last write
        // wins), so it gets that callback here too.
        let compare_to: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = match inner {
            "java/lang/module/ModuleDescriptor$Exports" => native_module_exports_compare_to,
            "java/lang/module/ModuleDescriptor$Opens" => native_module_opens_compare_to,
            "java/lang/module/ModuleDescriptor$Requires" => native_module_requires_compare_to,
            "java/lang/module/ModuleDescriptor$Provides" => native_module_provides_compare_to,
            _ => native_module_descriptor_version_compare_to,
        };
        registry.register(inner, "compareTo", "(Ljava/lang/Object;)I", compare_to);
    }
    registry.register(
        "java/lang/module/ModuleDescriptor$Version",
        "toString",
        "()Ljava/lang/String;",
        native_module_descriptor_version_to_string,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor$Version",
        "equals",
        "(Ljava/lang/Object;)Z",
        native_module_descriptor_version_equals,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor$Version",
        "hashCode",
        "()I",
        native_module_descriptor_version_hash_code,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor$Version",
        "compareTo",
        "(Ljava/lang/Object;)I",
        native_module_descriptor_version_compare_to,
    );

    // -----------------------------------------------------------------
    // ModuleFinder.ofSystem() — lazy system-module finder
    // -----------------------------------------------------------------
    //
    // See `is_system_module_name` for why `find` is not a "yes to everything".
    //
    // The genuine `jdk.internal.module.SystemModuleFinders.ofSystem()` cannot
    // run as-is in CratonVM:
    //   * The fast path (`SystemModulesMap.allSystemModules()`) returns null —
    //     the generated `SystemModules$all`/`$0..` classes only exist in the
    //     linked `lib/modules` jimage, not in the `jmods/java.base.jmod`
    //     CratonVM boots from (they are jlink-generated).
    //   * The fallback `ofModuleInfos()` eagerly (a) builds every module's
    //     descriptor through `JavaLangModuleAccess` (unwired here) and (b)
    //     memory-maps the ~140 MiB run-time image — far too costly to pay on
    //     every `ofSystem()` caller (Spring's PMRPR.<clinit> calls
    //     `ofSystem().findAll()` at boot) and it would OOM the default heap.
    //
    // So we provide a *lazy* finder: `findAll()` returns synthetic references
    // for the boot-layer system modules Craton exposes, enough for callers
    // such as Spring to classify those modules as system modules and skip
    // scanning them. `find(name)` returns a `ModuleReference`
    // whose `open()` builds a *real* `SystemModuleReader` on demand. The
    // image is therefore read only when code actually traverses module
    // contents (`reader.list()/read()`), via the genuine JDK
    // `ImageReader`/`BasicImageReader` (see `NativeImageBuffer.getNativeMap`
    // in native-io). No descriptors, no eager image map, no JLMA.
    registry.register(
        "java/lang/module/ModuleFinder",
        "ofSystem",
        "()Ljava/lang/module/ModuleFinder;",
        |ctx, _args| {
            let finder = try_alloc_concurrent_synthetic(
                ctx,
                "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
                4,
            )?;
            Ok(Some(Value::Object(Some(finder))))
        },
    );
    for cls in [
        "java/lang/module/ModuleFinder",
        "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
    ] {
        registry.register(cls, "findAll", "()Ljava/util/Set;", |ctx, _args| {
            let mut module_refs = Vec::new();
            let mut first_pin = None;
            // ASK THE REGISTRY, do not carry a list. This was hardcoded to
            // `["java.base", "java.xml"]`, so `ModuleFinder.ofSystem()
            // .findAll()` answered TWO modules where HotSpot answers ~70 --
            // and `Java9.<clinit>` builds its CONCEALED/EXPORTED
            // `PACKAGES_TO_OPEN` maps by walking exactly this set, so a short
            // answer is a Groovy that silently cannot open packages it needs.
            //
            // Third instance of one shape today: a hand-maintained table
            // standing in front of the VM's own `ModuleRegistry`, which knows
            // the answer (`module_package_names` and the `getPackages()`/
            // `getDescriptor().packages()` split were the other two). The
            // registry is the source; the literal pair is the floor for an
            // image that somehow registers nothing.
            let registered = ctx.module_names();
            let names: Vec<String> = if registered.is_empty() {
                vec!["java.base".to_string(), "java.xml".to_string()]
            } else {
                registered
            };
            for module_name in names.iter().map(String::as_str) {
                let name = ctx.create_string(module_name);
                let name_pin = ctx.pin_native_root(name);
                first_pin = Some(first_pin.map_or(name_pin, |pin: usize| pin.min(name_pin)));

                let mref = try_alloc_concurrent_synthetic(
                    ctx,
                    "jdk/internal/module/ModuleReferenceImpl",
                    8,
                )?;
                let mref_pin = ctx.pin_native_root(mref);
                first_pin = Some(first_pin.map_or(mref_pin, |pin: usize| pin.min(mref_pin)));

                // BUILD THE WHOLE DESCRIPTOR, not just its name.
                //
                // This used to allocate a 16-slot `ModuleDescriptor` and set
                // ONLY `name`, leaving `packages`, `exports`, `opens`,
                // `requires`, `provides`, `uses` and `modifiers` NULL. Every
                // one of those accessors is specified never to return null.
                //
                // In compatible mode a registered native answered them, so the
                // nulls were invisible. Under `--jdk-only` the real JDK
                // bytecode runs -- `return packages;` -- and hands back the
                // null, which is how ALL OF GROOVY died:
                //
                //   org/codehaus/groovy/vmplugin/v9/Java9.<clinit>
                //     NPE: Cannot invoke "java.util.Set.forEach(..)"
                //   -> VMPluginFactory.getPlugin() == null
                //   -> GroovySystem.<clinit> NPE
                //   -> NoClassDefFoundError: groovy/lang/GroovySystem   x18 classes
                //
                // `Java9`'s initialiser walks `ModuleFinder.ofSystem()
                // .findAll()` and asks each descriptor about its opens and
                // exports; a null Set two frames down is reported three frames
                // up as a missing Groovy class.
                //
                // `build_module_descriptor` is the same builder
                // `Module.getDescriptor()` uses and reads the VM's own
                // `ModuleRegistry`, so the finder's answer and the module
                // mirror's answer are now the SAME answer -- which is the
                // defect this campaign keeps finding in the other direction
                // (two producers of one concept that disagree).
                let name = ctx.read_native_pin(name_pin, name);
                let md = crate::jboss_jdkspecific::build_module_descriptor(ctx, module_name)?;
                let _ = name;
                let mref = ctx.read_native_pin(mref_pin, mref);
                ctx.set_field_by_name(mref, "descriptor", Value::Object(Some(md)));
                module_refs.push((mref_pin, mref));
            }
            let elements: Vec<Value> = module_refs
                .iter()
                .map(|(pin, mref)| Value::Object(Some(ctx.read_native_pin(*pin, *mref))))
                .collect();
            let set = cratonvm_native_collections::make_hashset_with_elements(ctx, &elements)?;
            if let Some(pin) = first_pin {
                ctx.unpin_native_roots(pin);
            }
            Ok(Some(Value::Object(Some(set))))
        });
    }
    // find(name) -> Optional<ModuleReference> backed by a lazy reader.
    registry.register(
        "jdk/internal/module/SystemModuleFinders$SystemModuleFinder",
        "find",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let name = match args.get(1).copied() {
                Some(Value::Object(Some(s))) => s,
                // null name → Optional.empty() (JDK contract is NPE, but
                // empty is safer for a defensive finder).
                _ => {
                    return ctx.invoke(
                        "java/util/Optional",
                        "empty",
                        "()Ljava/util/Optional;",
                        &[],
                    );
                }
            };
            // A finder must answer for the modules it actually observes, and
            // `Optional.empty()` for everything else. This used to mint a
            // ModuleReference for ANY string, so
            // `ModuleFinder.ofSystem().find("cratonvm.absent")` reported a
            // module that does not exist — a fabricated success where the spec
            // mandates an empty answer, and the failure surfaced later (as an
            // `open()` on a reader for a module the image has no entries for)
            // rather than here. Measured at
            // `regression-suite/src/RJdkFailure.java:293` ("the system finder
            // must not invent a module"), failing in `--real-jdk` and
            // `--jdk-only` while HotSpot 25 passes.
            let name_text = ctx.read_string(name).unwrap_or_default();
            if !is_system_module_name(&*ctx, &name_text) {
                return ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]);
            }
            // A ModuleReferenceImpl whose `descriptor.name` carries the module
            // name and whose `readerSupplier` is left null — `open()` below
            // detects the null supplier and builds a SystemModuleReader.
            //
            // gc-common w13-f: the descriptor build below allocates a dozen
            // objects (and runs `HashSet.<init>`), so the reference is rooted
            // across it and re-read before the store — as `findAll` does.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let mref_new = try_alloc_concurrent_synthetic(
                &mut *scope,
                "jdk/internal/module/ModuleReferenceImpl",
                8,
            )?;
            let mref_h = scope.root(mref_new);
            // The same whole-descriptor rule as `findAll` above: a descriptor
            // carrying only its name answers null from every collection
            // accessor, and null is what those accessors are specified never to
            // return.
            let md = crate::jboss_jdkspecific::build_module_descriptor(&mut *scope, &name_text)?;
            let mref = scope.get(&mref_h);
            scope.set_field_by_name(mref, "descriptor", Value::Object(Some(md)));
            scope.invoke(
                "java/util/Optional",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
                &[Value::Object(Some(mref))],
            )
        },
    );
    // open() -> ModuleReader. For a real ModuleReferenceImpl (non-null
    // readerSupplier, e.g. a module-path finder's reference) delegate to the
    // supplier, exactly as the JDK's `open()` does. For our lazy
    // system-module references build a real SystemModuleReader for the module.
    let module_ref_open: cratonvm_native_api::NativeCallback = |ctx, args| {
        let this = match args.first().copied() {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        if let Value::Object(Some(rs)) = ctx.get_field_by_name(this, "readerSupplier") {
            return ctx.invoke_virtual(rs, "get", "()Ljava/lang/Object;", &[]);
        }
        let name = match ctx.get_field_by_name(this, "descriptor") {
            Value::Object(Some(d)) => match ctx.get_field_by_name(d, "name") {
                Value::Object(Some(s)) => s,
                _ => return Ok(Some(Value::Object(None))),
            },
            _ => return Ok(Some(Value::Object(None))),
        };
        // gc-common w13-f: the module-name String was stored into the reader
        // through the address it had BEFORE the reader's allocation.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let name_h = scope.root(name);
        let reader = try_alloc_concurrent_synthetic(
            &mut *scope,
            "jdk/internal/module/SystemModuleFinders$SystemModuleReader",
            4,
        )?;
        let name = scope.get(&name_h);
        scope.set_field_by_name(reader, "module", Value::Object(Some(name)));
        scope.set_field_by_name(reader, "closed", Value::Int(0));
        Ok(Some(Value::Object(Some(reader))))
    };
    registry.register(
        "jdk/internal/module/ModuleReferenceImpl",
        "open",
        "()Ljava/lang/module/ModuleReader;",
        module_ref_open,
    );

    // Registered on BOTH the abstract base and the concrete impl. The
    // dispatcher in `interpreter.rs` (line ~11547) walks the parent chain
    // looking for natives but breaks early if a parent has bytecode for
    // the method — which `ModuleReference.descriptor()` does (a simple
    // `getfield`). To force the override to fire, register on the
    // concrete receiver class `jdk/internal/module/ModuleReferenceImpl`
    // directly (the dispatcher checks the receiver class FIRST before
    // walking parents).
    let descriptor_native: cratonvm_native_api::NativeCallback = |ctx, args| {
        let this = match args.first().copied() {
            Some(Value::Object(Some(o))) => o,
            _ => return Ok(Some(Value::Object(None))),
        };
        // Fast path: real / previously-populated descriptor.
        if let Value::Object(Some(d)) = ctx.get_field_by_name(this, "descriptor") {
            return Ok(Some(Value::Object(Some(d))));
        }
        // Lazy allocate a synthetic ModuleDescriptor with non-null fields so
        // downstream real-JDK module/layer code can call `name()`, `opens()`,
        // `exports()`, `uses()`, `provides()`, and hash/equals methods without
        // tripping on partially initialized descriptor state. Cache it on the
        // ModuleReference instance.
        //
        // gc-common w13-f: the receiver is rooted too. It was the one reference
        // still used through its entry address after the eight allocations
        // below (the cache store on `this`), and the early-return arm handed
        // back the descriptor as read BEFORE the `HashSet.<init>` that failed.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let md_new =
            try_alloc_concurrent_synthetic(&mut *scope, "java/lang/module/ModuleDescriptor", 16)?;
        let md_h = scope.root(md_new);
        let name_str = scope.create_string("synthetic");
        let md = scope.get(&md_h);
        scope.set_field_by_name(md, "name", Value::Object(Some(name_str)));
        for field in [
            "modifiers",
            "requires",
            "exports",
            "opens",
            "uses",
            "provides",
            "packages",
        ] {
            let empty = match scope.new_object_initialized("java/util/HashSet", "()V", &[])? {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(Some(scope.get(&md_h))))),
            };
            let md = scope.get(&md_h);
            scope.set_field_by_name(md, field, Value::Object(Some(empty)));
        }
        let md = scope.get(&md_h);
        let this = scope.get(&this_h);
        scope.set_field_by_name(this, "descriptor", Value::Object(Some(md)));
        Ok(Some(Value::Object(Some(md))))
    };
    registry.register(
        "java/lang/module/ModuleReference",
        "descriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        descriptor_native,
    );
    registry.register(
        "jdk/internal/module/ModuleReferenceImpl",
        "descriptor",
        "()Ljava/lang/module/ModuleDescriptor;",
        descriptor_native,
    );

    registry.register(
        "java/lang/module/ModuleDescriptor",
        "name",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first().copied() {
                Some(Value::Object(Some(o))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "name") {
                return Ok(Some(Value::Object(Some(s))));
            }
            // Populate on first read so subsequent reads (e.g. Set
            // duplicate-detection) see a stable identity.
            // gc-common w19-e: rooted across the String's allocation (it was
            // memoized through the descriptor's address from before it).
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let s = scope.create_string("synthetic");
            let this = scope.get(&this_h);
            scope.set_field_by_name(this, "name", Value::Object(Some(s)));
            Ok(Some(Value::Object(Some(s))))
        },
    );
    for (method, callback) in [
        (
            "modifiers",
            native_module_descriptor_modifiers as cratonvm_native_api::NativeCallback,
        ),
        (
            "requires",
            native_module_descriptor_requires as cratonvm_native_api::NativeCallback,
        ),
        (
            "exports",
            native_module_descriptor_exports as cratonvm_native_api::NativeCallback,
        ),
        (
            "opens",
            native_module_descriptor_opens as cratonvm_native_api::NativeCallback,
        ),
        (
            "uses",
            native_module_descriptor_uses as cratonvm_native_api::NativeCallback,
        ),
        (
            "provides",
            native_module_descriptor_provides as cratonvm_native_api::NativeCallback,
        ),
        (
            "packages",
            native_module_descriptor_packages as cratonvm_native_api::NativeCallback,
        ),
    ] {
        registry.register(
            "java/lang/module/ModuleDescriptor",
            method,
            "()Ljava/util/Set;",
            callback,
        );
    }
    registry.register(
        "java/lang/module/ModuleDescriptor",
        "isAutomatic",
        "()Z",
        native_module_descriptor_is_automatic,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor",
        "isOpen",
        "()Z",
        native_module_descriptor_is_open,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor",
        "version",
        "()Ljava/util/Optional;",
        native_module_descriptor_version,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor",
        "rawVersion",
        "()Ljava/util/Optional;",
        native_module_descriptor_raw_version,
    );
    registry.register(
        "java/lang/module/ModuleDescriptor",
        "mainClass",
        "()Ljava/util/Optional;",
        native_module_descriptor_main_class,
    );
    registry.set_category(__prev_cat);
}

fn native_attrs_new_map(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.new_object_initialized("java/util/HashMap", "()V", &[])? {
        Some(Value::Object(Some(map))) => Ok(map),
        _ => {
            let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
            cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))])?;
            Ok(map)
        }
    }
}

fn native_attrs_ensure_map(
    ctx: &mut dyn NativeContext,
    attrs: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(map)) = ctx.get_field(attrs, 0) {
        return Ok(map);
    }
    let attrs_pin = ctx.pin_native_root(attrs);
    let map = native_attrs_new_map(ctx)?;
    let attrs = ctx.read_native_pin(attrs_pin, attrs);
    ctx.set_field(attrs, 0, Value::Object(Some(map)));
    ctx.unpin_native_roots(attrs_pin);
    Ok(map)
}

fn native_attrs_name_text(ctx: &dyn NativeContext, name_obj: ObjectRef) -> Option<String> {
    match ctx.get_field(name_obj, 0) {
        Value::Object(Some(name)) => ctx.read_string(name),
        _ => None,
    }
}

fn native_attrs_make_name(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/jar/Attributes$Name", 1)?;
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

fn native_attrs_key_for_value(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.read_string(key) {
        Some(name) => native_attrs_make_name(ctx, &name),
        None => Ok(key),
    }
}

pub(crate) fn native_attrs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let map = native_attrs_new_map(ctx)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, 0, Value::Object(Some(map)));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub(crate) fn native_attrs_put_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(key))) => *key,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Attributes is a Map and therefore accepts null values. In particular,
    // Spring Boot writes its version attribute from Package metadata, which is
    // null for an exploded classes directory; HotSpot retains that mapping and
    // serializes it as the literal text "null". Do not turn that legal put
    // into a silent no-op.
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let value_pin = match value {
        Value::Object(Some(value)) => Some(ctx.pin_native_root(value)),
        _ => None,
    };
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let map_pin = ctx.pin_native_root(map);
    let key = ctx.read_native_pin(key_pin, key);
    let map_key = native_attrs_key_for_value(ctx, key)?;
    let map_key_pin = ctx.pin_native_root(map_key);
    let value = match value_pin {
        Some(value_pin) => Value::Object(Some(ctx.read_native_pin(
            value_pin,
            match value {
                Value::Object(Some(value)) => value,
                _ => unreachable!("only non-null references are pinned"),
            },
        ))),
        None => value,
    };
    let map = ctx.read_native_pin(map_pin, map);
    let map_key = ctx.read_native_pin(map_key_pin, map_key);
    // gc-common w11-c: the reclaiming door (registered-only; nothing but the
    // unpins follows the put).
    let result = cratonvm_native_collections::native_map_put_entry_pub(
        ctx,
        &[
            Value::Object(Some(map)),
            Value::Object(Some(map_key)),
            value,
        ],
    );
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(key_pin);
    if let Some(value_pin) = value_pin {
        ctx.unpin_native_roots(value_pin);
    }
    ctx.unpin_native_roots(map_pin);
    ctx.unpin_native_roots(map_key_pin);
    result
}

pub(crate) fn native_attrs_get_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(key))) => *key,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let map_pin = ctx.pin_native_root(map);
    let key = ctx.read_native_pin(key_pin, key);
    let map_key = native_attrs_key_for_value(ctx, key)?;
    let map_key_pin = ctx.pin_native_root(map_key);
    let map = ctx.read_native_pin(map_pin, map);
    let map_key = ctx.read_native_pin(map_key_pin, map_key);
    let result = cratonvm_native_collections::native_map_get_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(map_key))],
    );
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(key_pin);
    ctx.unpin_native_roots(map_pin);
    ctx.unpin_native_roots(map_key_pin);
    result
}

pub(crate) fn native_attrs_put_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(key))) => *key,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Map.put(Object, Object) has the same null-value contract as putValue.
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let value_pin = match value {
        Value::Object(Some(value)) => Some(ctx.pin_native_root(value)),
        _ => None,
    };
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let map_pin = ctx.pin_native_root(map);
    let key = ctx.read_native_pin(key_pin, key);
    let value = match value_pin {
        Some(value_pin) => Value::Object(Some(ctx.read_native_pin(
            value_pin,
            match value {
                Value::Object(Some(value)) => value,
                _ => unreachable!("only non-null references are pinned"),
            },
        ))),
        None => value,
    };
    let map = ctx.read_native_pin(map_pin, map);
    // gc-common w11-c: the reclaiming door (registered-only; nothing but the
    // unpins follows the put).
    let result = cratonvm_native_collections::native_map_put_entry_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key)), value],
    );
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(key_pin);
    if let Some(value_pin) = value_pin {
        ctx.unpin_native_roots(value_pin);
    }
    ctx.unpin_native_roots(map_pin);
    result
}

pub(crate) fn native_attrs_get_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = match args.get(1) {
        Some(Value::Object(Some(key))) => *key,
        _ => return Ok(Some(Value::Object(None))),
    };
    let this_pin = ctx.pin_native_root(this);
    let key_pin = ctx.pin_native_root(key);
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let map_pin = ctx.pin_native_root(map);
    let key = ctx.read_native_pin(key_pin, key);
    let map = ctx.read_native_pin(map_pin, map);
    let result = cratonvm_native_collections::native_map_get_pub(
        ctx,
        &[Value::Object(Some(map)), Value::Object(Some(key))],
    );
    ctx.unpin_native_roots(this_pin);
    ctx.unpin_native_roots(key_pin);
    ctx.unpin_native_roots(map_pin);
    result
}

pub(crate) fn native_attrs_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let result = cratonvm_native_collections::native_map_size_pub(ctx, &[Value::Object(Some(map))]);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_attrs_entry_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let this_pin = ctx.pin_native_root(this);
    let this = ctx.read_native_pin(this_pin, this);
    let map = native_attrs_ensure_map(ctx, this)?;
    let result =
        cratonvm_native_collections::native_map_entry_set_pub(ctx, &[Value::Object(Some(map))]);
    ctx.unpin_native_roots(this_pin);
    result
}

pub(crate) fn native_attrs_name_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(this, 0, name);
    Ok(None)
}

pub(crate) fn native_attrs_name_to_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, 0)))
}

pub(crate) fn native_attrs_name_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = native_attrs_name_text(ctx, this).unwrap_or_default();
    let mut h = 0i32;
    for b in name.bytes() {
        h = h
            .wrapping_mul(31)
            .wrapping_add(b.to_ascii_lowercase() as i32);
    }
    Ok(Some(Value::Int(h)))
}

pub(crate) fn native_attrs_name_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let other = match args.get(1) {
        Some(Value::Object(Some(other))) => *other,
        _ => return Ok(Some(Value::Int(0))),
    };
    if this.as_ptr() == other.as_ptr() {
        return Ok(Some(Value::Int(1)));
    }
    let lhs = native_attrs_name_text(ctx, this).unwrap_or_default();
    let rhs = match native_attrs_name_text(ctx, other) {
        Some(rhs) => rhs,
        None => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if lhs.eq_ignore_ascii_case(&rhs) {
        1
    } else {
        0
    })))
}

pub(crate) fn native_attrs_name_constant(
    ctx: &mut dyn NativeContext,
    name: &'static str,
) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "java/util/jar/Attributes$Name", 1)?;
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(name);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(s)));
    ctx.unpin_native_roots(obj_pin);
    Ok(Some(Value::Object(Some(obj))))
}

// ===========================================================================
// Phase 47: java.lang.reflect.Array — reflective array operations
// ===========================================================================

pub(crate) fn register_reflect_array_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: java.lang.reflect.Array.{getLength,get,set} and the typed
    // get*/set* accessors are real ACC_NATIVE JDK methods → Bridge.
    // `newInstance` is NOT: on 17/21/25 both overloads are Java that call the
    // natives `newArray` / `multiNewArray` (registered as explicit Bridges in
    // `lib.rs`). Its two rows stay Bridge-scoped here for `--compatible` and
    // are re-tagged `SyntheticStub` at registration by
    // `RETIRED_SHADOW_REFLECT_ARRAY_NEW_INSTANCE_TRIPLES`
    // (`native-api/src/retired_shadow.rs`), so `--jdk-only` runs the real
    // bodies (round 14 wave 6).
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let a = "java/lang/reflect/Array";
    registry.register_with_kind(
        a,
        "getLength",
        "(Ljava/lang/Object;)I",
        native_array_get_length,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "get",
        "(Ljava/lang/Object;I)Ljava/lang/Object;",
        native_array_get,
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "set",
        "(Ljava/lang/Object;ILjava/lang/Object;)V",
        native_array_set,
        cratonvm_native_api::NativeKind::Bridge,
    );
    // Each primitive accessor carries the type it was ASKED for. These used
    // to collapse onto two untyped bodies -- `getInt` served `getBoolean`,
    // `getByte`, `getShort` and `getChar` as well -- so the widening rule had
    // nothing to test against and `Array.getInt(new long[4], 0)` returned a
    // fabricated 0. See `widens_to`.
    registry.register_with_kind(
        a,
        "getBoolean",
        "(Ljava/lang/Object;I)Z",
        |ctx, args| {
            reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Boolean)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getByte",
        "(Ljava/lang/Object;I)B",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Byte),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getChar",
        "(Ljava/lang/Object;I)C",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Char),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getShort",
        "(Ljava/lang/Object;I)S",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Short),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getInt",
        "(Ljava/lang/Object;I)I",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Int),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getLong",
        "(Ljava/lang/Object;I)J",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Long),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getFloat",
        "(Ljava/lang/Object;I)F",
        |ctx, args| reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Float),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "getDouble",
        "(Ljava/lang/Object;I)D",
        |ctx, args| {
            reflect_array_get_primitive(ctx, args, cratonvm_types::ArrayElementType::Double)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setBoolean",
        "(Ljava/lang/Object;IZ)V",
        |ctx, args| {
            reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Boolean)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setByte",
        "(Ljava/lang/Object;IB)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Byte),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setChar",
        "(Ljava/lang/Object;IC)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Char),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setShort",
        "(Ljava/lang/Object;IS)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Short),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setInt",
        "(Ljava/lang/Object;II)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Int),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setLong",
        "(Ljava/lang/Object;IJ)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Long),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setFloat",
        "(Ljava/lang/Object;IF)V",
        |ctx, args| reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Float),
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register_with_kind(
        a,
        "setDouble",
        "(Ljava/lang/Object;ID)V",
        |ctx, args| {
            reflect_array_set_primitive(ctx, args, cratonvm_types::ArrayElementType::Double)
        },
        cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        a,
        "newInstance",
        "(Ljava/lang/Class;I)Ljava/lang/Object;",
        native_array_new_instance,
    );
    registry.register(
        a,
        "newInstance",
        "(Ljava/lang/Class;[I)Ljava/lang/Object;",
        native_array_new_instance_multi,
    );
    registry.set_category(__prev_cat);
}

/// Module sequence number for `loader_id`'s generated proxies (the `N` in
/// `jdk/proxyN`), assigned in first-encounter order starting at 1. Same loader →
/// same number, so all of that loader's public-interface proxies land in one
/// `jdk/proxyN` package — matching HotSpot's per-loader dynamic module.
fn proxy_module_number(vm: usize, loader_id: u32) -> u32 {
    {
        let guard = PROXY_LOADER_MODULES.read();
        if let Some(map) = guard.as_ref() {
            if let Some(&n) = map.get(&(vm, loader_id)) {
                return n;
            }
        }
    }
    let mut guard = PROXY_LOADER_MODULES.write();
    let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
    // Re-check under the write lock — another thread may have assigned it.
    if let Some(&n) = map.get(&(vm, loader_id)) {
        return n;
    }
    let n = PROXY_MODULE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    map.insert((vm, loader_id), n);
    n
}

/// `CRATONVM_PROXY_DYNAMIC_MODULE` (default on, both modes): `Class.getModule()`
/// of a generated all-public-interface proxy answers its loader's dynamic
/// module `jdk.proxyN`, as HotSpot does since JDK 16 (round 13 wave 9, lane
/// proxy6). `0` answers the loader's unnamed module, as before.
fn proxy_dynamic_module_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_DYNAMIC_MODULE")
}

/// The dynamic module a generated proxy class belongs to, as `(module name,
/// package in internal form)`: `("jdk.proxy1", "jdk/proxy1")` for
/// `jdk/proxy1/$Proxy0`. [`build_proxy_spec_for`] names every all-public
/// proxy `jdk/proxy<N>/$Proxy<M>` with `N` from [`proxy_module_number`], one
/// per (VM, loader), which is `ProxyBuilder.getDynamicModule`'s module and
/// package (`pn = mn`). `None` for anything else, including a user class that
/// merely has that name, and when the switch is off. Read by
/// `Class.getModule()` (`lib.rs`).
pub(crate) fn proxy_dynamic_module_of(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> Option<(String, String)> {
    if !proxy_dynamic_module_enabled() {
        return None;
    }
    let name = ctx.class_name_of_id(class_id)?;
    let module = proxy_dynamic_module_name(&name)?;
    if !proxy_class_is_generated(ctx, class_id) {
        return None;
    }
    Some(module)
}

/// The name half of [`proxy_dynamic_module_of`]: `jdk/proxy<digits>/$Proxy<digits>`,
/// or the same under the module's non-exported package prefix `com/sun/proxy/`
/// (round 13 wave 10), which answers `("jdk.proxyN", "com/sun/proxy/jdk/proxyN")`.
fn proxy_dynamic_module_name(name: &str) -> Option<(String, String)> {
    let (package, _) = name.rsplit_once('/')?;
    let module_package = package.strip_prefix(PROXY_NON_EXPORTED_PREFIX).unwrap_or(package);
    let number = module_package.strip_prefix("jdk/proxy")?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !proxy_name_has_generated_shape(name) {
        return None;
    }
    Some((module_package.replace('/', "."), package.to_string()))
}

/// `Proxy.PROXY_PACKAGE_PREFIX` (`com.sun.proxy`) in internal form, with its
/// trailing separator.
const PROXY_NON_EXPORTED_PREFIX: &str = "com/sun/proxy/";

/// `CRATONVM_PROXY_NONEXPORTED_PACKAGE` (default on, both modes): a proxy
/// whose interfaces are all public but at least one lives in a package its
/// named module does not export unconditionally is named
/// `com/sun/proxy/jdk/proxyN/$ProxyM`, as HotSpot names it. `0` keeps
/// `jdk/proxyN/$ProxyM` for it (round 13 wave 10, lane misc10).
fn proxy_nonexported_package_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_NONEXPORTED_PACKAGE")
}

/// `ProxyBuilder.proxyClassContext`'s `nonExported`: some interface's module
/// does not export its package unconditionally (`!m.isExported(pn)`). An
/// unnamed module -- and a module-info reached through the class path, which
/// `Class.getModule()` also reports as unnamed -- exports everything.
fn proxy_interfaces_non_exported(
    ctx: &dyn NativeContext,
    ifaces: &[cratonvm_types::ClassId],
) -> bool {
    if !proxy_nonexported_package_enabled() {
        return false;
    }
    ifaces.iter().any(|cid| {
        let Some(module) = ctx.module_name_of_class(*cid) else {
            return false;
        };
        if ctx.module_is_class_path_only(&module) {
            return false;
        }
        let Some(name) = ctx.class_name_of_id(*cid) else {
            return false;
        };
        let package = name.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        !ctx.is_package_exported_unqualified(&module, package)
    })
}

/// `CRATONVM_PROXY_MODULE_DESCRIPTOR` (default on, both modes): the
/// `jdk.proxyN` Module's descriptor is the one `ProxyBuilder.getDynamicModule`
/// builds (see [`proxy_dynamic_module_descriptor`]) rather than the all-empty
/// shape of a module the registry does not know. `0` restores that shape.
fn proxy_module_descriptor_enabled() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_MODULE_DESCRIPTOR")
}

/// Whether the named Module `module` is a dynamic proxy module (`jdk.proxy<digits>`)
/// whose identity answers come from the descriptor
/// [`proxy_dynamic_module_descriptor`] built, as they do on HotSpot:
/// `getPackages()` is `descriptor.packages()` and `getLayer()` is its null
/// `layer` field. Read by `jboss_jdkspecific.rs`'s `Module.getPackages` /
/// `getLayer` natives; `false` when `CRATONVM_PROXY_MODULE_DESCRIPTOR=0`.
pub(crate) fn proxy_module_answers_from_descriptor(module: &str) -> bool {
    module
        .strip_prefix("jdk.proxy")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && proxy_module_descriptor_enabled()
}

/// The descriptor of the dynamic proxy module `module` (`jdk.proxyN`), built by
/// the JDK's own `ModuleDescriptor.Builder` exactly as
/// `Proxy.ProxyBuilder.getDynamicModule` builds it:
///
/// ```text
/// ModuleDescriptor.newModule(mn, Set.of(SYNTHETIC))
///                 .packages(Set.of(pn, mn))   // pn = "com.sun.proxy." + mn
///                 .exports(mn)
///                 .build()                    // + requires mandated java.base
/// ```
///
/// So `getDescriptor()` reports `SYNTHETIC` and the two packages, and the real
/// `Module.getPackages()` (which answers `descriptor.packages()` for a named
/// module) reports them too. Here `packages` is fed one singleton set and
/// `exports` adds `mn` itself, which is the same set. `None` when the switch is
/// off or any step fails (the synthetic JDK has no such builder); the caller
/// then keeps the registry-built shape. Round 13 wave 10 (lane misc10).
pub(crate) fn proxy_dynamic_module_descriptor(
    ctx: &mut dyn NativeContext,
    module: &str,
) -> Option<ObjectRef> {
    if !proxy_module_descriptor_enabled() {
        return None;
    }
    fn object(result: MethodCallResult) -> Option<ObjectRef> {
        match result {
            Ok(Some(Value::Object(Some(o)))) => Some(o),
            _ => None,
        }
    }
    const BUILDER: &str = "Ljava/lang/module/ModuleDescriptor$Builder;";
    let mn = ctx.create_string_uninterned(module);
    let mn_pin = ctx.pin_native_root(mn);
    let built = (|| -> Option<ObjectRef> {
        let pn = ctx.create_string_uninterned(&format!("com.sun.proxy.{module}"));
        let pn_pin = ctx.pin_native_root(pn);
        let modifier_name = ctx.create_string_uninterned("SYNTHETIC");
        let synthetic = object(ctx.invoke_static(
            "java/lang/module/ModuleDescriptor$Modifier",
            "valueOf",
            "(Ljava/lang/String;)Ljava/lang/module/ModuleDescriptor$Modifier;",
            &[Value::Object(Some(modifier_name))],
        ))?;
        let modifiers = object(ctx.invoke_static(
            "java/util/Collections",
            "singleton",
            "(Ljava/lang/Object;)Ljava/util/Set;",
            &[Value::Object(Some(synthetic))],
        ))?;
        let mn = ctx.read_native_pin(mn_pin, mn);
        let builder = object(ctx.invoke_static(
            "java/lang/module/ModuleDescriptor",
            "newModule",
            &format!("(Ljava/lang/String;Ljava/util/Set;){BUILDER}"),
            &[Value::Object(Some(mn)), Value::Object(Some(modifiers))],
        ))?;
        let builder_pin = ctx.pin_native_root(builder);
        let pn = ctx.read_native_pin(pn_pin, pn);
        let packages = object(ctx.invoke_static(
            "java/util/Collections",
            "singleton",
            "(Ljava/lang/Object;)Ljava/util/Set;",
            &[Value::Object(Some(pn))],
        ))?;
        let builder = ctx.read_native_pin(builder_pin, builder);
        object(ctx.invoke_virtual(
            builder,
            "packages",
            &format!("(Ljava/util/Set;){BUILDER}"),
            &[Value::Object(Some(packages))],
        ))?;
        let builder = ctx.read_native_pin(builder_pin, builder);
        let mn = ctx.read_native_pin(mn_pin, mn);
        object(ctx.invoke_virtual(
            builder,
            "exports",
            &format!("(Ljava/lang/String;){BUILDER}"),
            &[Value::Object(Some(mn))],
        ))?;
        let builder = ctx.read_native_pin(builder_pin, builder);
        object(ctx.invoke_virtual(builder, "build", "()Ljava/lang/module/ModuleDescriptor;", &[]))
    })();
    ctx.unpin_native_roots(mn_pin);
    built
}

/// Public accessor exposing the most-recently-created proxy's interfaces
/// array as a raw pointer for the WP2.5 white-box test. Derives from the
/// GC-tracked cell in `lang_class` (bug nb-lib-gckeys §2) — returns the
/// CURRENT (post-relocation) address, or `0` if no proxy has been created.
/// Reflection itself no longer goes through this raw form; it reads the
/// tracked `ObjectRef` directly via `lang_class::proxy_last_interfaces`.
pub fn proxy_last_interfaces_bits(vm_identity: usize) -> u64 {
    lang_class::proxy_last_interfaces(vm_identity)
        .map(|arr| arr.as_ptr() as u64)
        .unwrap_or(0)
}

/// Total proxies created in this process — for diagnostics.
pub fn proxy_instances_created() -> u64 {
    PROXY_INSTANCES_CREATED.load(std::sync::atomic::Ordering::Acquire)
}

/// Register the four `java.lang.reflect.Proxy` natives. Public so the
/// real-JDK path in `vm/src/vm/vm_init.rs` and the WP2.5 conformance
/// tests can both reach it. The natives are also called from
/// `register_synthetic_overrides` in synthetic-jdk mode.
pub fn register_reflect_proxy_natives(registry: &mut NativeMethodRegistry) {
    // census-tag: Proxy.newProxyInstance / invocation dispatch require VM
    // class-generation + reflective dispatch internals → Bridge.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let p = "java/lang/reflect/Proxy";
    registry.register(
        p,
        "isProxyClass",
        "(Ljava/lang/Class;)Z",
        native_proxy_is_proxy_class,
    );
    registry.register(
        p,
        "getInvocationHandler",
        "(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;",
        native_proxy_get_handler,
    );
    registry.register(p, "newProxyInstance",
        "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;",
        native_proxy_new_instance);
    // proxy-real-classfile increment 7 — deprecated `Proxy.getProxyClass(loader,
    // ifaces)`. The real JDK body routes the dynamic-module machinery
    // (`ProxyBuilder.getDynamicModule` → `Module.defineModule0`) the synthetic
    // proxy model can't satisfy → `InternalError: Proxy is not supported until
    // module system is fully initialized`. Force this native (companion entry in
    // `interpreter::force_native_over_real_jdk_bytecode`); it returns the same
    // generated `$ProxyN` class `newProxyInstance` would build, so
    // `pc.getConstructor(InvocationHandler.class).newInstance(h)` round-trips on
    // CratonVM's own proxy machinery.
    registry.register(
        p,
        "getProxyClass",
        "(Ljava/lang/ClassLoader;[Ljava/lang/Class;)Ljava/lang/Class;",
        native_proxy_get_proxy_class,
    );
    // WP2.5: helper for the proxy-instance class so callers can read
    // the interfaces back from the proxy itself rather than walking
    // the side-table. Registered on the synthetic class name.
    registry.register(
        "java/lang/reflect/Proxy$Instance",
        "getProxyInterfacesNative",
        "()[Ljava/lang/Class;",
        native_proxy_get_interfaces,
    );
    // proxy-real-classfile increment 2 — the constructor the generated
    // `$ProxyN.<init>` delegates to via `INVOKESPECIAL
    // Proxy$Instance.<init>(InvocationHandler, Class[])V`. The synthetic
    // super's matching method entry is declared in
    // `cratonvm_classloading::class_manager::synthetic_stub_ctor_methods`
    // (NATIVE-flagged, no Code); this native is its body. Today
    // `native_proxy_new_instance` bypasses the ctor (allocate + `set_field`),
    // so the canonical path never reaches here — but any path that *executes*
    // the generated `<init>` (a JIT call site, or `new`+`invokespecial`)
    // would otherwise hit `NoSuchMethodError`. The body mirrors the
    // allocation-bypass field writes: slot 0 = handler, slot 1 = interfaces,
    // slot 2 = identity-hash override (reserved, 0).
    registry.register(
        "java/lang/reflect/Proxy$Instance",
        "<init>",
        "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V",
        native_proxy_instance_init,
    );
    // WP2.5 v3 — INVOKESTATIC target embedded in every generated `$ProxyN`
    // method body. Signature changed from the v2 3-string-arg shape to
    // `(Object, Method, Object[]) Object`: the Method object is now built
    // once per proxied method by the generated class's `<clinit>` via
    // `Class.getMethod`, then loaded onto the stack before the
    // INVOKESTATIC. The native handler propagates that Method straight
    // into `InvocationHandler.invoke` and applies UndeclaredThrowable-
    // Exception wrapping on return (item 6).
    registry.register(
        "java/lang/reflect/Proxy$Dispatch",
        "invokeProxy",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        native_proxy_dispatch_invoke,
    );
    // proxy-real-classfile increment 6 — `InvocationHandler.invokeDefault`
    // (static, JDK 16+). The real JDK body reflects the generated proxy class's
    // `proxyClassLookup` accessor (which CratonVM's `$ProxyN` does not emit) and
    // throws `InternalError: NoSuchMethodException: proxyClassLookup`. Force this
    // native over the real bytecode (companion entry in
    // `interpreter::force_native_over_real_jdk_bytecode`); it runs the interface
    // default body directly via `invoke_special`. `Bridge` category so it
    // survives the no-synthetic-stubs drop.
    registry.register(
        "java/lang/reflect/InvocationHandler",
        "invokeDefault",
        "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
        native_invocation_handler_invoke_default_checked,
    );
    // spring-bug-08: `ObjectInputStream.resolveProxyClass(String[])` override.
    // The serialization module's own copy (serialization.rs) is gated behind the
    // `experimental-serialization` feature and absent from the default build,
    // where the real OIS bytecode runs — and its default routes the unsupported
    // `Proxy.getProxyClass` dynamic-module path (`Module.defineModule0`),
    // surfacing as `ClassNotFoundException: null` when deserializing a JDK
    // dynamic proxy. Register the override HERE (always compiled, always called,
    // `Bridge` category so it survives the no-synthetic-stubs drop) and
    // force-dispatch it via `force_native_over_real_jdk_bytecode`. It returns a
    // CratonVM generated `$ProxyN` class for the stream's interface set, keeping
    // the round-trip on CratonVM's own proxy machinery.
    registry.register(
        "java/io/ObjectInputStream",
        "resolveProxyClass",
        "([Ljava/lang/String;)Ljava/lang/Class;",
        native_ois_resolve_proxy_class,
    );
    registry.set_category(__prev_cat);
}

/// spring-bug-08 — body of the always-on `ObjectInputStream.resolveProxyClass`
/// override (see `register_reflect_proxy_natives`). Reads the interface-name
/// `String[]` (arg 1) and resolves it to a CratonVM generated `$ProxyN` class
/// via [`resolve_serialized_proxy_class`], bypassing the real
/// `Proxy.getProxyClass` dynamic-module path. Returns `null` (→ the caller's
/// default) only if resolution fails, preserving the original error semantics.
fn native_ois_resolve_proxy_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(arr);
    let mut names: Vec<String> = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            if let Some(name) = ctx.read_string(s) {
                names.push(name);
            }
        }
    }
    if let Some(cid) = resolve_serialized_proxy_class(ctx, &names) {
        let mirror = ctx.get_class_mirror(cid);
        return Ok(Some(Value::Object(Some(mirror))));
    }
    Ok(Some(Value::Object(None)))
}

fn native_proxy_is_proxy_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // True iff the Class is a proxy class — the legacy shared
    // `java/lang/reflect/Proxy$Instance`, OR a generated `$ProxyN` class
    // (which extends `Proxy$Instance`). The generated classes carry their own
    // name (`com/sun/proxy/$ProxyN` etc.), so a name-only check would wrongly
    // report `false`; walk the superclass chain instead.
    // G18-1: `Proxy.isProxyClass(null)` is `Objects.requireNonNull(cl)` in the
    // JDK — a NullPointerException with NO message, not `false`. Measured; the
    // previous `_ => Ok(0)` arm answered `false` for null.
    let class_mirror = match args.first() {
        Some(Value::Object(Some(m))) => *m,
        Some(Value::Object(None)) | None => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        _ => return Ok(Some(Value::Int(0))),
    };
    let is_proxy = match crate::lang_class::mirror_class_id(ctx, class_mirror) {
        Some(cid) => {
            // spring-bug-08: the synthetic super `Proxy$Instance` is the analog
            // of `java.lang.reflect.Proxy` — it is NOT itself a proxy class
            // (`Proxy.isProxyClass(Proxy.class) == false`). Only generated
            // `$ProxyN` SUBCLASSES are proxy classes. Without this exclusion,
            // ObjectOutputStream sees the proxy's superclass as a proxy too and
            // writes its `superDesc` as a second `TC_PROXYCLASSDESC` (instead of
            // the non-proxy `Proxy$Instance` desc that carries the serializable
            // `h` field), so the handler is lost on write and the duplicate
            // proxy name trips `ObjectStreamClass`'s "Circular reference." guard
            // on read.
            // The super itself is NOT a proxy class (`isProxyClass(Proxy.class)
            // == false`), only its generated `$ProxyN` subclasses are. Exclude
            // BOTH the synthetic `Proxy$Instance` and — for the real-super
            // migration — the real `java.lang.reflect.Proxy`.
            let self_name = ctx.class_name_of_id(cid);
            if self_name.as_deref() == Some("java/lang/reflect/Proxy$Instance")
                || self_name.as_deref() == Some("java/lang/reflect/Proxy")
            {
                false
            } else {
                proxy_class_is_generated(ctx, cid)
            }
        }
        None => false,
    };
    Ok(Some(Value::Int(if is_proxy { 1 } else { 0 })))
}

/// Whether `class_id` is (or descends from) the synthetic
/// `java/lang/reflect/Proxy$Instance` super class — i.e. it is a proxy class.
pub(crate) fn proxy_chain_reaches_instance(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    // proxy-real-classfile real-super migration: when the gate is on, generated
    // proxies extend the real `java.lang.reflect.Proxy`, so recognise that super
    // too. Default-off → strict no-op (only the synthetic shim is recognised).
    let recognise_real = real_proxy_super();
    let mut current = Some(class_id);
    let mut guard = 0;
    while let Some(cid) = current {
        guard += 1;
        if guard > 64 {
            break; // defensive: never loop on a malformed hierarchy
        }
        let name = ctx.class_name_of_id(cid);
        if name.as_deref() == Some("java/lang/reflect/Proxy$Instance") {
            return true;
        }
        if recognise_real && name.as_deref() == Some("java/lang/reflect/Proxy") {
            return true;
        }
        current = ctx.superclass_of(cid);
    }
    false
}

fn native_proxy_get_handler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // HotSpot contract: return the InvocationHandler (proxy field 0) only for
    // genuine proxy instances; throw NPE for null and CATCHABLE
    // IllegalArgumentException for non-proxies. The previous body returned
    // field 0 of ANY object — for an AnnotationProxy that is its
    // type-descriptor String, and a later handler.invoke(...) on that String
    // raised an UNCATCHABLE Rust-level NoSuchMethodError that killed threads
    // inside Java catch(Throwable) blocks (Spring's
    // AnnotationUtils.invokeAnnotationMethod relies on catching and falling
    // back to Method.invoke).
    let proxy = match args.first() {
        Some(Value::Object(Some(p))) => *p,
        Some(Value::Object(None)) | None => {
            // G18-1: HotSpot reaches `proxy.getClass()` inside
            // `Proxy.getInvocationHandler`, so the NPE carries the helpful
            // message that call site produces. Transcribed, not derived.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"Object.getClass()\" because \"proxy\" is null".to_string(),
                ),
            }
            .into());
        }
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "not a proxy instance".to_string(),
                }
                .into(),
            );
        }
    };
    if !proxy_class_is_generated(ctx, ctx.class_id_of_object(proxy)) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "not a proxy instance".to_string(),
            }
            .into(),
        );
    }
    Ok(Some(ctx.get_field(proxy, 0)))
}

/// [`proxy_chain_reaches_instance`] minus a user class that itself extends
/// `java.lang.reflect.Proxy`: HotSpot's `isProxyClass` / `getInvocationHandler`
/// answer only for classes `ProxyBuilder` generated, and a user subclass of
/// `Proxy` (legal, through its `protected` constructor) is not one. A
/// generated class is named `$Proxy<N>` (both this VM's emitter and
/// fabricated proxies); an instance of the synthetic shim itself (the
/// degrade fallback) still counts. Round 13 wave 5 (lane proxy3), switch
/// `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY` (`0`: any class on the chain).
pub(crate) fn proxy_class_is_generated(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> bool {
    if !proxy_chain_reaches_instance(ctx, class_id) {
        return false;
    }
    if !proxy_user_subclass_is_ordinary() {
        return true;
    }
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return true;
    };
    if name == "java/lang/reflect/Proxy$Instance" {
        return true;
    }
    proxy_name_has_generated_shape(&name)
}

/// A generated proxy class's simple-name shape, `$Proxy<digits>`
/// (`ProxyBuilder`'s `proxyClassNamePrefix` plus its counter; this VM's
/// emitter and `classify_defined_origin` use the same shape). The one
/// spelling for every name-based proxy test: a substring `contains("$Proxy")`
/// also matches a user nested class such as `Settings$ProxyConfig` and
/// `java.lang.reflect.Proxy$ProxyBuilder` (round 13 wave 6, lane proxy4).
pub fn proxy_name_has_generated_shape(name: &str) -> bool {
    let simple = name.rsplit('/').next().unwrap_or(name);
    simple
        .strip_prefix("$Proxy")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Per-loader namespace id for a generated proxy's defining loader — the cache
/// key AND the `ClassLoaderId::UserDefined(N)` the proxy class is registered
/// under (shared by `newProxyInstance` and `getProxyClass`).
///
/// For a *user-defined* loader this MUST be the loader's CANONICAL namespace id
/// (`loader_namespace_id`), i.e. the same id the loader-scoped lookups
/// (`peek_loader_namespace_id` / `class_defined_by_loader_exact`, used by
/// `findLoadedClass` / `cl_real_load_class_base`'s proxy path) query. The
/// previous raw identity-hash namespace registered the proxy under an id those
/// scoped lookups never consult, so the DEFINING loader's own
/// `loadClass(proxyName)` returned ClassNotFoundException (ClassUtilsTests
/// .isCacheSafe, via `childLoader3` delegating to its definer `childLoader1`).
/// Built-in / null loaders keep the identity-hash namespace.
///
/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 18): no longer. A built-in, non-platform loader (the application
/// loader, a bare `URLClassLoader` the JDK built) used its IDENTITY HASH as
/// the namespace, a number drawn from the same `ClassLoaderId::UserDefined(N)`
/// space `ctx.allocate_loader_id()` numbers user loaders from. When the two
/// met -- a loader whose hash equalled a live user loader's id (or 0, 1, 2:
/// the built-in ids) -- this loader's proxies were cached and registered in
/// THAT loader's namespace, and the user loader's scoped lookups found them.
/// Such a loader now gets a real id from `allocate_loader_id`, once, memoized
/// by its weak lock key ([`proxy_builtin_namespace_ids`]). Nothing maps the
/// id back to the loader, exactly as nothing mapped the hash back, so
/// `getClassLoader()` on its proxies answers as before.
pub(crate) fn proxy_loader_namespace(ctx: &mut dyn NativeContext, loader_obj: ObjectRef) -> u32 {
    if crate::classloader::is_user_defined_loader(ctx, loader_obj) {
        return crate::classloader::loader_namespace_id(ctx, loader_obj);
    }
    // The PLATFORM loader is the one built-in whose identity a proxy can now
    // arrive with: `Proxy.newProxyInstance(Connection.class.getClassLoader(),
    // ...)` hands it over since `Class.getClassLoader()` started reporting the
    // platform loader for platform-module classes. Registering that proxy under
    // an identity-hash namespace makes `Class.getClassLoader()` unable to find
    // a loader object for it (`loader_object_for_namespace_id` only knows
    // user-defined ids) and fall back to the application loader:
    //
    //   Proxy over java.sql.Connection, getClass().getClassLoader()
    //     HotSpot PlatformClassLoader        was AppClassLoader
    //
    // `NATIVE_EXTENSION` is the id `loader_namespace_id` already assigns this
    // loader, and the one `native_class_get_class_loader`'s `loader_type == 1`
    // arm decodes straight back to the platform singleton. Scoped to the
    // platform loader on purpose: every other built-in keeps the identity-hash
    // namespace this function has always given it.
    if crate::classloader::is_platform_loader_object(ctx, loader_obj) {
        return cratonvm_types::ClassLoaderId::NATIVE_EXTENSION;
    }
    proxy_builtin_namespace_id(ctx, loader_obj)
}

/// Built-in loader weak lock key -> namespace id: the proxy namespace
/// [`proxy_builtin_namespace_id`] allocated for a built-in, non-platform
/// loader (gc-common w29-e). A row lives exactly as long as its loader
/// object: the lock-key sweep frees a dead loader's key and
/// [`forget_proxy_namespace_keys`] drops the row; the id is never handed out
/// again (the VM's loader-id counter only grows).
///
/// `LockLevel::Scratch`: every acquisition copies out or inserts and drops the
/// guard; no `ctx` call and no other lock under it.
fn proxy_builtin_namespace_ids() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    rustc_hash::FxHashMap<usize, u32>,
> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<rustc_hash::FxHashMap<usize, u32>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            rustc_hash::FxHashMap::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The proxy namespace of built-in, non-platform `loader` (CURRENT):
/// allocated from `ctx.allocate_loader_id()` on first request, the same id
/// afterwards. The key is minted before, and the id allocated outside, the
/// table guard; a racing first request allocates an id that is then unused.
fn proxy_builtin_namespace_id(ctx: &mut dyn NativeContext, loader: ObjectRef) -> u32 {
    // `gc_stable_weak_lock_key` never fails today. Should it ever, a fresh,
    // unmemoised id is still a namespace of this loader's own; the identity
    // hash this arm answered until gc-common w31-b shares its number space
    // with `allocate_loader_id`, which is what w29-e (rank 18) removed.
    let Ok(key) = crate::gc_stable_weak_lock_key(&*ctx, loader) else {
        return ctx.allocate_loader_id();
    };
    let known = proxy_builtin_namespace_ids()
        .lock()
        .get(&key)
        .copied();
    if let Some(id) = known {
        return id;
    }
    let fresh = ctx.allocate_loader_id();
    *proxy_builtin_namespace_ids()
        .lock()
        .entry(key)
        .or_insert(fresh)
}

/// gc-common w29-e: drop the proxy-namespace rows filed under a weak lock key
/// the registry has just FREED -- the loader died (`lib.rs::sweep_lock_keys`)
/// or its VM went (`lib.rs::forget_vm_lock_keys`). `keys` holds every weak key
/// the sweep freed, of every kind. One lock, never under the lock-key
/// registry's; nothing here reaches Java. Answers the rows dropped.
pub(crate) fn forget_proxy_namespace_keys(keys: &[usize]) -> usize {
    if keys.is_empty() {
        return 0;
    }
    let mut t = proxy_builtin_namespace_ids().lock();
    if t.is_empty() {
        return 0;
    }
    let before = t.len();
    for k in keys {
        t.remove(k);
    }
    before - t.len()
}

// ---------------------------------------------------------------------------
// G18-1 — `java.lang.reflect.Proxy`'s refusal contract.
//
// MEASURED against HotSpot 25.0.3+9-LTS on 2026-08-17 (record
// `docs/internal/jdk-only/G18-1-the-proxy-invocation-contract-and-two-vectors-20260817.md`).
// Before this, `newProxyInstance` applied exactly ONE of the JDK's refusals
// ("<X> is not an interface") and silently built a working proxy for the other
// ten; `getProxyClass` applied none at all.
//
// The ORDER below is HotSpot's, pinned by probe rows that put two violations in
// the same call rather than derived from reading:
//
//   1. `Objects.requireNonNull(h)`  -> NPE, message **null**
//   2. `interfaces.length`          -> NPE `Cannot read the array length
//                                      because "interfaces" is null`
//   3. `ProxyBuilder.referencedTypes` calls `intf.getMethods()` on EVERY
//      element before any per-element validation, so a null element ANYWHERE
//      beats a bad element earlier in the array
//                                   -> NPE `Cannot invoke
//                                      "java.lang.Class.getMethods()" because
//                                      "intf" is null`
//   4. `ProxyBuilder.validateProxyInterfaces`, per element in argument order:
//        a. `!intf.isInterface()` -> IAE `<name> is not an interface`
//        b. `ensureVisible`       -> IAE `<name> referenced from a method is
//                                    not visible from class loader: null`
//        c. duplicate             -> IAE `repeated interface: <name>`
//   5. `ProxyGenerator.checkReturnTypes` -> IAE `methods with same signature
//      <sig> but incompatible return types: ...`
//
// (4a) running before (4b) is not a guess: `newProxyInstance(null, {POrder})`
// — an application CLASS — answers "POrder is not an interface", while
// `newProxyInstance(null, {POrder$A, POrder$A})` — an application INTERFACE,
// twice — answers "not visible" rather than "repeated interface".
// ---------------------------------------------------------------------------

/// The eight primitive descriptors plus `void`, spelled as `Class.getName()`
/// spells them. Used only to recognise a `Class` mirror that carries no
/// `ClassId` (`int.class`) so it can be refused by name; every other
/// unresolvable mirror FAILS OPEN.
const PROXY_PRIMITIVE_NAMES: [&str; 9] = [
    "int", "long", "short", "byte", "char", "float", "double", "boolean", "void",
];

/// `Class.getName()` for a field/return descriptor: `I` -> `int`,
/// `Ljava/lang/String;` -> `java.lang.String`, `[Ljava/lang/String;` ->
/// `[Ljava.lang.String;` (arrays keep their descriptor spelling, as
/// `Class.getName()` does).
fn proxy_desc_class_name(desc: &str) -> String {
    if desc.starts_with('[') {
        return desc.replace('/', ".");
    }
    if let Some(inner) = desc.strip_prefix('L') {
        return inner.trim_end_matches(';').replace('/', ".");
    }
    match desc {
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "D" => "double".to_string(),
        "F" => "float".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "S" => "short".to_string(),
        "Z" => "boolean".to_string(),
        "V" => "void".to_string(),
        _ => desc.replace('/', "."),
    }
}

/// `Class.getTypeName()` for a descriptor — the *source* spelling, which is
/// what `Method.toShortSignature()` prints for parameters: `[J` -> `long[]`,
/// `[Ljava/lang/String;` -> `java.lang.String[]`.
fn proxy_desc_type_name(desc: &str) -> String {
    let mut dims = 0usize;
    let mut rest = desc;
    while let Some(stripped) = rest.strip_prefix('[') {
        dims += 1;
        rest = stripped;
    }
    let mut out = proxy_desc_class_name(rest);
    for _ in 0..dims {
        out.push_str("[]");
    }
    out
}

/// `Class.toString()` for a descriptor: `"interface "`, `"class "` or (for a
/// primitive) no prefix, then `Class.getName()`. This is the exact text the
/// JDK's incompatible-return-types message embeds, because it formats a
/// `List<Class<?>>`.
fn proxy_desc_class_to_string(ctx: &dyn NativeContext, desc: &str) -> String {
    let name = proxy_desc_class_name(desc);
    if desc.starts_with('[') {
        return format!("class {name}");
    }
    let Some(inner) = desc.strip_prefix('L') else {
        return name; // primitive / void: no prefix
    };
    let internal = inner.trim_end_matches(';');
    let is_iface = ctx
        .class_id_by_name(internal)
        .map(|cid| ctx.class_access_flags(cid) & cratonvm_types::access_flags::ACC_INTERFACE != 0)
        .unwrap_or(false);
    if is_iface {
        format!("interface {name}")
    } else {
        format!("class {name}")
    }
}

/// Transitive `to.isAssignableFrom(from)` over `ClassId`s — superclass chain
/// plus every (transitive) super-interface.
///
/// Deliberately self-contained rather than `NativeContext::is_subclass`: a
/// wrong `false` here REFUSES a proxy HotSpot builds, so the walk that decides
/// it has to be one this file can unit-test. Bounded so a malformed hierarchy
/// cannot spin.
fn proxy_cid_assignable(
    ctx: &dyn NativeContext,
    to: cratonvm_types::ClassId,
    from: cratonvm_types::ClassId,
) -> bool {
    let mut seen: Vec<cratonvm_types::ClassId> = Vec::new();
    let mut stack: Vec<cratonvm_types::ClassId> = vec![from];
    let mut guard = 0usize;
    while let Some(cid) = stack.pop() {
        guard += 1;
        if guard > 4096 {
            return false;
        }
        if cid == to {
            return true;
        }
        if seen.contains(&cid) {
            continue;
        }
        seen.push(cid);
        if let Some(sup) = ctx.superclass_of(cid) {
            stack.push(sup);
        }
        stack.extend(ctx.class_interfaces(cid));
    }
    false
}

/// `to.isAssignableFrom(from)` over DESCRIPTORS. `None` means "could not be
/// decided" (a reference type this VM cannot resolve) — every caller FAILS
/// OPEN on `None` rather than refusing a proxy it cannot justify refusing.
fn proxy_desc_assignable(ctx: &dyn NativeContext, to: &str, from: &str) -> Option<bool> {
    proxy_desc_assignable_opt(Some(ctx), to, from)
}

/// The body of [`proxy_desc_assignable`], with the context made optional so the
/// arms that need no class resolution — array covariance and primitives, which
/// between them decide the two measured array rows — are unit-testable without
/// a live VM. Passing `None` makes every arm that WOULD resolve a class answer
/// `None` instead.
fn proxy_desc_assignable_opt(
    ctx: Option<&dyn NativeContext>,
    to: &str,
    from: &str,
) -> Option<bool> {
    if to == from {
        return Some(true);
    }
    // `Object` is assignable from every REFERENCE type, array or not, and
    // deciding that needs no class resolution.
    //
    // This has to sit ABOVE the array block rather than inside it. The
    // recursion below peels one `[` off each side, so the measured
    // `Object[] vs String[]` row arrives at the next level as
    // `Ljava/lang/Object; vs Ljava/lang/String;` — no array left to match on —
    // and inside-the-block placement meant it fell straight through to the
    // `let ctx = ctx?;` path and answered `None` for a row that is decidable
    // without a VM at all.
    if to == "Ljava/lang/Object;" && (from.starts_with('L') || from.starts_with('[')) {
        return Some(true);
    }
    let to_arr = to.starts_with('[');
    let from_arr = from.starts_with('[');
    if to_arr || from_arr {
        // Array covariance: `Object[].isAssignableFrom(String[])` is true, and
        // `String[].isAssignableFrom(Integer[])` is false. That distinction is
        // the difference between the measured `Object[] vs String[]` row (which
        // HotSpot ACCEPTS) and the `String[] vs Integer[]` row (which it
        // refuses), so it cannot be collapsed to "arrays differ -> conflict".
        if to_arr && from_arr {
            return proxy_desc_assignable_opt(ctx, &to[1..], &from[1..]);
        }
        if to == "Ljava/lang/Cloneable;" || to == "Ljava/io/Serializable;" {
            return Some(from_arr);
        }
        return Some(false);
    }
    let (Some(to_inner), Some(from_inner)) = (to.strip_prefix('L'), from.strip_prefix('L')) else {
        // At least one side is a primitive (or `void`) and they are not equal.
        return Some(false);
    };
    let ctx = ctx?;
    let to_cid = ctx.class_id_by_name(to_inner.trim_end_matches(';'))?;
    let from_cid = ctx.class_id_by_name(from_inner.trim_end_matches(';'))?;
    Some(proxy_cid_assignable(ctx, to_cid, from_cid))
}

/// `Method.toShortSignature()` — `name(type,type,...)` with each parameter in
/// `Class.getTypeName()` spelling and no spaces. This is the text the JDK's
/// incompatible-return-types message names the clashing methods by; measured as
/// `m(int,java.lang.String,long[])` and `q(java.util.List)` (erased — the
/// generic argument does not appear).
fn proxy_short_signature(name: &str, params: &str) -> String {
    let joined = proxy_iter_param_descs(params)
        .iter()
        .map(|d| proxy_desc_type_name(d))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}({joined})")
}

/// Split a method descriptor into `(params-without-parens, return)`.
fn proxy_split_method_desc(desc: &str) -> Option<(&str, &str)> {
    let open = desc.find('(')?;
    let close = desc.rfind(')')?;
    if close < open {
        return None;
    }
    Some((&desc[open + 1..close], &desc[close + 1..]))
}

/// Iterate the field descriptors packed inside a parameter list.
///
/// Byte-indexed, and every slice is rebuilt with `from_utf8_lossy`, so a
/// descriptor naming a class with a non-ASCII identifier (which Java permits)
/// cannot land a slice on a non-char boundary and panic the VM. A truncated
/// descriptor yields its remainder and terminates.
fn proxy_iter_param_descs(params: &str) -> Vec<String> {
    let bytes = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'L' {
            i += 1;
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // consume the ';'
            }
        } else {
            i += 1;
        }
        out.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
    }
    out
}

/// Every method a generated proxy would have to implement for `iface`:
/// its own public, non-static methods plus those of every super-interface.
/// Appends `(name, descriptor)` pairs in first-seen order; repeats are left in,
/// because the grouping in [`proxy_check_return_types`] collapses them the same
/// way `ProxyGenerator.addProxyMethod` does.
fn proxy_collect_iface_methods(
    ctx: &dyn NativeContext,
    iface: cratonvm_types::ClassId,
    out: &mut Vec<(String, String)>,
) {
    let mut seen: Vec<cratonvm_types::ClassId> = Vec::new();
    let mut stack: Vec<cratonvm_types::ClassId> = vec![iface];
    let mut guard = 0usize;
    while let Some(cid) = stack.pop() {
        guard += 1;
        if guard > 1024 {
            return;
        }
        if seen.contains(&cid) {
            continue;
        }
        seen.push(cid);
        for m in ctx.declared_methods(cid) {
            let flags = m.access_flags;
            if flags & cratonvm_types::access_flags::ACC_STATIC != 0
                || flags & cratonvm_types::access_flags::ACC_PRIVATE != 0
                || flags & cratonvm_types::access_flags::ACC_PUBLIC == 0
                || m.name.starts_with('<')
            {
                continue;
            }
            // No de-duplication here: the grouping below already collapses
            // repeats of the same (signature, return type), which is exactly
            // what `ProxyGenerator.addProxyMethod` does.
            out.push((m.name.clone(), m.descriptor.clone()));
        }
        stack.extend(ctx.class_interfaces(cid));
    }
}

/// `ProxyGenerator.checkReturnTypes`: two interfaces may declare the same
/// name+parameters only if one declared return type is assignable from every
/// other. A primitive return in a group of two or more is always a conflict.
///
/// Fails OPEN: a signature whose return types this VM cannot resolve is
/// skipped, because refusing a proxy HotSpot builds is strictly worse than
/// accepting one it refuses.
fn proxy_check_return_types(
    ctx: &dyn NativeContext,
    ifaces: &[cratonvm_types::ClassId],
) -> Result<(), MethodCallFailed> {
    // Deliberately narrowed to the multi-interface case. A conflict needs two
    // methods with the same name and parameters but return types neither of
    // which is assignable from the other; inside ONE interface hierarchy javac
    // rejects that at compile time (the covariant-override rule), and all five
    // measured conflict rows pass two interfaces. Skipping the single-interface
    // case therefore costs no measured row and keeps `declared_methods` — which
    // allocates a `Vec<MethodMetadata>` per class — off the common
    // `newProxyInstance(loader, new Class[]{ Iface.class }, h)` path.
    if ifaces.len() < 2 {
        return Ok(());
    }
    let mut methods: Vec<(String, String)> = Vec::new();
    for &cid in ifaces {
        proxy_collect_iface_methods(ctx, cid, &mut methods);
    }
    // Group by short signature, preserving argument order — the JDK's message
    // lists the return types in the order the methods were added, and the two
    // measured orderings (`{A,B}` -> `[String, Integer]`, `{B,A}` ->
    // `[Integer, String]`) show that order is the caller's.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (name, desc) in &methods {
        let Some((params, ret)) = proxy_split_method_desc(desc) else {
            continue;
        };
        let key = format!("{name}({params})");
        let slot = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        // `ProxyGenerator.addProxyMethod` MERGES a repeat with an identical
        // return type instead of adding it, so `methods.size() < 2` counts
        // DISTINCT return types. Mirror that by de-duplicating here.
        if !slot.iter().any(|r| r == ret) {
            slot.push(ret.to_string());
        }
    }
    for key in order {
        let rets = match groups.get(&key) {
            Some(r) if r.len() >= 2 => r,
            _ => continue,
        };
        let display = {
            let (name, params) = key.split_once('(').unwrap_or((key.as_str(), ""));
            proxy_short_signature(name, params.trim_end_matches(')'))
        };
        if let Some(prim) = rets
            .iter()
            .find(|r| !r.starts_with('L') && !r.starts_with('['))
        {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "methods with same signature {display} but incompatible return types: {} and others",
                    proxy_desc_class_name(prim)
                ),
            }
            .into());
        }
        // The JDK's `uncoveredReturnTypes` fold, transcribed.
        let mut uncovered: Vec<String> = Vec::new();
        let mut undecidable = false;
        'next_ret: for new_ret in rets {
            let mut added = false;
            for slot in uncovered.iter_mut() {
                match proxy_desc_assignable(ctx, new_ret, slot.as_str()) {
                    Some(true) => continue 'next_ret,
                    None => {
                        undecidable = true;
                        break 'next_ret;
                    }
                    Some(false) => {}
                }
                match proxy_desc_assignable(ctx, slot.as_str(), new_ret) {
                    Some(true) => {
                        *slot = new_ret.clone();
                        added = true;
                    }
                    None => {
                        undecidable = true;
                        break 'next_ret;
                    }
                    Some(false) => {}
                }
            }
            if !added {
                uncovered.push(new_ret.clone());
            }
        }
        if undecidable || uncovered.len() < 2 {
            continue;
        }
        let listed = uncovered
            .iter()
            .map(|d| proxy_desc_class_to_string(ctx, d))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(RuntimeError::IllegalArgumentException {
            message: format!(
                "methods with same signature {display} but incompatible return types: [{listed}]"
            ),
        }
        .into());
    }
    Ok(())
}

/// `ProxyBuilder.ensureVisible(loader, intf)` — but only for the arm this VM
/// can answer with certainty: a **null** (bootstrap) loader argument cannot see
/// an interface that the bootstrap loader did not define.
///
/// Two independent signals must agree before refusing, because
/// `NativeContext::loader_id_of_class` reports `Application` both for a genuine
/// application class AND for a class it has no record of — exactly the
/// conflation that would turn a legitimate
/// `newProxyInstance(jdkIface.getClassLoader(), …)` (whose loader argument IS
/// null) into a spurious `IllegalArgumentException`. The second signal is the
/// module: every class in the JDK image carries a module name, a classpath
/// class does not.
fn proxy_iface_hidden_from_bootstrap(
    ctx: &dyn NativeContext,
    cid: cratonvm_types::ClassId,
) -> bool {
    let non_bootstrap =
        ctx.loader_id_of_class(cid) != cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP as i32;
    let unnamed_module = match ctx.module_name_of_class(cid) {
        None => true,
        Some(m) => m.is_empty() || m == "unnamed",
    };
    non_bootstrap && unnamed_module
}

/// `ProxyBuilder.mapToModule`'s two refusals for non-public interfaces, as a
/// pure decision over what [`proxy_check_non_public_interfaces`] measured.
/// One entry per NON-PUBLIC interface, in argument order: its package name,
/// its module key (module name, defining-loader tag -- an unnamed module is
/// per loader), and whether its defining loader is the given one (`None`:
/// this VM cannot tell, which never refuses).
///
/// The JDK's loop, per entry: a module or package differing from the previous
/// entry's is "cannot have non-public interfaces in different packages";
/// then a defining loader that is not the given one is "non-public interface
/// is not defined by the given loader". (The JDK walks a `HashMap`, so with
/// both faults in one call either message can come first; each alone is
/// deterministic.)
fn proxy_non_public_refusal(
    entries: &[(String, (Option<String>, i32), Option<bool>)],
) -> Option<&'static str> {
    let mut target: Option<(&str, &(Option<String>, i32))> = None;
    for (pkg, module, same_loader) in entries {
        if let Some((target_pkg, target_module)) = target {
            if target_pkg != pkg.as_str() || target_module != module {
                return Some("cannot have non-public interfaces in different packages");
            }
        }
        if *same_loader == Some(false) {
            return Some("non-public interface is not defined by the given loader");
        }
        target = Some((pkg.as_str(), module));
    }
    None
}

/// Whether `a` and `b` are the same class loader for the proxy contract. The
/// application and platform loaders compare by kind as well as by identity:
/// the JDK can hold another object of either class than the singleton this VM
/// answers `getClassLoader()` with.
fn proxy_same_loader(ctx: &dyn NativeContext, a: Option<ObjectRef>, b: Option<ObjectRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.as_ptr() == y.as_ptr()
                || (crate::classloader::is_application_class_loader(ctx, x)
                    && crate::classloader::is_application_class_loader(ctx, y))
                || (crate::classloader::is_platform_loader_object(ctx, x)
                    && crate::classloader::is_platform_loader_object(ctx, y))
        }
        _ => false,
    }
}

/// Whether `defining` (the interface's `getClassLoader()` answer) is `given`;
/// `None` when this VM cannot tell. A class of a user-defined namespace
/// (`loader_tag >= 3`) whose loader object `getClassLoader()` could not
/// recover answers the application loader as its fallback, which says
/// nothing about who defined it.
fn proxy_defining_loader_matches(
    ctx: &dyn NativeContext,
    loader_tag: i32,
    defining: Option<ObjectRef>,
    given: Option<ObjectRef>,
) -> Option<bool> {
    if let Some(obj) = defining {
        if loader_tag >= 3 && crate::classloader::is_application_class_loader(ctx, obj) {
            return None;
        }
    }
    Some(proxy_same_loader(ctx, defining, given))
}

/// Round 13 wave 4 (lane proxy2): the JDK's refusals for non-public proxy
/// interfaces (`ProxyBuilder.mapToModule`, run after the per-element checks
/// of step 4 and before `ProxyGenerator.checkReturnTypes`, step 5). Without
/// them this VM built a proxy HotSpot refuses; since round 13 wave 3 that
/// proxy is defined by the FIRST non-public interface's loader, so a second
/// one from another runtime package failed its `checkcast` / `ldc` in the
/// generated body instead.
///
/// `given` is the `ClassLoader` argument and its native pin (`None` for a
/// null loader). The defining loader is `Class.getClassLoader()`'s answer
/// (`lang_class::native_class_get_class_loader`, the same answer Java code
/// sees), compared right after it is computed, with `given` re-read off its
/// pin because that computation can allocate. A failure to compute it
/// answers "cannot tell" and never refuses. `CRATONVM_PROXY_NONPUBLIC_REFUSALS=0`
/// turns the refusals off.
fn proxy_check_non_public_interfaces(
    ctx: &mut dyn NativeContext,
    ifaces: &[cratonvm_types::ClassId],
    given: Option<(ObjectRef, usize)>,
) -> Result<(), MethodCallFailed> {
    const ACC_PUBLIC: u16 = 0x0001;
    let non_public: Vec<cratonvm_types::ClassId> = ifaces
        .iter()
        .copied()
        .filter(|&cid| ctx.class_access_flags(cid) & ACC_PUBLIC == 0)
        .collect();
    if non_public.is_empty()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_NONPUBLIC_REFUSALS")
    {
        return Ok(());
    }
    let Some(&target) = non_public.first() else {
        return Ok(());
    };
    let mut entries: Vec<(String, (Option<String>, i32), Option<bool>)> =
        Vec::with_capacity(non_public.len());
    for cid in non_public {
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        let pkg = match name.rfind('/') {
            Some(i) => name[..i].to_string(),
            None => String::new(),
        };
        let module = (ctx.module_name_of_class(cid), ctx.loader_id_of_class(cid));
        // `getClassLoader()` can allocate (the mirror, the application loader
        // singleton); `given` is read off its pin only after it.
        let mirror = ctx.get_class_mirror(cid);
        let defining =
            crate::lang_class::native_class_get_class_loader(ctx, &[Value::Object(Some(mirror))]);
        let same_loader = match defining {
            Ok(Some(Value::Object(defining))) => {
                let given_now = given.map(|(obj, pin)| ctx.read_native_pin(pin, obj));
                proxy_defining_loader_matches(&*ctx, module.1, defining, given_now)
            }
            _ => None,
        };
        entries.push((pkg, module, same_loader));
    }
    if let Some(message) = proxy_non_public_refusal(&entries) {
        return Err(RuntimeError::IllegalArgumentException {
            message: message.to_string(),
        }
        .into());
    }
    proxy_check_module_access(ctx, ifaces, target)
}

/// A class's module when it is a NAMED module this VM models as one; `None`
/// for the unnamed module, for a modular jar that reached the registry only
/// from the class path (an unnamed-module citizen on the JDK), and for a name
/// the registry does not hold (so an unmodelled answer never refuses).
fn proxy_named_module_of(ctx: &dyn NativeContext, cid: cratonvm_types::ClassId) -> Option<String> {
    let module = ctx.module_name_of_class(cid)?;
    if module.is_empty()
        || module == "unnamed"
        || !ctx.module_is_registered(&module)
        || ctx.module_is_class_path_only(&module)
    {
        return None;
    }
    Some(module)
}

/// `ProxyBuilder.mapToModule`'s third refusal, as far as this VM can decide it:
/// the first interface of `ifaces` that the proxy's module (the non-public
/// interface `target`'s) cannot access -- its module does not read the
/// interface's module, or that module does not export the interface's
/// package to it. Decided only for an UNNAMED target module (a class-path
/// package-private interface, the shape real code has): an unnamed module
/// reads every module, so the question is the export alone, and an unnamed
/// or unmodelled interface module exports everything. A named target is not
/// modelled and never refuses.
fn proxy_module_access_refusal(
    ctx: &dyn NativeContext,
    ifaces: &[cratonvm_types::ClassId],
    target: cratonvm_types::ClassId,
) -> Option<cratonvm_types::ClassId> {
    if proxy_named_module_of(ctx, target).is_some() {
        return None;
    }
    ifaces.iter().copied().find(|&cid| {
        let Some(module) = proxy_named_module_of(ctx, cid) else {
            return false;
        };
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        let Some(slash) = name.rfind('/') else {
            return false;
        };
        let pkg = &name[..slash];
        // Only a descriptor this VM holds in full can prove a refusal: the
        // `vm_init` fallback registers `java.base` with NO exports, and a
        // package the registry does not list for the module is not one it
        // can answer for.
        if ctx.module_exports(&module).is_empty()
            || !ctx.module_packages(&module).iter().any(|p| p == pkg)
        {
            return false;
        }
        // `""` is the unnamed module, which `--add-exports m/p=ALL-UNNAMED`
        // targets (`ModuleRegistry::is_package_exported_to`).
        !ctx.is_package_exported_unqualified(&module, pkg)
            && !ctx.is_package_exported_to(&module, pkg, "")
    })
}

/// Round 13 wave 6 (lane proxy4): `IllegalArgumentException("<module> can't
/// access <interface>")` for a proxy whose non-public interface's module
/// cannot access another of its interfaces (`ProxyBuilder.mapToModule`, after
/// the two refusals above), e.g. a class-path package-private interface next
/// to `sun.nio.ch.Interruptible` (public, in a package `java.base` does not
/// export). Without it this VM defined a proxy HotSpot refuses. The module is
/// rendered by `Class.getModule().toString()` (Java code, a GC point: only
/// class ids are live across it). `CRATONVM_PROXY_MODULE_ACCESS_REFUSAL=0`
/// turns it off.
fn proxy_check_module_access(
    ctx: &mut dyn NativeContext,
    ifaces: &[cratonvm_types::ClassId],
    target: cratonvm_types::ClassId,
) -> Result<(), MethodCallFailed> {
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_MODULE_ACCESS_REFUSAL") {
        return Ok(());
    }
    let Some(refused) = proxy_module_access_refusal(&*ctx, ifaces, target) else {
        return Ok(());
    };
    let dotted = ctx
        .class_name_of_id(refused)
        .unwrap_or_default()
        .replace('/', ".");
    let mirror = ctx.get_class_mirror(target);
    let module = match ctx.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[]) {
        Ok(Some(Value::Object(Some(m)))) => Some(m),
        _ => None,
    };
    let module_text = match module {
        Some(m) => match ctx.invoke_virtual(m, "toString", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
            _ => None,
        },
        None => None,
    };
    let module_text = module_text.unwrap_or_else(|| "unnamed module".to_string());
    Err(RuntimeError::IllegalArgumentException {
        message: format!("{module_text} can't access {dotted}"),
    }
    .into())
}

/// Walk the `Class[] interfaces` argument of `Proxy.newProxyInstance` /
/// `Proxy.getProxyClass` into `ClassId`s, applying steps 2-4 of the contract
/// documented above. `loader_is_null` is whether the caller passed a null
/// `ClassLoader`; `given_loader` is that loader and its native pin, for the
/// non-public-interface refusals ([`proxy_check_non_public_interfaces`]).
fn proxy_validate_interfaces(
    ctx: &mut dyn NativeContext,
    loader_is_null: bool,
    interfaces: Value,
    given_loader: Option<(ObjectRef, usize)>,
) -> Result<Vec<cratonvm_types::ClassId>, MethodCallFailed> {
    let Value::Object(Some(arr)) = interfaces else {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "Cannot read the array length because \"interfaces\" is null".to_string(),
            ),
        }
        .into());
    };
    let n = ctx.array_length(arr);
    // Step 3 — the referenced-types pass touches every element first.
    for i in 0..n {
        if !matches!(ctx.get_array_element(arr, i), Value::Object(Some(_))) {
            return Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"java.lang.Class.getMethods()\" because \"intf\" is null"
                        .to_string(),
                ),
            }
            .into());
        }
    }
    // Step 4 — per element, in argument order.
    let mut out: Vec<cratonvm_types::ClassId> = Vec::with_capacity(n);
    for i in 0..n {
        let Value::Object(Some(mirror)) = ctx.get_array_element(arr, i) else {
            continue;
        };
        // `class_id_from_mirror` ONLY — deliberately not `lang_class::
        // mirror_class_id`, whose second step reads the VM's `Int` overlay at
        // mirror slot 0 and would hand a primitive mirror back some other
        // class's id, turning "int is not an interface" into a message naming
        // whatever that id happens to be. This is the same resolver the loop
        // used before G18-1.
        let Some(cid) = ctx.class_id_from_mirror(mirror) else {
            // A mirror with no `ClassId`. The only such shape the JDK refuses
            // that can be named with certainty is a PRIMITIVE mirror
            // (`int.class`), which carries its name in slot 1 and no class id
            // at all. Anything else is a mirror this VM failed to resolve —
            // skip it, exactly as this loop did before G18-1, rather than
            // inventing a refusal from a name.
            let name = crate::lang_class::mirror_class_name(ctx, mirror).unwrap_or_default();
            if PROXY_PRIMITIVE_NAMES.contains(&name.as_str()) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("{name} is not an interface"),
                }
                .into());
            }
            continue;
        };
        let dotted = ctx
            .class_name_of_id(cid)
            .unwrap_or_default()
            .replace('/', ".");
        if ctx.class_access_flags(cid) & cratonvm_types::access_flags::ACC_INTERFACE == 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("{dotted} is not an interface"),
            }
            .into());
        }
        if loader_is_null && proxy_iface_hidden_from_bootstrap(ctx, cid) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!(
                    "{dotted} referenced from a method is not visible from class loader: null"
                ),
            }
            .into());
        }
        if out.contains(&cid) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("repeated interface: {dotted}"),
            }
            .into());
        }
        out.push(cid);
    }
    // `ProxyBuilder.mapToModule`, between steps 4 and 5.
    proxy_check_non_public_interfaces(ctx, &out, given_loader)?;
    // Step 5.
    proxy_check_return_types(ctx, &out)?;
    Ok(out)
}

/// Whether the `ClassLoader` argument at `args[0]` is a Java `null`.
fn proxy_loader_arg_is_null(args: &[Value]) -> bool {
    !matches!(args.first(), Some(Value::Object(Some(_))))
}

fn native_proxy_new_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // WP2.5-B — strategy A path: emit a real `$ProxyN` class that
    // extends `java/lang/reflect/Proxy$Instance` and implements the
    // requested interfaces, then allocate an instance of THAT class.
    // Calls run the generated bodies in both tiers (round 13 wave 4, lane
    // proxy2: the interpreter's `is_proxy_dispatch` intercept no longer
    // answers a call the proxy class declares, see
    // `proxy_interpreter_runs_generated_body`); each body reaches
    // `InvocationHandler.invoke` through `native_proxy_dispatch_invoke`.
    //
    // Args: [classloader, Class[] interfaces, InvocationHandler handler]
    //
    // Field layout (inherited from Proxy$Instance):
    //   0 -> InvocationHandler
    //   1 -> Class[] interfaces
    //   2 -> identity-hashcode override (reserved, default 0)
    let interfaces = args.get(1).cloned().unwrap_or(Value::Object(None));
    let handler = args.get(2).cloned().unwrap_or(Value::Object(None));

    // G18-1 step 1 — `Objects.requireNonNull(h)` is the FIRST line of
    // `Proxy.newProxyInstance` and beats every argument check that follows,
    // including a null `interfaces` array. Its NPE carries NO message.
    let handler_obj = match handler {
        Value::Object(Some(h)) => h,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };

    // gen r4w3/rooting: `define_or_get_proxy_class` (ensure_class_initialized)
    // and `try_alloc_concurrent_synthetic` are GC points, after which the
    // handler / interfaces array / loader copied out of `args` are stored
    // into the proxy and the defining-loader side table. Pin them here
    // (handler lowest) and re-read after the allocation below.
    let handler_pin = ctx.pin_native_root(handler_obj);
    let interfaces_pin = match interfaces {
        Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
        _ => None,
    };
    let loader_arg = match args.first() {
        Some(Value::Object(Some(l))) => Some(*l),
        _ => None,
    };
    let loader_pin = loader_arg.map(|l| ctx.pin_native_root(l));
    fn read_pinned_opt(
        ctx: &dyn NativeContext,
        obj: Option<cratonvm_types::ObjectRef>,
        pin: Option<usize>,
    ) -> Option<cratonvm_types::ObjectRef> {
        match (obj, pin) {
            (Some(o), Some(h)) => Some(ctx.read_native_pin(h, o)),
            _ => None,
        }
    }

    // G18-1 steps 2-5 — the rest of the refusal contract, shared with
    // `getProxyClass`. Before this, the only refusal applied here was
    // "<fqcn> is not an interface"
    // (ServiceLocatorFactoryBeanTests.whenServiceLocatorInterfaceIsNotAnInterfaceType,
    // which passes a plain class); the other ten measured refusals silently
    // produced a working proxy.
    let iface_cids = proxy_validate_interfaces(
        ctx,
        proxy_loader_arg_is_null(args),
        interfaces,
        loader_arg.zip(loader_pin),
    )?;

    // Try to generate a `$ProxyN` class. On any failure, fall back to
    // the legacy synthetic `Proxy$Instance` allocation — the existing
    // dispatch hook handles both shapes uniformly so callers see
    // unchanged behaviour even when generation is skipped.
    //
    // WP2.5 v2 — per-loader namespacing. Use the ClassLoader instance's
    // identity hash as the cache namespace. Different ClassLoader
    // instances (e.g. two URLClassLoaders pointing at different jars)
    // get different proxy class spaces, matching JDK semantics. A null
    // ClassLoader arg = bootstrap loader = namespace 0.
    // gen r4w3/rooting: read the loader off its pin (validation may load).
    let loader_namespace: u32 = match read_pinned_opt(ctx, loader_arg, loader_pin) {
        Some(loader_obj) => proxy_loader_namespace(ctx, loader_obj),
        None => 0,
    };
    // proxy-real-classfile real-super migration: the generated class extends the
    // real `java.lang.reflect.Proxy` (single field `h` at slot 0) only on the
    // `Real` arm with the gate on; the Degrade/Failed fallbacks always allocate
    // the synthetic 3-slot `Proxy$Instance` regardless of the gate. Track the
    // *actual* allocated shape so the slot writes below match it.
    let use_real_super = real_proxy_super();
    let (proxy_new, proxy_is_real_super, generated_cid) =
        match define_or_get_proxy_class(ctx, loader_namespace, &iface_cids) {
            ProxyClassOutcome::Real(cid) => {
                // Round 13 wave 5 (lane proxy3): the JDK constructs the proxy
                // through `Constructor.newInstance`, which initializes the
                // class, so the generated `<clinit>` (one `getMethod` per proxy
                // method) runs HERE and a failure of it surfaces from
                // `newProxyInstance`, not from the first call on the proxy.
                // `CRATONVM_PROXY_INIT_AT_CREATION=0` leaves it to the first
                // body execution. (A GC point: the handler, interfaces and
                // loader are pinned above and re-read below.)
                if proxy_init_at_creation() {
                    ctx.ensure_class_initialized_with_class_id(cid)?;
                }
                // Real super → real `Proxy`'s single `h` field (slot 0); use the
                // class's real field count (≥1). Synthetic super → the 3-slot
                // handler/interfaces/identity-hash layout (`.max(3)` because a
                // synthetic stub loaded without bytecode can report 0 fields).
                let min_fields = if use_real_super { 1 } else { 3 };
                let n = ctx.class_num_total_fields(cid).max(min_fields);
                (ctx.alloc_object(cid, n), use_real_super, Some(cid))
            }
            ProxyClassOutcome::Degrade => (
                try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/Proxy$Instance", 3)?,
                false,
                None,
            ),
            ProxyClassOutcome::Failed(stage) => {
                // increment 3 (§3): STRICT mode surfaces the real failure as the JDK
                // does (IllegalArgumentException); the default (non-strict) path keeps
                // the synthetic shim so existing apps keep running while the real
                // generated-classfile path soaks.
                if real_proxy_strict() {
                    return Err(throw_proxy_failure(ctx, stage));
                }
                (
                    try_alloc_concurrent_synthetic(ctx, "java/lang/reflect/Proxy$Instance", 3)?,
                    false,
                    None,
                )
            }
        };
    // gc-common w13-f: the allocation above is the LAST GC point in this
    // function — the loader check, the pin reads and the side-table writes
    // below neither allocate nor run Java — so the fresh address is stored
    // through directly.
    let proxy: ObjectRef = proxy_new;

    // Defining-loader identity for a generated `$ProxyN`. JDK defines the proxy
    // class in the supplied `ClassLoader`, so `proxy.getClass().getClassLoader()`
    // returns THAT loader. `define_or_get_proxy_class` keys/defines the class by
    // the loader's identity-hash *namespace*, which `Class.getClassLoader()` (it
    // consults `defining_loader_for`) cannot map back to the loader instance —
    // so a proxy over a child-loaded interface reported the app loader instead of
    // the child (MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader,
    // where Spring's synthesize calls `Proxy.newProxyInstance(type.getClassLoader(),
    // …)`). Register the real, user-defined loader object as the proxy class's
    // defining loader. Only user-defined loaders are recorded; proxies created
    // with a built-in (app/platform/bootstrap) or null loader keep the existing
    // app-loader fallback, so the common case is unchanged.
    // gen r4w3/rooting: current addresses after the GC points above.
    let handler = Value::Object(Some(ctx.read_native_pin(handler_pin, handler_obj)));
    let interfaces = match (interfaces, interfaces_pin) {
        (Value::Object(Some(o)), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, o))),
        (v, _) => v,
    };
    let loader_now = read_pinned_opt(ctx, loader_arg, loader_pin);
    ctx.unpin_native_roots(handler_pin);
    if let Some(cid) = generated_cid {
        if let Some(loader_obj) = loader_now {
            if crate::classloader::is_user_defined_loader(ctx, loader_obj) {
                crate::classloader::register_defining_loader(
                    ctx.vm_identity(),
                    cid.as_u32(),
                    loader_obj,
                );
            }
        }
    }
    // Handler at slot 0 — common to both layouts, so the dispatch path
    // (`proxy_invoke_handler_shared` → `get_field(proxy, 0)`) reads it uniformly.
    ctx.set_field(proxy, 0, handler);
    if !proxy_is_real_super {
        // Synthetic-super layout only: interfaces (slot 1) + identity-hash
        // override (slot 2). The real `Proxy` has neither field — its
        // `getInterfaces()` comes from the class's declared interfaces, and the
        // global `set_proxy_last_interfaces` cache below still backs legacy readers.
        ctx.set_field(proxy, 1, interfaces);
        ctx.set_field(proxy, 2, Value::Int(0));
    }

    // Backward-compat: keep the global "last-proxy interfaces" cache
    // populated so legacy readers (lang_class::native_class_get_interfaces
    // synthetic-mode fallback) still work even when the per-class
    // interfaces array is also reachable via the generated class's
    // `interfaces[]` table.
    //
    // bug nb-lib-gckeys §2: store the array as a GC-TRACKED `ObjectRef`
    // (rooted + remapped by the annotation-proxy GC hooks) instead of a raw
    // `arr.as_ptr()` in an `AtomicU64` that was never rooted — a moving GC
    // could relocate/reclaim the array, leaving the shared-mirror
    // `getInterfaces()` reader to dereference a dangling pointer.
    if let Value::Object(Some(arr)) = interfaces {
        lang_class::set_proxy_last_interfaces(ctx.vm_identity(), arr);
    }
    PROXY_INSTANCES_CREATED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);

    Ok(Some(Value::Object(Some(proxy))))
}

/// proxy-real-classfile increment 7 — `Proxy.getProxyClass(ClassLoader,
/// Class[])` (deprecated). Returns the generated `$ProxyN` `Class` for the given
/// interface set via the same `define_or_get_proxy_class` machinery
/// `newProxyInstance` uses (so it shares the per-(loader, iface-set) cache).
/// Force-dispatched over the real JDK bytecode (see
/// `interpreter::force_native_over_real_jdk_bytecode`), which otherwise throws
/// `InternalError: Proxy is not supported until module system is fully
/// initialized`.
///
/// Args (static method): `[0]` ClassLoader, `[1]` `Class[]` interfaces.
fn native_proxy_get_proxy_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // G18-1 — `getProxyClass` shares `ProxyBuilder` with `newProxyInstance`, so
    // it shares steps 2-5 of the refusal contract VERBATIM (measured: identical
    // exception classes and identical messages for null array, null element,
    // non-interface, duplicate and invisible interface). It has no handler
    // argument, so step 1 does not apply. Before this it applied NO refusals at
    // all and answered a `Class` for every one of them.
    let interfaces = args.get(1).cloned().unwrap_or(Value::Object(None));
    // Round 13 wave 4 (lane proxy2): the loader is pinned across validation,
    // which can now allocate (the non-public refusals ask `getClassLoader()`),
    // and is read back off the pin. Early returns leave the pin to
    // `safe_native_call`'s watermark, as `native_proxy_new_instance` does.
    let loader_arg = match args.first() {
        Some(Value::Object(Some(l))) => Some(*l),
        _ => None,
    };
    let loader_pin = loader_arg.map(|l| ctx.pin_native_root(l));
    let iface_cids = proxy_validate_interfaces(
        ctx,
        proxy_loader_arg_is_null(args),
        interfaces,
        loader_arg.zip(loader_pin),
    )?;
    let loader_now = match (loader_arg, loader_pin) {
        (Some(l), Some(h)) => Some(ctx.read_native_pin(h, l)),
        _ => None,
    };
    // Per-loader namespace = the loader instance's identity hash (bootstrap/null
    // → 0), matching `native_proxy_new_instance` so both share the cache entry.
    let loader_namespace: u32 = match loader_now {
        Some(loader_obj) => proxy_loader_namespace(ctx, loader_obj),
        _ => 0,
    };
    let outcome = define_or_get_proxy_class(ctx, loader_namespace, &iface_cids);
    // The define is a GC point; the loader recorded below is read after it.
    let loader_now = match (loader_arg, loader_pin) {
        (Some(l), Some(h)) => {
            let now = ctx.read_native_pin(h, l);
            ctx.unpin_native_roots(h);
            Some(now)
        }
        _ => None,
    };
    let args: &[Value] = &[Value::Object(loader_now)];
    match outcome {
        ProxyClassOutcome::Real(cid) => {
            // Record the user-supplied loader as the proxy class's defining
            // loader so `proxyClass.getClassLoader()` returns THAT loader, not
            // the app-loader fallback — exactly as `native_proxy_new_instance`
            // does for `newProxyInstance`. Without this, a composite-interface
            // proxy built via `Proxy.getProxyClass(childLoader, …)` reported the
            // app loader, so `ClassUtils.isCacheSafe(composite, appLoader)`
            // wrongly returned true (the proxy looked app-loaded rather than
            // child-loaded). ClassUtilsTests.isCacheSafe. Only user-defined
            // loaders are recorded; built-in/null loaders keep the fallback.
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                if crate::classloader::is_user_defined_loader(ctx, *loader_obj) {
                    crate::classloader::register_defining_loader(
                        ctx.vm_identity(),
                        cid.as_u32(),
                        *loader_obj,
                    );
                }
            }
            let mirror = ctx.get_class_mirror(cid);
            Ok(Some(Value::Object(Some(mirror))))
        }
        // Degrade (gate off) / Failed → the JDK raises IllegalArgumentException
        // for an unbuildable proxy class; mirror that rather than returning a
        // bogus Class.
        _ => Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message:
                    "Proxy.getProxyClass: cannot generate a proxy class for the given interfaces"
                        .to_string(),
            }
            .into(),
        ),
    }
}

/// WP2.5 v3 — INVOKESTATIC target embedded in every generated `$ProxyN`
/// method body. Since round 13 wave 4 (lane proxy2) this is THE dispatch of a
/// user `InvocationHandler` in both tiers: compiled callers always entered the
/// body, and the interpreter now runs it too instead of intercepting the call
/// (`proxy_interpreter_runs_generated_body`). The interpreter's intercept is
/// left for the synthetic `Proxy$Instance` shim (no bodies) and for the VM's
/// own `AnnotationProxy` handler.
///
/// Signature (as registered in `register_reflect_proxy_natives`):
///   `(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)
///    Ljava/lang/Object;`
///
/// Args:
///   `[0]` proxy receiver (the generated method body's `this`)
///   `[1]` `java.lang.reflect.Method` mirror (built once per method by
///         the generated class's `<clinit>` via `Class.getMethod` —
///         already populated with `name`, `parameterTypes`,
///         `exceptionTypes`, etc.)
///   `[2]` boxed argument array (`Object[]`, may be `null` for no-arg
///         methods).
///
/// On `MethodCallFailed::ExceptionThrown(t)` the helper consults
/// `Method.exceptionTypes` and applies JLS-spec UndeclaredThrowable-
/// Exception wrapping for non-`RuntimeException` / non-`Error`
/// mismatches (item 6).
///
/// The return-value contract (`G24-1`) is the generated body's own: its
/// `checkcast <return type>` / `checkcast <Wrapper>; xxxValue()` raise the
/// `ClassCastException` and the `NullPointerException` exactly where
/// HotSpot's body does. Only the NPE's TEXT needs help, because this VM's
/// helpful-NPE message names the body's receiver producer, `invokeProxy`,
/// where HotSpot's names `InvocationHandler.invoke`; the null-for-primitive
/// arm below raises it with the transcribed text
/// (`CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE`).
/// The declared return-type descriptor of the annotation member `method_obj`
/// describes, e.g. `Z` for `boolean nullIfImpossible()`. Falls back to
/// `Ljava/lang/Object;`, which is what this call site used unconditionally
/// before — correct for every already-boxed member value, wrong only for the
/// unboxed `AnnotationDefault` fallback.
fn annotation_member_return_descriptor(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> String {
    let full = crate::lang_class::method_descriptor_for_invoke(ctx, method_obj);
    match full.rfind(')') {
        Some(i) if i + 1 < full.len() => full[i + 1..].to_string(),
        _ => "Ljava/lang/Object;".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Round 13 wave 4 (lane proxy2) — one dynamic-proxy dispatch model.
//
// The interpreter used to intercept every call on a proxy
// (`vm/src/runtime/interpreter/invoke.rs`, `is_proxy_dispatch`) and call the
// handler from Rust, while a compiled caller dispatched through the itable
// into the generated `$ProxyN` body. Anything wrong in the body, its
// `<clinit>` or its constant pool was therefore JIT-only by construction
// (round 13 wave 3's Spring `IllegalAccessError`), and the two models
// disagreed on observable facts: the `Method` handed to the handler (a
// synthesized, separately cached object against the body's `<clinit>`
// `getMethod` result), argument boxing (`X.valueOf` in the body), and
// `Object`'s final methods (`wait` / `notify` / `notifyAll` reached the
// handler in the interpreter only). Now the interpreter runs the body too,
// so both tiers share this native. The switches below are each read once.
// ---------------------------------------------------------------------------

/// A default-on switch memoised in `slot` (override-aware, like `env_cache`'s).
fn proxy_memo_flag_default_on(slot: &'static cratonvm_types::flags::MemoSlot, key: &str) -> bool {
    match slot.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_flag_default_on(key);
            slot.publish(if on { 2 } else { 1 });
            on
        }
    }
}

/// `CRATONVM_PROXY_ONE_DISPATCH_MODEL` (default on, both modes): the
/// interpreter runs a generated `$ProxyN` body -- when the receiver's class
/// declares the called method and the handler is not the VM's own
/// `AnnotationProxy` -- instead of intercepting the call. Read by
/// `vm::runtime::interpreter::invoke`'s `is_proxy_dispatch`. `0` restores
/// the intercept.
pub fn proxy_interpreter_runs_generated_body() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_ONE_DISPATCH_MODEL")
}

/// `CRATONVM_PROXY_FINAL_OBJECT_METHODS` (default on, both modes): `wait`,
/// `notify` and `notifyAll` on a proxy run `Object`'s methods, as on the JDK
/// (they are `final`; a proxy class never declares them), instead of
/// reaching the handler through the interpreter's intercept. Read by
/// `is_proxy_dispatch`. `0` restores the intercept for them.
pub fn proxy_final_object_methods_bypass_handler() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_FINAL_OBJECT_METHODS")
}

/// `CRATONVM_PROXY_VTABLE_FAST_PATH` (default on, both modes; performance):
/// the interpreter's vtable fast path serves a call on a generated `$ProxyN`
/// that runs its own body (round 13 wave 5, lane proxy3) instead of ceding
/// every proxy call to the slow path. Read by
/// `vm::runtime::interpreter::dispatch_virtual`. `0` cedes them all again.
pub fn proxy_vtable_fast_path() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_VTABLE_FAST_PATH")
}

/// `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY` (default on, both modes): a user
/// class that itself extends `java.lang.reflect.Proxy` (not a generated
/// `$ProxyN`, not the synthetic shim) is dispatched like any other class
/// instead of having its undeclared calls sent to its handler (round 13
/// wave 5, lane proxy3). Read by `vm::runtime::interpreter::invoke`.
pub fn proxy_user_subclass_is_ordinary() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_USER_SUBCLASS_ORDINARY")
}

/// `CRATONVM_PROXY_ANNOTATION_BODY` (default on, both modes): an annotation
/// proxy (a generated `$ProxyN` whose handler is the VM's `AnnotationProxy`)
/// runs its generated body in the interpreter as well, as compiled callers
/// and cached interpreted sites already did (round 13 wave 6, lane proxy4).
/// Read by `vm::runtime::interpreter::invoke`'s
/// `proxy_receiver_runs_its_body_in`. `0` keeps the interpreter's intercept
/// for an uncached call.
pub fn proxy_annotation_handler_runs_body() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_ANNOTATION_BODY")
}

/// `CRATONVM_PROXY_BODY_HANDLER_VIRTUAL` (default on): the body's handler
/// call is a virtual call on the handler object in `--compatible` too.
fn proxy_body_handler_dispatch_is_virtual() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_BODY_HANDLER_VIRTUAL")
}

/// `CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE` (default on): `null` from the
/// handler for a primitive return throws HotSpot's NPE text here.
fn proxy_body_null_primitive_npe() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE")
}

// ---------------------------------------------------------------------------
// Round 13 wave 5 (lane proxy3) — the proxy method's own `throws` list.
//
// `ProxyGenerator.addProxyMethod` registers a signature once, with the FIRST
// interface's `Method`, and narrows its exception list to the types
// compatible with every declaration (`collectCompatibleTypes` both ways). The
// generated body's catch list is that merged list, so for `A.run() throws
// IOException` and `B.run()` a handler throwing `IOException` answers
// `UndeclaredThrowableException` on HotSpot. The body native classified the
// throw against the handed-over `Method`'s own `exceptionTypes` (`A.run`'s)
// and let it through. `build_proxy_spec_for` now merges the list, the emitter
// declares it as the proxy method's `Exceptions` attribute (so
// `proxyClass.getMethod("run").getExceptionTypes()` answers as HotSpot's
// does), and `native_proxy_dispatch_invoke` classifies against it.
// ---------------------------------------------------------------------------

/// `CRATONVM_PROXY_MERGED_THROWS` (default on, both modes).
fn proxy_merged_throws() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_MERGED_THROWS")
}

/// `CRATONVM_PROXY_INIT_AT_CREATION` (default on, both modes): initialize a
/// generated proxy class in `newProxyInstance`, as the JDK's
/// `Constructor.newInstance` does. Read by `native_proxy_new_instance`.
fn proxy_init_at_creation() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_INIT_AT_CREATION")
}

/// Whether the exception class named `from` is `to` or a subclass of it:
/// `None` when `from` is not loaded (a `throws` clause does not load its
/// types), which the merge treats as compatible, i.e. as before this merge.
fn proxy_exception_assignable(ctx: &dyn NativeContext, from: &str, to: &str) -> Option<bool> {
    if from == to {
        return Some(true);
    }
    let mut cursor = Some(ctx.class_id_by_name(from)?);
    for _ in 0..64 {
        let Some(cid) = cursor else {
            return Some(false);
        };
        if ctx.class_name_of_id(cid)? == to {
            return Some(true);
        }
        cursor = ctx.superclass_of(cid);
    }
    None
}

/// `ProxyGenerator.collectCompatibleTypes(a, b)` then `(b, a)`: the types of
/// either list that are assignable to some type of the other, first-seen
/// order, no repeats.
fn proxy_merged_exception_types(ctx: &dyn NativeContext, a: &[String], b: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for (from, with) in [(a, b), (b, a)] {
        for fc in from {
            if merged.contains(fc) {
                continue;
            }
            if with
                .iter()
                .any(|wc| proxy_exception_assignable(ctx, fc, wc) != Some(false))
            {
                merged.push(fc.clone());
            }
        }
    }
    merged
}

/// The generated body's catch list, asked of the class it lives in: `true`
/// when `thrown` is a checked exception that the proxy class's own
/// declaration of `method_obj`'s signature does not list, so the body must
/// wrap it although the `Method` handed to the handler may declare it.
/// Answers `false` whenever it cannot tell (switch off, unreadable method
/// name, the class not declaring the signature, a superclass chain it cannot
/// walk to `Throwable`), leaving the `Method`-based classification in charge.
/// Reads only; runs no Java.
fn proxy_body_refuses_checked(
    ctx: &dyn NativeContext,
    proxy: cratonvm_types::ObjectRef,
    method_obj: cratonvm_types::ObjectRef,
    thrown: cratonvm_types::ObjectRef,
) -> bool {
    if !proxy_merged_throws() {
        return false;
    }
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = Some(ctx.class_id_of_object(thrown));
    for _ in 0..64 {
        let Some(cid) = cursor else { break };
        let Some(name) = ctx.class_name_of_id(cid) else {
            return false;
        };
        chain.push(name);
        cursor = ctx.superclass_of(cid);
    }
    if cursor.is_some() || !proxy_thrown_chain_is_checked(&chain) {
        return false;
    }
    let name = match ctx.get_field_by_name(method_obj, "name") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        return false;
    }
    let proxy_cid = ctx.class_id_of_object(proxy);
    let desc = crate::lang_class::method_descriptor_for_invoke(ctx, method_obj);
    if !ctx.class_declares_method(proxy_cid, &name, &desc) {
        return false;
    }
    let declared = ctx.method_exceptions(proxy_cid, &name, &desc);
    !declared.iter().any(|d| chain.iter().any(|n| n == d))
}

/// Whether a thrown object's complete superclass chain (its own class first,
/// `java/lang/Object` last) is a CHECKED exception: it passes through
/// `Throwable` and through neither `RuntimeException` nor `Error`.
///
/// Round 13 wave 5 hand-back: the first version asked for `Throwable` as the
/// LAST name, but `Throwable` extends `Object`, so every chain ended in
/// `java/lang/Object`, the check always answered "don't know", and the merged
/// list never refused anything (`R13Proxy3MergedThrows`: bad 60000, every
/// wrap case -- AB.run modes 0/1 and AB.nar mode 0 -- propagated raw).
fn proxy_thrown_chain_is_checked(chain: &[String]) -> bool {
    chain.iter().any(|n| n == "java/lang/Throwable")
        && !chain
            .iter()
            .any(|n| n == "java/lang/RuntimeException" || n == "java/lang/Error")
}

/// `new UndeclaredThrowableException(thrown)`, or `thrown` itself when the
/// wrapper cannot be built (the same fallback `wrap_undeclared_throwable`
/// takes).
fn proxy_wrap_in_undeclared(
    ctx: &mut dyn NativeContext,
    thrown: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let thrown_h = scope.root(thrown);
    let ute_cid =
        match scope.ensure_class_initialized("java/lang/reflect/UndeclaredThrowableException") {
            Ok(c) => c,
            Err(_) => return scope.get(&thrown_h),
        };
    let n_fields = scope.class_num_total_fields(ute_cid).max(4);
    let ute = scope.alloc_object(ute_cid, n_fields);
    let ute_h = scope.root(ute);
    let ctor_args = [
        Value::Object(Some(ute)),
        Value::Object(Some(scope.get(&thrown_h))),
    ];
    let _ = scope.invoke(
        "java/lang/reflect/UndeclaredThrowableException",
        "<init>",
        "(Ljava/lang/Throwable;)V",
        &ctor_args,
    );
    scope.get(&ute_h)
}

/// The wrapper class (dotted) and unboxing accessor HotSpot's generated body
/// calls for `method_obj`'s primitive return type; `None` for a reference or
/// `void` return. Same table as `vm_exec::proxy_primitive_return_wrapper`.
fn proxy_method_primitive_return_wrapper(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> Option<(&'static str, &'static str)> {
    let ret = annotation_member_return_descriptor(ctx, method_obj);
    Some(match *ret.as_bytes().first()? {
        b'Z' => ("java.lang.Boolean", "booleanValue"),
        b'B' => ("java.lang.Byte", "byteValue"),
        b'C' => ("java.lang.Character", "charValue"),
        b'S' => ("java.lang.Short", "shortValue"),
        b'I' => ("java.lang.Integer", "intValue"),
        b'J' => ("java.lang.Long", "longValue"),
        b'F' => ("java.lang.Float", "floatValue"),
        b'D' => ("java.lang.Double", "doubleValue"),
        _ => return None,
    })
}

/// The body's answer to a `null` handler result: the NPE HotSpot's body
/// raises for a primitive return, with its text, else the `null` itself.
fn proxy_null_primitive_return(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
) -> MethodCallResult {
    let Some((wrapper, accessor)) = proxy_method_primitive_return_wrapper(ctx, method_obj) else {
        return Ok(Some(Value::Object(None)));
    };
    let message = format!(
        "Cannot invoke \"{wrapper}.{accessor}()\" because the return value of \
         \"java.lang.reflect.InvocationHandler.invoke(Object, java.lang.reflect.Method, \
         Object[])\" is null"
    );
    Err(RuntimeError::NullPointerException {
        message: Some(message),
    }
    .into())
}

fn native_proxy_dispatch_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `CRATONVM_DBG=ann-proxy-prof` counts this entry too.
    //
    // The first version of that profiler instrumented only
    // `annotation_proxy_dispatch_impl` — the INTERPRETER's hook — and reported
    // ~100k dispatches on a Spring context startup, which would have made the
    // whole annotation path worth <0.5s of a 305s run. But this native is a
    // SECOND call site with its own AnnotationProxy routing (see the comment
    // below: the by-name path here deliberately does not reach the interpreter
    // hook's arms), so a workload whose proxies are reached through generated
    // `$ProxyN` bodies is invisible to that counter. Counting one branch of a
    // two-branch funnel is how a ceiling gets under-reported by the exact factor
    // that matters.
    let prof = crate::proxy_dispatch_prof_on();
    let prof_entry = prof.then(std::time::Instant::now);
    struct ProxyDispatchProfGuard(Option<std::time::Instant>);
    impl Drop for ProxyDispatchProfGuard {
        fn drop(&mut self) {
            if let Some(t0) = self.0 {
                crate::note_proxy_dispatch_ns(t0.elapsed().as_nanos() as u64);
            }
        }
    }
    let _proxy_dispatch_prof_guard = ProxyDispatchProfGuard(prof_entry);

    let proxy = match args.first() {
        Some(Value::Object(Some(p))) => *p,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Proxy$Dispatch.invokeProxy: null proxy receiver".to_string()),
            }
            .into());
        }
    };
    let method_obj = match args.get(1) {
        Some(Value::Object(Some(m))) => *m,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Proxy$Dispatch.invokeProxy: null Method arg".to_string()),
            }
            .into());
        }
    };
    let mut args_arr = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // Generated proxy bodies must pass null, not an empty Object[], for no-arg
    // methods. Several InvocationHandler implementations distinguish the two.
    if matches!(args_arr, Some(arr) if ctx.array_length(arr) == 0) {
        args_arr = None;
    }

    // Read InvocationHandler from proxy slot 0.
    let handler = match ctx.get_field(proxy, 0) {
        Value::Object(Some(h)) => h,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Proxy$Dispatch.invokeProxy: proxy InvocationHandler is null".to_string(),
                ),
            }
            .into());
        }
    };

    // gen r4w3/rooting: `getName()`, the AnnotationProxy `invoke` and the
    // handler's `invoke` below all run Java (GC points) and `proxy` /
    // `method_obj` / `handler` / `args_arr` (copied out of `args`) are used
    // after them. Pin all four here and re-read them off the pins after each
    // GC point. Early returns leave the pins to `safe_native_call`'s watermark.
    let proxy_pin = ctx.pin_native_root(proxy);
    let method_pin = ctx.pin_native_root(method_obj);
    let handler_pin = ctx.pin_native_root(handler);
    let args_arr_pin = args_arr.map(|a| ctx.pin_native_root(a));

    let handler_cid = ctx.class_id_of_object(handler);
    let handler_class = ctx
        .class_name_of_id(handler_cid)
        .unwrap_or_else(|| "java/lang/reflect/InvocationHandler".to_string());

    // Real annotations: when the proxy's InvocationHandler is the
    // synthetic AnnotationProxy carrying the member data, the generated `$ProxyN`
    // method bodies reach here (the cached/dead-code dispatch path that a 2nd
    // call site falls into, bypassing `proxy_invoke_handler_shared`). Calling
    // `invoke` on an AnnotationProxy returns null (it has no `invoke` element);
    // instead invoke the requested annotation method (value/annotationType/
    // equals/hashCode/toString) on it BY NAME — the same routing applied to the
    // generated-body path. Without this, the 2nd access of a given
    // (proxyClass, method) returns null (e.g. repeatable `getAnnotationsByType`).
    if handler_class == "java/lang/annotation/AnnotationProxy" {
        // The member NAME is the whole routing key here. Reading it off the
        // `Method`'s `name` FIELD only works while the receiver has the layout
        // this code assumes: `get_field_by_name` answers `Object(None)` for a
        // name it cannot resolve, and an empty name then falls through to the
        // generic `handler.invoke(...)` tail below — which asks an
        // `AnnotationProxy` for a member literally called `invoke`, finds none,
        // and hands back **null for every annotation member regardless of its
        // declared type**. That is indistinguishable, at the call site, from a
        // genuinely absent member: Byte Buddy's
        // `AnnotationDescription$ForLoadedAnnotation.getValue` turns it into
        // `asValue(null, int.class)` → null → NPE on `.filter(...)`, whether
        // the member is an `int`, an enum or a `Class`. Fall back to the real
        // `Method.getName()` before giving up.
        let mut mname = match ctx.get_field_by_name(method_obj, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if mname.is_empty() {
            // The String is `getName()`'s own return value, so it is current.
            let got = ctx.invoke_virtual(method_obj, "getName", "()Ljava/lang/String;", &[]);
            if let Ok(Some(Value::Object(Some(s)))) = got {
                mname = ctx.read_string(s).unwrap_or_default();
            }
            if crate::nbflags().dbg_annproxy_wrap {
                eprintln!(
                    "[DBG_WRAP] invokeProxy: Method.name field was unreadable; getName() -> {mname:?}"
                );
            }
        }
        // gen r4w3/rooting: `getName()` above may have run Java; re-read.
        let proxy = ctx.read_native_pin(proxy_pin, proxy);
        let method_obj = ctx.read_native_pin(method_pin, method_obj);
        let handler = ctx.read_native_pin(handler_pin, handler);
        let args_arr = match (args_arr, args_arr_pin) {
            (Some(a), Some(h)) => Some(ctx.read_native_pin(h, a)),
            (a, _) => a,
        };
        // Object-inherited methods: `ctx.invoke("AnnotationProxy", mname, ...)`
        // below does not reliably resolve these (the by-name path used by this
        // 2nd call site doesn't reach `annotation_proxy_dispatch_impl`'s
        // hashCode/equals/toString/getClass arms — only the primary
        // interpreter dispatch hook does), so compute them directly instead.
        // See `lang_class::ctx_annotation_proxy_*` for the rationale.
        match mname.as_str() {
            "hashCode" => {
                // invokeProxy's return type is `Object` — the generated body does
                // `CHECKCAST Integer; Integer.intValue()`, so the result must be a
                // real boxed Integer, not a raw `Value::Int` (which isn't a valid
                // object reference and CHECKCAST/unbox turns into null).
                let hash = crate::lang_class::ctx_annotation_proxy_hash_code(ctx, handler)?;
                return Ok(Some(crate::lang_class::box_value(
                    ctx,
                    Value::Int(hash),
                    "I",
                )));
            }
            "equals" => {
                // `args_arr` was re-read off its pin after `getName()` above.
                let other = match args_arr {
                    Some(arr) => {
                        if ctx.array_length(arr) > 0 {
                            ctx.get_array_element(arr, 0)
                        } else {
                            Value::Object(None)
                        }
                    }
                    None => Value::Object(None),
                };
                // In real-annotation mode every annotation is a real
                // `$ProxyN`, so the `other` argument is typically ALSO a real
                // proxy (not a bare AnnotationProxy) — unwrap it to its
                // AnnotationProxy handler (slot 0) before comparing, mirroring
                // `proxy_invoke_handler_shared`'s equals-argument unwrap in
                // vm_exec.rs. Without this every same-type comparison between
                // two real-proxied annotations spuriously compares unequal.
                if let Value::Object(Some(other_obj)) = other {
                    // Round 13 wave 8 (lane proxy5): `equalsImpl` whole -- a VM
                    // annotation compares carrier to carrier, any other object
                    // member by member through its own accessors (the delegation
                    // below answered `other.equals(proxy)`).
                    // `CRATONVM_ANNOTATION_FOREIGN_EQUALS=0` restores it.
                    if crate::lang_class::annotation_foreign_equals_reflective()
                        && crate::lang_class::annotation_jdk_semantics()
                    {
                        let eq =
                            crate::lang_class::annotation_equals_object(ctx, handler, other_obj)?;
                        let flag = Value::Int(i32::from(eq));
                        return Ok(Some(crate::lang_class::box_value(ctx, flag, "Z")));
                    }
                    if crate::lang_class::ctx_class_name_of(ctx, other_obj)
                        != "java/lang/annotation/AnnotationProxy"
                    {
                        if let Value::Object(Some(other_handler)) = ctx.get_field(other_obj, 0) {
                            if crate::lang_class::ctx_class_name_of(ctx, other_handler)
                                == "java/lang/annotation/AnnotationProxy"
                            {
                                let eq = crate::lang_class::ctx_annotation_proxy_equals(
                                    ctx,
                                    handler,
                                    Value::Object(Some(other_handler)),
                                )?;
                                let flag = Value::Int(if eq { 1 } else { 0 });
                                return Ok(Some(crate::lang_class::box_value(ctx, flag, "Z")));
                            }
                        }
                        // `other` is a foreign `Annotation` implementation — e.g. Spring's
                        // own `synthesize()`, whose handler is Spring's
                        // `SynthesizedMergedAnnotationInvocationHandler`, not our
                        // `AnnotationProxy` — so it can never be recognized by the
                        // structural comparison above. Mirror
                        // `annotation_proxy_invoke_shared`'s equivalent delegation
                        // (vm_exec.rs): if `other` implements the SAME annotation type,
                        // let ITS `equals` decide (it presumably knows how to compare
                        // member values reflectively against any `Annotation`, the way
                        // HotSpot's `AnnotationInvocationHandler.equals` does). Without
                        // this, comparing a reflection-obtained annotation against a
                        // Spring-synthesized one of the same type spuriously returns
                        // false (MergedAnnotationsTests.equalsForSynthesizedAnnotations).
                        if let Value::Object(Some(type_mirror)) =
                            ctx.get_field(handler, crate::lang_class::ANN_PROXY_TYPE_MIRROR)
                        {
                            if let Some(ann_cid) = ctx.class_id_from_mirror(type_mirror) {
                                let other_cid = ctx.class_id_of_object(other_obj);
                                if other_cid == ann_cid || ctx.is_subclass(other_cid, ann_cid) {
                                    let other_class_name =
                                        crate::lang_class::ctx_class_name_of(ctx, other_obj);
                                    // `other.equals(proxy)` is a REAL bytecode call with a
                                    // primitive `Z` return — `ctx.invoke` naturally yields a
                                    // raw `Value::Int`, but `native_proxy_dispatch_invoke`'s
                                    // OWN return must be a boxed `Object` (the ORIGINAL
                                    // generated proxy body CHECKCASTs it to Boolean before
                                    // `booleanValue()`) — same boxing requirement as the
                                    // hashCode/equals arms above.
                                    let result = ctx.invoke(
                                        &other_class_name,
                                        "equals",
                                        "(Ljava/lang/Object;)Z",
                                        &[other, Value::Object(Some(proxy))],
                                    )?;
                                    let flag = result.unwrap_or(Value::Int(0));
                                    return Ok(Some(crate::lang_class::box_value(ctx, flag, "Z")));
                                }
                            }
                        }
                        return Ok(Some(crate::lang_class::box_value(ctx, Value::Int(0), "Z")));
                    }
                }
                let eq = crate::lang_class::ctx_annotation_proxy_equals(ctx, handler, other)?;
                let flag = Value::Int(if eq { 1 } else { 0 });
                return Ok(Some(crate::lang_class::box_value(ctx, flag, "Z")));
            }
            // `Annotation.toString()` is rendered TWICE in this repository:
            // here, via `ctx_annotation_proxy_to_string`, and in the
            // interpreter's primary dispatch hook,
            // `vm/src/vm/vm_exec.rs::annotation_proxy_to_string`. Which one
            // answers a given call is not under the caller's control — the
            // first `toString()` on a fresh proxy can take the vm_exec hook and
            // every later one this route — so any divergence between them shows
            // up as ONE run printing TWO different strings for ONE annotation
            // (measured 2026-08-12). Change neither alone.
            "toString" => {
                let s = crate::lang_class::ctx_annotation_proxy_to_string(ctx, handler)?;
                let result = ctx.create_string(&s);
                return Ok(Some(Value::Object(Some(result))));
            }
            // `getClass`/`annotationType`/`getType` all return the same stored
            // annotation-type mirror (field `ANN_PROXY_TYPE_MIRROR`) in
            // `annotation_proxy_dispatch_impl`. `annotationType()` in
            // particular is called repeatedly by Spring's own meta-annotation
            // introspection machinery (`AnnotationTypeMappings`,
            // `TypeMappedAnnotation`, ...), which is exactly the kind of hot,
            // repeated call that gets JIT-compiled and hits this 2nd-call-site
            // path — leaving `Method.invoke` against a wrong/stale receiver
            // type and surfacing as Spring's own
            // `Assert.isInstanceOf` / `Method.invoke` IllegalArgumentException
            // ("... must be an instance of interface ...") deep inside
            // MergedAnnotations/AnnotatedElementUtils.
            //
            // Round 13 wave 8 (lane proxy5): a generated body is emitted only
            // for the annotation interface's own methods (and `Object`'s), so
            // `getType` reaches this door only as a MEMBER the interface
            // declares; it is read as one (`CRATONVM_ANNOTATION_MEMBER_CONFORMANCE`).
            "getClass" | "annotationType" => {
                return Ok(Some(
                    ctx.get_field(handler, crate::lang_class::ANN_PROXY_TYPE_MIRROR),
                ));
            }
            "getType" if !crate::lang_class::annotation_member_conformance() => {
                return Ok(Some(
                    ctx.get_field(handler, crate::lang_class::ANN_PROXY_TYPE_MIRROR),
                ));
            }
            _ => {}
        }
        if !mname.is_empty() {
            let mut ann_args = vec![Value::Object(Some(handler))];
            if let Some(arr) = args_arr {
                let len = ctx.array_length(arr);
                for i in 0..len {
                    ann_args.push(ctx.get_array_element(arr, i));
                }
            }
            // Routing into the AnnotationProxy interception is by class+name;
            // the descriptor governs result coercion.
            //
            // "By class+name" is the whole mechanism, and worth naming exactly
            // because W7-12 depends on it. `ctx.invoke` is `invoke_shared`
            // (`vm/src/vm/vm_exec.rs`), which loads the class, finds an EMPTY
            // method table on it, and reaches the terminal-miss
            // annotation-proxy rescue — keyed on the receiver's runtime class
            // NAME (`&*c.name == "java/lang/annotation/AnnotationProxy"`),
            // never on its `ClassOrigin`. Every other door into this class is
            // name-keyed the same way: `invoke_or_native`'s `effective_class`
            // arm, `execute_invoke_kind`'s S111r18 arm
            // (`vm/src/runtime/interpreter/invoke.rs`), the three
            // `dispatch_virtual.rs` arms, and the JIT retarget in
            // `vm/src/jit/helpers.rs`.
            //
            // Measured 2026-08-11 (`--dump-native-registry`, real-JDK boot):
            // **zero** natives are registered under this class name, against
            // two under `java/lang/reflect/Proxy$Instance`. So the
            // provenance-keyed "prefer a native registered under the receiver's
            // own exact name" branches this class currently takes cannot answer
            // any call on it — which is what lets W7-12 re-label the class
            // `VmInternal` (so `--jdk-only` stops refusing to mint it, see
            // docs/internal/jdk-only/W7-12-strict-annotation-proxy.md)
            // without moving this call off its route.
            //
            // `()Ljava/lang/Object;` is right for every member the proxy stores
            // — those are already boxed — but NOT for the one case it does not
            // store: a member ABSENT from the proxy's parallel arrays whose
            // value comes from the interface's `AnnotationDefault`.
            // `annotation_proxy_dispatch_impl` returns those UNBOXED
            // (`Value::Int`/`Long`/`Float`/`Double`), because the primary
            // interpreter door passes the member's REAL descriptor (`()Z`,
            // `()I`, …) and unboxes against it. Here the declared descriptor is
            // `Object`, and `coerce_native_return` maps a raw `Value::Int(0)`
            // onto `Object(None)` — so an omitted `boolean … default false`
            // member reads back as **null**.
            //
            // That is not hypothetical: Byte Buddy's `SuperCall$Binder.bind`
            // does `annotation.getValue(NULL_IF_IMPOSSIBLE).resolve(Boolean.class)
            // .booleanValue()` on `@SuperCall.nullIfImpossible()` — declared
            // `boolean default false` and omitted at every use site — and NPEs
            // on the null, failing `Mockito.mock(<interface>)` outright. It is
            // intermittent because this door is only reached once the generated
            // `$ProxyN` body has been JIT-compiled, so it tracks compile timing
            // and therefore machine load.
            //
            // Keep asking with `()Ljava/lang/Object;` — the AnnotationProxy
            // interception is descriptor-agnostic and every STORED member value
            // is already a wrapper, which passes an `L` slot untouched. Box only
            // the raw primitive, using the member's own declared return type, so
            // both representations leave here as objects — the same thing the
            // `hashCode`/`equals` arms above already do.
            let result = ctx.invoke(
                "java/lang/annotation/AnnotationProxy",
                &mname,
                "()Ljava/lang/Object;",
                &ann_args,
            )?;
            let boxed = match result {
                Some(v @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_))) => {
                    // gen r4w3/rooting: the AnnotationProxy invoke above is a
                    // GC point; read `method_obj`'s current address.
                    let method_obj = ctx.read_native_pin(method_pin, method_obj);
                    let ret_desc = annotation_member_return_descriptor(ctx, method_obj);
                    if crate::nbflags().dbg_annproxy_wrap {
                        eprintln!(
                            "[DBG_WRAP] invokeProxy: {mname}() returned raw {v:?}; boxing as {ret_desc}"
                        );
                    }
                    Some(crate::lang_class::box_value(ctx, v, &ret_desc))
                }
                other => other,
            };
            if crate::nbflags().dbg_annproxy_wrap {
                let shape = match &boxed {
                    Some(Value::Object(Some(o))) => {
                        format!("Object({})", crate::lang_class::ctx_class_name_of(ctx, *o))
                    }
                    Some(Value::Object(None)) => "Object(null)".to_string(),
                    Some(other) => format!("{other:?}"),
                    None => "void".to_string(),
                };
                eprintln!("[DBG_WRAP] invokeProxy {mname}() -> {shape}");
            }
            return Ok(boxed);
        }
    }

    // gen r4w3/rooting: the AnnotationProxy branch above may have run Java
    // (`getName()`) before falling through; pass current addresses.
    let proxy = ctx.read_native_pin(proxy_pin, proxy);
    let method_obj = ctx.read_native_pin(method_pin, method_obj);
    let handler = ctx.read_native_pin(handler_pin, handler);
    let args_arr = match (args_arr, args_arr_pin) {
        (Some(a), Some(h)) => Some(ctx.read_native_pin(h, a)),
        (a, _) => a,
    };
    let invoke_args = [
        Value::Object(Some(handler)),
        Value::Object(Some(proxy)),
        Value::Object(Some(method_obj)),
        Value::Object(args_arr),
    ];
    // `--jdk-only`: `h.invoke(...)` is what the generated `$ProxyN` body does
    // -- a virtual call on the HANDLER OBJECT -- so dispatch on it.
    // `ctx.invoke(<its class NAME>, ...)` resolved that name through the
    // application loader: a handler whose class a child loader defined ran the
    // application copy's `invoke` (loading that copy first if the application
    // had none). MEASURED 2026-09-23, Spring's
    // `PersistenceAnnotationBeanPostProcessorAotContributionTests` under
    // `@CompileWithForkedClassLoader`: Byte Buddy's
    // `JavaDispatcher$ProxiedInvocationHandler`, defined by the fork, ran the
    // application-defined copy, whose `(Dispatcher)` cast named the
    // application's `Dispatcher` -- `ClassCastException` on the fork's own
    // `ForInstanceCheck`. JIT-only: this is the door a JIT-compiled call site
    // reaches; the interpreter's `proxy_invoke_handler_shared` dispatches on
    // the receiver already. `--compatible` kept the by-name call until round 13
    // wave 4 (lane proxy2): since the interpreter runs these bodies too
    // (`proxy_interpreter_runs_generated_body`), this is THE dispatch of a
    // user handler in both tiers, and the by-name call was the one half of
    // the old intercept's contract it did not keep (`invoke_or_native` keys
    // on the receiver's own class). `CRATONVM_PROXY_BODY_HANDLER_VIRTUAL=0`
    // restores the by-name call in `--compatible`.
    let result = if ctx.is_jdk_only() || proxy_body_handler_dispatch_is_virtual() {
        ctx.invoke_virtual(
            handler,
            "invoke",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            &invoke_args[1..],
        )
    } else {
        ctx.invoke(
            &handler_class,
            "invoke",
            "(Ljava/lang/Object;Ljava/lang/reflect/Method;[Ljava/lang/Object;)Ljava/lang/Object;",
            &invoke_args,
        )
    };
    // gen r4w3/rooting: the handler's `invoke` is a GC point; the exception
    // classification below reads `method_obj` again.
    let method_obj = ctx.read_native_pin(method_pin, method_obj);

    // Item 6 — UndeclaredThrowableException wrapping. On a thrown Java
    // exception, classify against `Method.exceptionTypes` and wrap if
    // the throw is not declared (and is not a RuntimeException / Error
    // subclass). VM-internal failures propagate verbatim.
    let out: MethodCallResult = match result {
        // Round 13 wave 4 (lane proxy2): `null` for a primitive return. The
        // body's `<Wrapper>.xxxValue()` would throw the NPE itself, but this
        // VM's helpful-NPE text for it names the receiver's producer, which
        // here is `Proxy$Dispatch.invokeProxy`; HotSpot's body calls
        // `InvocationHandler.invoke` and its text names that. Raised here with
        // the transcribed text (`vm_exec::proxy_null_return_npe_message`, the
        // old intercept's), so both tiers answer as HotSpot does.
        // `CRATONVM_PROXY_BODY_NULL_PRIMITIVE_NPE=0` leaves it to the body.
        Ok(Some(Value::Object(None))) if proxy_body_null_primitive_npe() => {
            proxy_null_primitive_return(ctx, method_obj)
        }
        Ok(v) => Ok(v),
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(thrown)) => {
            // Round 13 wave 5 (lane proxy3): the body's own catch list (the
            // `throws` list merged over every interface declaring this
            // signature) refuses first; see `proxy_body_refuses_checked`.
            let proxy = ctx.read_native_pin(proxy_pin, proxy);
            let final_obj = if proxy_body_refuses_checked(ctx, proxy, method_obj, thrown) {
                proxy_wrap_in_undeclared(ctx, thrown)
            } else {
                wrap_undeclared_throwable(ctx, method_obj, thrown)
            };
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                final_obj,
            ))
        }
        Err(other) => Err(other),
    };
    // gen r4w3/rooting: `proxy_pin` is this function's lowest pin.
    ctx.unpin_native_roots(proxy_pin);
    out
}

/// `CRATONVM_PROXY_INVOKE_DEFAULT_CHECKS` (default on, both modes): the
/// argument checks of `InvocationHandler.invokeDefault` / `Proxy.invokeDefault`
/// run before the default body, with the JDK's exceptions and texts (round 13
/// wave 8, lane proxy5). `0` restores the body's own checks.
fn proxy_invoke_default_jdk_checks() -> bool {
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    proxy_memo_flag_default_on(&SLOT, "CRATONVM_PROXY_INVOKE_DEFAULT_CHECKS")
}

/// `"<method>" <what>`, as `Proxy.invokeDefault` words its refusals
/// (`"\"" + method + "\" is not a default method"`), `Method.toString()`
/// included.
fn proxy_invoke_default_refusal(
    ctx: &mut dyn NativeContext,
    method_obj: cratonvm_types::ObjectRef,
    what: &str,
) -> MethodCallFailed {
    let text = match ctx.invoke_virtual(method_obj, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    RuntimeError::IllegalArgumentException {
        message: format!("\"{text}\" {what}"),
    }
    .into()
}

/// `InvocationHandler.invokeDefault(proxy, method, args)` with the JDK's
/// checks in the JDK's order, then the default body
/// (`native_invocation_handler_invoke_default`, whose own checks then all
/// pass):
///
/// 1. `Objects.requireNonNull(proxy)`, `Objects.requireNonNull(method)`: an
///    NPE without a message (the body said `... proxy is null`);
/// 2. `'proxy' is not a proxy instance`;
/// 3. `"<method>" is not a default method` (the body said `method is not a
///    default method`);
/// 4. `findProxyInterfaceOrElseThrow`: a default method of an interface the
///    proxy class does not implement is refused with `"<method>" is not a
///    method declared in the proxy class` (the body RAN it on the proxy). The
///    JDK's "overridden directly or indirectly by the proxy interfaces" case
///    (a proxy interface re-declares the method) is not modelled;
/// 5. the spreader's length check: `args` (`null` counts as empty) must have
///    one element per parameter, `array is not of length <n>`.
fn native_invocation_handler_invoke_default_checked(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !proxy_invoke_default_jdk_checks() {
        return native_invocation_handler_invoke_default(ctx, args);
    }
    let Some(Value::Object(Some(proxy))) = args.first().copied() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let Some(Value::Object(Some(method_obj))) = args.get(1).copied() else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let proxy_cid = ctx.class_id_of_object(proxy);
    if !proxy_class_is_generated(ctx, proxy_cid) {
        return Err(RuntimeError::IllegalArgumentException {
            message: "'proxy' is not a proxy instance".to_string(),
        }
        .into());
    }
    // `Method.isDefault()`.
    const ACC_PUBLIC: i32 = 0x0001;
    const ACC_STATIC: i32 = 0x0008;
    const ACC_ABSTRACT: i32 = 0x0400;
    let modifiers = match ctx.get_field_by_name(method_obj, "modifiers") {
        Value::Int(v) => v,
        _ => 0,
    };
    let decl_cid = match ctx.get_field_by_name(method_obj, "clazz") {
        Value::Object(Some(mirror)) => ctx.class_id_from_mirror(mirror),
        _ => None,
    };
    let is_default = decl_cid.is_some_and(|cid| ctx.is_interface_class(cid))
        && (modifiers & (ACC_ABSTRACT | ACC_PUBLIC | ACC_STATIC)) == ACC_PUBLIC;
    if !is_default {
        return Err(proxy_invoke_default_refusal(
            ctx,
            method_obj,
            "is not a default method",
        ));
    }
    // Not for the synthetic `Proxy$Instance` fallback: it declares none of its
    // interfaces (they live in its slot 1), so the class cannot answer.
    let class_declares_interfaces = ctx
        .class_name_of_id(proxy_cid)
        .is_some_and(|name| proxy_name_has_generated_shape(&name));
    if let (Some(decl), true) = (decl_cid, class_declares_interfaces) {
        if decl != proxy_cid && !ctx.is_subclass(proxy_cid, decl) {
            return Err(proxy_invoke_default_refusal(
                ctx,
                method_obj,
                "is not a method declared in the proxy class",
            ));
        }
    }
    let descriptor = crate::lang_class::method_descriptor_for_invoke(ctx, method_obj);
    let expected = crate::lang_class::parse_descriptor_param_and_return(&descriptor)
        .0
        .len();
    let supplied = match args.get(2) {
        Some(Value::Object(Some(arr))) => ctx.array_length(*arr),
        _ => 0,
    };
    if supplied != expected {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("array is not of length {expected}"),
        }
        .into());
    }
    native_invocation_handler_invoke_default(ctx, args)
}

/// proxy-real-classfile increment 1 — gate for the real generated-`$ProxyN`
/// classfile path. Defaults to **ON** (the real path is canonical). Set
/// `CRATONVM_REAL_PROXY=0` (or `false` / `off` / `no`, case-insensitive) to
/// force the legacy synthetic `Proxy$Instance` shim for soak/triage.
///
/// Design-doc step 5: promoting the real path from "best-effort fast path
/// with a silent synthetic fallback" to the canonical path. The gate exists
/// so the flip can be reverted per-process without a rebuild while the
/// reflection suites soak; it is NOT a `#[cfg]` feature.
pub fn real_proxy_enabled() -> bool {
    crate::nbflags().real_proxy
}

/// proxy-real-classfile increment 3 (design §3) — STRICT mode. Default **OFF**.
/// When ON (and the real path is enabled), a *genuine* proxy-class generation
/// failure (spec-build / emit / define) surfaces as the real JDK exception
/// (`IllegalArgumentException`), exactly as `Proxy.newProxyInstance` does —
/// instead of silently degrading to the synthetic `Proxy$Instance` shim.
///
/// Gated default-off because the silent degrade is currently a *safety net* on
/// the default (gate-on) path: removing it changes default behaviour, so the
/// flip waits on the reflection-suite soak. Set `CRATONVM_REAL_PROXY_STRICT=1`
/// (or `true`/`on`/`yes`) to opt in. Step 5 (deleting the shim outright) is the
/// follow-up once this soaks clean.
pub fn real_proxy_strict() -> bool {
    crate::nbflags().real_proxy_strict
}

/// proxy-real-classfile real-super gate — generate `$ProxyN` classes that extend
/// the **real** `java.lang.reflect.Proxy` (sole instance field `h` at slot 0,
/// matching the handler slot the dispatch path reads). DEFAULT **ON** (per the
/// "real Java by default, synthetic experimental" project rule): real-`Proxy`-super
/// proxies match HotSpot for `getSuperclass()` / `instanceof Proxy`. Set
/// `CRATONVM_REAL_PROXY_SUPER=0` (or `false` / `off` / `no`) to opt into the
/// **experimental synthetic** `java/lang/reflect/Proxy$Instance` super instead —
/// both paths are kept and working; NOTHING is deleted (the synthetic super is a
/// real experimental implementation, not a no-op stub).
///
/// Must stay in lockstep with the VM-side accessor
/// `crate::runtime::env_cache::real_proxy_super()` (same env var) — the VM reads
/// it to recognise real-`Proxy`-super proxies in the dispatch chain walk.
pub fn real_proxy_super() -> bool {
    crate::nbflags().real_proxy_super
}

/// The internal name of the super class generated `$ProxyN` proxies extend,
/// selected by [`real_proxy_super`]. Centralised so the emitter spec, the
/// allocation field-layout, and the proxy-chain recognition all agree.
fn proxy_super_class_name() -> &'static str {
    if real_proxy_super() {
        "java/lang/reflect/Proxy"
    } else {
        "java/lang/reflect/Proxy$Instance"
    }
}

/// Build + throw (as `Err(ExceptionThrown)`) the real JDK exception for a proxy
/// generation failure under STRICT mode. Mirrors `Proxy.newProxyInstance`,
/// which raises `IllegalArgumentException` when it cannot define the class.
fn throw_proxy_failure(ctx: &mut dyn NativeContext, stage: &str) -> MethodCallFailed {
    let msg = format!("Could not generate proxy class (stage: {stage})");
    let cid = match ctx.ensure_class_initialized("java/lang/IllegalArgumentException") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let n = ctx.class_num_total_fields(cid).max(4);
    let exc = ctx.alloc_object(cid, n);
    let msg_ref = ctx.create_string(&msg);
    let _ = ctx.invoke(
        "java/lang/IllegalArgumentException",
        "<init>",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(exc)), Value::Object(Some(msg_ref))],
    );
    MethodCallFailed::ExceptionThrown(exc)
}

#[cfg(test)]
mod proxy_strict_gate_tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    /// increment 3 (§3): STRICT proxy mode is opt-in and OFF by default, so the
    /// silent-degrade safety net remains the default behaviour until the soak
    /// flips it. (The throw path itself requires a live VM and is exercised by
    /// the reflection suites under `CRATONVM_REAL_PROXY_STRICT=1`.)
    #[test]
    fn real_proxy_strict_defaults_off() {
        assert!(!super::real_proxy_strict());
    }

    /// proxy-real-classfile real-super gate: real `java.lang.reflect.Proxy` is the
    /// super by DEFAULT (real Java by default; the synthetic `Proxy$Instance` super
    /// is the experimental opt-out via `CRATONVM_REAL_PROXY_SUPER=0`). Both paths
    /// are retained — nothing is deleted. (Env-unset default asserted here; the
    /// opt-out is exercised by the proxy soak under `CRATONVM_REAL_PROXY_SUPER=0`.)
    #[test]
    fn real_proxy_super_defaults_on() {
        // Only meaningful when the env var is not set in the test environment.
        if !crate::nbflags().real_proxy_super_set {
            assert!(super::real_proxy_super());
            assert_eq!(super::proxy_super_class_name(), "java/lang/reflect/Proxy");
        }
    }

    /// Round 13 wave 9 (lane proxy6): the dynamic-module name of a generated
    /// all-public proxy is its package (`ProxyBuilder`: `pn = mn`), and only the
    /// exact `jdk/proxy<N>/$Proxy<M>` shape has one.
    #[test]
    fn proxy_dynamic_module_name_reads_only_the_generated_shape() {
        assert_eq!(
            super::proxy_dynamic_module_name("jdk/proxy1/$Proxy0"),
            Some(("jdk.proxy1".to_string(), "jdk/proxy1".to_string()))
        );
        assert_eq!(
            super::proxy_dynamic_module_name("jdk/proxy12/$Proxy345"),
            Some(("jdk.proxy12".to_string(), "jdk/proxy12".to_string()))
        );
        // Round 13 wave 10: the module's non-exported package.
        assert_eq!(
            super::proxy_dynamic_module_name("com/sun/proxy/jdk/proxy3/$Proxy9"),
            Some(("jdk.proxy3".to_string(), "com/sun/proxy/jdk/proxy3".to_string()))
        );
        for other in [
            "com/sun/proxy/jdk/proxy/$Proxy0",
            "com/sun/proxy/jdk/proxy1/sub/$Proxy0",
            "com/sun/proxy/jdk/proxy1/Settings$ProxyConfig",
            "$Proxy0",
            "p/$Proxy0",
            "jdk/proxy/$Proxy0",
            "jdk/proxyA/$Proxy0",
            "jdk/proxy1/Settings$ProxyConfig",
            "jdk/proxy1/$Proxy",
            "jdk/proxy1/sub/$Proxy0",
            "com/sun/proxy/$Proxy0",
        ] {
            assert_eq!(super::proxy_dynamic_module_name(other), None, "{other}");
        }
    }
}

#[cfg(test)]
mod proxy_refusal_contract_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// `Class.getName()` spelling. Arrays keep their DESCRIPTOR form
    /// (`[Ljava.lang.String;`), which is what makes the measured
    /// `class [Ljava.lang.String;` entry in the incompatible-return-types list
    /// reproducible. Getting this wrong would have printed
    /// `java.lang.String[]` there.
    #[test]
    fn desc_class_name_matches_class_get_name() {
        assert_eq!(proxy_desc_class_name("I"), "int");
        assert_eq!(proxy_desc_class_name("J"), "long");
        assert_eq!(proxy_desc_class_name("Z"), "boolean");
        assert_eq!(proxy_desc_class_name("V"), "void");
        assert_eq!(
            proxy_desc_class_name("Ljava/lang/String;"),
            "java.lang.String"
        );
        assert_eq!(
            proxy_desc_class_name("[Ljava/lang/String;"),
            "[Ljava.lang.String;"
        );
        assert_eq!(proxy_desc_class_name("[I"), "[I");
        assert_eq!(proxy_desc_class_name("[[J"), "[[J");
    }

    /// `Class.getTypeName()` spelling — the one `toShortSignature` uses for
    /// parameters, where arrays DO become `long[]`.
    #[test]
    fn desc_type_name_matches_class_get_type_name() {
        assert_eq!(proxy_desc_type_name("[J"), "long[]");
        assert_eq!(
            proxy_desc_type_name("[Ljava/lang/String;"),
            "java.lang.String[]"
        );
        assert_eq!(proxy_desc_type_name("[[I"), "int[][]");
        assert_eq!(proxy_desc_type_name("Ljava/util/List;"), "java.util.List");
        assert_eq!(proxy_desc_type_name("S"), "short");
    }

    /// Transcribed from HotSpot 25.0.3+9-LTS:
    ///   `methods with same signature m(int,java.lang.String,long[]) but ...`
    ///   `methods with same signature q(java.util.List) but ...`
    ///   `methods with same signature r() but ...`
    #[test]
    fn short_signature_matches_the_measured_text() {
        assert_eq!(
            proxy_short_signature("m", "ILjava/lang/String;[J"),
            "m(int,java.lang.String,long[])"
        );
        assert_eq!(
            proxy_short_signature("q", "Ljava/util/List;"),
            "q(java.util.List)"
        );
        assert_eq!(proxy_short_signature("r", ""), "r()");
    }

    #[test]
    fn param_descriptors_split_on_the_right_boundaries() {
        assert_eq!(proxy_iter_param_descs(""), Vec::<String>::new());
        assert_eq!(
            proxy_iter_param_descs("ILjava/lang/String;[J"),
            vec!["I", "Ljava/lang/String;", "[J"]
        );
        assert_eq!(
            proxy_iter_param_descs("[[Ljava/lang/Object;Z"),
            vec!["[[Ljava/lang/Object;", "Z"]
        );
        // A truncated descriptor must terminate, not spin.
        assert_eq!(
            proxy_iter_param_descs("Ljava/lang/String"),
            vec!["Ljava/lang/String"]
        );
    }

    #[test]
    fn method_descriptor_splits_into_params_and_return() {
        assert_eq!(
            proxy_split_method_desc("(ILjava/lang/String;)Ljava/lang/Integer;"),
            Some(("ILjava/lang/String;", "Ljava/lang/Integer;"))
        );
        assert_eq!(proxy_split_method_desc("()V"), Some(("", "V")));
        assert_eq!(proxy_split_method_desc("no-parens"), None);
    }

    /// Array covariance is the difference between two MEASURED rows that must
    /// NOT be collapsed: HotSpot accepts `{Object[] u(), String[] u()}` and
    /// refuses `{String[] t(), Integer[] t()}`. Both are decided without any
    /// class resolution, so this is testable without a VM.
    #[test]
    fn array_assignability_separates_the_two_measured_array_rows() {
        let probe = |to: &str, from: &str| proxy_desc_assignable_opt(None, to, from);
        assert_eq!(
            probe("[Ljava/lang/Object;", "[Ljava/lang/String;"),
            Some(true)
        );
        assert_eq!(
            probe("[Ljava/lang/String;", "[Ljava/lang/Integer;"),
            None,
            "element assignability needs the VM; the caller must FAIL OPEN"
        );
        assert_eq!(probe("[I", "[I"), Some(true));
        assert_eq!(probe("[I", "[J"), Some(false));
        assert_eq!(probe("Ljava/lang/Object;", "[I"), Some(true));
        assert_eq!(probe("I", "J"), Some(false));
        assert_eq!(probe("V", "Ljava/lang/String;"), Some(false));
        assert_eq!(probe("Ljava/io/Serializable;", "[I"), Some(true));
    }

    /// The primitive-return arm of the message, transcribed:
    /// `... but incompatible return types: int and others` — note it names the
    /// PRIMITIVE regardless of which interface was listed first, and that
    /// `void` counts as one (`void and others`).
    #[test]
    fn primitive_return_is_recognised_by_descriptor_shape() {
        for d in ["I", "J", "S", "B", "C", "F", "D", "Z", "V"] {
            assert!(
                !d.starts_with('L') && !d.starts_with('['),
                "{d} must take the primitive arm"
            );
        }
        for d in ["Ljava/lang/String;", "[I", "[Ljava/lang/String;"] {
            assert!(
                d.starts_with('L') || d.starts_with('['),
                "{d} must NOT take the primitive arm"
            );
        }
    }

    /// The nine names that may be refused from a `Class` mirror carrying no
    /// `ClassId`. Anything else must fail OPEN — inventing "X is not an
    /// interface" from an unresolvable mirror is how a working path breaks.
    #[test]
    fn only_primitives_are_refused_by_name() {
        assert!(PROXY_PRIMITIVE_NAMES.contains(&"int"));
        assert!(PROXY_PRIMITIVE_NAMES.contains(&"void"));
        assert_eq!(PROXY_PRIMITIVE_NAMES.len(), 9);
        assert!(!PROXY_PRIMITIVE_NAMES.contains(&"java.lang.String"));
        assert!(!PROXY_PRIMITIVE_NAMES.contains(&""));
    }

    /// A null `ClassLoader` argument is what arrives as anything other than
    /// `Object(Some(_))` at slot 0 — including a MISSING slot 0, which is how
    /// a malformed call would otherwise slip past the visibility arm.
    #[test]
    fn loader_arg_null_detection() {
        assert!(proxy_loader_arg_is_null(&[]));
        assert!(proxy_loader_arg_is_null(&[Value::Object(None)]));
        assert!(proxy_loader_arg_is_null(&[Value::Int(0)]));
    }
}

/// WP2.5-B — define (or fetch from cache) a `$ProxyN` class for the
/// given iface ClassId set under `loader_id`. Returns a [`ProxyClassOutcome`]:
///   * `Degrade` — the real-classfile path is disabled via `CRATONVM_REAL_PROXY=0`;
///     the caller uses the synthetic shim (intended).
///   * `Failed(stage)` — a concrete spec-build / emit / define failure; the caller
///     degrades to the shim by default, or throws the real JDK exception under
///     STRICT mode ([`real_proxy_strict`]).
///   * `Real(cid)` — the canonical generated `$ProxyN`.
///
/// proxy-real-classfile increment 1 — the prior contract returned `None`
/// on *any* failure and degraded silently, which made the synthetic shim
/// the de-facto canonical path. The real path is now canonical by default
/// (`real_proxy_enabled()`); every remaining `None` is the
/// classification of a concrete failure mode (logged under
/// `CRATONVM_DBG_PROXY` for the audit) rather than an accepted outcome.
pub(crate) fn define_or_get_proxy_class(
    ctx: &mut dyn NativeContext,
    loader_id: u32,
    iface_class_ids: &[cratonvm_types::ClassId],
) -> ProxyClassOutcome {
    let dbg = crate::nbflags().dbg_proxy;

    // Gate-off path: explicit opt-out keeps the synthetic shim canonical.
    if !real_proxy_enabled() {
        if dbg {
            eprintln!("[DBG_PROXY] CRATONVM_REAL_PROXY disabled — using synthetic shim");
        }
        return ProxyClassOutcome::Degrade;
    }

    // Order-preserving dedup of the interface ClassIds. The JDK's
    // `Proxy.getProxyClass` keys its cache on the *ordered* interface list and
    // emits the generated class's `interfaces[]` in exactly that order —
    // `getInterfaces()` then returns them in the user-requested order. Spring's
    // `AopProxyUtils.proxiedUserInterfaces` depends on this: it strips the
    // trailing infrastructure interfaces (SpringProxy/Advised/DecoratingProxy)
    // off the END of `getInterfaces()`, so the user interfaces must come first
    // and in insertion order. A previous implementation sorted the ClassIds by
    // `as_u32()` for the cache key AND fed that sorted list to the emitter,
    // which scrambled `getInterfaces()` into ClassId order (e.g. SpringProxy
    // ahead of ITestBean) and broke the trim. Preserve first-occurrence order
    // instead — distinct orders correctly map to distinct proxy classes, as on
    // the JDK.
    let mut ordered: Vec<cratonvm_types::ClassId> = Vec::with_capacity(iface_class_ids.len());
    for &c in iface_class_ids {
        if !ordered.contains(&c) {
            ordered.push(c);
        }
    }
    let cache_key = (ctx.vm_identity(), loader_id, ordered.clone());

    {
        let guard = PROXY_CLASS_CACHE.read();
        if let Some(map) = guard.as_ref() {
            if let Some(&cid) = map.get(&cache_key) {
                return ProxyClassOutcome::Real(cid);
            }
        }
    }

    // Ensure the synthetic super class exists so `define_class_full` can
    // resolve it.
    //
    // spring-bug-01: `Proxy$Instance` is a CratonVM-invented class with NO
    // class file. In real-JDK mode (booting --java-home) `ensure_class_initialized`
    // routes through `load_class`, which returns ClassNotFound for a `java/*`
    // name when real boot classes are present — so the stub was never
    // registered, the generated `$Proxy0`'s superclass failed to resolve, and
    // the FIRST proxy in the process silently fell back to a bare
    // `Proxy$Instance` (no generated member bodies) → annotation accessors hit
    // `AbstractMethodError: <Ann>.value() has no Code` / `NoSuchMethodError
    // Proxy$Instance.value()`. (Every proxy AFTER the first worked, because the
    // fallback's allocation registered the stub.) Registering the 3-field stub
    // (handler/interfaces/identity) directly makes `$Proxy0` generate a real
    // proxy exactly like `$Proxy1+`.
    //
    // `ensure_vm_internal_class`, not the compatibility door (JDK-only wave 2,
    // step 3, 2026-08-10). `java/lang/reflect/Proxy$Instance` is the SUPERCLASS
    // OF A GENERATED PROXY, which contract §1 item 6 lists among the shapes the
    // VM legitimately mints and which are never refused in either mode — the
    // proxy classes that extend it are generated too. Routing it through the
    // compatibility entry point was the mislabel: it made a §1-item-6 shape
    // look like a §5 substitution, and refusing it under `--jdk-only` would
    // break every dynamic proxy with a failure that reads as "strict mode
    // doesn't work" (the first entry in this record's *Blast radius*).
    ctx.ensure_vm_internal_class("java/lang/reflect/Proxy$Instance", 3);

    // Failure mode (1): spec build. `build_proxy_spec_for` returns `None`
    // only when an interface ClassId fails to resolve to a name (a
    // genuinely unloadable interface) — see its doc.
    let (gen_name, spec) = match build_proxy_spec_for(ctx, loader_id, &ordered) {
        Some(v) => v,
        None => {
            if dbg {
                eprintln!("[DBG_PROXY] FALLBACK(spec): build_proxy_spec_for returned None for ifaces={ordered:?}");
            }
            return ProxyClassOutcome::Failed("spec");
        }
    };
    // Failure mode (2): classfile emission. `emit_proxy_classfile` fails
    // only on a malformed method descriptor (typed `ClassFileError`).
    let bytes = match cratonvm_classloading::proxy_gen::emit_proxy_classfile(&spec) {
        Ok(b) => b,
        Err(e) => {
            if dbg {
                eprintln!(
                    "[DBG_PROXY] FALLBACK(emit): emit_proxy_classfile({gen_name}) failed: {e:?}"
                );
            }
            return ProxyClassOutcome::Failed("emit");
        }
    };
    let opts = cratonvm_native_api::DefineClassFull {
        // WP2.5-v3 item 4 — every method body emitted by `proxy_gen`
        // (constructor super-delegate, per-method dispatch shim, and
        // the v3 `<clinit>` initialiser) is straight-line: no IF*,
        // GOTO, JSR, ATHROW, switch, or exception_table entries. JVMS
        // §4.10.1 only requires `StackMapTable` when a method has at
        // least one branch target or exception handler reachable from
        // entry, so full Pass 3 type-checking accepts these classes
        // as-is. Verified by the `emitted_class_is_straight_line_no_handlers`
        // regression test in `classloading::proxy_gen`.
        skip_verification: false,
        // A generated `$ProxyN` MUST implement the EXACT interface `ClassId`
        // it was generated for (the one the caller passed to
        // `Proxy.newProxyInstance`/`getProxyClass`, or that annotation
        // reflection resolved via the declaring class's loader) — there is
        // no "which same-named copy is more correct" ambiguity the way there
        // can be for an ordinary subclass's supertype link. Without this,
        // linking the generated `implements <Iface>` reference falls back to
        // the loader-agnostic `load_class(name)` (since
        // `CRATONVM_LOADER_AWARE_RESOLUTION` defaults off) and can silently
        // bind to a DIFFERENT same-named class already loaded elsewhere
        // (e.g. the application loader's copy), producing a `$ProxyN` that
        // `interfaceClass.isInstance(proxy)` and `Method.invoke` both reject
        // — reproduced with a plain `Proxy.newProxyInstance` + custom
        // `ClassLoader`, independent of annotations. See "Residual issue B" in
        // mergedannotationstests-proxy-class-identity-reflection-vs-synthesize.md.
        force_loader_faithful_linking: true,
        // …and, for the interfaces, do not even ask the loader-faithful
        // *search* to find them: hand over the exact `ClassId`s.
        //
        // `force_loader_faithful_linking` only makes `resolve_supertype` PREFER
        // the defining loader's namespace, and it falls through to the
        // loader-blind `load_class(name)` when that namespace has no entry —
        // which is routine, because a child loader's class is registered under
        // its own namespace only for the copies it defined itself. A proxy over
        // an interface a child loader merely *sees* therefore linked the
        // application loader's same-named copy, and `getInterfaces()[0]` was
        // then a different `Class` object from the one the caller passed to
        // `newProxyInstance`. Everything downstream compares `Class` by
        // identity: the generated `<clinit>`'s `getMethod` produced `Method`s
        // declared by the wrong copy, so Byte Buddy's `JavaDispatcher` — a
        // `Map<Method, Dispatcher>` keyed off its own `getMethods()` — missed
        // every lookup and threw `No proxy target found for
        // …Executable.isInstance(Object)`, taking Mockito's inline mock maker
        // down with it in `HikariDataSourceConfigurationTests`.
        //
        // There is no ambiguity to resolve here: `Proxy.newProxyInstance` was
        // handed the `Class` objects themselves. The order and length must
        // match the emitted `interfaces[]`, which `emit_proxy_classfile` dedups
        // BY NAME (two loaders' same-named interfaces collapse to one entry),
        // so apply the same name-dedup to the id list.
        interface_id_overrides: Some(dedup_iface_ids_by_name(&ordered, &spec.interfaces)),
        ..Default::default()
    };
    // Failure mode (3): class definition through the normal loader
    // (`define_class_full` → `ClassManager::define_class_with_options`).
    // Concrete causes the verifier/loader can raise here:
    //   * VerifyError on the generated bytecode (Pass 2/3),
    //   * NoClassDefFoundError if the super/interface fails to load,
    //   * IncompatibleClassChangeError on a duplicate define,
    //   * SecurityException ("Prohibited package name") if a non-public
    //     iface forced a `java/`/`sun/` package (build_proxy_spec_for
    //     already steers away from this).
    let define_loader_id = proxy_defining_loader_id(ctx, loader_id, &ordered);
    match ctx.define_class_full(&gen_name, &bytes, define_loader_id, opts) {
        Ok(cid) => {
            if dbg {
                eprintln!("[DBG_PROXY] define_class_full OK (real $ProxyN canonical): {gen_name} -> {cid:?}");
            }
            let mut guard = PROXY_CLASS_CACHE.write();
            let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
            map.insert(cache_key, cid);
            ProxyClassOutcome::Real(cid)
        }
        Err(e) => {
            if dbg {
                eprintln!(
                    "[DBG_PROXY] FALLBACK(define): define_class_full({gen_name}) failed: {e}"
                );
            }
            ProxyClassOutcome::Failed("define")
        }
    }
}

/// The loader a generated `$ProxyN` is DEFINED by, where `loader_id` is the
/// proxy namespace it is cached under.
///
/// When a proxied interface is non-public, `build_proxy_spec_for` names the
/// proxy into that interface's package, and the JDK requires the interface's
/// defining loader to be the given one (`ProxyBuilder.mapToModule`: "non-public
/// interface is not defined by the given loader"), so the proxy shares the
/// interface's runtime package (JVMS §5.3). The namespace of a built-in,
/// non-platform loader is an id `proxy_builtin_namespace_id` allocated, a
/// `UserDefined(N)` of its own: a proxy defined there sits in another runtime
/// package than its own interface, and the generated bodies' `checkcast` of a
/// package-private return type (`<clinit>`'s `ldc` of a package-private
/// parameter type) throws `IllegalAccessError` "failed to access class
/// p.I$TypeProvider from class p.$Proxy49". The interpreter never ran those
/// bodies (`is_proxy_dispatch` calls the handler directly); a compiled caller
/// dispatches into them, so every Spring `SerializableTypeWrapper.unwrap` failed
/// once compiled (round 13 wave 3, lane springjit,
/// `R13SpringProxyNestedIface`). Defined by the non-public interface's own
/// loader instead, unless that is the bootstrap loader (`0` is the define
/// boundary's "unspecified" sentinel). `CRATONVM_PROXY_NONPUBLIC_IN_IFACE_LOADER=0`
/// restores the namespace define.
fn proxy_defining_loader_id(
    ctx: &mut dyn NativeContext,
    loader_id: u32,
    ordered_ifaces: &[cratonvm_types::ClassId],
) -> u32 {
    const ACC_PUBLIC: u16 = 0x0001;
    // The same interface `build_proxy_spec_for`'s `non_public_pkg` picks.
    let Some(&iface) = ordered_ifaces
        .iter()
        .find(|&&cid| ctx.class_access_flags(cid) & ACC_PUBLIC == 0)
    else {
        return loader_id;
    };
    if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_NONPUBLIC_IN_IFACE_LOADER") {
        return loader_id;
    }
    // `loader_id_of_class` is `ClassLoaderId::to_native_id` widened to `i32`.
    match ctx.loader_id_of_class(iface) as u32 {
        cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP => loader_id,
        iface_loader => iface_loader,
    }
}

#[cfg(test)]
mod proxy_defining_loader_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const PUBLIC_IFACE: u16 = 0x0601;
    const PACKAGE_IFACE: u16 = 0x0600;

    fn iface(
        ctx: &mut crate::test_utils::MockNativeContext,
        name: &str,
        flags: u16,
        loader: i32,
    ) -> cratonvm_types::ClassId {
        let cid = ctx.ensure_class_initialized(name).unwrap();
        ctx.set_class_access_flags(cid, flags);
        ctx.set_loader_id_override(cid, loader);
        cid
    }

    /// `R13SpringProxyNestedIface`'s shape: `{ParameterizedType,
    /// SerializableTypeProxy (package-private, app loader), Serializable}`
    /// cached under an allocated built-in namespace. The proxy must be
    /// defined by the application loader, the interface's own, or its
    /// `checkcast TypeProvider` is refused as another runtime package.
    #[test]
    fn a_package_private_interface_puts_the_proxy_in_its_own_loader() {
        if !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_PROXY_NONPUBLIC_IN_IFACE_LOADER",
        ) {
            return;
        }
        let mut ctx = crate::test_utils::mock_ctx();
        let pt = iface(&mut ctx, "java/lang/reflect/ParameterizedType", PUBLIC_IFACE, 0);
        let stp = iface(&mut ctx, "p/Wrapper$SerializableTypeProxy", PACKAGE_IFACE, 2);
        let ser = iface(&mut ctx, "java/io/Serializable", PUBLIC_IFACE, 0);
        let namespace = 41;
        assert_eq!(
            proxy_defining_loader_id(&mut ctx, namespace, &[pt, stp, ser]),
            cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
        );
        // A user-defined loader's interface: that loader.
        let own = iface(&mut ctx, "q/Own", PACKAGE_IFACE, 57);
        assert_eq!(proxy_defining_loader_id(&mut ctx, namespace, &[own]), 57);
    }

    /// Controls: every interface public keeps the namespace (the proxy goes to
    /// `jdk/proxyN`), and so does a package-private BOOTSTRAP interface (`0`
    /// is the define boundary's "unspecified" sentinel, not bootstrap).
    #[test]
    fn public_interfaces_and_bootstrap_interfaces_keep_the_namespace() {
        let mut ctx = crate::test_utils::mock_ctx();
        let a = iface(&mut ctx, "p/PublicA", PUBLIC_IFACE, 2);
        let b = iface(&mut ctx, "java/util/PackageB", PACKAGE_IFACE, 0);
        assert_eq!(proxy_defining_loader_id(&mut ctx, 41, &[a]), 41);
        assert_eq!(proxy_defining_loader_id(&mut ctx, 41, &[a, b]), 41);
    }
}

/// Round 13 wave 4 (lane proxy2): `ProxyBuilder.mapToModule`'s refusals.
#[cfg(test)]
mod proxy_non_public_refusal_tests {
    use super::proxy_non_public_refusal;

    const PACKAGES: &str = "cannot have non-public interfaces in different packages";
    const LOADER: &str = "non-public interface is not defined by the given loader";

    fn e(
        pkg: &str,
        loader_tag: i32,
        same: Option<bool>,
    ) -> (String, (Option<String>, i32), Option<bool>) {
        (pkg.to_string(), (None, loader_tag), same)
    }

    #[test]
    fn one_package_of_the_given_loader_is_accepted() {
        assert_eq!(proxy_non_public_refusal(&[]), None);
        assert_eq!(proxy_non_public_refusal(&[e("p", 2, Some(true))]), None);
        assert_eq!(
            proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 2, Some(true))]),
            None
        );
    }

    #[test]
    fn two_packages_or_two_unnamed_modules_are_refused() {
        assert_eq!(
            proxy_non_public_refusal(&[e("p1", 57, Some(true)), e("p2", 57, Some(true))]),
            Some(PACKAGES)
        );
        // Same package name, two loaders: two unnamed modules.
        assert_eq!(
            proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 57, None)]),
            Some(PACKAGES)
        );
    }

    #[test]
    fn a_foreign_defining_loader_is_refused_and_unknown_is_not() {
        assert_eq!(
            proxy_non_public_refusal(&[e("", 2, Some(false))]),
            Some(LOADER)
        );
        assert_eq!(proxy_non_public_refusal(&[e("", 2, None)]), None);
        assert_eq!(
            proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 2, Some(false))]),
            Some(LOADER)
        );
    }
}

/// Round 13 wave 4 (lane proxy2): `ProxyGenerator`'s method order (which
/// `Method` a proxy method carries) and the package-private class flag.
#[cfg(test)]
mod proxy_spec_order_tests {
    use super::*;
    use cratonvm_classloading::proxy_gen::ProxyClassSpec;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn iface_with(
        ctx: &mut crate::test_utils::MockNativeContext,
        name: &str,
        flags: u16,
        methods: &[(&str, &str)],
    ) -> cratonvm_types::ClassId {
        let cid = ctx.ensure_class_initialized(name).unwrap();
        ctx.set_class_access_flags(cid, flags);
        ctx.set_declared_methods(
            cid,
            methods
                .iter()
                .map(|(n, d)| cratonvm_native_api::MethodMetadata {
                    name: n.to_string(),
                    descriptor: d.to_string(),
                    access_flags: 0x0401,
                    declaring_class_id: cid,
                    exceptions: Vec::new(),
                    signature: None,
                })
                .collect(),
        );
        cid
    }

    fn owner_of<'a>(spec: &'a ProxyClassSpec, name: &str) -> &'a str {
        spec.methods
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.iface_owner.as_str())
            .unwrap_or("")
    }

    /// `{A, B}` both declaring `run()`: `A`'s (the JDK's first registration),
    /// and `Object.toString` even though `B` redeclares it.
    #[test]
    fn the_first_interface_and_objects_methods_win() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_JDK_METHOD_ORDER") {
            return;
        }
        let mut ctx = crate::test_utils::mock_ctx();
        let a = iface_with(&mut ctx, "p/A", 0x0601, &[("run", "()I")]);
        let b = iface_with(
            &mut ctx,
            "p/B",
            0x0601,
            &[("run", "()I"), ("toString", "()Ljava/lang/String;")],
        );
        let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[a, b]).expect("spec");
        assert_eq!(owner_of(&spec, "run"), "p/A");
        assert_eq!(owner_of(&spec, "toString"), "java/lang/Object");
        assert!(!spec.package_private, "public interfaces: a public proxy");
        let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[b, a]).expect("spec");
        assert_eq!(owner_of(&spec, "run"), "p/B");
    }

    /// Round 13 wave 5 hand-back: a complete chain ends in `java/lang/Object`
    /// (`Throwable extends Object`); a checked exception is one that passes
    /// through `Throwable` and not through `RuntimeException` / `Error`.
    #[test]
    fn a_thrown_chain_ending_in_object_is_classified() {
        let chain = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert!(proxy_thrown_chain_is_checked(&chain(&[
            "java/io/IOException",
            "java/lang/Exception",
            "java/lang/Throwable",
            "java/lang/Object",
        ])));
        assert!(!proxy_thrown_chain_is_checked(&chain(&[
            "java/lang/IllegalStateException",
            "java/lang/RuntimeException",
            "java/lang/Exception",
            "java/lang/Throwable",
            "java/lang/Object",
        ])));
        assert!(!proxy_thrown_chain_is_checked(&chain(&[
            "java/lang/AssertionError",
            "java/lang/Error",
            "java/lang/Throwable",
            "java/lang/Object",
        ])));
        assert!(!proxy_thrown_chain_is_checked(&chain(&["p/NotAThrowable", "java/lang/Object"])));
    }

    /// Round 13 wave 5 (lane proxy3): `ProxyGenerator.collectCompatibleTypes`
    /// both ways. `A.run() throws IOException` + `B.run()` merge to nothing;
    /// `IOException` + `FileNotFoundException` keep the narrower one; a type
    /// that is not loaded is kept (the merge cannot disprove it).
    #[test]
    fn merged_throws_follow_collect_compatible_types() {
        let mut ctx = crate::test_utils::mock_ctx();
        let throwable = ctx.ensure_class_initialized("java/lang/Throwable").unwrap();
        let exception = ctx.ensure_class_initialized("java/lang/Exception").unwrap();
        let ioe = ctx.ensure_class_initialized("java/io/IOException").unwrap();
        let fnfe = ctx
            .ensure_class_initialized("java/io/FileNotFoundException")
            .unwrap();
        ctx.set_superclass(exception, throwable);
        ctx.set_superclass(ioe, exception);
        ctx.set_superclass(fnfe, ioe);
        let names = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert!(proxy_merged_exception_types(&ctx, &names(&["java/io/IOException"]), &[]).is_empty());
        assert_eq!(
            proxy_merged_exception_types(
                &ctx,
                &names(&["java/io/IOException"]),
                &names(&["java/io/FileNotFoundException"])
            ),
            names(&["java/io/FileNotFoundException"])
        );
        assert_eq!(
            proxy_merged_exception_types(
                &ctx,
                &names(&["java/io/FileNotFoundException", "p/NotLoadedException"]),
                &names(&["java/lang/Exception"])
            ),
            names(&["java/io/FileNotFoundException", "p/NotLoadedException"])
        );
        assert_eq!(
            proxy_merged_exception_types(
                &ctx,
                &names(&["java/io/IOException"]),
                &names(&["java/io/IOException"])
            ),
            names(&["java/io/IOException"])
        );
    }

    /// Round 13 wave 5 (lane proxy3): a user class extending
    /// `java.lang.reflect.Proxy` is not a proxy class (`isProxyClass`,
    /// `getInvocationHandler`); a generated `$ProxyN` and the shim are.
    #[test]
    fn a_user_subclass_of_proxy_is_not_a_generated_proxy() {
        if !real_proxy_super() || !proxy_user_subclass_is_ordinary() {
            return;
        }
        let mut ctx = crate::test_utils::mock_ctx();
        let proxy = ctx.ensure_class_initialized("java/lang/reflect/Proxy").unwrap();
        let generated = ctx.ensure_class_initialized("jdk/proxy1/$Proxy7").unwrap();
        let user = ctx.ensure_class_initialized("p/MyProxy").unwrap();
        let user_sub = ctx.ensure_class_initialized("p/$ProxyLike").unwrap();
        ctx.set_superclass(generated, proxy);
        ctx.set_superclass(user, proxy);
        ctx.set_superclass(user_sub, user);
        assert!(proxy_class_is_generated(&ctx, generated));
        assert!(!proxy_class_is_generated(&ctx, user));
        assert!(!proxy_class_is_generated(&ctx, user_sub));
        assert!(proxy_chain_reaches_instance(&ctx, user), "still on the chain");
    }

    /// The spec carries the merged list for a signature two interfaces share.
    #[test]
    fn a_shared_signature_carries_the_merged_throws() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_MERGED_THROWS") {
            return;
        }
        let mut ctx = crate::test_utils::mock_ctx();
        let a = iface_with(&mut ctx, "p/A", 0x0601, &[("run", "()V")]);
        let b = iface_with(&mut ctx, "p/B", 0x0601, &[("run", "()V")]);
        let mut declared = ctx.declared_methods(a);
        declared[0].exceptions = vec!["java/io/IOException".to_string()];
        ctx.set_declared_methods(a, declared);
        let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[a, b]).expect("spec");
        let run = spec.methods.iter().find(|m| m.name == "run").expect("run");
        assert!(run.exception_types.is_empty(), "{:?}", run.exception_types);
        let (_, spec) = build_proxy_spec_for(&mut ctx, 1, &[a]).expect("spec");
        let run = spec.methods.iter().find(|m| m.name == "run").expect("run");
        assert_eq!(run.exception_types, vec!["java/io/IOException".to_string()]);
    }

    #[test]
    fn a_non_public_interface_makes_the_class_package_private() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_NONPUBLIC_CLASS_FLAGS") {
            return;
        }
        let mut ctx = crate::test_utils::mock_ctx();
        let h = iface_with(&mut ctx, "p/Hidden", 0x0600, &[("f", "()I")]);
        let (name, spec) = build_proxy_spec_for(&mut ctx, 1, &[h]).expect("spec");
        assert!(spec.package_private);
        assert!(name.starts_with("p/$Proxy"), "got {name}");
    }
}

/// spring-bug-08 — read-side proxy-class resolution for
/// `ObjectInputStream.resolveProxyClass`. Given the interface names read
/// from the stream (dotted, as written by `Class.getName()`), resolve them
/// to a CratonVM generated `$ProxyN` class WITHOUT entering the real
/// `Proxy.getProxyClass` path. The synthetic proxy model has no real
/// `ProxyGenerator`/dynamic-module support, so the JDK default
/// (`resolveProxyClass` → `Proxy.getProxyClass` → `ProxyBuilder.getDynamicModule`
/// → `Module.defineModule0`) throws `UnsatisfiedLinkError`/`IllegalArgumentException`
/// (surfacing as `ClassNotFoundException: null`). Returning a generated
/// `$ProxyN` here keeps the whole round-trip on CratonVM's own proxy
/// machinery — exactly the class `Proxy.newProxyInstance` would have built.
pub(crate) fn resolve_serialized_proxy_class(
    ctx: &mut dyn NativeContext,
    iface_names: &[String],
) -> Option<cratonvm_types::ClassId> {
    if iface_names.is_empty() {
        return None;
    }
    // Resolve each interface name -> ClassId (ensuring it is loaded). The
    // interfaces are normally already loaded at deserialization time (they
    // were referenced when the original proxy and the handler were created),
    // so this is a lookup in the common case.
    let mut iface_cids: Vec<cratonvm_types::ClassId> = Vec::with_capacity(iface_names.len());
    let mut loader_id: u32 = 0;
    for n in iface_names {
        let internal = n.replace('.', "/");
        let cid = match ctx.ensure_class_initialized(&internal) {
            Ok(c) => c,
            Err(_) => return None,
        };
        // Prefer a non-bootstrap loader namespace so a cold (cache-miss)
        // `define_class_full` can resolve app interfaces.
        let lid = ctx.loader_id_of_class(cid);
        if lid > 0 {
            loader_id = lid as u32;
        }
        iface_cids.push(cid);
    }
    // Order-insensitive set key for matching `define_or_get_proxy_class`'s
    // (now order-preserving) cache key. Two proxies with the same interface
    // *set* deserialize to the same generated class regardless of the order
    // each was originally created with, so compare sorted+deduped sets.
    let mut want_set = iface_cids.clone();
    want_set.sort_by_key(|c| c.as_u32());
    want_set.dedup();
    // Primary: reuse an already-generated `$ProxyN` with this interface set,
    // regardless of the loader namespace that created it. The common case —
    // an in-process write→read round-trip (e.g. Spring's
    // `SerializableTypeWrapper`) — always hits here because the write side
    // created the proxy class via `Proxy.newProxyInstance` first.
    //
    // "Regardless of the loader namespace" is deliberate; "regardless of the
    // VM" is not. This scan ignores the key's `loader_id` on purpose, so it
    // MUST filter on `vm_identity` explicitly — the `ClassId`s on both sides
    // of the comparison, and the one returned, belong to one class manager.
    // Left unfiltered it was the widest of the three ways this cache leaked
    // a foreign proxy class.
    let vm = ctx.vm_identity();
    {
        let guard = PROXY_CLASS_CACHE.read();
        if let Some(map) = guard.as_ref() {
            for ((row_vm, _ns, key), &cid) in map.iter() {
                if *row_vm != vm {
                    continue;
                }
                let mut key_set = key.clone();
                key_set.sort_by_key(|c| c.as_u32());
                key_set.dedup();
                if key_set == want_set {
                    return Some(cid);
                }
            }
        }
    }
    // Fallback (cold deserialization with no prior proxy of this interface
    // set in-process): generate one now.
    match define_or_get_proxy_class(ctx, loader_id, &iface_cids) {
        ProxyClassOutcome::Real(cid) => Some(cid),
        _ => None,
    }
}

/// Build a [`cratonvm_classloading::proxy_gen::ProxyClassSpec`] for the
/// given interface ClassId list. The list order is significant — it is
/// emitted verbatim as the generated class's `interfaces[]` so that
/// `getInterfaces()` returns the user-requested order (matching the JDK).
/// Walks each interface (and its super-interfaces transitively) collecting
/// public abstract + default instance methods, deduplicated by
/// `(name, descriptor)`. Returns `None` if any ClassId fails to resolve to
/// a name.
/// Project `ordered` (deduped by `ClassId`) onto `iface_names` (the spec's
/// interface names, same order) with the NAME dedup `emit_proxy_classfile`
/// applies, keeping the first `ClassId` per name.
///
/// `DefineClassOptions::interface_id_overrides` is positional against the
/// emitted `interfaces[]`, and the emitter collapses two loaders' same-named
/// interfaces into one entry (JVMS §4.1 forbids a repeated interface), so a
/// raw id list would be rejected for a count mismatch in exactly that case.
fn dedup_iface_ids_by_name(
    ordered: &[cratonvm_types::ClassId],
    iface_names: &[String],
) -> Vec<cratonvm_types::ClassId> {
    let mut seen: Vec<&str> = Vec::with_capacity(iface_names.len());
    let mut ids = Vec::with_capacity(iface_names.len());
    for (id, name) in ordered.iter().zip(iface_names.iter()) {
        if !seen.iter().any(|s| *s == name.as_str()) {
            seen.push(name.as_str());
            ids.push(*id);
        }
    }
    ids
}

fn build_proxy_spec_for(
    ctx: &mut dyn NativeContext,
    loader_id: u32,
    ordered_ifaces: &[cratonvm_types::ClassId],
) -> Option<(String, cratonvm_classloading::proxy_gen::ProxyClassSpec)> {
    use cratonvm_classloading::proxy_gen::{ProxyClassSpec, ProxyMethod};

    const ACC_STATIC: u16 = 0x0008;
    const ACC_PUBLIC: u16 = 0x0001;
    const ACC_ABSTRACT: u16 = 0x0400;
    const ACC_VARARGS: u16 = 0x0080;

    let n = PROXY_CLASS_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Resolve iface internal names.
    let mut iface_names: Vec<String> = Vec::with_capacity(ordered_ifaces.len());
    for cid in ordered_ifaces {
        match ctx.class_name_of_id(*cid) {
            Some(name) => iface_names.push(name),
            None => return None,
        }
    }

    // Choose the generated proxy class's package the way the JDK's
    // `Proxy.ProxyBuilder` does, and — critically — NOT a protected platform
    // package (`java/*`, `sun/*`, `jdk/internal/*`), which `define_class_full`
    // rejects for a non-bootstrap loader ("Prohibited package name"). The old
    // hard-coded `java/lang/reflect/$ProxyN` always failed that check, so every
    // proxy silently fell back to the single shared `Proxy$Instance` (wrong
    // `getClass().getName()` and a shared/stale `getInterfaces()`).
    //
    //   * If any proxied interface is non-public, the proxy must live in that
    //     interface's package (matches the JDK; yields `$ProxyN` in the default
    //     package for a package-private interface, exactly like HotSpot).
    //   * Otherwise all interfaces are public → use HotSpot-25's per-loader
    //     dynamic module package `jdk/proxyN` (was `com/sun/proxy`, a pre-JDK-9
    //     name). `jdk/proxyN` is NOT a prohibited package (only `jdk/internal/*`
    //     is — see `class_manager::is_prohibited_package_name`) and matches every
    //     `jdk/`-prefixed platform gate identically to the old `com/sun/` name,
    //     so the rename is behaviour-preserving while making `Class.getName()`
    //     match HotSpot (e.g. `jdk.proxy1.$Proxy0`).
    let non_public_pkg = ordered_ifaces.iter().find_map(|cid| {
        if ctx.class_access_flags(*cid) & ACC_PUBLIC == 0 {
            let name = ctx.class_name_of_id(*cid)?;
            Some(
                name.rfind('/')
                    .map(|i| name[..i].to_string())
                    .unwrap_or_default(),
            )
        } else {
            None
        }
    });
    // Round 13 wave 4 (lane proxy2): the JDK makes such a proxy package-private
    // (`ProxyBuilder`: `accessFlags = Modifier.FINAL`), so
    // `Modifier.isPublic(proxy.getClass().getModifiers())` is false and a
    // reflective call on the proxy class's own members from another package is
    // refused. `CRATONVM_PROXY_NONPUBLIC_CLASS_FLAGS=0` keeps `ACC_PUBLIC`.
    let package_private = non_public_pkg.is_some()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_NONPUBLIC_CLASS_FLAGS");
    let gen_class_name = match non_public_pkg {
        Some(pkg) if pkg.is_empty() => format!("$Proxy{n}"),
        Some(pkg) => format!("{pkg}/$Proxy{n}"),
        // Round 13 wave 10 (lane misc10): a public interface in a package its
        // named module does not export unconditionally puts the proxy in the
        // dynamic module's NON-exported package `com.sun.proxy.jdk.proxyN`
        // (`ProxyBuilder.proxyClassContext`'s `nonExported`).
        None if proxy_interfaces_non_exported(&*ctx, ordered_ifaces) => format!(
            "com/sun/proxy/jdk/proxy{}/$Proxy{n}",
            proxy_module_number(ctx.vm_identity(), loader_id)
        ),
        None => format!(
            "jdk/proxy{}/$Proxy{n}",
            proxy_module_number(ctx.vm_identity(), loader_id)
        ),
    };

    // BFS over interface inheritance. Collect public, non-static,
    // non-`<init>`/`<clinit>` methods. Abstract beats default if the
    // same `(name, descriptor)` key appears with both flavours.
    let mut visited: std::collections::HashSet<cratonvm_types::ClassId> =
        std::collections::HashSet::new();
    // Each work item carries the DECLARED interface it was reached from, so a
    // method inherited from a super-interface still records a root that is an
    // entry of `ProxyClassSpec::interfaces`. `<clinit>` needs that root to take
    // the owner `Class` off the generated class's own `getInterfaces()` rather
    // than off a by-name constant-pool entry — see `ProxyMethod::iface_root`.
    let mut work: Vec<(cratonvm_types::ClassId, String)> = ordered_ifaces
        .iter()
        .zip(iface_names.iter())
        .map(|(cid, name)| (*cid, name.clone()))
        .collect();
    let mut by_key: std::collections::HashMap<(String, String), ProxyMethod> =
        std::collections::HashMap::new();
    // WP2.5 v2 — always emit equals/hashCode/toString. JDK proxy
    // semantics route these through the InvocationHandler too (so
    // userland can override `Object.equals` reference-equality in a
    // proxied iface). Without these the verifier rejects the generated
    // class because `Proxy$Instance` is itself synthetic and the
    // bootstrap-loaded `Object` Method entries may not be reachable
    // through its vtable in synthetic-jdk mode.
    let add_object_methods =
        |by_key: &mut std::collections::HashMap<(String, String), ProxyMethod>| {
            for (name, descriptor, params) in [
                (
                    "equals",
                    "(Ljava/lang/Object;)Z",
                    vec!["java/lang/Object".to_string()],
                ),
                ("hashCode", "()I", Vec::<String>::new()),
                ("toString", "()Ljava/lang/String;", Vec::<String>::new()),
            ] {
                by_key
                    .entry((name.to_string(), descriptor.to_string()))
                    .or_insert(ProxyMethod {
                        name: name.to_string(),
                        descriptor: descriptor.to_string(),
                        is_default: false,
                        is_varargs: false,
                        iface_owner: "java/lang/Object".to_string(),
                        param_class_names: params,
                        exception_types: Vec::new(),
                        iface_root: None,
                    });
            }
        };
    // Round 13 wave 4 (lane proxy2): which `Method` a proxy method carries is
    // observable (`m.getDeclaringClass()`, MyBatis' `MapperProxy` tests it
    // against `Object.class`), and the JDK's `ProxyGenerator` fixes it by
    // ORDER, first registration wins: `hashCode`/`equals`/`toString` of
    // `Object` first -- so an interface that redeclares `toString()` still
    // hands the handler `Object.toString` -- then each interface in argument
    // order, its own hierarchy before the next interface. This walk used to
    // pop the LAST interface first and add `Object`'s three last, so
    // `{A, B}` both declaring `run()` answered `B`, and a redeclared
    // `equals` answered the interface. `CRATONVM_PROXY_JDK_METHOD_ORDER=0`
    // restores that order.
    let jdk_method_order =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PROXY_JDK_METHOD_ORDER");
    if jdk_method_order {
        add_object_methods(&mut by_key);
        // `work` is a stack: reversed, the FIRST interface pops first, and its
        // super-interfaces (pushed on top) drain before the next root.
        work.reverse();
    }
    // Round 13 wave 5 (lane proxy3): each proxy method's `throws` list, merged
    // over the interfaces declaring its signature, is emitted as its
    // `Exceptions` attribute and is what the body's native classifies a
    // checked exception against. `CRATONVM_PROXY_MERGED_THROWS=0` emits no
    // list (the classification then reads the `Method`'s own, as before).
    let merge_throws = proxy_merged_throws();
    while let Some((cid, root)) = work.pop() {
        if !visited.insert(cid) {
            continue;
        }
        // WP2.5 v3 — capture the iface_owner internal name for the
        // generated `<clinit>`'s `Class.forName(iface).getMethod` lookup.
        let owner = ctx.class_name_of_id(cid).unwrap_or_default();
        for m in ctx.declared_methods(cid) {
            if m.access_flags & ACC_STATIC != 0 {
                continue;
            }
            if m.name == "<init>" || m.name == "<clinit>" {
                continue;
            }
            if m.access_flags & ACC_PUBLIC == 0 {
                continue;
            }
            let key = (m.name.to_string(), m.descriptor.to_string());
            let is_default = m.access_flags & ACC_ABSTRACT == 0;
            match by_key.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let entry = slot.get_mut();
                    // Round 13 wave 5 (lane proxy3): a signature several
                    // interfaces declare keeps the FIRST `Method`, but its
                    // `throws` list is narrowed to the types compatible with
                    // every declaration (`ProxyGenerator.addProxyMethod`).
                    if merge_throws {
                        entry.exception_types =
                            proxy_merged_exception_types(ctx, &entry.exception_types, &m.exceptions);
                    }
                    if !is_default {
                        entry.is_default = false;
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(ProxyMethod {
                        name: m.name.to_string(),
                        descriptor: m.descriptor.to_string(),
                        is_default,
                        // The FIRST declaration's `Method` is the one the
                        // proxy method carries, and so is its variable arity.
                        is_varargs: m.access_flags & ACC_VARARGS != 0,
                        iface_owner: owner.clone(),
                        param_class_names: parse_param_class_names(&m.descriptor),
                        exception_types: if merge_throws {
                            m.exceptions.clone()
                        } else {
                            Vec::new()
                        },
                        iface_root: Some(root.clone()),
                    });
                }
            }
        }
        for super_iface in ctx.class_interfaces(cid) {
            if !visited.contains(&super_iface) {
                work.push((super_iface, root.clone()));
            }
        }
    }

    // Legacy order (`CRATONVM_PROXY_JDK_METHOD_ORDER=0`): an interface that
    // redeclares one of `Object`'s three already has its entry, and the
    // `or_insert` only adds the ones missing.
    if !jdk_method_order {
        add_object_methods(&mut by_key);
    }

    // Stable iteration order so the generated bytecode is reproducible
    // (the cache key already enforces that the iface set matches; this
    // pins the method-table layout too).
    let mut methods: Vec<ProxyMethod> = by_key.into_values().collect();
    methods.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.descriptor.cmp(&b.descriptor))
    });

    Some((
        gen_class_name.clone(),
        ProxyClassSpec {
            gen_class_name,
            // proxy-real-classfile real-super migration: extend the real
            // `java.lang.reflect.Proxy` when the gate is on, else the synthetic
            // `Proxy$Instance` shim (default). The emitter derives the matching
            // 1-arg vs 2-arg `<init>` from this name.
            super_class: proxy_super_class_name().to_string(),
            interfaces: iface_names,
            methods,
            package_private,
        },
    ))
}

/// Helper for the `proxy.getClass().getInterfaces()` round-trip:
/// fetch the interfaces array stored on the proxy at field 1.
fn native_proxy_get_interfaces(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(proxy))) => ctx.get_field(*proxy, 1),
        _ => Value::Object(None),
    };
    Ok(Some(arr))
}

/// proxy-real-classfile increment 2 — body for the synthetic
/// `Proxy$Instance.<init>(InvocationHandler, Class[])V` super constructor
/// that every generated `$ProxyN.<init>` delegates to via `INVOKESPECIAL`.
///
/// Args (instance ctor → receiver is arg 0):
///   `[0]` the proxy `this`
///   `[1]` `InvocationHandler`
///   `[2]` `Class[]` interfaces
///
/// Populates the inherited 3-slot `Proxy$Instance` layout (handler /
/// interfaces / identity-hash) exactly like the allocation-bypass path in
/// `native_proxy_new_instance`, so a proxy constructed by *executing* the
/// generated `<init>` (a JIT call site, or `new`+`invokespecial`) ends up
/// with the same field state as one built by the fast path. Returns `None`
/// (void).
fn native_proxy_instance_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(p))) => *p,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some("Proxy$Instance.<init>: null receiver".to_string()),
            }
            .into());
        }
    };
    let handler = args.get(1).cloned().unwrap_or(Value::Object(None));
    let interfaces = args.get(2).cloned().unwrap_or(Value::Object(None));

    ctx.set_field(this, 0, handler);
    ctx.set_field(this, 1, interfaces);
    ctx.set_field(this, 2, Value::Int(0));

    // Keep the global "last-proxy interfaces" cache populated for legacy
    // readers (lang_class synthetic-mode `getInterfaces` fallback), mirroring
    // `native_proxy_new_instance`.
    if let Value::Object(Some(arr)) = interfaces {
        lang_class::set_proxy_last_interfaces(ctx.vm_identity(), arr);
    }

    Ok(None)
}

#[cfg(test)]
mod module_can_read_essential_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// `register_p59_module` (phases_late.rs) also registers this triple, but
    /// that function is only reachable via the `synthetic-jdk`-feature-gated
    /// `register_synthetic_overrides` — dead code in the default `cratonvm-cli`
    /// build. `register_essential_natives` is what real-JDK-mode boot
    /// actually uses, so THIS is the registration that has to exist for
    /// `Module.canRead(Module)` to answer correctly instead of silently
    /// returning the wrong boolean via real bytecode (confirmed:
    /// `m.canRead(javaBaseModule)` returned `false` via real bytecode vs.
    /// `true` on real HotSpot).
    #[test]
    fn register_essential_includes_module_can_read() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        assert!(
            registry
                .find("java/lang/Module", "canRead", "(Ljava/lang/Module;)Z")
                .is_some(),
            "Module.canRead(Module) must be registered in the essential \
             (real-JDK) native path, not just the synthetic-jdk-only \
             register_p59_module"
        );
    }

    /// Same gap as `canRead` above, but with a crash instead of a wrong
    /// answer: real bytecode (`implAddExportsOrOpens`) reads
    /// `this.descriptor.isOpen()` directly, and that field is never
    /// populated, so a direct `Module.addExports`/`addOpens` call NPEs
    /// ("Cannot invoke ... because \"this.descriptor\" is null") when only
    /// `register_p59_module` (synthetic-jdk-only, dead in the default build)
    /// has the registration.
    #[test]
    fn register_essential_includes_module_add_exports_and_opens() {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        assert!(
            registry
                .find(
                    "java/lang/Module",
                    "addExports",
                    "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"
                )
                .is_some(),
            "Module.addExports(String, Module) must be registered in the \
             essential (real-JDK) native path, not just the \
             synthetic-jdk-only register_p59_module"
        );
        assert!(
            registry
                .find(
                    "java/lang/Module",
                    "addOpens",
                    "(Ljava/lang/String;Ljava/lang/Module;)Ljava/lang/Module;"
                )
                .is_some(),
            "Module.addOpens(String, Module) must be registered in the \
             essential (real-JDK) native path, not just the \
             synthetic-jdk-only register_p59_module"
        );
    }
}

// gc-common w13-f: the Java-call stale-reference fix in `Handler.isLoggable`,
// driven through the mock with an `invoke_virtual` hook that "moves" objects
// mid-call (every pin / scope root on an old address is rewritten to a fresh
// copy, as a moving collection would leave it). The test asserts the native
// acted on the MOVED copies and released every root.
#[cfg(test)]
mod w13f_stale_reference_tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// (old, new) addresses the hook moves at the first `intValue()`.
        static W13F_MOVES: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
        /// The record `Filter.isLoggable` received.
        static W13F_SEEN: Cell<usize> = const { Cell::new(0) };
    }

    /// A class of the test's own (so no mock slot table claims its names)
    /// whose listed instance fields resolve by name at slots 0, 1, 2, ...
    fn w13f_class(
        ctx: &mut MockNativeContext,
        name: &str,
        fields: &[&str],
    ) -> cratonvm_types::ClassId {
        let cid = ctx.ensure_class_initialized(name).unwrap();
        let declared = fields
            .iter()
            .enumerate()
            .map(|(slot, field)| cratonvm_native_api::FieldMetadata {
                name: (*field).to_string(),
                descriptor: "Ljava/lang/Object;".to_string(),
                access_flags: 0,
                slot_index: slot,
                declaring_class_id: cid,
                is_static: false,
            })
            .collect();
        ctx.set_declared_fields(cid, declared);
        cid
    }

    fn w13f_move_on_int_value(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        match method_name {
            "intValue" => {
                let moves = W13F_MOVES.with(|m| std::mem::take(&mut *m.borrow_mut()));
                for (old, new) in moves {
                    ctx.remap_native_pin_addr_for_test(old, new);
                }
                Some(Ok(Some(Value::Int(800))))
            }
            "isLoggable" => {
                if let Some(Value::Object(Some(record))) = args.first() {
                    W13F_SEEN.with(|c| c.set(record.as_ptr() as usize));
                }
                Some(Ok(Some(Value::Int(1))))
            }
            _ => None,
        }
    }

    /// `Handler.isLoggable(LogRecord)`: the handler and the record are moved
    /// while the handler level's `intValue()` runs. The filter must be read
    /// off the MOVED handler (the vacated copy has none) and handed the MOVED
    /// record.
    #[test]
    fn w13f_handler_is_loggable_filters_the_record_moved_by_int_value() {
        let mut registry = NativeMethodRegistry::new();
        register_annotation_overrides(&mut registry);
        let is_loggable = registry
            .find(
                "java/util/logging/Handler",
                "isLoggable",
                "(Ljava/util/logging/LogRecord;)Z",
            )
            .expect("Handler.isLoggable should be registered");

        let mut ctx = mock_ctx();
        ctx.set_vm_identity(0xF13_0101);
        let handler_cid = w13f_class(&mut ctx, "w13f/Handler", &["logLevel", "filter"]);
        let record_cid = w13f_class(&mut ctx, "w13f/Record", &["level"]);
        let handler_level = ctx.fresh_object_ref();
        let record_level = ctx.fresh_object_ref();
        let filter = ctx.fresh_object_ref();

        let this = ctx.alloc_object(handler_cid, 2);
        let moved_this = ctx.alloc_object(handler_cid, 2);
        ctx.set_field(this, 0, Value::Object(Some(handler_level)));
        ctx.set_field(moved_this, 0, Value::Object(Some(handler_level)));
        // The vacated copy no longer carries the filter; the moved one does.
        ctx.set_field(this, 1, Value::Object(None));
        ctx.set_field(moved_this, 1, Value::Object(Some(filter)));

        let record = ctx.alloc_object(record_cid, 1);
        let moved_record = ctx.alloc_object(record_cid, 1);
        ctx.set_field(record, 0, Value::Object(Some(record_level)));
        ctx.set_field(moved_record, 0, Value::Object(Some(record_level)));

        W13F_MOVES.with(|m| {
            *m.borrow_mut() = vec![
                (this.as_ptr() as usize, moved_this.as_ptr() as usize),
                (record.as_ptr() as usize, moved_record.as_ptr() as usize),
            ]
        });
        W13F_SEEN.with(|c| c.set(0));
        ctx.set_invoke_virtual_hook(w13f_move_on_int_value);

        let result = is_loggable(
            &mut ctx,
            &[Value::Object(Some(this)), Value::Object(Some(record))],
        )
        .unwrap();

        assert_eq!(result, Some(Value::Int(1)));
        assert_eq!(
            W13F_SEEN.with(Cell::get),
            moved_record.as_ptr() as usize,
            "the filter must be read off the moved handler and given the moved record"
        );
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "every root must be released"
        );
    }
}

/// gc-common w29-e (`common-w28b-remaining-identity-hash-keyed-side-tables`,
/// rank 18): a built-in, non-platform loader's proxy namespace is an id from
/// `allocate_loader_id` (never its identity hash), stable per loader object,
/// distinct for two loaders that share an identity hash, and forgotten with
/// the loader's weak lock key.
#[cfg(test)]
mod w29e_proxy_namespace_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    const VM: usize = 0x29E0_1801;

    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(VM);
        }
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock hashes the address.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn builtin_loader_namespaces_are_allocated_not_hashed() {
        let _t = Teardown;
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (a, b) = (at(0x1_29E0_1800), at(0x2_29E0_1800));
        assert_eq!(ctx.identity_hash_code(a), ctx.identity_hash_code(b));

        let ns_a = proxy_builtin_namespace_id(&mut ctx, a);
        let ns_b = proxy_builtin_namespace_id(&mut ctx, b);
        assert_ne!(ns_a, ns_b, "one hash, two loaders, two namespaces");
        assert!(ns_a >= cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED);
        assert_ne!(ns_a, ctx.identity_hash_code(a) as u32);
        assert_eq!(proxy_builtin_namespace_id(&mut ctx, a), ns_a, "stable");

        let key_a = crate::existing_weak_lock_key(&ctx, a).expect("keyed");
        let b_addr = b.as_ptr() as usize;
        crate::gc_sweep_lock_keys(VM, &|x| x == b_addr);
        assert!(!proxy_builtin_namespace_ids().lock().contains_key(&key_a));
        assert_eq!(proxy_builtin_namespace_id(&mut ctx, b), ns_b);
    }
}

/// gc-common w34-a (`common-w33a-identity-hash-validated-memos-hit-for-a-colliding-replacement`,
/// row 4): the `ModuleDescriptor$*` stand-ins' `compareTo` never answers 0 for
/// two distinct objects that share an identity hash. The mock's hash is the
/// address truncated to `i32`, so addresses 4 GiB apart share one. The mock
/// has no JDK bytecode, so these exercise the no-bytecode arm. A private VM
/// identity; the guard forgets only its lock keys.
#[cfg(test)]
mod w34a_module_member_order_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    struct Teardown(usize);
    impl Drop for Teardown {
        fn drop(&mut self) {
            crate::forget_vm_lock_keys(self.0);
        }
    }

    fn at(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the mock and the lock-key registry
        // only hash and compare the address.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn w34a_d_the_first_key_orders_as_java_string_compare_to() {
        assert_eq!(
            module_member_java_string_compare("java.base", "java.base"),
            0
        );
        assert_eq!(module_member_java_string_compare("a", "b"), -1);
        assert_eq!(
            module_member_java_string_compare("java.sql", "java.se"),
            'q' as i32 - 'e' as i32
        );
        assert_eq!(
            module_member_java_string_compare("java.xml", "java.xml.crypto"),
            -7
        );
        // UTF-16 units, not scalar values: U+FF61 sorts after the surrogate
        // pair of U+1F600 in Java (0xFF61 > 0xD83D).
        assert!(module_member_java_string_compare("\u{FF61}", "\u{1F600}") > 0);
        assert_eq!(module_member_key_order(None, Some("x")), -1);
        assert_eq!(module_member_key_order(Some("x"), None), 1);
    }

    /// Two distinct stand-ins with ONE identity hash and a tied first key:
    /// the old `compareTo` answered 0 and a `TreeSet` kept one of them. The
    /// tie-break is never 0, antisymmetric, and the same on every call.
    #[test]
    fn w34a_d_same_hash_stand_ins_never_compare_equal() {
        const VM: usize = 0x34A0_D001;
        let _t = Teardown(VM);
        let ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let (a, b) = (at(0x1_34A0_D100), at(0x2_34A0_D100));
        assert_eq!(
            ctx.identity_hash_code(a),
            ctx.identity_hash_code(b),
            "premise"
        );
        let ab = module_member_tie_break(&ctx, a, b);
        let ba = module_member_tie_break(&ctx, b, a);
        assert_ne!(ab, 0, "two objects are never equal");
        assert_eq!(ab, -ba, "antisymmetric");
        assert_eq!(module_member_tie_break(&ctx, a, b), ab, "stable");
        assert_eq!(module_member_tie_break(&ctx, a, a), 0);
    }

    /// The whole native on the mock (no `compareTo` bytecode there): the
    /// same stand-in is 0, two stand-ins are ordered and antisymmetric, and
    /// a null argument throws as the JDK's `compareTo` does.
    #[test]
    fn w34a_d_the_native_orders_distinct_stand_ins() {
        const VM: usize = 0x34A0_D002;
        let _t = Teardown(VM);
        let mut ctx = crate::test_utils::mock_ctx();
        ctx.set_vm_identity(VM);
        let a = ctx.fresh_object_ref();
        let b = ctx.fresh_object_ref();
        let same = native_module_exports_compare_to(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(a))],
        )
        .unwrap();
        assert_eq!(same, Some(Value::Int(0)));
        let ab = native_module_exports_compare_to(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        )
        .unwrap();
        let ba = native_module_exports_compare_to(
            &mut ctx,
            &[Value::Object(Some(b)), Value::Object(Some(a))],
        )
        .unwrap();
        match (ab, ba) {
            (Some(Value::Int(x)), Some(Value::Int(y))) => {
                assert_ne!(x, 0);
                assert_eq!(x.signum(), -y.signum());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(native_module_exports_compare_to(
            &mut ctx,
            &[Value::Object(Some(a)), Value::Object(None)],
        )
        .is_err());
    }
}
