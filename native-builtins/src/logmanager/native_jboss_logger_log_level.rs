#[allow(unused_imports)]
use super::*;

/// `Logger.log(Level, Supplier<String>)` — like `logRaw` above, the real
/// bytecode dereferences `this.loggerNode` (`isLoggableLevel` check)
/// BEFORE it ever builds the `ExtLogRecord` and calls `logRaw`, so
/// intercepting only `logRaw` isn't enough. Confirmed via an isolated
/// repro (`org.jboss.logmanager.Logger.getLogger(name).log(Level, Supplier)`)
/// that this overload NPEs the same way `log(LogRecord)` below does — see
/// that one's doc comment for the actual `testsuite/model`/`KcRunner` crash
/// this pair fixes. Bypass the loggerNode check entirely, mirroring
/// `native_jboss_logger_log_raw`'s formatting.
pub fn native_jboss_logger_log_level_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // gci i40/jb (`jboss_console_sink_off`, default ON): HotSpot's body is
    // `if (!loggerNode.isLoggableLevel(level)) return; logRaw(new
    // ExtLogRecord(level, supplier.get(), ..))` -- gate, evaluate the
    // supplier, publish to the handler chain, and drop what no handler took.
    // The JUL overload's native does exactly that without reading
    // `loggerNode` (its console fallback asks `jul_console_sink_applies`,
    // which is off under the JBoss manager), so this stand-in no longer
    // prints a stdout line of its own and no longer evaluates a suppressed
    // supplier. gci i41/rv: the switch as this VM latched it.
    if console_sink_switches(ctx.vm_identity()).jboss_off {
        return native_jul_logger_log_supplier(ctx, args);
    }
    let logger = match args.first() {
        Some(Value::Object(Some(o))) => match ctx.get_field(*o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "<root>".to_string()),
            _ => "<root>".to_string(),
        },
        _ => "<root>".to_string(),
    };
    let mut level_name = String::from("INFO");
    if let Some(Value::Object(Some(lvl))) = args.get(1) {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(*lvl, "name") {
            if let Some(n) = ctx.read_string(s) {
                level_name = n;
            }
        }
    }
    let tag = match level_name.as_str() {
        "SEVERE" => "ERROR",
        "WARNING" => "WARN",
        "INFO" | "CONFIG" => "INFO",
        // Suppress fine-grained trace noise (matches the logp interceptor).
        "FINE" | "FINER" | "FINEST" => return Ok(None),
        other => other,
    };
    let message = match args.get(2) {
        Some(Value::Object(Some(supplier))) => jul_resolve_msg(ctx, *supplier),
        _ => String::new(),
    };
    crate::emit_framework_log(ctx, &format!("{tag} [{logger}] {message}"));
    Ok(None)
}

/// Surface a throwable that was passed to a logging native. WildFly's
/// `WFLYSRV0055: Caught exception during boot` is logged with the real
/// boot exception as the trailing `Throwable` argument — but the previous
/// code only printed the literal text `(with throwable)` and discarded the
/// exception entirely, hiding the actual boot-failure cause.
///
/// This walks the throwable: class name + `detailMessage`, the captured
/// stack trace (looked up by the throwable's address, which is how the VM
/// keys retained traces), and the full `cause` chain. It mirrors HotSpot's `printStackTrace`
/// shape closely enough to diagnose boot failures from the log alone.
pub fn dump_throwable_to_stderr(ctx: &mut dyn NativeContext, throwable: ObjectRef, indent: &str) {
    let mut current = Some(throwable);
    let mut depth = 0usize;
    // gc-common w12-b: every `emit_framework_log` below may dispatch `println`
    // bytecode on an overridden `System.out`, so the current throwable is
    // pinned per turn and re-read after the emits, and the cycle guard keys
    // on the identity hash (a list of addresses goes stale across a move).
    let mut seen: Vec<i32> = Vec::new();
    while let Some(t) = current {
        // Guard against cyclic cause chains.
        let identity = ctx.identity_hash_code(t);
        if seen.contains(&identity) || depth > 16 {
            break;
        }
        seen.push(identity);
        let t_pin = ctx.pin_native_root(t);

        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(t))
            .unwrap_or_else(|| "java/lang/Throwable".to_string())
            .replace('/', ".");
        let detail = match crate::lang_misc::throwable_message_or_npe_extended(ctx, t) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        let prefix = if depth == 0 { "" } else { "Caused by: " };
        match &detail {
            Some(m) if !m.is_empty() => {
                crate::emit_framework_log(ctx, &format!("{indent}{prefix}{cls}: {m}"))
            }
            _ => crate::emit_framework_log(ctx, &format!("{indent}{prefix}{cls}")),
        }
        let t = ctx.read_native_pin(t_pin, t);

        // The captured trace is stored OUTERMOST-first (see
        // `stackwalker::capture_full_trace`); `printStackTrace` prints the
        // throw site first, so walk it from the end -- and a cap keeps the
        // innermost frames, not `main`'s end of the stack.
        if let Some(frames) = ctx.get_throwable_stack_trace(t) {
            for f in frames.iter().rev().take(48) {
                let where_ = match (&f.source_file, f.line_number) {
                    (Some(sf), n) if n >= 0 => format!("({sf}:{n})"),
                    (Some(sf), _) => format!("({sf})"),
                    (None, -2) => "(Native Method)".to_string(),
                    _ => "(Unknown Source)".to_string(),
                };
                // Round 14 wave 2 (lane trace): the `java.base/` prefix
                // `StackTraceElement.toString()` prints (`t` is re-read
                // through its pin below; the mirror read may allocate).
                let prefix = match f.class_id {
                    Some(cid) => crate::lang_misc::printed_frame_prefix(ctx, cid),
                    None => String::new(),
                };
                crate::emit_framework_log(
                    ctx,
                    &format!(
                        "{indent}    at {prefix}{}.{}{where_}",
                        f.class_name.replace('/', "."),
                        f.method_name
                    ),
                );
            }
        }

        // Walk to the cause (named `cause`; `this` is the JDK
        // "uninitialized" sentinel and means no cause).
        let t = ctx.read_native_pin(t_pin, t);
        ctx.unpin_native_roots(t_pin);
        let next = match crate::lang_misc::throwable_field_get(ctx, t, "cause") {
            Value::Object(Some(c)) if c != t => Some(c),
            _ => None,
        };
        current = next;
        depth += 1;
    }
}

/// WildFly visibility: intercept
/// `org/jboss/logging/JBossLogManagerLogger.doLog(Level,String fqcn,Object
/// message,Object[] params,Throwable)` and emit the formatted line to
/// stderr. WildFly's `Logger.info(...)` / `Logger.severe(...)` /
/// `ServerLogger.WFLYSRV*` chain all funnel into `doLog`/`doLogf` before
/// touching `org.jboss.logmanager.Logger.logRaw` (which our null-safe
/// stub previously swallowed). By printing here we surface the boot
/// progress without needing LogRecord field-offset guesses.
/// DEBUG/TRACE level filtering for [`native_jboss_logging_logger_do_log`] /
/// `..._do_logf`. **Default ON**; `CRATONVM_JBOSS_LOGGER_LEVEL_FILTER=0`
/// turns it off.
///
/// Corrects the record from the 2026-07-27 write-up, which claimed
/// "`org.jboss.logging.Logger.debugf` and friends do NOT check the level
/// themselves". They DO -- `Logger.debugf` is
/// `if (isEnabled(DEBUG)) doLogf(...)`, straight from the 3.6.2 bytecode.
///
/// The actual reason the Keycloak 26.6.1 boot emitted 2992 lines against
/// HotSpot's 10 is that these natives ARE the sink: they print every record
/// they are handed straight to stderr, bypassing the backend's HANDLER chain,
/// which is where a real jboss-logmanager setup filters. `isEnabled` answers
/// `true` on both VMs (Quarkus runs its root logger at ALL and filters at the
/// console handler), so HotSpot drops those records at the handler and we
/// printed them all. Verified with an isolated jboss-logging probe: under
/// `-Djava.util.logging.manager=org.jboss.logmanager.LogManager` HotSpot
/// prints NOTHING for `tracef`/`debugf`/`infof` (no handler configured) while
/// CratonVM printed all three.
///
/// So the filter models the missing handler threshold: a default console
/// handler at INFO, unless the application explicitly configured a lower level
/// on the logger (or an ancestor), which `jul_ancestor_explicit_level` sees
/// via the `setLevel` natives. INFO and above are never touched, so the
/// WildFly/JBoss boot visibility these natives exist for (`WFLYSRV*`,
/// `WFLYCTL*`, the throwable dump) is unaffected either way.
///
/// Measured on the real Keycloak 26.6.1 boot: 11331 -> 307 lines, with every
/// INFO/WARN/ERROR retained and the startup markedly faster (the formatting
/// and I/O of ~11k TRACE lines is a pure-overhead tax on the slowest phase of
/// the run).
pub fn jboss_logger_level_filter() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            None::<String>.as_deref(),
            Some("0")
        )
    })
}

/// Would the real backend have DROPPED this record for being below the
/// logger's configured level?
///
/// These `doLog`/`doLogf` natives stand in for the concrete backend
/// (`JBossLogManagerLogger`, `JDKLogger`, `Slf4jLogger`, ...), whose real
/// implementations all start with an `isEnabled(level)` check --
/// `org.jboss.logging.Logger.debugf(...)` and friends do NOT check the level
/// themselves, they delegate that to `doLog`/`doLogf`. Ours checked nothing,
/// so EVERY `tracef`/`debugf` call in the process was formatted and written to
/// stderr no matter how the application had configured logging.
///
/// On the real Keycloak 26.6.1 boot that meant 2992 log lines against
/// HotSpot's 10, dominated by Hibernate's per-attribute `org.hibernate.orm.boot`
/// TRACE binding messages -- a large, pure-overhead slowdown on a startup path
/// that is already the slowest part of the run, plus a log in which the actual
/// INFO-level progress was unfindable.
///
/// Only DEBUG and TRACE are gated, and only under
/// [`jboss_logger_level_filter`] (opt-in). INFO and above always emit, so the
/// WildFly/JBoss boot visibility these natives were written for (`WFLYSRV*`,
/// `WFLYCTL*`, and the throwable dump) is unaffected either way.
pub fn jboss_record_suppressed_by_level(vm: usize, level_name: &str, logger_name: &str) -> bool {
    if !jboss_logger_level_filter() || !matches!(level_name, "DEBUG" | "TRACE") {
        return false;
    }
    // jboss-logging `Level` -> the `java.util.logging.Level` value it
    // translates to (`JDKLogger.translate`): TRACE=FINEST, DEBUG=FINE.
    let record_value = if level_name == "TRACE" { 300 } else { 500 };
    // Same threshold resolution `native_jul_logger_is_loggable` uses: the
    // nearest ancestor logger with an EXPLICITLY configured level (recorded by
    // the `setLevel` natives -- which is how Quarkus/JBoss LogManager applies
    // `--log-level=debug`), else the JDK root default of INFO (800).
    //
    // NOT the receiver's own `isEnabled`: in real-JDK mode that bottoms out in
    // `java.util.logging.Logger.isLoggable`, whose `config.levelValue` CratonVM
    // never initialises -- it reads 0, so every level compares as enabled and
    // the check answers `true` for TRACE on a default-configured logger.
    let threshold = jul_ancestor_explicit_level(vm, logger_name).unwrap_or(800);
    record_value < threshold
}

/// Render `format` + `params` through the REAL `java.lang.String.format`.
///
/// That is exactly what jboss-logging's concrete backends do for the `logf`
/// family (`JBossLogManagerLogger` builds an `ExtLogRecord` with
/// `FormatStyle.PRINTF`, which `String.format`s it). Returns `None` when the
/// call is unavailable or throws -- e.g. a genuinely malformed format string,
/// where HotSpot would propagate an `IllegalFormatException` out of the log
/// call -- so the caller can fall back to the approximate in-Rust pass rather
/// than lose the line entirely.
pub fn jboss_printf_format(
    ctx: &mut dyn NativeContext,
    format: &str,
    params_pin: usize,
    params: ObjectRef,
) -> Option<String> {
    let fmt_obj = ctx.create_string(format);
    let fmt_pin = ctx.pin_native_root(fmt_obj);
    // `create_string` can collect: re-derive BOTH references from their pins.
    let params = ctx.read_native_pin(params_pin, params);
    let fmt_obj = ctx.read_native_pin(fmt_pin, fmt_obj);
    match ctx.invoke(
        "java/lang/String",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(fmt_obj)), Value::Object(Some(params))],
    ) {
        Ok(Some(Value::Object(Some(rendered)))) => ctx.read_string(rendered),
        _ => None,
    }
}

/// Apply `java.text.MessageFormat` parameters to a `doLog` message.
///
/// `Logger.logv(...)` and `Logger.log(Level, Object, Object[], Throwable)`
/// carry `{0}`-style parameters that the concrete backend applies via
/// `ExtLogRecord`'s `FormatStyle.MESSAGE_FORMAT`. Returns `None` when there is
/// nothing to substitute or the call fails, leaving the raw pattern in place.
pub fn jboss_message_format(
    ctx: &mut dyn NativeContext,
    pattern: &str,
    params_pin: usize,
    params: ObjectRef,
) -> Option<String> {
    if !pattern.contains('{') {
        return None;
    }
    let params_probe = ctx.read_native_pin(params_pin, params);
    if ctx.array_length(params_probe) == 0 {
        return None;
    }
    let pat_obj = ctx.create_string(pattern);
    let pat_pin = ctx.pin_native_root(pat_obj);
    let params = ctx.read_native_pin(params_pin, params);
    let pat_obj = ctx.read_native_pin(pat_pin, pat_obj);
    match ctx.invoke(
        "java/text/MessageFormat",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[Value::Object(Some(pat_obj)), Value::Object(Some(params))],
    ) {
        Ok(Some(Value::Object(Some(rendered)))) => ctx.read_string(rendered),
        _ => None,
    }
}

pub fn native_jboss_logging_logger_do_log(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Layout: this, level, fqcn, message, params, throwable
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let message_obj = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let params_obj = match args.get(4) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let throwable_obj = match args.get(5) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // `isEnabled` below re-enters Java and can move objects, so root every
    // argument first and re-read them through their pins afterwards.
    let mut pin_base = None;
    let mut pin = |ctx: &mut dyn NativeContext, o: Option<ObjectRef>| {
        o.map(|object| {
            let p = ctx.pin_native_root(object);
            pin_base.get_or_insert(p);
            (p, object)
        })
    };
    let this_pin = pin(ctx, this);
    let level_pin = pin(ctx, level_obj);
    let message_pin = pin(ctx, message_obj);
    let params_pin = pin(ctx, params_obj);
    let throwable_pin = pin(ctx, throwable_obj);

    let level_name = level_pin
        .map(|(p, o)| ctx.read_native_pin(p, o))
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "INFO".to_string());
    let logger_name = this_pin
        .map(|(p, o)| ctx.read_native_pin(p, o))
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    if jboss_record_suppressed_by_level(ctx.vm_identity(), &level_name, &logger_name) {
        if let Some(p) = pin_base {
            ctx.unpin_native_roots(p);
        }
        return Ok(None);
    }
    let message = message_pin
        .map(|(p, o)| ctx.read_native_pin(p, o))
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    // args[4] carries the MessageFormat parameters and was ignored outright,
    // so every `logv`-family line printed its raw pattern -- e.g. Agroal's
    // `{0}: Validation test on connection {1}` on the Keycloak boot.
    let message = match params_pin {
        Some((p, o)) => jboss_message_format(ctx, &message, p, o).unwrap_or(message),
        None => message,
    };
    crate::emit_framework_log(ctx, &format!("{level_name} [{logger_name}] {message}"));
    if let Some((p, original)) = throwable_pin {
        let t = ctx.read_native_pin(p, original);
        dump_throwable_to_stderr(ctx, t, "    ");
    }
    if let Some(p) = pin_base {
        ctx.unpin_native_roots(p);
    }
    Ok(None)
}

/// Same as `do_log` but for the printf-style `doLogf(Level,String fqcn,
/// String format,Object[] params,Throwable)`. Substitutes `%s`/`%%`/`%n`
/// from the params array so callers like `WFLYCTL0013` show their full
/// failure description instead of literal `%s`.
pub fn native_jboss_logging_logger_do_logf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let format_obj = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let params_obj = match args.get(4) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let throwable_obj = match args.get(5) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };

    // `Object::toString` below re-enters Java and can trigger a moving GC.
    // Root every object argument before the first field/string read so the
    // params array and trailing throwable remain refreshable throughout the
    // whole formatting pass.
    let mut pin_base = None;
    let this_pin = this.map(|object| {
        let pin = ctx.pin_native_root(object);
        pin_base.get_or_insert(pin);
        (pin, object)
    });
    let level_pin = level_obj.map(|object| {
        let pin = ctx.pin_native_root(object);
        pin_base.get_or_insert(pin);
        (pin, object)
    });
    let format_pin = format_obj.map(|object| {
        let pin = ctx.pin_native_root(object);
        pin_base.get_or_insert(pin);
        (pin, object)
    });
    let params_pin = params_obj.map(|object| {
        let pin = ctx.pin_native_root(object);
        pin_base.get_or_insert(pin);
        (pin, object)
    });
    let throwable_pin = throwable_obj.map(|object| {
        let pin = ctx.pin_native_root(object);
        pin_base.get_or_insert(pin);
        (pin, object)
    });

    let logger_name = this_pin
        .and_then(|(pin, object)| {
            let object = ctx.read_native_pin(pin, object);
            match ctx.get_field_by_name(object, "name") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            }
        })
        .unwrap_or_default();
    let level_name = level_pin
        .and_then(|(pin, object)| {
            let object = ctx.read_native_pin(pin, object);
            match ctx.get_field_by_name(object, "name") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            }
        })
        .unwrap_or_else(|| "INFO".to_string());
    // Drop below-threshold records BEFORE the (expensive) parameter
    // `toString` + format pass -- see `jboss_record_suppressed_by_level`.
    if jboss_record_suppressed_by_level(ctx.vm_identity(), &level_name, &logger_name) {
        if let Some(pin) = pin_base {
            ctx.unpin_native_roots(pin);
        }
        return Ok(None);
    }

    let format = format_pin
        .and_then(|(pin, object)| {
            let object = ctx.read_native_pin(pin, object);
            ctx.read_string(object)
        })
        .unwrap_or_default();
    // Render through the REAL `String.format` -- what the concrete backends
    // do. The in-Rust pass below only ever understood `%s`/`%%`/`%n`, so any
    // other conversion (`%d`, `%b`, `%x`, `%.2f`, `%c`, ...) BOTH survived
    // into the output verbatim AND desynchronised the parameter cursor,
    // shifting every later `%s` onto the wrong argument
    // (`"d=%d s=%s", 42, "str"` printed `d=%d s=42`). It is kept as a
    // fallback for a failed/absent `String.format` only.
    let message = if let Some((params_pin, params)) = params_pin {
        if let Some(rendered) = jboss_printf_format(ctx, &format, params_pin, params) {
            rendered
        } else {
            let params = ctx.read_native_pin(params_pin, params);
            let n = ctx.array_length(params);
            // Snapshot and root every object element before invoking even the
            // first `toString`. Keeping raw Values here made a later element stale
            // whenever an earlier element's callback collected.
            let elems: Vec<(Value, Option<usize>)> = (0..n)
                .map(|i| {
                    let value = ctx.get_array_element(params, i);
                    let pin = match value {
                        Value::Object(Some(object)) => Some(ctx.pin_native_root(object)),
                        _ => None,
                    };
                    (value, pin)
                })
                .collect();
            let mut param_strs: Vec<String> = Vec::with_capacity(n);
            for (elem, elem_pin) in elems {
                let elem = match (elem, elem_pin) {
                    (Value::Object(Some(original)), Some(pin)) => {
                        Value::Object(Some(ctx.read_native_pin(pin, original)))
                    }
                    (value, _) => value,
                };
                let s = match elem {
                    Value::Object(Some(o)) => {
                        if let Some(s) = ctx.read_string(o) {
                            s
                        } else {
                            let cn = ctx
                                .class_name_of_id(ctx.class_id_of_object(o))
                                .unwrap_or_else(|| "?".to_string());
                            let ts_result =
                                ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[]);
                            match ts_result {
                                Ok(Some(Value::Object(Some(sr)))) => {
                                    ctx.read_string(sr).unwrap_or_else(|| format!("{{{cn}}}"))
                                }
                                _ => format!("{{{cn}}}"),
                            }
                        }
                    }
                    Value::Object(None) => "null".to_string(),
                    v => format!("{v:?}"),
                };
                param_strs.push(s);
            }
            let mut result = String::with_capacity(format.len() + 64);
            let mut param_idx = 0usize;
            let mut chars = format.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '%' {
                    result.push(c);
                    continue;
                }
                // Consume a whole `%[argument_index$][flags][width][.precision]c`
                // spec. Anything that is not `%%` or `%n` consumes one parameter,
                // even when we cannot reproduce its exact rendering -- keeping the
                // cursor aligned matters more than the individual conversion.
                let mut spec = String::new();
                let mut conversion = None;
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == '%' {
                        conversion = Some(next);
                        break;
                    }
                    spec.push(next);
                }
                match conversion {
                    Some('%') => result.push('%'),
                    Some('n') => result.push('\n'),
                    Some(_) => {
                        result
                            .push_str(param_strs.get(param_idx).map(|s| s.as_str()).unwrap_or("?"));
                        param_idx += 1;
                    }
                    None => {
                        result.push('%');
                        result.push_str(&spec);
                    }
                }
            }
            result
        }
    } else {
        format
    };
    crate::emit_framework_log(ctx, &format!("{level_name} [{logger_name}] {message}"));
    if let Some((pin, original)) = throwable_pin {
        let t = ctx.read_native_pin(pin, original);
        dump_throwable_to_stderr(ctx, t, "    ");
    }
    if let Some(pin) = pin_base {
        ctx.unpin_native_roots(pin);
    }
    Ok(None)
}

/// Round 92: direct `org/jboss/logging/Logger.info/warn/error/debug/...`
/// overload intercepts. WildFly's `ServerLogger.WFLY*` calls funnel
/// through these methods on the abstract `Logger` base class
/// (`info(Object)`, `infof(String, Object...)`, `infov(String,
/// Object...)`, etc.) which in pristine code dispatch via virtual call
/// to `doLog`/`doLogf` on a concrete subtype. The Round 90 doLog/doLogf
/// natives only fire on a handful of subclasses; in practice WildFly's
/// per-module logger isn't always one of them. Registering the natives
/// on the abstract `Logger` base ensures every WFLY* boot message
/// surfaces regardless of which concrete subclass implements it.
///
/// All overloads share a single helper: read `this.name`, format the
/// message (best-effort — we don't do printf substitution), emit to
/// stderr at the named level.
pub fn jboss_logger_emit(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    // The "message" parameter is at args[1] for instance methods. It may
    // be a String, an Object whose toString() we can't easily call, or a
    // format-string followed by varargs. Print whatever String we find.
    let mut message = String::new();
    let mut throwable: Option<ObjectRef> = None;
    for arg in args.iter().skip(1) {
        if let Value::Object(Some(o)) = arg {
            if message.is_empty() {
                if let Some(s) = ctx.read_string(*o) {
                    message.push_str(&s);
                    continue;
                }
            }
            // A non-String object argument that subclasses Throwable is
            // the exception passed to `error(Object, Throwable)` etc. —
            // surface it instead of silently dropping it.
            if throwable.is_none() {
                let cn = ctx.class_name_of_id(ctx.class_id_of_object(*o));
                let is_throwable = cn
                    .as_deref()
                    .map(|n| {
                        n.ends_with("Exception") || n.ends_with("Error") || n.ends_with("Throwable")
                    })
                    .unwrap_or(false)
                    || ctx
                        .get_field_by_name(*o, "detailMessage")
                        .as_object()
                        .is_some();
                if is_throwable {
                    throwable = Some(*o);
                }
            }
        }
    }
    // `eprintln!` writes to the process's raw OS stderr, bypassing the
    // Java-level `System.out`/`System.err` `PrintStream` that JUnit5's
    // `OutputCaptureExtension` substitutes — same bug class as
    // `log_simple`'s fix below; route through the live stream instead.
    // See propertiesmigration-logfactory-oom-residual-FIXED.md.
    // gc-common w12-b: the emit may run `println` bytecode, so the throwable
    // (an `args` copy) is pinned across it and re-read.
    let throwable_pin = throwable.map(|t| ctx.pin_native_root(t));
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
    if let (Some(t), Some(pin)) = (throwable, throwable_pin) {
        let t = ctx.read_native_pin(pin, t);
        ctx.unpin_native_roots(pin);
        dump_throwable_to_stderr(ctx, t, "    ");
    }
}

/// `Logger.{info,warn,error}(String loggerFqcn, Object message, Throwable t)` —
/// the forms `DelegatingBasicLogger` delegates to. args[1] is the WRAPPER-CLASS
/// FQCN (e.g. "org.jboss.logging.DelegatingBasicLogger"), NOT the message; the
/// generic `jboss_logger_emit` takes the first String arg as the message, so it
/// printed the FQCN and dropped the real message and throwable — hiding e.g.
/// the WildFly subsystem-test boot error behind
/// `ERROR [org.jboss.as.controller] org.jboss.logging.DelegatingBasicLogger`.
/// Read the message at args[2] (invoking toString() for non-String objects)
/// and the throwable at args[3].
pub fn jboss_logger_emit_fqcn(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let message_obj = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let throwable_obj = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // A non-String message invokes Java `toString`; root both it and the
    // trailing throwable before that callback so the latter cannot remain as
    // a stale entry-argument copy.
    let message_pin = message_obj.map(|object| (ctx.pin_native_root(object), object));
    let throwable_pin = throwable_obj.map(|object| (ctx.pin_native_root(object), object));
    let pin_base = message_pin
        .map(|(pin, _)| pin)
        .or_else(|| throwable_pin.map(|(pin, _)| pin));
    let message = match message_pin {
        Some((pin, original)) => {
            let o = ctx.read_native_pin(pin, original);
            if let Some(s) = ctx.read_string(o) {
                s
            } else {
                let cn = ctx
                    .class_name_of_id(ctx.class_id_of_object(o))
                    .unwrap_or_else(|| "?".to_string());
                match ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[]) {
                    Ok(Some(Value::Object(Some(sr)))) => {
                        ctx.read_string(sr).unwrap_or_else(|| format!("{{{cn}}}"))
                    }
                    _ => format!("{{{cn}}}"),
                }
            }
        }
        None if matches!(args.get(2), Some(Value::Object(None))) => "null".to_string(),
        None => String::new(),
    };
    // See `jboss_logger_emit`'s comment above — same raw-eprintln bypass.
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
    if let Some((pin, original)) = throwable_pin {
        let throwable = ctx.read_native_pin(pin, original);
        dump_throwable_to_stderr(ctx, throwable, "    ");
    }
    if let Some(pin) = pin_base {
        ctx.unpin_native_roots(pin);
    }
}

pub fn native_jboss_logger_info_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "INFO");
    Ok(None)
}
pub fn native_jboss_logger_warn_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "WARN");
    Ok(None)
}
pub fn native_jboss_logger_error_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "ERROR");
    Ok(None)
}

pub fn native_jboss_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "INFO");
    Ok(None)
}
pub fn native_jboss_logger_warn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "WARN");
    Ok(None)
}
pub fn native_jboss_logger_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "ERROR");
    Ok(None)
}
pub fn native_jboss_logger_fatal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "FATAL");
    Ok(None)
}
pub fn native_jboss_logger_debug(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Suppress debug — too noisy.
    Ok(None)
}
pub fn native_jboss_logger_trace(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Generic `java/util/logging/Logger.log(Level, String)` intercept so
/// any JUL-direct caller (Hibernate, Mojarra, etc.) also surfaces.
pub fn native_jul_logger_log_level_msg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Null `Level` throws; a null MESSAGE does not. Measured both ways:
    // `log(null, "msg")` NPEs, `log(SEVERE, (String) null)` returns. The level
    // gate below would raise the same NPE; the check is kept first so the
    // order is explicit.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    // gci i25/jl: HotSpot's body opens with `if (!isLoggable(level)) return;`.
    // This overload never gated, so a FINE record on an INFO logger reached
    // every handler AND the console sink. Shares the sibling overloads' walk
    // (reads `args[0..2]` only; not a GC point, so `args` stay current).
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let message_obj = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let level_name = level_obj
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "INFO".to_string());
    let message = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    // gci i25/jl: the logger's own chain first -- its filter, then every
    // handler of every level (`publish_to_jul_handlers_outcome`) -- then the
    // name-keyed side rows other objects of this name filed, unless the
    // filter rejected the record; the console sink only when nothing took
    // it. This used to print the console line UNCONDITIONALLY between the two
    // publishes, so a logger with a handler printed every record twice (once
    // by the handler, once here) and a rejecting filter printed it anyway;
    // the `info`/`logp` family has published-then-fallen-back since the
    // `jul_convenience_with_console_fallback` fix.
    //
    // gc-common w12-b: the publishes run filter / handler bytecode, so the
    // three references are pinned and re-read after the first.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let message_pin = message_obj.map(|o| (ctx.pin_native_root(o), o));
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p))
        .or_else(|| message_pin.map(|(p, _)| p));
    // A null MESSAGE still publishes a record (getMessage() == null): HotSpot
    // builds `new LogRecord(level, null)`. This path published nothing for it
    // (review i25/rv item 14; predates i24/lk).
    let outcome = match (this, level_obj) {
        (Some(logger), Some(level)) => publish_to_jul_handlers_outcome(
            ctx,
            logger,
            level,
            message_obj,
            None,
            None,
            None,
            None,
        ),
        _ => Ok(JulPublish::NoHandler),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            if let Some(base) = base_pin {
                ctx.unpin_native_roots(base);            }
            return Err(e);
        }
    };
    if outcome != JulPublish::Filtered {
        let this = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let level_obj = level_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let message_obj = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        publish_jul_handlers(ctx, this, level_obj, message_obj);
    }
    // gci i31/hp: the sink only before the LogManager is initialized
    // (`jul_console_sink_applies`).
    if outcome == JulPublish::NoHandler && jul_console_sink_applies(&*ctx) {
        // Plain Rust strings, read before the publishes: no re-read needed.
        crate::emit_framework_log(ctx, &format!("{level_name} [{logger_name}] {message}"));
    }
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    Ok(None)
}

/// `java/util/logging/Logger.log(Level, String, Object)` — single-param
/// sibling of `log(Level, String, Object[])`. The JDK wraps `param1` in a
/// one-element `Object[]` before building the LogRecord, and
/// `LogRecord.getParameters()` must report it exactly that way, so do the same
/// here rather than pre-formatting the text.
pub fn native_jul_logger_log_param(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null `Level` throws. A null single PARAMETER does not — measured:
    // `log(SEVERE, "msg", (Object) null)` returns on HotSpot, and the JDK
    // still wraps that null in a one-element array.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    // gci i25/jl: `if (!isLoggable(level)) return;` -- before the array
    // allocation, as HotSpot gates before building the record.
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let message_obj = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let param_obj = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // GC SAFETY: the `Object[]` allocation below can move every argument.
    // Pin them all first, then re-derive each one from its pin.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let message_pin = message_obj.map(|o| (ctx.pin_native_root(o), o));
    let param_pin = param_obj.map(|o| (ctx.pin_native_root(o), o));
    let params = ctx.new_array(ArrayElementType::Reference, 1);
    let params_pin = ctx.pin_native_root(params);
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p))
        .or_else(|| message_pin.map(|(p, _)| p))
        .or_else(|| param_pin.map(|(p, _)| p))
        .unwrap_or(params_pin);
    let param_obj = param_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let params = ctx.read_native_pin(params_pin, params);
    ctx.set_array_element(params, 0, Value::Object(param_obj));
    let this = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let level_obj = level_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let message_obj = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let params = ctx.read_native_pin(params_pin, params);
    let result = jul_log_parameterized(ctx, this, level_obj, message_obj, Some(params), None);
    ctx.unpin_native_roots(base_pin);
    result
}

/// `java/util/logging/Logger.log(Level, String, Object[])` — the
/// `MessageFormat`-style parameterized overload. The real JDK builds a
/// `LogRecord`, resolves `{0}`/`{1}`/... placeholders against the
/// `Object[] params` via `java.text.MessageFormat`, and routes it through
/// the (unwired) handler chain. Without a native here the call fell
/// through to the real bytecode's private `Logger.getEffectiveLoggerBundle()`
/// (via `doLog`), which reads the instance field `loggerBundle` — never
/// populated on our synthetic 3-field `Logger` (see `LOGGER_NUM_FIELDS`
/// doc above) — and NPEs (`Cannot invoke
/// "Logger$LoggerBundle.isSystemBundle()" because "lb" is null"`).
///
/// Jython 2.7.4's `org.python.core.PrePy.maybeWrite` is exactly this
/// caller: `logger.log(level, "{0}: {1}", new Object[]{a, b})` for every
/// warning/error Jython prints during `PySystemState` bootstrap
/// (`initConsole` → `writeConsoleWarning`), so any embedder that boots a
/// `PythonInterpreter`/JSR-223 `jython` engine hit this NPE before a
/// single line of Python ever ran. Build the record with the RAW pattern and
/// the parameter array attached, exactly as HotSpot does — the `{n}`
/// substitution belongs to the Formatter, and only the console-sink fallback
/// (for a logger with no handler chain at all) performs it here.
pub fn native_jul_logger_log_params(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null `Level` throws. A null PARAMETER ARRAY does not — measured:
    // `log(SEVERE, "msg", (Object[]) null)` returns on HotSpot.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    // gci i25/jl: `if (!isLoggable(level)) return;` (was never asked here).
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let message_obj = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let params_arr = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    jul_log_parameterized(ctx, this, level_obj, message_obj, params_arr, None)}

/// Shared body of the parameterized / throwable-carrying `Logger.log`
/// overloads.
///
/// Builds a real `LogRecord` around the RAW message pattern plus its
/// `parameters` array and/or `thrown`, and publishes it through the logger's
/// handler chain (its own handlers, else the nearest ancestor's, honouring
/// `useParentHandlers`). Only when NO handler took the record does it fall
/// back to the console sink these natives used to write unconditionally — so
/// output that exists today is preserved for handler-less loggers, while a
/// logger with a handler now observes what HotSpot delivers: the untouched
/// pattern in `getMessage()`, the arguments in `getParameters()`, and the
/// throwable in `getThrown()`.
pub fn jul_log_parameterized(
    ctx: &mut dyn NativeContext,
    this: Option<ObjectRef>,
    level_obj: Option<ObjectRef>,
    message_obj: Option<ObjectRef>,
    params: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
) -> MethodCallResult {
    // GC SAFETY: everything below (record construction, handler dispatch,
    // `toString()` on the parameters) is GC-capable. Pin every reference up
    // front and re-derive each one after every such boundary.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let message_pin = message_obj.map(|o| (ctx.pin_native_root(o), o));
    let params_pin = params.map(|o| (ctx.pin_native_root(o), o));
    let thrown_pin = thrown.map(|o| (ctx.pin_native_root(o), o));
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p))
        .or_else(|| message_pin.map(|(p, _)| p))
        .or_else(|| params_pin.map(|(p, _)| p))
        .or_else(|| thrown_pin.map(|(p, _)| p));
    // gci i25/jl: the logger's filter is asked inside the publish, with the
    // record the handlers get (`publish_to_jul_handlers_outcome`); a rejected
    // record counts as disposed of, so the console fallback below stays quiet.
    // The level gate is each caller's (HotSpot's `isLoggable(level)`).
    let this = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let level_obj = level_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let message_obj = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let params = params_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let thrown = thrown_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    // gci i32/jh (`i31-hp-report.md` review item 5): HotSpot's
    // `Logger.log(LogRecord)` lets the `LogRecord` constructor's, the filter's
    // and a handler's exception propagate out of the logging call (no catch
    // around `handler.publish`). This swallowed every one of them and fell
    // back to the console sink -- and since i31/hp gated that sink, the
    // record vanished with no trace. The other JUL doors (`log(Level,
    // String)`, `info`, `logp`, `log(LogRecord)`) already propagate. A null
    // message publishes too (`new LogRecord(level, null)`; review i25/rv
    // item 14), as on `log(Level, String)`; it fell to the sink before.
    let outcome = match (this, level_obj) {
        (Some(logger), Some(level)) => publish_to_jul_handlers_outcome(
            ctx,
            logger,
            level,
            message_obj,
            None,
            None,
            params,
            thrown,
        ),
        _ => Ok(JulPublish::NoHandler),
    };
    let delivered = match outcome {
        Ok(outcome) => outcome.disposed(),
        Err(e) => {
            if let Some(base) = base_pin {
                ctx.unpin_native_roots(base);            }
            return Err(e);
        }
    };
    if jul_dbg_enabled() {
        eprintln!(
            "[JUL-DBG] jul_log_parameterized: params={} thrown={} delivered_to_handler={delivered}",
            params.is_some(),
            thrown.is_some()
        );
    }
    // gci i31/hp: the sink only before the LogManager is initialized
    // (`jul_console_sink_applies`).
    if !delivered && jul_console_sink_applies(&*ctx) {
        let this = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let level_obj = level_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let message_obj = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let logger_name = this
            .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            })
            .unwrap_or_default();
        let tag = jul_level_tag(ctx, level_obj);
        let mut text = message_obj
            .and_then(|o| ctx.read_string(o))
            .unwrap_or_default();
        // The console sink has no Formatter, so substitute the placeholders
        // here (params are logged values, not user format strings — a plain
        // positional replace is sufficient; MessageFormat's quoting /
        // choice-format machinery is not needed).
        if let Some((pin, obj)) = params_pin {
            let arr = ctx.read_native_pin(pin, obj);
            let n = ctx.array_length(arr);
            for i in 0..n {
                let arr = ctx.read_native_pin(pin, obj);
                let rendered = match ctx.get_array_element(arr, i) {
                    Value::Object(Some(o)) => jul_resolve_msg(ctx, o),
                    _ => String::new(),
                };
                text = text.replace(&format!("{{{i}}}"), &rendered);
            }
        }
        match thrown_pin.map(|(p, o)| ctx.read_native_pin(p, o)) {
            Some(t) => {
                let rendered = jul_render_throwable(ctx, t);
                if text.is_empty() {
                    crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {rendered}"));
                } else {
                    crate::emit_framework_log(
                        ctx,
                        &format!("{tag} [{logger_name}] {text}\n{rendered}"),
                    );
                }
            }
            None => crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {text}")),
        }
    }
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    Ok(None)
}

pub fn native_jul_log_record_get_message(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(record))) = args.first() else {
        return Ok(Some(Value::Object(None)));
    };
    let record_id = ctx.get_field(*record, 1).as_long().unwrap_or_default();
    // GC SAFETY (2026-07-21, DoHead sporadic-residuals follow-up): switched
    // from the interned `ctx.create_string(&text)` to the uninterned,
    // headroom-checked `create_string_uninterned_gc_safe` -- the correct,
    // established choice for a native caller producing a dynamic string
    // (see that function's own doc comment). This alone does not fully
    // close a residual, much rarer heap-corruption symptom found while
    // verifying it; see docs/known-issues (or the linked follow-up task) for
    // the open investigation. Also semantically more correct regardless:
    // `LogRecord.getMessage()` is a dynamically produced string, not a
    // literal, so it should not participate in the intern pool.
    // gc-common w13-b: this VM's table, and the text is copied out before the
    // string is allocated, so the table lock is not held across the allocation.
    let text = log_record_messages(ctx.vm_identity())
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&record_id)
        .cloned();
    let message = text.map(|text| ctx.create_string_uninterned_gc_safe(&text));
    if message.is_some() {
        return Ok(Some(Value::Object(message)));
    }
    // Side-table miss. Two other shapes reach here:
    //   * a real-layout LogRecord, whose `<init>` stored the message under the
    //     field NAME rather than in the table; and
    //   * a SYNTHETIC one, which has no field names at all, so that same
    //     `<init>` no-opped and the message landed in the slot fallback added
    //     alongside it (level = 0, message = 1).
    // Reading only the table made `new LogRecord(level, msg).getMessage()`
    // return null for both.
    if let Value::Object(Some(s)) = ctx.get_field_by_name(*record, "message") {
        return Ok(Some(Value::Object(Some(s))));
    }
    if ctx.object_num_fields(*record) > 1 {
        if let Value::Object(Some(s)) = ctx.get_field(*record, 1) {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(None)))
}

/// Store an explicit handler without relying on the private JDK Logger layout.
pub fn native_jul_logger_add_handler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(handler)))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    // GC SAFETY (2026-07-21, DoHead sporadic-residuals follow-up): `logger`
    // and `handler` are used below across several `invoke_virtual`/
    // `alloc_concurrent_synthetic` calls (arbitrary Java bytecode and heap
    // allocation, both GC-capable) without ever being pinned -- same hazard
    // class as the three JUL sibling functions already fixed. Pin both up
    // front and re-derive them after every GC-capable call.
    let logger_pin = ctx.pin_native_root(*logger);
    let handler_pin = ctx.pin_native_root(*handler);
    let name = read_jul_logger_name(ctx, *logger);
    let is_root = name.is_empty();
    let mut all = logger_handlers(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // gci i24/lk: the row names its owner (this Logger), so it goes with it.
    let row: NamedHandlerRow = (logger.as_ptr() as u64, handler.as_ptr() as u64);
    let handlers = all.entry(name).or_default();
    if !handlers.contains(&row) {
        handlers.push(row);
    }
    drop(all);
    if is_root && tomcat_classloader_log_manager_requested(ctx) {
        // `Thread.currentThread` / `getContextClassLoader` are bytecode: the
        // handler's address is re-read after them (gc-common w12-b; the raw
        // argument copy used to be the one recorded). The loader's key is
        // taken straight away, before any other GC point (gc-common w14-d).
        let loader_key = tomcat_loader_key(tomcat_context_loader(ctx));
        let handler = ctx.read_native_pin(handler_pin, *handler);
        let mut roots = tomcat_juli_root_handler_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entries = roots.entry(loader_key).or_default();
        if !entries.iter().any(|&addr| addr == handler.as_ptr() as u64) {
            entries.push(handler.as_ptr() as u64);
        }
    }
    let logger = ctx.read_native_pin(logger_pin, *logger);
    let handler = ctx.read_native_pin(handler_pin, *handler);
    // The compatibility map above is name-keyed for legacy synthetic JUL
    // callers. JULI must additionally retain handlers by logger identity:
    // two webapps may both configure the root logger named "" but with
    // independent FileHandlers. The delivery bridge consumes this side table.
    // gc-common w14-d: every exit below releases the frame once. The list
    // allocation, its `native_al_init` and the `size` / `get` calls used to
    // return through `?` with `logger_pin` (and the frame above it) still
    // pinned; only the final add's error path released it.
    let result = (|| -> Result<(), MethodCallFailed> {
        let side_list = match crate::jul_logger_handlers_get(ctx, logger) {
            Some(list) => list,
            None => {
                let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
                let list_pin = ctx.pin_native_root(list);
                cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))])?;
                let list = ctx.read_native_pin(list_pin, list);
                let logger = ctx.read_native_pin(logger_pin, logger);
                crate::jul_logger_handlers_set(ctx, logger, list);
                list
            }
        };
        let side_list_pin = ctx.pin_native_root(side_list);
        let size = match ctx.invoke_virtual(side_list, "size", "()I", &[])? {
            Some(Value::Int(size)) if size > 0 => size as usize,
            _ => 0,
        };
        let mut already_present = false;
        for index in 0..size {
            let side_list = ctx.read_native_pin(side_list_pin, side_list);
            let handler = ctx.read_native_pin(handler_pin, handler);
            if ctx.invoke_virtual(
                side_list,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(index as i32)],
            )? == Some(Value::Object(Some(handler)))
            {
                already_present = true;
                break;
            }
        }
        if !already_present {
            let side_list = ctx.read_native_pin(side_list_pin, side_list);
            let handler = ctx.read_native_pin(handler_pin, handler);
            // gc-common w11-c: the reclaiming door. Nothing is used after the
            // add, and this function already runs bytecode (`size`/`get`
            // above), so every Rust caller treats it as a GC point already.
            cratonvm_native_collections::native_al_add_entry_pub(
                ctx,
                &[Value::Object(Some(side_list)), Value::Object(Some(handler))],
            )?;
        }
        Ok(())
    })();
    ctx.unpin_native_roots(logger_pin);
    result.map(|()| None)
}

/// Drop `owner`'s rows for `handler` from one name's name-keyed rows.
///
/// gci i25/jl (review i25/rv item 15): `removeHandler` used to drop the
/// handler's row for EVERY object of the name, so `b.removeHandler(h)` also
/// took `h` off a different same-named Logger `a` that had added it, where
/// HotSpot's `removeHandler` touches only the receiver's own `config.handlers`.
pub fn remove_named_handler_row(rows: &mut Vec<NamedHandlerRow>, owner: u64, handler: u64) {
    rows.retain(|&(row_owner, row_handler)| !(row_owner == owner && row_handler == handler));
}

pub fn native_jul_logger_remove_handler(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(handler)))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    let (logger, handler) = (*logger, *handler);
    let name = read_jul_logger_name(ctx, logger);
    {
        let mut all = logger_handlers(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(handlers) = all.get_mut(&name) {
            remove_named_handler_row(
                handlers,
                logger.as_ptr() as u64,
                handler.as_ptr() as u64,
            );
        }
    }
    // gc-common w14-d: `addHandler` on the root under JULI also files the
    // handler in the per-context-loader root registry, and nothing took it out
    // again: the next JULI child logger created for that loader while the root
    // had no handler list copied the removed handler back in. Remove it from
    // the same loader's row. `tomcat_context_loader` runs bytecode, so both
    // objects are rooted across it and re-read afterwards.
    let (logger, handler) = if name.is_empty() && tomcat_classloader_log_manager_requested(ctx) {
        let logger_pin = ctx.pin_native_root(logger);
        let handler_pin = ctx.pin_native_root(handler);
        let loader_key = tomcat_loader_key(tomcat_context_loader(ctx));
        let logger = ctx.read_native_pin(logger_pin, logger);
        let handler = ctx.read_native_pin(handler_pin, handler);
        forget_tomcat_juli_root_handler(vm, loader_key, handler.as_ptr() as u64);
        ctx.unpin_native_roots(logger_pin);
        (logger, handler)
    } else {
        (logger, handler)
    };
    // `addHandler` writes TWO tables — the name-keyed compatibility map above
    // AND the identity-keyed `ArrayList` side list that
    // `resolve_jul_handler_chain` (and therefore every publish) actually reads.
    // This function only ever cleared the first, so a removed handler kept
    // receiving every subsequent record: `addHandler(h); removeHandler(h)`
    // left `h` live for the rest of the process. That is what
    // `AbstractQuarkusExtensionTest.afterAll` relies on to stop an
    // `InMemoryLogHandler` from one test class collecting the next one's
    // output. Drop it from the side list too.
    remove_from_jul_handler_side_list(ctx, logger, handler)?;
    Ok(None)
}

/// Remove `handler` from `logger`'s identity-keyed handler `ArrayList`, if it
/// has one. No-op when the logger has no side list or the handler is absent.
pub fn remove_from_jul_handler_side_list(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    handler: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let Some(side_list) = crate::jul_logger_handlers_get(ctx, logger) else {
        return Ok(());
    };
    let side_list_pin = ctx.pin_native_root(side_list);
    let handler_pin = ctx.pin_native_root(handler);
    let result = (|| -> Result<(), MethodCallFailed> {
        let size = match ctx.invoke_virtual(side_list, "size", "()I", &[])? {
            Some(Value::Int(size)) if size > 0 => size as usize,
            _ => 0,
        };
        // Walk backwards so a removal cannot shift an index we have not
        // visited yet.
        for index in (0..size).rev() {
            let side_list = ctx.read_native_pin(side_list_pin, side_list);
            let handler = ctx.read_native_pin(handler_pin, handler);
            let found = ctx.invoke_virtual(
                side_list,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(index as i32)],
            )? == Some(Value::Object(Some(handler)));
            if found {
                let side_list = ctx.read_native_pin(side_list_pin, side_list);
                ctx.invoke_virtual(
                    side_list,
                    "remove",
                    "(I)Ljava/lang/Object;",
                    &[Value::Int(index as i32)],
                )?;            }        }
        Ok(())
    })();
    ctx.unpin_native_roots(side_list_pin);
    result
}

/// Publish a real LogRecord to every explicit handler. Console emission alone
/// is insufficient: JUL users legitimately install in-process handlers to
/// collect records, including Tomcat's standalone startup test.
pub fn publish_jul_handlers(
    ctx: &mut dyn NativeContext,
    logger: Option<ObjectRef>,
    level: Option<ObjectRef>,
    message: Option<ObjectRef>,
) {
    publish_jul_handlers_src(ctx, logger, level, message, None, None)
}

/// `publish_jul_handlers` with the caller-provided source class/method pair
/// (`Logger.logp` args) stamped into each record so JULI's OneLineFormatter
/// prints the real source instead of "null.null".
pub fn publish_jul_handlers_src(
    ctx: &mut dyn NativeContext,
    logger: Option<ObjectRef>,
    level: Option<ObjectRef>,
    message: Option<ObjectRef>,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
) {
    let vm = ctx.vm_identity();
    let (Some(logger), Some(level), Some(message)) = (logger, level, message) else {
        return;
    };
    // GC SAFETY (2026-07-21, DoHead sporadic-residuals follow-up): this
    // function is `publish_to_jul_handlers_src`'s sibling (same job, a
    // different handler registry) and had the same unpinned-receiver-
    // across-`invoke_virtual` hazard, but worse -- `record` and `handler`
    // were never pinned at ALL (not even once), and `level`/`message`
    // (this function's own parameters) were reused after the GC-capable
    // `setMessage` call below without ever being pinned either. Caught via
    // a hand-written `CRATONVM_DBG_GC_STRESS` repro that reliably produced
    // heap corruption (a non-String object landing in a `List<String>`)
    // even after the sibling function was fixed -- this function runs on
    // every JUL log call too (`native_jul_logger_logp` calls both
    // unconditionally) and was never touched by that earlier fix. Pin
    // `level`/`message` up front (mirroring `publish_to_jul_handlers_src`),
    // and pin `record`/`handler` immediately as each is obtained per
    // iteration, refreshing every one of them from its pin before any use
    // that follows a GC-capable call (`new_object`, `setMessage`,
    // `publish`).
    let level_pin = ctx.pin_native_root(level);
    let message_pin = ctx.pin_native_root(message);
    let src_cls_pin = src_cls.map(|o| (ctx.pin_native_root(o), o));
    let src_mth_pin = src_mth.map(|o| (ctx.pin_native_root(o), o));
    // gci i24/lk: only the rows ANOTHER object of this name filed; the logged
    // Logger's own handlers are delivered once, by the identity-keyed list
    // (`named_handlers_to_publish`).
    let logger_name = read_jul_logger_name(ctx, logger);
    let handlers: Vec<u64> = logger_handlers(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&logger_name)
        .map(|rows| named_handlers_to_publish(rows, logger.as_ptr() as u64))
        .unwrap_or_default();
    // gci i24/lk: nothing to deliver is now the common case (the logged
    // Logger's own rows are skipped), so do not pay the thread lookup.
    if handlers.is_empty() {
        ctx.unpin_native_roots(level_pin);
        return;
    }
    // SAFETY: the addresses come from `logger_handlers`, which
    // `gc_scan_logmanager_roots` roots and `gc_update_logmanager_refs`
    // repoints. `handlers` is a Rust SNAPSHOT of it that no collection
    // repoints, so every handler is pinned BEFORE the first `publish`
    // (gc-common w12-b: each used to be reconstructed from the snapshot just
    // before its own turn, i.e. after the previous handler's bytecode).
    let handler_pins: Vec<(usize, ObjectRef)> = handlers
        .iter()
        .map(|&addr| {
            let handler = unsafe { object_from_u64(addr) };
            (ctx.pin_native_root(handler), handler)
        })
        .collect();
    // Resolve the producing thread's Java tid once, BEFORE any record
    // allocation below can move freshly-created objects. gci i24/lk: after the
    // handler pins, not between the snapshot and them -- a thread lookup that
    // materialises the `Thread` mirror allocates, and the snapshot is not
    // remapped by a collection.
    let producer_tid = crate::current_java_thread_tid(ctx);
    let producer_short_tid = crate::short_thread_id(producer_tid);
    for &(handler_pin, handler) in &handler_pins {
        // The real LogRecord constructor reaches private JDK state that is not
        // materialized on our compact JUL path. Handlers require the public
        // record fields, in particular `message`, so initialize that stable
        // surface directly.
        let mut record = match ctx.new_object("java/util/logging/LogRecord") {
            Ok(Some(Value::Object(Some(record)))) => record,
            _ => continue,
        };
        let record_pin = ctx.pin_native_root(record);
        // gc-common w12-b: read after the record's allocation, not before it.
        let level = ctx.read_native_pin(level_pin, level);
        let message = ctx.read_native_pin(message_pin, message);
        ctx.set_field_by_name(record, "level", Value::Object(Some(level)));
        ctx.set_field_by_name(record, "message", Value::Object(Some(message)));
        stamp_inferred_caller(ctx, &mut record);
        // The stamp allocates two strings.
        let message = ctx.read_native_pin(message_pin, message);
        // Real JDK LogRecord's instance layout is level, sequenceNumber,
        // sourceClassName, sourceMethodName, message. Keep a slot fallback for
        // the private-field resolver path used by compact allocations.
        ctx.set_field(record, 4, Value::Object(Some(message)));
        // Records must carry the producing thread's id: JULI's OneLineFormatter
        // resolves record.getLongThreadID() via ThreadMXBean.getThreadInfo(long),
        // which throws IllegalArgumentException for the 0 an unpopulated record
        // reports (seen as "ErrorManager: 5" on every AsyncFileHandler format).
        ctx.set_field_by_name(record, "longThreadID", Value::Long(producer_tid));
        ctx.set_field_by_name(record, "threadID", Value::Int(producer_short_tid));
        if let Some((pin, obj)) = src_cls_pin {
            let src = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "sourceClassName", Value::Object(Some(src)));
        }
        if let Some((pin, obj)) = src_mth_pin {
            let src = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "sourceMethodName", Value::Object(Some(src)));
        }
        // Prefer the JDK setter too: it writes the resolved private slot even
        // when the compact allocator has not materialized field metadata yet.
        let _ = ctx.invoke_virtual(
            record,
            "setMessage",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(message))],
        );
        // `setMessage` above can GC; refresh every reference touched again
        // below before using any of them.
        let record = ctx.read_native_pin(record_pin, record);
        let message = ctx.read_native_pin(message_pin, message);
        let handler = ctx.read_native_pin(handler_pin, handler);
        // sequenceNumber survives object forwarding and gives the side table a
        // stable identity across the moving collector.
        let record_id = next_log_record_id();
        ctx.set_field(record, 1, Value::Long(record_id));
        // gc-common w13-b: this VM's table and its own cap
        // (`remember_log_record_message`).
        let text = ctx.read_string(message).unwrap_or_default();
        remember_log_record_message(ctx.vm_identity(), record_id, text);
        let _ = ctx.invoke_virtual(
            handler,
            "publish",
            "(Ljava/util/logging/LogRecord;)V",
            &[Value::Object(Some(record))],
        );
        // Every handler pin sits below this turn's record pin.
        ctx.unpin_native_roots(record_pin);
    }
    ctx.unpin_native_roots(level_pin);
}

/// `java/util/logging/Logger.logp(Level, sourceClass, sourceMethod, msg)`
/// intercept. JULI's `DirectJDKLog` (used by Tomcat for every
/// `log.warn/error/info(...)` call) delegates to this method instead of
/// the simpler `Logger.warning(String)`. Without a native, the call
/// drops into our synthetic Logger object (which has no real Handler
/// chain) and the message is silently discarded — that's the
/// "Bootstrap rc=0, no output" symptom for `Bootstrap version` and
/// every other JULI-driven Tomcat command.
pub fn native_jul_logger_logp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null `Level` throws. The SOURCE CLASS and SOURCE METHOD do not —
    // measured: `logp(SEVERE, null, null, "msg")` returns on HotSpot, and so
    // does `entering(null, null)` / `exiting(null, null)` / `throwing(null,
    // null, null)`. Three nulls in one call, one of which is fatal and two of
    // which are ordinary: this signature is the clearest single refutation of
    // a blanket JUL null rule in the whole package.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    // gci i25/jl: HotSpot's `logp` opens with `if (!isLoggable(level))
    // return;`. This native published a below-threshold FINE/FINER/FINEST
    // record to every handler anyway (the trace-level arm below). JULI's
    // `DirectJDKLog` asks the same `isLoggable` before calling here, so its
    // records are unchanged; a direct `logp` caller now sees HotSpot's drop.
    // Reads `args[0..2]` only and is not a GC point.
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // args[2] = source class, args[3] = source method, args[4] = msg.
    // args[5] (if present) = Throwable (5-arg overload). We surface the
    // throwable's class name + message to match Hotspot's
    // SimpleFormatter output shape closely enough for boot-trace.
    let message_obj = match args.get(4) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let throwable_obj = match args.get(5) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // The explicit source class/method pair JULI's DirectJDKLog resolves
    // from the caller stack. Stamped into bridged records so
    // OneLineFormatter prints the real source instead of "null.null".
    let src_cls_obj = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let src_mth_obj = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // GC SAFETY (2026-07-21, DoHead sporadic-residuals follow-up, third
    // layer of the same hazard): this function holds `this`/`level_obj`/
    // `message_obj`/`src_cls_obj`/`src_mth_obj`/`throwable_obj` as raw,
    // unpinned locals and passes the SAME raw values into TWO separate
    // GC-capable helper calls in sequence (`publish_jul_handlers_src` then
    // `publish_to_jul_handlers_src`, both of which allocate `LogRecord`s
    // and `invoke_virtual` into arbitrary handler bytecode). Even with both
    // of those helpers internally pinning their OWN parameters correctly
    // (see their own GC SAFETY comments), a pin only protects the object
    // it is given -- if the value handed in in the FIRST place is already
    // stale (because it went unrefreshed across the first helper's
    // GC-triggering calls), pinning it in the second helper just locks in
    // the wrong object. Confirmed via a `CRATONVM_DBG_GC_STRESS` repro that
    // still reproduced heap corruption with both helpers fixed. Pin every
    // argument object up front and re-derive each one from its pin after
    // the first `publish_jul_handlers_src` call, before it is used again.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let message_pin = message_obj.map(|o| (ctx.pin_native_root(o), o));
    let throwable_pin = throwable_obj.map(|o| (ctx.pin_native_root(o), o));
    let src_cls_pin = src_cls_obj.map(|o| (ctx.pin_native_root(o), o));
    let src_mth_pin = src_mth_obj.map(|o| (ctx.pin_native_root(o), o));
    // `pin_native_root` returns the pre-push stack index, and these six are
    // pinned in a fixed sequential order above, so the smallest present
    // index (the first of them that is `Some`) is the correct base for a
    // single `unpin_native_roots` covering all of them at once.
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p))
        .or_else(|| message_pin.map(|(p, _)| p))
        .or_else(|| throwable_pin.map(|(p, _)| p))
        .or_else(|| src_cls_pin.map(|(p, _)| p))
        .or_else(|| src_mth_pin.map(|(p, _)| p));
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let level_name = level_obj
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "INFO".to_string());
    // Whether the fine-grained levels reach the console is decided by the
    // logger's CONFIGURED level, not by the level name alone.
    //
    // This used to be a flat `"FINE" | "FINER" | "FINEST" => never console`
    // arm. That silently defeated the whole per-logger level configuration
    // surface: JULI's `DirectJDKLog` gates on `logger.isLoggable(level)` and
    // then emits through `logp(...)` — exactly this native — so once a user
    // set e.g. `org.apache.coyote.http2.level = FINEST` in
    // `conf/logging.properties`, `isLoggable` correctly returned true (it
    // consults `jul_ancestor_explicit_level`) and `logp` then threw the
    // record away anyway. Tomcat produced 268 FINE/FINER lines on HotSpot and
    // 0 on CratonVM for the same config, which made `FINE`-level diagnosis of
    // any Tomcat issue impossible.
    //
    // Gate on the same effective threshold `native_jul_logger_is_loggable`
    // uses. With no explicit level configured anywhere on the logger's
    // ancestry the threshold stays at the JDK root default of INFO, so the
    // default-quiet console behaviour this arm was written for is unchanged.
    let level_value = jul_standard_level_value(&level_name).unwrap_or(800);
    let console_threshold =
        jul_ancestor_explicit_level(ctx.vm_identity(), &logger_name).unwrap_or(800);
    let console_allows_fine = level_value >= console_threshold;

    // Map JUL level names to the same compact tags log_simple uses so
    // grep-able output is consistent across the JUL native surface.
    let tag = match level_name.as_str() {
        "SEVERE" => "ERROR",
        "WARNING" => "WARN",
        "INFO" => "INFO",
        "CONFIG" => "INFO",
        // Below the configured threshold: keep it off the console, but still
        // publish to explicitly installed handlers. Tomcat's LogCapture sets a
        // logger to FINE specifically to assert a recoverable handshake
        // underflow, and that path must keep working even when the console
        // stays quiet.
        "FINE" | "FINER" | "FINEST" if !console_allows_fine => {
            // gci i25/jl: the logger's own chain (filter, then every level)
            // first, then the name-keyed side rows unless the filter rejected
            // the record. Nothing reaches the console on this arm.
            // gci i25/jl: a null message still publishes (HotSpot's
            // `new LogRecord(level, null)`); review i25/rv item 14.
            let result = if let (Some(logger), Some(level), message) =
                (this, level_obj, message_obj)
            {
                // The 5-arg `logp` overload's Throwable belongs ON the record
                // (`LogRecord.getThrown()`), not only in a console detail
                // line: `Logger.throwing` is defined in terms of exactly this
                // call, and a handler that reports the throwable saw null.
                publish_to_jul_handlers_outcome(
                    ctx,
                    logger,
                    level,
                    message,
                    src_cls_obj,
                    src_mth_obj,
                    None,
                    throwable_obj,
                )
            } else {
                Ok(JulPublish::NoHandler)
            };
            if matches!(
                result,
                Ok(JulPublish::NoHandler | JulPublish::Delivered | JulPublish::Silenced)
            ) {
                let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
                let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
                let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
                let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
                let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
                publish_jul_handlers_src(
                    ctx,
                    this,
                    level_obj,
                    message_obj,
                    src_cls_obj,
                    src_mth_obj,
                );
            }
            if let Some(base) = base_pin {
                ctx.unpin_native_roots(base);            }
            return result.map(|_| None);        }
        other => other,
    };
    let message = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    // gci i25/jl: the filter is no longer asked here on a separate record
    // whose answer was dropped (`notify_jul_logger_filter`, removed); the
    // publish below asks it with the handlers' record and honours it, and the
    // name-keyed side delivery runs after it, only for a record it admitted.
    // Nothing has run bytecode since entry, so the references are current.
    // Compose the console line NOW (while the Throwable's fields are readable)
    // but do not emit it yet — the real handler chain gets first refusal, and
    // we only fall back to the console sink if nothing accepted the record.
    // Otherwise a config-installed `ConsoleHandler` and this sink both print
    // it and every line appears twice (HotSpot prints it once).
    let console_line = if let Some(t) = throwable_obj {
        // Detail-line, mirroring Tomcat's expectation that a throwable
        // is co-located with the message. We pull the throwable's
        // class name and detail message via standard fields; if the
        // synthetic Throwable layout doesn't carry them we fall back to
        // a bare class label so the line still emits.
        let cls = {
            let cid = ctx.class_id_of_object(t);
            ctx.class_name_of_id(cid)
                .unwrap_or_else(|| "Throwable".to_string())
        };
        let detail = match crate::lang_misc::throwable_message_or_npe_extended(ctx, t) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if detail.is_empty() {
            format!("{tag} [{logger_name}] {message} ({cls})")
        } else {
            format!("{tag} [{logger_name}] {message} ({cls}: {detail})")
        }
    } else {
        format!("{tag} [{logger_name}] {message}")
    };
    // Reading the Throwable's fields above can allocate (`read_string`), so
    // every reference below has to come off its pin again.
    let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let throwable_obj = throwable_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    // gci i25/jl: a null message publishes too (review i25/rv item 14).
    let result = if let (Some(logger), Some(level), message) = (this, level_obj, message_obj) {
        // Same as the trace-level arm above: keep the Throwable on the record.
        publish_to_jul_handlers_outcome(
            ctx,
            logger,
            level,
            message,
            src_cls_obj,
            src_mth_obj,
            None,
            throwable_obj,
        )
    } else {
        Ok(JulPublish::NoHandler)
    };
    // gci i25/jl: the name-keyed side rows (handlers ANOTHER object of this
    // name filed) after the logger's own chain, and only for a record its
    // filter admitted. Before, they ran first and regardless of the filter.
    if matches!(
        result,
        Ok(JulPublish::NoHandler | JulPublish::Delivered | JulPublish::Silenced)
    ) {
        let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
        let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
        let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
        let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
        let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
        publish_jul_handlers_src(ctx, this, level_obj, message_obj, src_cls_obj, src_mth_obj);
    }
    // `console_line` is a plain Rust String, so emitting it here (after the
    // publish) needs no further pin refresh. A filtered record is disposed of
    // (HotSpot prints nothing for it).
    // gci i32/jh: an `Err` (the `LogRecord` constructor's, the filter's or a
    // handler's exception) propagates below and prints nothing, as HotSpot's
    // `log(LogRecord)` throws before any console line; it used to count as
    // "not disposed" and print the sink line AND throw.
    let disposed = !result.as_ref().is_ok_and(|o| !o.disposed());
    // gci i31/hp: once the LogManager is initialized the root's real
    // `ConsoleHandler` is HotSpot's only console (`jul_console_sink_applies`).
    if !disposed && jul_console_sink_applies(&*ctx) {
        crate::emit_framework_log(ctx, &console_line);
    }
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    result.map(|_| None)}

/// Shared body of `java/util/logging/Logger.entering` / `exiting` /
/// `throwing`.
///
/// The spec defines all three purely as a FINER record carrying a fixed
/// message ("ENTRY"/"RETURN"/"THROW"), the caller-supplied source class and
/// method, and — for `throwing` — the Throwable. The real-JDK bytecode builds
/// that record itself and hands it to the private `doLog`, which dereferences
/// the private `loggerBundle` field; on a Logger this module allocated
/// natively (no constructor run) that field is null, so `throwing` died with
/// `NullPointerException: Cannot invoke
/// "java.util.logging.Logger$LoggerBundle.isSystemBundle()" because "lb" is
/// null` and the whole method-trace family was unreliable. Build and publish
/// the record from here so the outcome no longer depends on which JUL class
/// body is loaded.
///
/// `args` is `(this, sourceClass, sourceMethod[, thrown])`.
pub fn jul_trace_marker(ctx: &mut dyn NativeContext, args: &[Value], marker: &str) -> MethodCallResult {
    if jul_dbg_enabled() {
        eprintln!("[JUL-DBG] trace-marker native reached: {marker}");
    }
    let this = match args.first() {
        // No receiver: nothing to log against. A trace convenience method must
        // never raise out of the logging path itself.
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let src_cls = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let src_mth = match args.get(2) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let thrown = match args.get(3) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // `Level.<clinit>` and `create_string` both allocate: pin everything this
    // native holds and re-read each reference afterwards. `this` is pinned
    // first, so one `unpin_native_roots` releases the whole group.
    let base_pin = ctx.pin_native_root(this);
    let src_cls_pin = src_cls.map(|o| (ctx.pin_native_root(o), o));
    let src_mth_pin = src_mth.map(|o| (ctx.pin_native_root(o), o));
    let thrown_pin = thrown.map(|o| (ctx.pin_native_root(o), o));
    let level = resolve_standard_level(ctx, "FINER");
    let level_pin = level.map(|o| (ctx.pin_native_root(o), o));
    let message = ctx.create_string(marker);
    let message_pin = ctx.pin_native_root(message);
    let this = ctx.read_native_pin(base_pin, this);
    let message = ctx.read_native_pin(message_pin, message);
    let level = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_cls = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_mth = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let thrown = thrown_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    // gci i25/jl: HotSpot's `entering` / `exiting` are `logp(Level.FINER, ..)`
    // and `throwing` opens with `if (!isLoggable(Level.FINER)) return;` -- the
    // record is published only on a logger whose level admits FINER. This
    // native published to every handler regardless (and asked the filter on a
    // separate record whose answer it dropped; the publish below asks it).
    // `native_jul_logger_is_loggable` is not a GC point.
    if let Some(level) = level {
        let gate = native_jul_logger_is_loggable(
            ctx,
            &[Value::Object(Some(this)), Value::Object(Some(level))],
        );
        match gate {
            Ok(Some(Value::Int(0))) => {
                ctx.unpin_native_roots(base_pin);
                return Ok(None);
            }
            Err(e) => {
                ctx.unpin_native_roots(base_pin);
                return Err(e);
            }
            _ => {}
        }
    }
    // No console fallback: FINER is trace-level and `logp` deliberately
    // keeps FINE/FINER/FINEST off the console sink, so a logger with no
    // handler chain stays as quiet here as it is there.
    //
    // gci i32/jh: HotSpot's `entering` / `exiting` / `throwing` reach
    // `log(LogRecord)`, whose filter / handler exceptions (and the
    // `LogRecord` constructor's) propagate to the caller; this dropped them
    // (`let _ =`).
    let published = match level {
        Some(level) => {
            publish_to_jul_handlers_full(ctx, this, level, message, src_cls, src_mth, None, thrown)
                .map(|_| ())
        }
        None => Ok(()),
    };
    ctx.unpin_native_roots(base_pin);
    published.map(|()| None)
}

/// Stamp a bridge-built `LogRecord` with the class and method that called the
/// log method, the way `java.util.logging.LogRecord.inferCaller()` does.
///
/// Neither record site in this file ran it, so the pair stayed null on every
/// `logger.warning("...")`-shaped call — only the `logp` family, whose caller
/// hands the pair in, ever carried one. That is not a silent gap:
/// `SimpleFormatter`'s default pattern renders the source pair and falls back
/// to the LOGGER NAME when there is none, so the JDK's own formatter papers
/// over it and the output merely looks wrong. HotSpot renders
/// `RJdkLogging formattedOutputIsRealBytes`; CratonVM rendered
/// `rjdklogging.stream` (regression-suite RJdkLogging,
/// `formattedOutputIsRealBytes`).
///
/// EAGER, where the JDK is lazy, and the reason is measured rather than
/// stylistic. `inferCaller` can afford to run inside `getSourceClassName()`
/// because on HotSpot the frames it walks — `Logger.warning`, `Logger.log`,
/// `doLog` — are Java frames still on the stack when the formatter asks. On
/// CratonVM that whole chain is NATIVE, so a stack captured from the accessor
/// holds only `[caller…, Handler.publish]` with no `java/util/logging` frame
/// in it at all (measured: `frames=2 stack=["SrcProbe2.main",
/// "SrcProbe2$Cap.publish"]`), and the JDK's "skip the logging frames, take
/// the next one" latch can never trip. Here, at record construction, the
/// innermost Java frame IS the caller.
///
/// Known divergence, deliberate: reading the pair AFTER the log call has
/// returned yields the caller here and `null` on HotSpot, because HotSpot's
/// inference is lazy and fails once its frames are gone. Nothing rests on that
/// null — `SimpleFormatter` reads during `publish`, where both now answer
/// identically — and being deterministic is the better failure mode.
pub fn stamp_inferred_caller(ctx: &mut dyn NativeContext, record: &mut ObjectRef) {
    let frames = ctx.capture_stack_trace();
    // Outermost-first, so the innermost frame — the direct caller of the log
    // method — is the LAST one. Skip any logging/reflection frame a re-entrant
    // log call could have left on top.
    let Some(frame) = frames.iter().rev().find(|f| {
        let c = f.class_name.as_ref();
        !c.starts_with("java/util/logging/")
            && !c.starts_with("sun/util/logging/")
            && !c.starts_with("java/lang/reflect/")
            && !c.starts_with("jdk/internal/reflect/")    }) else {
        return;
    };
    let class = frame.class_name.replace('/', ".");
    let method = frame.method_name.as_ref().to_string();
    let record_pin = ctx.pin_native_root(*record);
    let cls_obj = ctx.create_string(&class);
    let cls_pin = ctx.pin_native_root(cls_obj);
    let mth_obj = ctx.create_string(&method);
    let cls_obj = ctx.read_native_pin(cls_pin, cls_obj);
    // Written back through `&mut`: this function already re-read its own copy,
    // but the CALLER's copy stayed at the pre-`capture_stack_trace` address and
    // three call sites kept using it.
    *record = ctx.read_native_pin(record_pin, *record);
    ctx.set_field_by_name(*record, "sourceClassName", Value::Object(Some(cls_obj)));
    ctx.set_field_by_name(*record, "sourceMethodName", Value::Object(Some(mth_obj)));
    // The real setters clear this; stamping the fields directly must too, or
    // the pair is written into a record that still believes it owes an
    // inference. That was inert while `getSourceClassName` was a shadow doing a
    // bare field read, but those four triples are retired under `--real-jdk`
    // since 2026-08-12 (native-api/src/retired_shadow.rs), so the REAL getter —
    // `if (needToInferCaller) inferCaller(); return sourceClassName;` — now
    // runs there and would overwrite this stamp with whatever its own walk
    // found. Every `java/util/logging/` entry point into this file is itself
    // retired in strict mode, so today that is reachable only via a non-JUL
    // receiver (e.g. the `org/jboss/logmanager/` bridges, which are not in the
    // retirement table); write the flag rather than rely on that staying true.
    //
    // Inert in `Compatible`, where the four accessors still dispatch as natives
    // and none of them reads this flag. W7-56-infercaller-strict-RETIRED-20260917.md
    if crate::log_record_real_layout(ctx, *record) {
        ctx.set_field_by_name(*record, "needToInferCaller", Value::Int(0));
    }
    ctx.unpin_native_roots(cls_pin);
    ctx.unpin_native_roots(record_pin);
}

/// Real JUL keeps `useParentHandlers` inside the private
/// `Logger$ConfigurationData` (`config`), which our natively-constructed
/// loggers never materialize — and the compact synthetic shape has no such
/// field at all. Read it by name and treat everything except an explicit
/// `false` as the JDK default (`true`), so a Logger shape that simply doesn't
/// carry the flag can never silence an ancestor's handlers.
///
/// Deliberately a pure field read: dispatching `getUseParentHandlers()` would
/// re-enter real bytecode that dereferences the same unmaterialized `config`.
///
/// FIRST CHOICE is `lib.rs`'s `jul_logger_use_parent_handlers_flag`, which is
/// where `Logger.setUseParentHandlers(Z)V` records the flag. That write and
/// this read used to be in different modules and never met: the setter stored
/// into the side table, this walked `config.useParentHandlers`, and `config` is
/// null on every logger this bridge mints — so the `false` was accepted, read
/// back correctly by `getUseParentHandlers()`, and then ignored by the only
/// consumer that matters. An accessor agreeing with itself is not evidence the
/// consumer agrees with it. Measured in `Compatible` before this change:
/// `setUseParentHandlers(false)` then `info(...)` still reached the parent's
/// Handler (`[INFO:not-up-to-parent]`), where HotSpot delivers nothing; under
/// `--real-jdk` no native holds either triple, real bytecode runs, and the walk
/// already stopped. So this closes a `Compatible`-only divergence and leaves
/// strict mode byte-identical. See
/// docs/feature-designs/native-dispatch-architecture.md.
///
/// An ABSENT entry is not `false`: it means nothing ever called the setter, so
/// fall through to the `config` read and its JDK default (`true`) rather than
/// letting a logger nobody configured silence its ancestors' handlers.
pub fn jul_use_parent_handlers(ctx: &dyn NativeContext, logger: ObjectRef) -> bool {
    if let Some(flag) = crate::jul_logger_use_parent_handlers_flag(ctx, logger) {
        return flag;
    }
    match ctx.get_field_by_name(logger, "config") {
        Value::Object(Some(config)) => !matches!(
            ctx.get_field_by_name(config, "useParentHandlers"),
            Value::Int(0)
        ),
        _ => true,
    }
}

/// The handler `ArrayList`s a record published for `logger` reaches, in the
/// order HotSpot's `Logger.log(LogRecord)` publishes to them: the logger's own
/// list, then -- while the level just visited has `useParentHandlers` -- the
/// list of each existing dotted-name ancestor, nearest first, up to the root.
///
/// gci i25/jl (`gci-i24lk-jul-delivery-review-findings-FIXED-20261016` F2): this
/// used to be `resolve_jul_handler_list`, which returned ONE list -- the
/// logger's own when it had one, else the first ancestor list found -- so a
/// logger with a handler of its own never reached its parents' handlers
/// (`Logger.getLogger("a").addHandler(capture)` silenced the root's
/// `ConsoleHandler`, which HotSpot keeps printing to), and an ancestor whose
/// `useParentHandlers` is false did not stop the walk. HotSpot:
///
/// ```text
/// Logger logger = this;
/// while (logger != null) {
///     for (Handler handler : logger.getHandlers()) handler.publish(record);
///     if (!logger.getUseParentHandlers()) break;
///     logger = logger.getParent();
/// }
/// ```
///
/// The parent is the nearest ancestor NAME that has a Logger object (the
/// `LogManager` rule `allocate_logger` also uses); an explicit `setParent` link
/// is not followed (residual, recorded in `i25-jl-report.md`). A level with two
/// objects (the JUL and JBoss shapes of one name) contributes the first one
/// that has a list, and its `useParentHandlers` decides whether the walk goes
/// on (the first object's when neither has a list).
///
/// Not a GC point: the walk reads side tables and fields only
/// (`existing_loggers_for_name` creates nothing). Callers still pin the lists
/// before publishing, because `Handler.publish` is bytecode.
pub fn resolve_jul_handler_chain(ctx: &mut dyn NativeContext, logger: ObjectRef) -> Vec<ObjectRef> {
    resolve_jul_handler_chain_and_stop(ctx, logger).0
}

/// [`resolve_jul_handler_chain`], plus whether the walk was STOPPED below the
/// root by a `useParentHandlers == false` level (gci i26/rv, item J1 of
/// `gci-i26rv-i25-default-path-review-findings-FIXED-20261016`). The console-sink
/// fallback stands in for the root's default `ConsoleHandler`, so a record
/// whose walk never reaches the root -- `setUseParentHandlers(false)` on a
/// logger with no handler of its own, the usual way to silence one -- must
/// not print it: HotSpot's loop `break`s there and nothing is written.
pub fn resolve_jul_handler_chain_and_stop(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> (Vec<ObjectRef>, bool) {
    let mut lists = Vec::new();
    if let Some(own) = jul_own_handler_list(ctx, logger) {
        lists.push(own);
    }
    let logger_name = read_jul_logger_name(ctx, logger);
    if !jul_use_parent_handlers(ctx, logger) {
        return (lists, jul_walk_stopped_below_root(&logger_name));
    }
    // This walk used to `get_or_create_logger(candidate)` -- always the
    // `java/util/logging/Logger` shape. Under
    // `java.util.logging.manager=org.jboss.logmanager.LogManager` every
    // factory hands back the `org/jboss/logmanager/Logger` shape instead, so
    // the handler Quarkus installs on the ROOT logger lives on a JBoss object
    // while the walk demand-created a brand-new JUL object for `""` and read
    // ITS (empty) side list. `existing_loggers_for_name` reports both shapes
    // and creates neither -- a name nobody has asked for cannot have handlers.
    let mut candidate: &str = &logger_name;
    while let Some(parent) = logger_name_parent(candidate) {
        candidate = parent;
        let level: Vec<ObjectRef> = existing_loggers_for_name(&*ctx, parent)
            .into_iter()
            .flatten()
            .collect();
        let Some(&first) = level.first() else {
            // No Logger object of this name: not a level of the parent chain.
            continue;
        };
        let mut decides = first;
        for &ancestor in &level {
            if let Some(list) = crate::jul_logger_handlers_get(ctx, ancestor) {
                decides = ancestor;
                if !lists.contains(&list) {
                    lists.push(list);
                }
                break;
            }
        }
        if !jul_use_parent_handlers(ctx, decides) {
            return (lists, jul_walk_stopped_below_root(parent));
        }
    }
    (lists, false)
}

/// gci i26/rv: a walk that stops at the level named `level` stopped BELOW
/// the root unless that level is the root (`""`, which has no parent anyway).
pub fn jul_walk_stopped_below_root(level: &str) -> bool {
    !level.is_empty()
}

/// `logger`'s OWN handler list: the identity-keyed side table, else the legacy
/// synthetic layout's slot-2 list. `None` when it has neither.
pub fn jul_own_handler_list(ctx: &mut dyn NativeContext, logger: ObjectRef) -> Option<ObjectRef> {
    if let Some(handlers) = crate::jul_logger_handlers_get(ctx, logger) {
        return Some(handlers);
    }
    // Only our legacy synthetic logger stores its parent/handler
    // fallback at raw slot 2.  On a real JDK Logger that slot is the
    // `name` String; treating it as an ArrayList reintroduces the
    // `java/lang/String.size()I` failure when Tomcat's
    // ClassLoaderLogManager creates a real per-webapp logger.
    let synthetic_layout = matches!(ctx.get_field(logger, LOGGER_FIELD_NAME),
        Value::Object(Some(name))
            if ctx.class_name_arc_of_id(ctx.class_id_of_object(name)).as_deref()
                == Some("java/lang/String"));
    if synthetic_layout {
        // `allocate_logger` now populates this same slot with a real parent
        // `Logger` (see ancestor walk below) rather than a handlers list --
        // don't misread it as one.
        if let Value::Object(Some(list)) = ctx.get_field(logger, LOGGER_FIELD_PARENT) {
            if ctx
                .class_name_arc_of_id(ctx.class_id_of_object(list))
                .as_deref()
                != Some(CLS_JUL_LOGGER)
            {
                return Some(list);            }        }    }
    None
}

/// Publish an ALREADY-CONSTRUCTED `LogRecord` through `logger`'s handler
/// chain — the `Logger.log(LogRecord)` path, and the tail of the real-JDK
/// `throwing`/`entering` bytecode when it reaches `doLog`.
///
/// Returns `true` when the record was disposed of -- at least one handler
/// accepted it, or the logger's `Filter` rejected it -- so the caller keeps
/// its console-sink fallback only for a record nothing took. gci i25/jl: the
/// filter is asked here, with this record, before any handler (HotSpot's
/// `Logger.log(LogRecord)`: `if (theFilter != null &&
/// !theFilter.isLoggable(record)) return;`); a filter that throws propagates,
/// as it does from HotSpot's `log`. Every level of the parent chain is
/// published to (`resolve_jul_handler_chain`).
pub fn publish_existing_record_to_jul_handlers(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    record: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let logger_pin = ctx.pin_native_root(logger);
    let record_pin = ctx.pin_native_root(record);
    // gci i26/rv (item J1): a walk stopped below the root by
    // `useParentHandlers == false` disposes of an untaken record silently
    // (the console sink stands in for the root's `ConsoleHandler`).
    let (chain, stopped_below_root) = resolve_jul_handler_chain_and_stop(ctx, logger);
    let has_filter = crate::jul_logger_filter_get(ctx, logger).is_some();
    if chain.is_empty() && !has_filter {
        ctx.unpin_native_roots(logger_pin);
        return Ok(stopped_below_root);
    }
    // Every list is pinned above `record_pin`, so `logger_pin` releases them.
    let chain_pins: Vec<(usize, ObjectRef)> = chain
        .iter()
        .map(|&list| (ctx.pin_native_root(list), list))
        .collect();
    let logger = ctx.read_native_pin(logger_pin, logger);
    let record = ctx.read_native_pin(record_pin, record);
    match jul_logger_filter_admits(ctx, logger, record) {
        Ok(true) => {}
        Ok(false) => {
            ctx.unpin_native_roots(logger_pin);
            return Ok(true);
        }
        Err(e) => {
            ctx.unpin_native_roots(logger_pin);
            return Err(e);
        }
    }
    // The filter is bytecode: re-derive both pinned references.
    let logger = ctx.read_native_pin(logger_pin, logger);
    let record = ctx.read_native_pin(record_pin, record);
    // Real `Logger.log(LogRecord)` stamps the logger name onto the record
    // before dispatch; handlers such as `SLF4JBridgeHandler` look it up and
    // silently drop records that carry a null name.
    if matches!(
        ctx.get_field_by_name(record, "loggerName"),
        Value::Object(None)
    ) {
        let name = read_jul_logger_name(ctx, logger);
        let name_obj = ctx.create_string(&name);
        let record = ctx.read_native_pin(record_pin, record);
        ctx.set_field_by_name(record, "loggerName", Value::Object(Some(name_obj)));
    }
    let mut delivered = false;
    for &(handlers_pin, handlers) in &chain_pins {
        let handlers = ctx.read_native_pin(handlers_pin, handlers);
        let size = match ctx.invoke_virtual(handlers, "size", "()I", &[]) {
            Ok(Some(Value::Int(size))) if size > 0 => size as usize,
            _ => continue,
        };
        match publish_record_to_jul_handler_list(
            ctx,
            handlers_pin,
            handlers,
            size,
            record_pin,
            record,
        ) {
            Ok(taken) => delivered |= taken,
            Err(e) => {
                ctx.unpin_native_roots(logger_pin);
                return Err(e);
            }
        }
    }
    ctx.unpin_native_roots(logger_pin);
    Ok(delivered || stopped_below_root)
}

/// One level of [`publish_existing_record_to_jul_handlers`]: offer the record
/// to each of the `size` handlers of the pinned list. A failing `get` and the
/// flush are swallowed as they always were on this path (see the flush note);
/// a failing `publish` propagates (gci i32/jh,
/// [`jul_handler_publish_failure_propagates`]).
pub fn publish_record_to_jul_handler_list(
    ctx: &mut dyn NativeContext,
    handlers_pin: usize,
    handlers: ObjectRef,
    size: usize,
    record_pin: usize,
    record: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let mut delivered = false;
    for index in 0..size {
        let handlers = ctx.read_native_pin(handlers_pin, handlers);
        let handler = match ctx.invoke_virtual(
            handlers,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(index as i32)],
        ) {
            Ok(Some(Value::Object(Some(handler)))) => handler,
            _ => continue,
        };
        let handler_pin = ctx.pin_native_root(handler);
        let handler = ctx.read_native_pin(handler_pin, handler);
        let record = ctx.read_native_pin(record_pin, record);
        match ctx.invoke_virtual(
            handler,
            "publish",
            "(Ljava/util/logging/LogRecord;)V",
            &[Value::Object(Some(record))],
        ) {
            Ok(_) => delivered = true,
            // gci i32/jh: as in `publish_record_to_jul_handler_list_strict`,
            // HotSpot's `log(LogRecord)` lets a handler's failure out.
            Err(e) if jul_handler_publish_failure_propagates() => {
                ctx.unpin_native_roots(handler_pin);
                return Err(e);
            }
            Err(_) => {}
        }
        // KEPT SWALLOW. HotSpot's `Logger.log` does not call `Handler.flush()`
        // at all — this flush is OURS, added so the native publication bridge
        // reaches the same durability the JDK gets by other means. There is no
        // JDK `catch` to copy, and turning a failure in work HotSpot never
        // performs into a Java-visible throw HotSpot never raises would be a
        // fresh divergence. `Handler.flush()` declares no checked exception,
        // and `StreamHandler.flush()` absorbs `Exception` into its
        // `ErrorManager` anyway.
        //
        // Residual: an `Error` is absorbed here too. This helper returns
        // `bool`, so narrowing it is a signature change, not a one-liner —
        // unlike its sibling in `publish_to_jul_handlers_full` below, which
        // returns a `Result` and IS narrowed.
        // W7-57-close-flush-swallow-sweep.md
        let handler = ctx.read_native_pin(handler_pin, handler);
        let _ = ctx.invoke_virtual(handler, "flush", "()V", &[]);
        // The top of the pin stack (bytecode pops what it pushes).
        ctx.unpin_native_roots(handler_pin);
    }
    Ok(delivered)
}

/// gci i32/jh (`i31-hp-report.md` review item 5): `CRATONVM_JUL_HANDLER_PUBLISH_THROWS`
/// (default ON, `=0` kills; `runtime_flag_default_on`). HotSpot's
/// `Logger.log(LogRecord)` calls every `handler.publish(record)` with no
/// catch, so a handler's exception leaves the logging call (the JDK's own
/// `StreamHandler`s report their I/O failures to their `ErrorManager` and do
/// not throw). Both handler loops of this file swallowed it and counted the
/// record as not taken, so the console sink printed it -- or, since i31/hp
/// gated the sink, it vanished. Off: the swallow as before. Read only on a
/// failed `publish`. No static.
pub fn jul_handler_publish_failure_propagates() -> bool {
    true
}

/// gci i25/jl (`gci-i24lk-jul-delivery-review-findings-FIXED-20261016` F1): ask
/// `logger`'s `Filter`, if it has one, whether `record` -- the record the
/// handlers will receive -- is loggable. `Ok(true)` with no filter; `Ok(false)`
/// only on an explicit `false`; an exception from `isLoggable` propagates, as
/// it does out of HotSpot's `Logger.log(LogRecord)`.
///
/// Replaces `notify_jul_logger_filter`, which built a SEPARATE record, called
/// the filter and discarded its answer, and was not called at all on
/// `log(Level, String)` or the `info`/`warning`/... family.
///
/// A GC point (the filter is bytecode): callers re-derive what they hold.
pub fn jul_logger_filter_admits(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    record: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    // Not a GC point (side-table read): `record` is still current below.
    let Some(filter) = crate::jul_logger_filter_get(ctx, logger) else {
        return Ok(true);
    };
    let answer = ctx.invoke_virtual(
        filter,
        "isLoggable",
        "(Ljava/util/logging/LogRecord;)Z",
        &[Value::Object(Some(record))],
    )?;
    Ok(jul_filter_answer_admits(answer))
}

/// The admission verdict of a `Filter.isLoggable` return value: only an
/// explicit `false` (`Int(0)`) rejects.
pub fn jul_filter_answer_admits(answer: Option<Value>) -> bool {
    !matches!(answer, Some(Value::Int(0)))
}

/// The record payloads the
/// parameterized / throwable-carrying `Logger.log` overloads must carry:
/// `params` (the `Object[]` behind `LogRecord.getParameters()`) and `thrown`
/// (`LogRecord.getThrown()`).
///
/// `message` stays the RAW pattern (`"one={0}"`) — HotSpot substitutes the
/// `{n}` placeholders in the *Formatter*, never in the record, so a handler
/// that inspects `getMessage()`/`getParameters()` must observe both halves
/// separately.
///
/// Returns `true` when at least one handler accepted the record, so callers
/// can keep the console-sink fallback for loggers that have no handler chain
/// at all instead of silently dropping the line.
/// The `(sourceClassName, sourceMethodName)` pair a `LogRecord` published from
/// this bridge should carry, in DOTTED form, or `None` when the stack offers no
/// caller (bootstrap, or a log driven entirely from native code).
///
/// This is `LogRecord$CallerFinder`'s job, ADAPTED rather than transcribed, and
/// the difference is the whole reason it works. Real `CallerFinder` starts with
/// `lookingForLogger = true` and returns nothing until it has SEEN a
/// `java.util.logging.Logger` frame — correct on HotSpot, where the record is
/// built by `Logger.doLog` with those frames live on the stack. In `Compatible`
/// this native IS the `Logger` frame and no Java frame is pushed for it, so a
/// literal transcription would look for a marker that is never present and
/// return `None` every time — a helper that cannot fire, which is how this
/// campaign's vacuous fixes have looked. Measured: the whole `Logger.log`/
/// `doLog`/`warning` run is absent from a `Compatible` capture taken inside a
/// Handler (`[RJdkLogging$Cap.publish, RJdkLogging.main]` against HotSpot's
/// `[…publish, Logger.log, Logger.doLog, Logger.log, Logger.warning, main]`).
///
/// So: take the INNERMOST Java frame, skipping any `Logger` frames that do
/// happen to be present. The skip is not dead code — real `Logger` bytecode can
/// still be on the stack on the mixed paths (`Logger.log(LogRecord)` reaching a
/// native handler, a convenience overload that resolved to bytecode), and
/// without it the record would name `java.util.logging.Logger` as its own
/// caller. The two class names are exactly the two `isLoggerImplFrame` admits;
/// deliberately not widened, because a name this predicate wrongly skips
/// silently attributes the record to its caller's caller.
///
/// `StackTraceEntry::class_name` is INTERNAL (slash) form — the same form
/// `lang_class`'s reflection-frame filter matches on — while
/// `sourceClassName` is read back by Java as a normal dotted class name, so the
/// winner is converted on the way out.
pub fn infer_jul_caller_source(ctx: &mut dyn NativeContext) -> Option<(String, String)> {
    // Outermost-first: `frames[0]` is `main`, the last entry is the innermost
    // Java frame. Documented on `capture_stack_trace` and relied on the same
    // way by `locale_resources::caller_bundle_class_loader`.
    let frames = ctx.capture_stack_trace();
    frames
        .iter()
        .rev()
        .find(|f| {
            &*f.class_name != "java/util/logging/Logger"
                && !f.class_name.starts_with("sun/util/logging/PlatformLogger")
        })
        .map(|f| (f.class_name.replace('/', "."), f.method_name.to_string()))
}

/// What [`publish_to_jul_handlers_outcome`] did with a record.
///
/// gci i25/jl: callers decide two things from it that a `bool` could not
/// carry together -- whether the console-sink fallback runs (only for
/// `NoHandler`) and whether the name-keyed side delivery
/// (`publish_jul_handlers_src`) runs (not for `Filtered`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JulPublish {
    /// No handler on the whole parent chain took the record (none exists, or
    /// none accepted it). The caller's console fallback applies.
    NoHandler,
    /// At least one handler accepted the record.
    Delivered,
    /// The logger's `Filter` answered `false`: nothing is published and
    /// nothing is printed (HotSpot's `Logger.log(LogRecord)` returns there).
    Filtered,
    /// gci i26/rv (item J1): no handler took the record AND the parent walk
    /// was stopped below the root by `useParentHandlers == false`, so the
    /// root's `ConsoleHandler` (the console sink's stand-in) is never reached:
    /// nothing is printed. The name-keyed side delivery still runs, as for
    /// `NoHandler`.
    Silenced,
}

impl JulPublish {
    /// `true` when the record was disposed of (no console fallback): the
    /// `bool` that [`publish_to_jul_handlers_full`] has always returned, with a
    /// filtered record counted as disposed.
    pub fn disposed(self) -> bool {
        !matches!(self, JulPublish::NoHandler)
    }
}

/// [`publish_to_jul_handlers_outcome`] reduced to "was the record disposed
/// of" (delivered, or rejected by the logger's filter).
#[allow(clippy::too_many_arguments)]
pub fn publish_to_jul_handlers_full(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    level: ObjectRef,
    message: ObjectRef,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
    params: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    publish_to_jul_handlers_outcome(
        ctx,
        logger,
        level,
        Some(message),
        src_cls,
        src_mth,
        params,
        thrown,
    )
    .map(JulPublish::disposed)
}

/// Build the `LogRecord` and publish it: the logger's `Filter` first (gci
/// i25/jl F1, with this very record, after it is complete and before any
/// handler), then every handler of every level of the parent chain (F2,
/// `resolve_jul_handler_chain`). The level gate (`isLoggable(level)`) is the
/// CALLER's, as in HotSpot, where it precedes the record's construction.
///
/// With no handler anywhere and no filter, nothing is built (the
/// handler-less console-fallback path pays no record and no stack capture).
/// With a filter and no handler, the record is built so the filter is asked,
/// as HotSpot asks it.
#[allow(clippy::too_many_arguments)]
pub fn publish_to_jul_handlers_outcome(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    level: ObjectRef,
    message: Option<ObjectRef>,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
    params: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
) -> Result<JulPublish, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(logger);
    let level_pin = ctx.pin_native_root(level);
    let message_pin = message.map(|o| (ctx.pin_native_root(o), o));
    // Released with base_pin below (stack discipline).
    let src_cls_pin = src_cls.map(|o| (ctx.pin_native_root(o), o));
    let src_mth_pin = src_mth.map(|o| (ctx.pin_native_root(o), o));
    let params_pin = params.map(|o| (ctx.pin_native_root(o), o));
    let thrown_pin = thrown.map(|o| (ctx.pin_native_root(o), o));
    let result: Result<JulPublish, MethodCallFailed> = (|| {
        let logger = ctx.read_native_pin(base_pin, logger);
        let (chain, stopped_below_root) = resolve_jul_handler_chain_and_stop(ctx, logger);
        // gci i26/rv (item J1): what "no handler took it" means here.
        let untaken = if stopped_below_root {
            JulPublish::Silenced
        } else {
            JulPublish::NoHandler
        };
        if chain.is_empty() && crate::jul_logger_filter_get(ctx, logger).is_none() {
            return Ok(untaken);
        }
        // Every list is pinned above `thrown_pin`; `base_pin` releases them.
        let chain_pins: Vec<(usize, ObjectRef)> = chain
            .iter()
            .map(|&list| (ctx.pin_native_root(list), list))
            .collect();
        let logger = ctx.read_native_pin(base_pin, logger);
        let level = ctx.read_native_pin(level_pin, level);
        let message = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let mut record = match ctx.new_object_initialized(
            "java/util/logging/LogRecord",
            "(Ljava/util/logging/Level;Ljava/lang/String;)V",
            &[Value::Object(Some(level)), Value::Object(message)],
        )? {
            Some(Value::Object(Some(record))) => record,
            _ => return Ok(JulPublish::NoHandler),
        };
        // GC SAFETY (2026-07-21, DoHead sporadic-residuals follow-up): pin
        // `record` immediately, before any of the field sets/invokes below.
        // `record` used to go unpinned until just before the handler loop,
        // well after `setMessage` (an `invoke_virtual` into real,
        // overridable `LogRecord` bytecode that can allocate and trigger a
        // moving GC) had already run and the subsequent `set_field(record,
        // 1, ...)` had already used the stale, unpinned reference -- caught
        // live via the `gen_heap` OOB-write corruption guard (backtrace
        // through this exact `set_field(record, 1, ...)` call, `index=1`,
        // landing on a fresh zero-field `java/lang/Object`). Same hazard
        // class as `native_bos_flush_locked`
        // (dohead-post-fix-sporadic-residuals-FIXED.md).
        let record_pin = ctx.pin_native_root(record);
        // GC SAFETY (2026-07-21, JulGcStressRepro checkcast root cause): the
        // LogRecord `<init>` native invoked by `new_object_initialized` above
        // materializes a java/time/Instant (allocates -> can trigger a moving
        // GC). `level`/`message` were last read from their pins BEFORE that
        // call; if a GC fired inside the ctor it already moved the young
        // message string AND fixed up the record's own `message` field during
        // evacuation -- after which the raw field writes below would store the
        // condemned from-space address right back over the corrected field.
        // `getMessage()` is a plain field read (phases_early lr_get), so it
        // then faithfully returns the poison: once young space is reset and
        // reused the address reads as a zero-header object (checkcast
        // "java.lang.Object cannot be cast to java.lang.String"), a foreign
        // byte[], or a different, later string. Unlike `invoke_virtual`
        // (whose entry barrier heals forwarded args), `set_field`/
        // `set_field_by_name` are direct heap writes with no healing --
        // re-derive both from their pins first.
        let level = ctx.read_native_pin(level_pin, level);
        let message = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        // The compact VM may not materialize the JDK's private LogRecord
        // layout through its constructor. FileHandler.isLoggable() and its
        // formatter consume the public level/message surface, so make that
        // surface explicit just as the direct-handler bridge does.
        ctx.set_field_by_name(record, "level", Value::Object(Some(level)));
        ctx.set_field_by_name(record, "message", Value::Object(message));
        stamp_inferred_caller(ctx, &mut record);
        // gc-common w12-b: the stamp allocates, and `logger` was last read
        // BEFORE the `LogRecord` constructor (bytecode) above; re-read both
        // `message` and `logger` here, and `record` after the name's
        // `create_string` below.
        let message = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let logger = ctx.read_native_pin(base_pin, logger);
        ctx.set_field(record, 4, Value::Object(message));
        // FIX (logbackloggingsystemtests-julbridge-loggername-null): this
        // synthetic `LogRecord` bypasses `Logger.log(LogRecord)`'s real
        // bytecode (which sets `loggerName` to `this.getName()` before
        // dispatch), so without this the record's `loggerName` field stays
        // null all the way to the ancestor's handlers below — e.g.
        // `org.slf4j.bridge.SLF4JBridgeHandler.publish()`, installed on the
        // JUL root by Spring Boot's `LogbackLoggingSystem`/`jul-to-slf4j`,
        // calls `LoggerFactory.getLogger(record.getLoggerName())` and
        // silently no-ops (real JUL's `Logger.log()` catches and reports any
        // `Handler.publish()` exception to `ErrorManager` rather than
        // propagating it, so a `LoggerFactory.getLogger(null)` NPE inside the
        // handler is never surfaced) — every JUL log call that must route
        // through an ancestor's handler (rather than the exact logger's own)
        // is silently dropped. Real-JDK A/B confirmed CratonVM-only
        // (`LogbackLoggingSystemTests`/`Log4J2LoggingSystemTests`
        // `loggingLevelIsPropagatedToJul`).
        let record_logger_name_str = read_jul_logger_name(ctx, logger);
        let record_logger_name = ctx.create_string(&record_logger_name_str);
        let record = ctx.read_native_pin(record_pin, record);
        ctx.set_field_by_name(
            record,
            "loggerName",
            Value::Object(Some(record_logger_name)),
        );
        if let Some((pin, obj)) = src_cls_pin {
            let src = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "sourceClassName", Value::Object(Some(src)));
        }
        if let Some((pin, obj)) = src_mth_pin {
            let src = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "sourceMethodName", Value::Object(Some(src)));
        }
        // No caller-supplied pair (only `logp` has one): infer it, the way real
        // `LogRecord.getSourceClassName()` does on first read. `SimpleFormatter`'s
        // DEFAULT pattern renders this pair as `%2$s` and falls back to the
        // logger NAME when it is null, so without this every JUL line CratonVM
        // formats reads `<logger.name>` where HotSpot reads `Class method` —
        // measured, `Compatible`: `psrc.y` vs `PSrc main`.
        //
        // Only reached once a handler chain exists (see the early return
        // above), so a handler-less logger on the console-fallback path — the
        // pre-`readConfiguration` Tomcat/Spring Boot state — still pays no
        // stack capture at all. That bound matters: HotSpot's cost is deferred
        // to whoever calls `getSourceClassName()`, and this is the nearest
        // equivalent placement we can reach from a native.
        //
        // A FALLBACK, not the primary inference. `stamp_inferred_caller` above
        // already stamped the pair (and cleared `needToInferCaller`), and this
        // block used to run unconditionally afterwards — so it took a SECOND
        // `capture_stack_trace` on every published record and then overwrote the
        // first answer with its own, which is the weaker of the two predicates.
        // Adjudicated against the JDK 25 source rather than by preference:
        // `LogRecord$CallerFinder.test` has TWO stages, a latch that skips until
        // it sees `java.util.logging.Logger` / `sun.util.logging.PlatformLogger*`
        // (`isLoggerImplFrame`) and then a FILTER,
        // `jdk.internal.logger.SurrogateLogger.isFilteredFrame` → `Formatting.
        // isFilteredFrame`, which skips all of `java.util.logging.`,
        // `sun.util.logging.`, `jdk.internal.logger.`,
        // `java.lang.invoke.MethodHandle*`, `java.security.AccessController` and
        // anything implementing `System.Logger`. `infer_jul_caller_source` skips
        // only the two LATCH names, so it can name `java.util.logging.Handler`
        // or a reflection frame as the caller; `stamp_inferred_caller`'s wider
        // skip set is the analogue of the filter stage and is the faithful one.
        // Kept as the last resort for the case the wider set rejects every
        // frame, where a narrow answer beats a null pair.
        // W7-35-jul-supplier-and-payload-residuals.md
        if src_cls_pin.is_none()
            && src_mth_pin.is_none()
            && !matches!(
                ctx.get_field_by_name(record, "sourceClassName"),
                Value::Object(Some(_))
            )
        {
            if let Some((cls, mth)) = infer_jul_caller_source(ctx) {
                let cls_obj = ctx.create_string(&cls);
                let cls_pin = ctx.pin_native_root(cls_obj);
                let mth_obj = ctx.create_string(&mth);
                // Both `create_string`s allocate; re-derive the record and the
                // first string before either is written.
                let cls_obj = ctx.read_native_pin(cls_pin, cls_obj);
                let record = ctx.read_native_pin(record_pin, record);
                ctx.set_field_by_name(record, "sourceClassName", Value::Object(Some(cls_obj)));
                ctx.set_field_by_name(record, "sourceMethodName", Value::Object(Some(mth_obj)));
            }
        }
        let record = ctx.read_native_pin(record_pin, record);
        // `getParameters()` / `getThrown()` are as much a part of the record's
        // public surface as the message is: HotSpot's `log(Level, String,
        // Object)` / `log(Level, String, Object[])` / `log(Level, String,
        // Throwable)` all stamp them here and let the Formatter do the `{n}`
        // substitution. Write the field directly AND drive the JDK setter, for
        // the same reason the message/level pair above does both.
        if let Some((pin, obj)) = params_pin {
            let params = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "parameters", Value::Object(Some(params)));
            let _ = ctx.invoke_virtual(
                record,
                "setParameters",
                "([Ljava/lang/Object;)V",
                &[Value::Object(Some(params))],
            );
        }
        let record = ctx.read_native_pin(record_pin, record);
        if let Some((pin, obj)) = thrown_pin {
            let thrown = ctx.read_native_pin(pin, obj);
            ctx.set_field_by_name(record, "thrown", Value::Object(Some(thrown)));
            let _ = ctx.invoke_virtual(
                record,
                "setThrown",
                "(Ljava/lang/Throwable;)V",
                &[Value::Object(Some(thrown))],
            );
        }
        // Both setters above are overridable bytecode and can GC.
        let record = ctx.read_native_pin(record_pin, record);
        let message = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let _ = ctx.invoke_virtual(
            record,
            "setMessage",
            "(Ljava/lang/String;)V",
            &[Value::Object(message)],
        );
        // `setMessage` above can GC; refresh both `record` and `message`
        // (the latter is also read again below, past the same call) before
        // touching either again.
        let record = ctx.read_native_pin(record_pin, record);
        let message = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        let record_id = next_log_record_id();
        ctx.set_field(record, 1, Value::Long(record_id));
        // The same retention cap as the sibling registry path. Without it this
        // identity-keyed delivery path -- the ONLY one that fires for a real
        // `Logger.getLogger(...)` logger (the name-keyed sibling's lookup
        // misses there, so its in-loop eviction never runs) -- grows the side
        // table by one entry per log call, forever. gc-common w13-b: the
        // table and its cap are this VM's (`remember_log_record_message`).
        // gci i25/jl (review i25/rv item 14): a NULL message files no row, so
        // the `getMessage` native misses the table and answers the record's
        // own null field, as HotSpot's `new LogRecord(level, null)` does.
        if let Some(text) = message.and_then(|m| ctx.read_string(m)) {
            remember_log_record_message(ctx.vm_identity(), record_id, text);
        }
        // gci i25/jl F1: the filter sees the record the handlers get, once,
        // before any of them (HotSpot's `Logger.log(LogRecord)`).
        let logger = ctx.read_native_pin(base_pin, logger);
        let record = ctx.read_native_pin(record_pin, record);
        if !jul_logger_filter_admits(ctx, logger, record)? {
            return Ok(JulPublish::Filtered);
        }
        let mut delivered = false;
        // gci i25/jl F2: every level of the parent chain, own list first.
        for &(handlers_pin, handlers) in &chain_pins {
            let handlers = ctx.read_native_pin(handlers_pin, handlers);
            let size = match ctx.invoke_virtual(handlers, "size", "()I", &[])? {
                Some(Value::Int(size)) if size > 0 => size as usize,
                _ => continue,
            };
            delivered |= publish_record_to_jul_handler_list_strict(
                ctx,
                handlers_pin,
                handlers,
                size,
                record_pin,
                record,
            )?;
        }
        Ok(if delivered {
            JulPublish::Delivered
        } else {
            untaken
        })
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

/// One level of [`publish_to_jul_handlers_outcome`]: offer the record to each
/// of the `size` handlers of the pinned list. Unlike
/// [`publish_record_to_jul_handler_list`], a failing `get` and a flush `Error`
/// propagate (this path has always been the narrowed one).
pub fn publish_record_to_jul_handler_list_strict(
    ctx: &mut dyn NativeContext,
    handlers_pin: usize,
    handlers: ObjectRef,
    size: usize,
    record_pin: usize,
    record: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    let mut delivered = false;
    for index in 0..size {
        let handlers = ctx.read_native_pin(handlers_pin, handlers);
        let handler = match ctx.invoke_virtual(
            handlers,
            "get",
            "(I)Ljava/lang/Object;",
            &[Value::Int(index as i32)],
        )? {
            Some(Value::Object(Some(handler))) => handler,
            _ => continue,
        };
        let handler_pin = ctx.pin_native_root(handler);
        let handler = ctx.read_native_pin(handler_pin, handler);
        let record = ctx.read_native_pin(record_pin, record);
        match ctx.invoke_virtual(
            handler,
            "publish",
            "(Ljava/util/logging/LogRecord;)V",
            &[Value::Object(Some(record))],
        ) {
            Ok(_) => delivered = true,
            // gci i32/jh: HotSpot's `Logger.log(LogRecord)` has no catch
            // around `handler.publish`; the failure leaves the logging call
            // and the rest of the chain is not offered the record. An early
            // return leaves `handler_pin` to the caller's base unpin.
            Err(e) if jul_handler_publish_failure_propagates() => return Err(e),
            Err(_) => {}
        }
        // FileHandler buffers output. The native publication bridge is
        // synchronous, so preserve JUL's observable completion contract
        // before the caller inspects its per-webapp log file.
        //
        // GC SAFETY: `publish` above is arbitrary, overridable Java
        // bytecode that can GC; refresh `handler` from its pin before
        // reusing it for `flush` below (same hazard class as the
        // `record`/`message` fix in the caller).
        //
        // KEPT SWALLOW, NARROWED. HotSpot's `Logger.log` makes no
        // `Handler.flush()` call at all, so there is no JDK `catch` to
        // copy and a Java-visible throw from work HotSpot never performs
        // would be a fresh divergence. What is NOT that is an `Error`: a
        // `NoSuchMethodError` here means our own dispatch failed to find
        // `flush`, which is a broken VM, not a busy log file.
        // `vm_only_best_effort` absorbs `Exception` and returns the rest.
        // W7-57-close-flush-swallow-sweep.md
        let handler = ctx.read_native_pin(handler_pin, handler);
        let flushed = ctx.invoke_virtual(handler, "flush", "()V", &[]);
        // An early return leaves `handler_pin` to the caller's base unpin.
        cratonvm_native_api::delegated_close::vm_only_best_effort(&*ctx, flushed)?;
        // The top of the pin stack (bytecode pops what it pushes).
        ctx.unpin_native_roots(handler_pin);
    }
    Ok(delivered)
}

/// `CRATONVM_DBG_JUL=1`: trace which JUL native each probe call actually
/// reaches and what it does with the record. Same shape (and rationale) as
/// `CRATONVM_DBG_DROPPED_STUBS` in `native-api::registry` — the JUL surface is
/// registered from six different registrars across three crates with
/// last-registration-wins semantics, so "which implementation ran" is not
/// answerable by reading the source alone. Cheap/no-op when unset.
pub fn jul_dbg_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())}

/// Is `o` a `java.lang.Throwable`?
///
/// Walks the object's OWN superclass chain comparing class NAMES, instead of
/// first resolving `java/lang/Throwable` to a `ClassId` by name. That matters:
/// `class_id_by_name` answers `None` both for "not loaded" and — since by-name
/// lookup became ambiguity-strict — for "several loaders define this name",
/// and a caller that reads `None` as "not a throwable" then silently drops the
/// `thrown` argument of `log(Level, String, Throwable)` instead of stamping it
/// on the record. The chain walk performs no by-name resolution and so cannot
/// fail that way. (STANDING: None != absent.)
pub fn jul_is_throwable(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(o);
    // Depth guard: a corrupt or self-referential chain must not spin here.
    for _ in 0..64 {
        match ctx.class_name_arc_of_id(cid).as_deref() {
            Some("java/lang/Throwable") => return true,
            // `Object` terminates the chain; `None` means the id is not a
            // real loaded class (synthetic lambda proxy, stale ref).
            Some("java/lang/Object") | None => return false,
            _ => {}
        }
        match ctx.superclass_of(cid) {
            Some(parent) if parent != cid => cid = parent,
            _ => return false,
        }
    }
    false
}

/// Render a JUL message value: read `String`s directly, invoke `get()` only
/// on actual `java.util.function.Supplier` instances, and use `toString()`
/// for ordinary parameter objects.
pub fn jul_resolve_msg(ctx: &mut dyn NativeContext, o: ObjectRef) -> String {
    if let Some(s) = ctx.read_string(o) {
        return s;
    }
    let supplier_class = ctx.class_id_by_name("java/util/function/Supplier");
    let object_class = ctx.class_id_of_object(o);
    let mut o = o;
    if supplier_class
        .is_some_and(|supplier| object_class == supplier || ctx.is_subclass(object_class, supplier))
    {
        // gc-common w12-b: `get()` is bytecode and, when it answers a
        // non-String, `o` is used again for `toString()`; it is pinned across
        // the call and re-read.
        let o_pin = ctx.pin_native_root(o);
        let got = ctx.invoke_virtual(o, "get", "()Ljava/lang/Object;", &[]);
        o = ctx.read_native_pin(o_pin, o);
        ctx.unpin_native_roots(o_pin);
        if let Ok(Some(Value::Object(Some(r)))) = got {
            if let Some(s) = ctx.read_string(r) {
                return s;
            }
        }
    }
    if let Ok(Some(Value::Object(Some(r)))) =
        ctx.invoke_virtual(o, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(s) = ctx.read_string(r) {
            return s;
        }
    }
    String::new()
}

/// Render a `Throwable` as `<class>: <detail>` plus its first few stack
/// frames, so swallowed errors logged via the throwable-carrying
/// `Logger.log` overloads are visible instead of silently dropped.
pub fn jul_render_throwable(ctx: &mut dyn NativeContext, t: ObjectRef) -> String {
    let cls = {
        let cid = ctx.class_id_of_object(t);
        ctx.class_name_of_id(cid)
            .unwrap_or_else(|| "java/lang/Throwable".to_string())
            .replace('/', ".")
    };
    let detail = match crate::lang_misc::throwable_message_or_npe_extended(ctx, t) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let mut out = if detail.is_empty() {
        cls
    } else {
        format!("{cls}: {detail}")
    };
    if let Ok(Some(Value::Object(Some(arr)))) = ctx.invoke_virtual(
        t,
        "getStackTrace",
        "()[Ljava/lang/StackTraceElement;",
        &[Value::Object(Some(t))],
    ) {
        // gc-common w12-b: every `toString()` below is bytecode; the array is
        // pinned across the loop and re-read per element.
        let arr_pin = ctx.pin_native_root(arr);
        let n = ctx.array_length(arr).min(15);
        for i in 0..n {
            let arr = ctx.read_native_pin(arr_pin, arr);
            if let Value::Object(Some(ste)) = ctx.get_array_element(arr, i) {
                if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke_virtual(
                    ste,
                    "toString",
                    "()Ljava/lang/String;",
                    &[Value::Object(Some(ste))],
                ) {
                    if let Some(frame) = ctx.read_string(s) {
                        out.push_str("\n\tat ");
                        out.push_str(&frame);
                    }
                }
            }
        }
        ctx.unpin_native_roots(arr_pin);
    }
    out
}

pub fn jul_level_tag(ctx: &mut dyn NativeContext, level_obj: Option<ObjectRef>) -> String {
    let level_name = level_obj
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_else(|| "INFO".to_string());
    match level_name.as_str() {
        "SEVERE" => "ERROR".to_string(),
        "WARNING" => "WARN".to_string(),
        other => other.to_string(),
    }
}

/// The throwable-carrying `java/util/logging/Logger.log` overloads:
/// `log(Level, String, Throwable)`, `log(Level, Supplier, Throwable)`, and
/// `log(Level, Throwable, Supplier)`. The real JDK builds a `LogRecord` and
/// routes through the (unwired) handler chain, so under the synthetic JUL
/// these records — and the THROWABLE they carry — were silently dropped.
/// JUnit's `ListenerRegistry.notifyEach` logs swallowed listener exceptions
/// exactly this way, so any such error was invisible. Identify the throwable
/// by `instanceof Throwable` and render it with a short stack trace.
///
/// **The level gate below is the twin of the one in
/// `native_jul_logger_log_supplier`, and is here for the same measurement.**
/// On a logger at `INFO`, HotSpot 25.0.3 evaluates neither
/// `log(Level.FINEST, throwable, supplier)` nor `log(Level.FINEST, supplier)`;
/// CratonVM `Compatible` evaluated and emitted both (measured 2026-08-11,
/// counting supplier invocations: HotSpot `0/0`, CratonVM `1/1`). Fixing one
/// of the pair and leaving the other is the exact "right in the members with
/// one implementation, wrong in the members with the other" shape this family
/// keeps producing, so both close together. `(Level, String, Throwable)` rides
/// the same native and gains the same gate, which is what its `(Level, String)`
/// sibling has always done.
pub fn native_jul_logger_log_throwable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // Gate BEFORE the supplier is resolved: not paying for a suppressed
    // supplier is the whole point of the overload. Shares the sibling
    // overloads' threshold walk rather than restating it — see the note on
    // `native_jul_logger_log_supplier`.
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    // A null `Supplier` throws here exactly as it does in the no-throwable
    // sibling — but this callback serves THREE descriptors and is never told
    // which one it was invoked under, so "the supplier slot is null" has to be
    // decided the same way the throwable is: by type.
    //
    // `(Level, Throwable, Supplier)` is the only one of the three whose slot 2
    // holds a `Throwable`, so a `Throwable` in slot 2 makes slot 3 the
    // supplier, and a null there is the NPE. Everything else is left alone,
    // and deliberately: measured, `log(SEVERE, (String) null, (Throwable)
    // null)` RETURNS — two nulls in the same two slots, and the opposite
    // verdict. Discriminating on the descriptor is not available; without the
    // type test this arm would have thrown on that legal call.
    if !jul_arg_is_null(args, 2) && jul_arg_is_null(args, 3) {
        if let Some(Value::Object(Some(o))) = args.get(2) {
            let o = *o;
            if jul_is_throwable(ctx, o) {
                return jul_throw_npe(JUL_NPE_NULL_SUPPLIER);
            }
        }
    }
    // Resolving the message may invoke Java supplier code and allocate. Keep
    // the receiver/level rooted while identifying the throwable so the later
    // record never receives an old moving-GC address.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let mut thrown_pin: Option<(usize, ObjectRef)> = None;
    let mut message_pin: Option<(usize, ObjectRef)> = None;
    // One native serves three descriptors — `(Level, String, Throwable)`,
    // `(Level, Supplier, Throwable)` and `(Level, Throwable, Supplier)` — and
    // the callback is handed only `args`, never the descriptor it was invoked
    // under, so the throwable has to be identified by type rather than by
    // position. `jul_is_throwable` walks the superclass chain by name for the
    // reason documented on it: the previous `class_id_by_name` +
    // `is_subclass` form silently classified the throwable as "not a
    // throwable" whenever that by-name lookup answered `None`, and the
    // argument was then dropped on the floor (the message slot was already
    // taken), costing the record its `thrown`.
    for slot in [2usize, 3usize] {
        if let Some(Value::Object(Some(o))) = args.get(slot) {
            let o = *o;
            if jul_is_throwable(ctx, o) {
                thrown_pin = Some((ctx.pin_native_root(o), o));
            } else if message_pin.is_none() {
                message_pin = Some((ctx.pin_native_root(o), o));
            }
        }
    }
    if jul_dbg_enabled() {
        eprintln!(
            "[JUL-DBG] log(Level,?,?) native reached: message_arg={} thrown_arg={}",
            message_pin.is_some(),
            thrown_pin.is_some()
        );
    }
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p))
        .or_else(|| match (message_pin, thrown_pin) {
            (Some((a, _)), Some((b, _))) => Some(a.min(b)),
            (Some((a, _)), None) => Some(a),
            (None, Some((b, _))) => Some(b),
            (None, None) => None,
        });
    // The `Supplier` overloads carry the message lazily. Resolve it to a real
    // `String` so the published record's `getMessage()` is the text HotSpot
    // would report; a plain `String` message is passed through untouched so
    // the RAW pattern survives.
    let message_obj = match message_pin {
        Some((pin, obj)) => {
            let o = ctx.read_native_pin(pin, obj);
            if ctx.read_string(o).is_some() {
                Some(o)
            } else {
                // Strict, for the reason given on `jul_resolve_msg_strict`: a
                // supplier that throws must propagate rather than be rendered
                // as its own `toString()`. Unpin before propagating.
                let text = match jul_resolve_msg_strict(ctx, o) {
                    Ok(t) => t.unwrap_or_else(|| "null".to_string()),
                    Err(e) => {
                        if let Some(base) = base_pin {
                            ctx.unpin_native_roots(base);                        }
                        return Err(e);
                    }
                };
                Some(ctx.create_string(&text))            }        }
        None => None,    };
    // `jul_resolve_msg`/`create_string` above allocate: re-derive the rest.
    let this = this_pin.map(|(pin, obj)| ctx.read_native_pin(pin, obj));
    let level_obj = level_pin.map(|(pin, obj)| ctx.read_native_pin(pin, obj));
    let thrown = thrown_pin.map(|(pin, obj)| ctx.read_native_pin(pin, obj));
    let result = jul_log_parameterized(ctx, this, level_obj, message_obj, None, thrown);
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    result
}

/// `java/util/logging/Logger.log(Level, Supplier<String>)` — message-supplier
/// overload with no throwable. Resolve the supplier and emit (otherwise the
/// synthetic JUL drops it, since the real LogRecord/handler path isn't wired).
///
/// **Two defects fixed here, both measured 2026-08-11 against HotSpot 25.0.3
/// with a `Handler` of the caller's own and a counting supplier.**
///
/// 1. *The record reached no handler.* This was the one `log` overload that
///    wrote the console sink WITHOUT first fanning the record out to the
///    logger's installed `Handler`s, so an application that installed one saw
///    seven of the eight `log` overloads — right in the members with one
///    implementation, wrong in the members with the other:
///
///    ```text
///    HotSpot          [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L, INFO:sup]
///    CratonVM compat  [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L]
///    ```
///
/// 2. *The level was never consulted, so the supplier ALWAYS ran.* Measured on
///    a logger at `FINE`, `log(Level.FINEST, supplier)` evaluated the supplier
///    and emitted a console line; HotSpot does neither. This is the worse half:
///    not evaluating a filtered-out supplier is the entire reason the overload
///    exists, and a caller whose supplier has a cost (or a side effect) paid it
///    on every suppressed call. The `(Level, String)` sibling has always gated
///    correctly, so this was also an inconsistency inside one family.
///
/// Both close by routing through the two pieces that already existed rather
/// than by growing a second implementation: [`native_jul_logger_is_loggable`]
/// is the level gate the sibling overloads use, and [`jul_log_parameterized`]
/// is the publish-then-fall-back-to-console path the other seven use.
///
/// **`Compatible`-mode effect, stated per case, because this is a
/// Compatible-visible behaviour change.** A logger with NO handler is
/// byte-for-byte unchanged: `jul_log_parameterized`'s console fallback formats
/// `{tag} [{name}] {text}`, the exact string this function used to build. A
/// logger WITH a handler now delivers the record and suppresses the duplicate
/// console line — which is what the other seven overloads already do. A
/// filtered-out call now emits nothing and evaluates nothing, where it used to
/// emit a console line; that line was not HotSpot's and not the sibling
/// overloads'.
///
/// Recorded in docs/feature-designs/native-dispatch-architecture.md;
/// found by W7-22 §4.1, which measured the fan-out half only.
pub fn native_jul_logger_log_supplier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // GATE BEFORE RESOLVING. `native_jul_logger_is_loggable` reads args[0] and
    // args[1] and ignores the rest, so it takes this call's own args — and
    // sharing it rather than restating the threshold walk is deliberate: that
    // walk is 60 lines of Level-shape fallbacks (real `config.levelObject`,
    // synthetic slot 1, the ancestor override table) and a second copy would
    // answer differently the first time one of those shapes changed.
    if matches!(
        native_jul_logger_is_loggable(ctx, args)?,
        Some(Value::Int(0))
    ) {
        return Ok(None);
    }
    // AFTER the gate, not before it. HotSpot's body is
    // `if (!isLoggable(level)) return; new LogRecord(level, msgSupplier.get())`
    // — so a null supplier on a SUPPRESSED level returns quietly and only
    // throws once the record is actually going to be built. Checking before
    // the gate would throw where the JDK returns.
    if jul_arg_is_null(args, 2) {
        return jul_throw_npe(JUL_NPE_NULL_SUPPLIER);
    }
    // GC SAFETY: resolving the message INVOKES the supplier — arbitrary Java
    // that allocates — so the receiver and the level can both move across it.
    // Same pin/re-derive discipline as `native_jul_logger_log_throwable`,
    // which resolves a supplier for the throwable-carrying overloads.
    let this_pin = this.map(|o| (ctx.pin_native_root(o), o));
    let level_pin = level_obj.map(|o| (ctx.pin_native_root(o), o));
    let base_pin = this_pin
        .map(|(p, _)| p)
        .or_else(|| level_pin.map(|(p, _)| p));
    // A supplier that THROWS propagates; `jul_resolve_msg` used to catch it
    // and log the supplier's `toString()` instead. Unpin on the way out —
    // an early `?` here would strand the roots pinned above.
    let resolved = match args.get(2) {
        Some(Value::Object(Some(o))) => jul_resolve_msg_strict(ctx, *o),
        _ => Ok(Some(String::new())),
    };
    let msg = match resolved {
        Ok(m) => m,
        Err(e) => {
            if let Some(base) = base_pin {
                ctx.unpin_native_roots(base);            }
            return Err(e);
        }
    };
    // `Supplier.get()` returning null is a null LogRecord message, which
    // `SimpleFormatter` renders as the four characters `null` — measured on
    // HotSpot as `SEVERE: null`. Rendering it here keeps the FORMATTED output
    // identical; `LogRecord.getMessage()` on such a record is not reachable
    // through this native and is not adjudicated.
    let msg = msg.unwrap_or_else(|| "null".to_string());
    let message_obj = ctx.create_string(&msg);
    let message_pin = ctx.pin_native_root(message_obj);
    // `jul_resolve_msg`/`create_string` both allocate: re-derive everything.
    let this = this_pin.map(|(pin, obj)| ctx.read_native_pin(pin, obj));
    let level_obj = level_pin.map(|(pin, obj)| ctx.read_native_pin(pin, obj));
    let message_obj = ctx.read_native_pin(message_pin, message_obj);
    let result = jul_log_parameterized(ctx, this, level_obj, Some(message_obj), None, None);
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    } else {
        ctx.unpin_native_roots(message_pin);
    }
    result
}

/// `java/util/logging/Logger.log(LogRecord)` — the overload many wrappers
/// (incl. JUnit's `LoggerFactory$DelegatingLogger`) build a `LogRecord`
/// directly and call. The real JDK routes it through the (unwired) handler
/// chain, so the record — and any THROWABLE it carries — was silently
/// dropped, hiding errors callers log-and-swallow. Read `level`/`message`/
/// `thrown` off the record by field name and emit.
///
/// **The level gate is the first statement of the real body and was missing
/// here.** JDK 25's `Logger.log(LogRecord record)` opens with
/// `if (!isLoggable(record.getLevel())) return;` — so a `FINEST` record handed
/// to a logger at `INFO` is dropped, not published. Without the gate this
/// overload was the one member of the eight-overload `log` family that
/// published unconditionally: the same "right in the members with one
/// implementation, wrong in the members with the other" split
/// [`native_jul_logger_log_supplier`] documents, one overload along. It shares
/// [`native_jul_logger_is_loggable`] rather than restating the threshold walk,
/// for the reason stated there — that walk is 60 lines of `Level`-shape
/// fallbacks and a second copy would drift the first time one shape changed.
/// It reads no field the body below does not already read, and allocates
/// nothing, so it sits above the pin/publish block.
/// docs/feature-designs/native-dispatch-architecture.md §7.
pub fn native_jul_logger_log_record(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // `log((LogRecord) null)` is `record.getLevel()` on HotSpot — the level
    // gate documented above IS the dereference, so the null record throws
    // before anything is published. This body returned quietly instead.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_RECORD);
    }
    let rec = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // The record's own level is the one to gate on — `log(LogRecord)` takes no
    // `Level` argument, so `args` cannot be forwarded to `is_loggable` the way
    // the `(Level, …)` overloads forward theirs. A record with no readable
    // `level` is left alone: `is_loggable` would score it as the INFO default
    // and could suppress a record whose level we simply failed to read.
    // Read out of the `if let` scrutinee on purpose: a method call there keeps
    // its receiver reborrow alive for the whole block, and the gate below needs
    // `ctx` mutably.
    let record_level = match ctx.get_field_by_name(rec, "level") {
        Value::Object(Some(level)) => Some(level),
        _ => None,
    };
    if let (Some(logger), Some(level)) = (this, record_level) {
        let gate_args = [Value::Object(Some(logger)), Value::Object(Some(level))];
        if matches!(
            native_jul_logger_is_loggable(ctx, &gate_args)?,
            Some(Value::Int(0))
        ) {
            return Ok(None);
        }
    }
    // Real `Logger.log(LogRecord)` fans the record out to the logger's own
    // handlers and then its ancestors'. Do that first — this is also the tail
    // of the real-JDK `throwing`/`entering` bytecode (via `doLog`), so a
    // record built by the JDK itself (carrying `thrown`, source class/method)
    // reaches an installed Handler instead of being flattened into a console
    // line. Only fall back to the console sink when nothing took it.
    let (this, rec) = match this {
        Some(logger) => {
            let this_pin = ctx.pin_native_root(logger);
            let rec_pin = ctx.pin_native_root(rec);
            let delivered = publish_existing_record_to_jul_handlers(ctx, logger, rec);
            // The publication is GC-capable and the console fallback below
            // reuses both references: re-derive them from their pins BEFORE
            // releasing the pin stack.
            let logger = ctx.read_native_pin(this_pin, logger);
            let rec = ctx.read_native_pin(rec_pin, rec);
            ctx.unpin_native_roots(this_pin);
            // gci i25/jl: `true` also for a record the logger's filter
            // rejected; a throwing filter propagates (HotSpot's `log`).
            if delivered? {
                return Ok(None);
            }
            (Some(logger), rec)
        }
        None => (None, rec),
    };
    // gci i31/hp: no handler took it -- HotSpot drops it (see
    // `jul_console_sink_applies`).
    if !jul_console_sink_applies(&*ctx) {
        return Ok(None);
    }
    let level_obj = match ctx.get_field_by_name(rec, "level") {
        Value::Object(o) => o,
        _ => None,
    };
    let tag = jul_level_tag(ctx, level_obj);
    let message = match ctx.get_field_by_name(rec, "message") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let thrown = match ctx.get_field_by_name(rec, "thrown") {
        Value::Object(Some(t)) => Some(t),
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    match thrown {
        Some(t) => {
            let r = jul_render_throwable(ctx, t);
            if message.is_empty() {
                crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {r}"));
            } else {
                crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message}\n{r}"));
            }
        }
        None => crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message}")),
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// The `java.util.logging` null axis
// ---------------------------------------------------------------------------
//
// MEASURED against HotSpot 25.0.3+9-LTS, one probe per method, printing the
// exception class AND its exact `getMessage()` — see
// docs/feature-designs/concurrent-gc-maturation.md
// for the full table. **The axis is not one rule.** In the same package, on the
// same argument:
//
//   * `Logger.setLevel(null)`, `Logger.setFilter(null)`, `Logger.removeHandler
//     (null)`, `Logger.getLogger(name, null)`, `Logger.severe((String) null)`,
//     `Logger.logp(level, null, null, msg)`, `Logger.entering(null, null)` and
//     every `LogRecord` setter except `setLevel`/`setInstant` RETURN NORMALLY;
//   * `Logger.addHandler(null)`, `Logger.setParent(null)`, a null `Level` on
//     any `log`/`logp`/`isLoggable` overload, a null `Supplier` on the two
//     supplier overloads, a null name on `Logger.getLogger`/`Level.parse`, a
//     null key on `LogManager.getLogger`/`getProperty`, a null `Logger` on
//     `LogManager.addLogger` and a null record on `Logger.log(LogRecord)`
//     THROW.
//
// A blanket "JUL rejects null" rule would break the first list; a blanket "JUL
// tolerates null" rule is what the second list was, and is the defect these
// constants close. HANDOFF-20260814 §5 names this exact family as the one where
// generalising from three rows cost real time. Only the rows below are
// adjudicated — nothing here licenses a null check on a method not listed.

/// HotSpot's helpful-NPE text for a null `Level` reaching a `Logger` entry
/// point whose real body opens on `level.intValue()`.
pub const JUL_NPE_NULL_LEVEL: &str =
    "Cannot invoke \"java.util.logging.Level.intValue()\" because \"level\" is null";

/// HotSpot's text for a null `Supplier` on `log(Level, Supplier)` /
/// `log(Level, Throwable, Supplier)`. The JDK's parameter is named
/// `msgSupplier`, and the helpful-NPE quotes the parameter name.
pub const JUL_NPE_NULL_SUPPLIER: &str =
    "Cannot invoke \"java.util.function.Supplier.get()\" because \"msgSupplier\" is null";

/// HotSpot's text for a null key reaching the `ConcurrentHashMap` behind
/// `Logger.getLogger` / `LogManager.getLogger` / `LogManager.getProperty`.
/// The NPE is raised by the map, not by JUL, which is why all three read
/// identically and why none of them names a `java.util.logging` type.
pub const JUL_NPE_NULL_KEY: &str = "Cannot invoke \"Object.hashCode()\" because \"key\" is null";

/// HotSpot's text for `Logger.log((LogRecord) null)`.
pub const JUL_NPE_NULL_RECORD: &str =
    "Cannot invoke \"java.util.logging.LogRecord.getLevel()\" because \"record\" is null";

/// HotSpot's text for `LogManager.addLogger(null)`.
pub const JUL_NPE_NULL_LOGGER: &str =
    "Cannot invoke \"java.util.logging.Logger.getName()\" because \"logger\" is null";

/// HotSpot's text for `Level.parse(null)`. `Level.parse` opens with
/// `name.length()`, so the helpful-NPE names `String.length()` — NOT a
/// hand-written "Name cannot be null", which is what this file answered
/// before and which no HotSpot build produces.
pub const JUL_NPE_NULL_LEVEL_NAME: &str = "Cannot invoke \"String.length()\" because \"name\" is null";

/// Raise the JUL null-argument NPE with HotSpot's own message text.
pub fn jul_throw_npe(message: &str) -> MethodCallResult {
    Err(RuntimeError::NullPointerException {
        message: Some(message.to_string()),
    }
    .into())
}

/// Is `args[idx]` absent or a null reference?
///
/// Deliberately distinguishes "no such argument" from "argument present and
/// null" nowhere: a native invoked under a descriptor that declares the slot
/// always receives it, and a missing slot means the callback was reached under
/// a descriptor it does not serve — in which case refusing is still right.
pub fn jul_arg_is_null(args: &[Value], idx: usize) -> bool {
    !matches!(args.get(idx), Some(Value::Object(Some(_))))
}

/// Resolve a `Logger.log` message argument that may be a `String` or a
/// `Supplier<String>`, with the real JDK's THREE outcomes rather than the
/// one-string-or-empty answer [`jul_resolve_msg`] gives.
///
/// `Ok(Some(s))` — resolved. `Ok(None)` — the supplier returned null, which the
/// JDK stores as a null `LogRecord` message and `SimpleFormatter` renders as
/// the four characters `null`. `Err(..)` — the supplier itself threw, and the
/// JDK lets that propagate to the caller of `log`; [`jul_resolve_msg`] caught
/// it and fell through to `toString()`, so a `Supplier` that blew up was
/// logged as `com.example.Sup@1a2b3c` and the exception vanished. Measured:
/// HotSpot propagates `IllegalStateException: supplier blew up`, this file
/// printed `JulSup$Boom@53c`.
pub fn jul_resolve_msg_strict(
    ctx: &mut dyn NativeContext,
    o: ObjectRef,
) -> Result<Option<String>, MethodCallFailed> {
    if let Some(s) = ctx.read_string(o) {
        return Ok(Some(s));
    }
    let supplier_class = ctx.class_id_by_name("java/util/function/Supplier");
    let object_class = ctx.class_id_of_object(o);
    if supplier_class
        .is_some_and(|supplier| object_class == supplier || ctx.is_subclass(object_class, supplier))
    {
        // NOT swallowed: `?` on purpose. This is the whole point of the strict
        // variant.
        // gc-common w12-b: `o` is pinned across `get()` (bytecode) and re-read
        // for the non-String fallback below.
        let o_pin = ctx.pin_native_root(o);
        let got = ctx.invoke_virtual(o, "get", "()Ljava/lang/Object;", &[]);
        let o = ctx.read_native_pin(o_pin, o);
        ctx.unpin_native_roots(o_pin);
        return match got? {
            Some(Value::Object(Some(r))) => Ok(Some(match ctx.read_string(r) {
                Some(s) => s,
                // `get()` answered a non-`String`, which generics make
                // unreachable from javac-compiled code and which nothing
                // measured produces. Keep [`jul_resolve_msg`]'s historical
                // answer for it rather than changing an unadjudicated row in
                // the same edit as three adjudicated ones.
                None => jul_resolve_msg(ctx, o),
            })),
            // `Supplier.get()` returned null — a null message, not an empty
            // one, and not a `toString()` of the supplier.
            _ => Ok(None),
        };
    }
    // Not a `Supplier` and not a `String`: keep the historical `toString()`
    // rendering rather than inventing a refusal for a shape no measurement
    // covers.
    Ok(Some(jul_resolve_msg(ctx, o)))
}

/// `java/util/logging/Logger.isLoggable(Level)Z`.
///
/// Our synthetic Logger objects carry a null `level` field and have no
/// parent/root Logger chain, so the real-JDK `isLoggable` bytecode
/// (which walks `getEffectiveLevel()`) returns `false` for *every*
/// level. JULI's `DirectJDKLog.log()` gates on
/// `if (logger.isLoggable(level))` before emitting — so a false return
/// here silently drops every Tomcat log line (the "Bootstrap version
/// prints nothing" symptom). Mirror the JDK default: the root logger
/// is INFO, so anything at INFO or higher (INTvalue >= 800) is
/// loggable, and FINE/FINER/FINEST are not.
/// The `intValue()` of a `Level` argument, through every layout one reaches
/// this VM in.
///
/// Extracted from [`native_jul_logger_is_loggable`] so the JBoss face
/// ([`native_jboss_logger_is_loggable`]) resolves a level exactly the same
/// way. The two differ only in their UNCONFIGURED default, and that difference
/// is real (see [`JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL`]) — everything before it
/// must not be allowed to drift, which is what a second copy would guarantee.
///
/// Three layouts, in order: a real `Level`'s `value` field; a `Level` whose
/// `name` resolves but whose `value` does not; and a fully synthetic `Level`
/// with no field names at all, whose layout is (name = slot 0, value = slot 1).
pub fn jul_requested_level_value(ctx: &dyn NativeContext, level: Option<ObjectRef>) -> Option<i32> {
    level
        .and_then(|o| match ctx.get_field_by_name(o, "value") {
            Value::Int(v) => Some(v),
            _ => None,
        })
        .or_else(|| {
            level
                .and_then(|o| match ctx.get_field_by_name(o, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                })
                .and_then(|n| jul_standard_level_value(&n))
        })
        .or_else(|| level.and_then(|o| synthetic_level_value(ctx, o)))
}

pub fn native_jul_logger_is_loggable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Logger.isLoggable(null)` is `level.intValue()` on HotSpot and therefore
    // an NPE — and because the `log(Level, Supplier)` / `log(Level, Throwable,
    // Supplier)` / `log(LogRecord)` overloads all gate through this function,
    // this one check is also what makes a null `Level` throw there, in the
    // right ORDER: HotSpot evaluates the level before touching the supplier,
    // so `log(null, validSupplier)` reports the level and never calls `get()`.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    let logger = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // `Level` exposes an int `value` field (e.g. WARNING=900, INFO=800,
    // CONFIG=700, FINE=500). Use the logger's configured level when it is a
    // real JUL Logger (LogCapture temporarily sets it to FINE); otherwise
    // mirror the JDK root default of INFO.
    let level_value = jul_requested_level_value(ctx, level_obj).unwrap_or(800);
    let configured_threshold = logger
        .and_then(|logger| match ctx.get_field_by_name(logger, "config") {
            Value::Object(Some(config)) => match ctx.get_field_by_name(config, "levelObject") {
                Value::Object(Some(level)) => match ctx.get_field_by_name(level, "value") {
                    Value::Int(value) => Some(value),
                    _ => None,                },
                _ => None,            },
            _ => None,
        })
        .or_else(|| {
            // Same for the logger's own configured level: a synthetic Logger
            // keeps it in slot 1, holding EITHER a `Level` object or the raw
            // int (the two `setLevel` registrations in `logging_shims` store
            // different shapes; the later one wins). Without this the
            // threshold stayed at the INFO default, so `setLevel(SEVERE)`
            // suppressed nothing.
            logger.and_then(|logger| {
                if ctx.object_num_fields(logger) <= LOGGER_FIELD_LEVEL {
                    return None;
                }
                match ctx.get_field(logger, LOGGER_FIELD_LEVEL) {
                    Value::Int(v) => Some(v),
                    Value::Object(Some(level)) => synthetic_level_value(ctx, level),
                    _ => None,
                }
            })
        })
        .unwrap_or(800);
    let vm = ctx.vm_identity();
    // The name read allocates and this is a per-log-site call, so skip it
    // entirely when the explicit-level table cannot answer — see
    // `no_explicit_logger_levels`.
    let threshold = if no_explicit_logger_levels(vm) {
        configured_threshold
    } else {
        logger
            .map(|logger| read_jul_logger_name(ctx, logger))
            .map(|name| jul_ancestor_explicit_level(vm, &name).unwrap_or(configured_threshold))
            .unwrap_or(configured_threshold)
    };
    Ok(Some(Value::Int(i32::from(jul_level_passes_threshold(
        level_value,
        threshold,
    )))))
}

/// HotSpot's `Logger.isLoggable` comparison:
/// `if (level.intValue() < levelValue || levelValue == offValue) return false;`.
///
/// gci i25/jl: the `offValue` half was missing, so a logger set to `OFF`
/// still admitted a record logged AT `Level.OFF` (`OFF >= OFF`), where
/// HotSpot admits nothing on an `OFF` logger.
pub fn jul_level_passes_threshold(level_value: i32, threshold: i32) -> bool {
    threshold != i32::MAX && level_value >= threshold
}

/// Read a synthetic `Level`'s int value: slot 1 directly, or slot 0's name
/// mapped through [`jul_standard_level_value`]. Used only after the by-name
/// lookups fail, i.e. for a receiver with no field names at all.
pub fn synthetic_level_value(ctx: &dyn NativeContext, level: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(level) > 1 {
        if let Value::Int(v) = ctx.get_field(level, 1) {
            return Some(v);
        }
    }
    if ctx.object_num_fields(level) > 0 {
        if let Value::Object(Some(s)) = ctx.get_field(level, 0) {
            if let Some(n) = ctx.read_string(s) {
                return jul_standard_level_value(&n);            }        }    }
    None
}

/// Map one of the 9 standard `java.util.logging.Level` names to its `int`
/// value. Mirrors `java.util.logging.Level`'s built-in constants.
pub fn jul_standard_level_value(name: &str) -> Option<i32> {
    Some(match name {
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

/// Look up the effective explicit level threshold for a logger name,
/// walking from the exact name up through its dotted-name ancestors to the
/// root ("") — mirroring real JUL's "inherit the nearest ancestor's
/// explicit level" semantics, which our flat name-keyed
/// `logger_explicit_levels` side table doesn't give us for free (a
/// descendant logger that never had `setLevel` called on it directly must
/// still see an ancestor's level, e.g. Spring Boot's
/// `JavaLoggingSystem.setLogLevel("org.springframework.boot", DEBUG)`
/// followed by a child logger's `.fine(...)` call).
/// Write (or clear, on `None`) one name-keyed level table entry.
///
/// Shared by the two tables — the effective level and the minimum level — so
/// the "a `None` REMOVES the row" rule cannot drift between them. Unlike
/// [`record_jul_logger_level`] this takes the name directly, so it can record
/// the ROOT (`""`): that function refuses an empty name because it derives it
/// from a Logger object, where an empty name also means "could not read one".
/// Here the caller has the real name in hand.
pub fn set_name_keyed_level(
    table: &'static Mutex<HashMap<String, i32>>,
    name: &str,
    value: Option<i32>,
) {
    let mut levels = table.lock().unwrap_or_else(|e| e.into_inner());
    match value {
        Some(value) => {
            levels.insert(name.to_string(), value);
        }
        None => {
            levels.remove(name);
        }
    }
}

/// `LoggerNode.effectiveMinLevel`, keyed by logger name — the SECOND threshold
/// `isLoggableLevel` checks, set only from
/// `LogContextInitializer.getMinimumLevel(name)`.
///
/// Separate from [`logger_explicit_levels`] because the two move
/// independently: `setLevel` changes the effective level and never the
/// minimum, and a provider that raises the minimum silences a category that
/// `setLevel` alone would have enabled.
pub fn logger_minimum_levels(vm: usize) -> &'static Mutex<HashMap<String, i32>> {
    // addr-keyed: a per_vm_table slot (VM identity keys; see per_vm_table).
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<String, i32>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// The nearest-ancestor minimum level for `logger_name`, or `None` when no
/// provider set one anywhere up the chain.
///
/// LIMITATION, stated because the difference is invisible with every provider
/// that exists today: real `LoggerNode` propagates `effectiveMinLevel` down
/// the tree as a running MAX (a child's is `max(parent's, its own)`), so a
/// deep node can be governed by a stricter ancestor even when a nearer one is
/// laxer. This walk takes the NEAREST entry instead. The two agree whenever a
/// provider's `getMinimumLevel` ignores the name — which both known providers
/// (`LogContextInitializer.DEFAULT` and Quarkus's `InitialConfigurator`, which
/// returns a constant) do.
pub fn jboss_ancestor_minimum_level(vm: usize, logger_name: &str) -> Option<i32> {
    let levels = logger_minimum_levels(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if levels.is_empty() {
        return None;
    }
    let mut candidate = logger_name;
    loop {
        if let Some(v) = levels.get(candidate) {
            return Some(*v);
        }
        candidate = match logger_name_parent(candidate) {
            Some(parent) => parent,
            None => return None,
        };
    }
}

/// True when NO logger anywhere has an explicit level, so
/// [`jul_ancestor_explicit_level`] is guaranteed to answer `None` for every
/// name and its caller need not read the receiver's name at all.
///
/// This is a fast path, not a shortcut. `isLoggable` is the method a logging
/// facade calls on every guarded log site — JRuby's
/// `JavaUtilLoggingLogger.isDebugEnabled()` is `logger.isLoggable(FINE)` — and
/// answering it costs a `read_jul_logger_name` (which allocates a Rust
/// `String` per call) plus a lock and a dotted-name walk. On the JBoss face
/// that method had been a constant `true`, so implementing it honestly puts
/// that cost on a path that had none. Almost every process configures no
/// levels at all, and for those this collapses back to one uncontended lock
/// and an `is_empty`.
///
/// The justification is the shape of the work, NOT a measurement. The
/// candidate workload (quarkus's JRuby/Asciidoctor class) swings 198-470 s
/// wall on this host across repeated runs of the SAME binary, so no A/B on it
/// can resolve a per-call cost — the first pair that looked like a 47%
/// regression was inside that band, and a later pair ran the other way by
/// more. Do not cite a number here without a workload whose variance is
/// smaller than the effect.
pub fn no_explicit_logger_levels(vm: usize) -> bool {
    logger_explicit_levels(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
}

pub fn jul_ancestor_explicit_level(vm: usize, logger_name: &str) -> Option<i32> {
    let levels = logger_explicit_levels(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut candidate = logger_name;
    loop {
        if let Some(v) = levels.get(candidate) {
            return Some(*v);
        }
        if candidate.is_empty() {
            return None;
        }
        candidate = match candidate.rfind('.') {
            Some(idx) => &candidate[..idx],
            None => "",
        };
    }
}

/// Record `logger.setLevel(level)` in the name-keyed explicit-level table.
///
/// EVERY `setLevel` registration must funnel through here (there are three
/// competing ones — `lib.rs`, `logging_shims::register_logging_natives` and
/// `register_slf4j_natives` — and which one wins the registry slot has changed
/// more than once). The table is what `isLoggable` consults first, so routing
/// all of them through it makes the answer independent of which slot shape the
/// winning `setLevel` happens to store.
pub fn record_jul_logger_level(ctx: &dyn NativeContext, logger: ObjectRef, level: Value) {
    let name = read_jul_logger_name(ctx, logger);
    if name.is_empty() {
        return;
    }
    let value = match level {
        // A `setLevel` that already reduced the Level to its int value.
        Value::Int(value) => Some(value),
        Value::Object(Some(level)) => match ctx.get_field_by_name(level, "value") {
            Value::Int(value) => Some(value),
            // Synthetic `Level`: `ensure_synthetic_class` mints UNNAMED
            // fields, so the by-name read above resolves nothing and this used
            // to record `None` — i.e. `setLevel(SEVERE)` REMOVED the entry and
            // suppressed nothing. Fall back to the synthetic layout
            // (name = slot 0, value = slot 1) the rest of this file reads.
            _ => synthetic_level_value(ctx, level),        },
        _ => None,
    };
    let mut levels = logger_explicit_levels(ctx.vm_identity())
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(value) = value {
        levels.insert(name, value);
    } else {
        levels.remove(&name);
    }
}

/// Publish to the real handler chain FIRST, and fall back to the native
/// console sink only when no handler accepted the record.
///
/// Order matters. `log_simple` used to run unconditionally *before* the
/// publish, so once a config file installed a real `ConsoleHandler` on the root
/// logger every line was printed TWICE — once by our sink and once by the
/// handler — where HotSpot prints it once. Measured on a plain-`LogManager`
/// program calling `readConfiguration()`: 2 lines per call before, 1 after,
/// matching HotSpot. It also removes a pre-existing duplicate under Tomcat
/// (stock-conf stdout drops 55 -> 6 lines; HotSpot's is 6).
///
/// `publish_to_jul_handlers_full` has always reported whether a handler took
/// the record (its doc even says the point is "so callers can keep the
/// console-sink fallback for loggers that have no handler chain at all"); the
/// result was simply discarded. This wires it up.
pub fn jul_convenience_with_console_fallback(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    jul_level: &str,
    console_tag: &str,
) -> MethodCallResult {
    // gc-common w12-b: the publish runs handler bytecode, so the console
    // fallback gets the receiver and message re-read through roots instead of
    // the entry `args` (which a collection does not rewrite).
    let mut scope = NativeHandleScope::new(ctx);
    let roots: Vec<Option<NativeHandle>> = args
        .iter()
        .map(|arg| match arg {
            Value::Object(Some(o)) => Some(scope.root(*o)),
            _ => None,
        })
        .collect();
    let delivered = publish_jul_convenience(&mut *scope, args, jul_level)?;
    // gci i31/hp: the sink only before the LogManager is initialized
    // (`jul_console_sink_applies`).
    if !delivered && jul_console_sink_applies(&*scope) {
        let args_now: Vec<Value> = args
            .iter()
            .zip(&roots)
            .map(|(arg, root)| match root {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => *arg,
            })
            .collect();
        log_simple(&mut *scope, &args_now, console_tag);
    }
    Ok(None)
}

pub fn native_jul_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "INFO", "INFO")
}
pub fn native_jul_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "WARNING", "WARN")
}
pub fn native_jul_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "SEVERE", "ERROR")
}
pub fn native_jul_logger_fine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_logger_fine_family(ctx, args, "FINE")
}
pub fn native_jul_logger_finer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_logger_fine_family(ctx, args, "FINER")
}
pub fn native_jul_logger_finest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_logger_fine_family(ctx, args, "FINEST")
}

/// `Logger.fine/finer/finest(String)`.
///
/// `finer`/`finest` used to share `fine`'s body and therefore reported
/// themselves as `FINE`, so a record emitted by `finest()` was published to
/// handlers at level 500 instead of 300 — a `FINEST`-only handler never saw
/// it, and a `FINE`-level one saw records it should have filtered out.
///
/// Console policy matches `native_jul_logger_logp`: quiet unless the logger's
/// configured (ancestor-inherited) level admits this level. Handlers are
/// always offered the record and apply their own `isLoggable` gate.
pub fn jul_logger_fine_family(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level_name: &str,
) -> MethodCallResult {
    // Handlers first; `log_simple` (which applies the ancestor-aware
    // `isLoggable` gate itself, so a logger with no configured fine level
    // stays console-quiet) only when nothing accepted the record — see
    // `jul_convenience_with_console_fallback` for why the order matters.
    jul_convenience_with_console_fallback(ctx, args, level_name, level_name)
}

/// Returns `true` when a real handler accepted the record, so the caller can
/// skip the native console echo and avoid printing the line twice.
pub fn publish_jul_convenience(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level_name: &str,
) -> Result<bool, MethodCallFailed> {
    // gci i26/rv (item J2): a NULL message is legal and still published --
    // HotSpot's `info(null)` is `log(INFO, null)`, a `new LogRecord(INFO,
    // null)` every handler receives (i25/jl fixed the same gap on
    // `log(Level, String)`). This used to answer "not delivered" for it, so
    // no handler saw the record and the console sink printed instead.
    let Some(Value::Object(Some(logger))) = args.first() else {
        return Ok(false);
    };
    let message = match args.get(1) {
        Some(Value::Object(m)) => *m,
        _ => None,
    };
    // gc-common w12-b: `resolve_standard_level` can run `Level.<clinit>`, so
    // the two argument references are pinned across it and re-read.
    let logger_pin = ctx.pin_native_root(*logger);
    let message_pin = message.map(|m| (ctx.pin_native_root(m), m));
    let level = resolve_standard_level(ctx, level_name);
    let logger = &ctx.read_native_pin(logger_pin, *logger);
    let message = message_pin.map(|(p, m)| ctx.read_native_pin(p, m));
    ctx.unpin_native_roots(logger_pin);
    let Some(level) = level else {
        return Ok(false);
    };
    // Gate on the real (ancestor-aware) effective level, matching the real
    // JDK's `Logger.info/warning/severe/fine(...)` convenience methods,
    // which all internally check `isLoggable` before publishing.
    let loggable = matches!(
        native_jul_logger_is_loggable(
            ctx,
            &[Value::Object(Some(*logger)), Value::Object(Some(level))]
        )?,
        Some(Value::Int(1))
    );
    if !loggable {
        return Ok(false);
    }
    // `native_jul_logger_is_loggable` reads fields only (not a GC point), so
    // `logger`, `level` and `message` are still current here.
    publish_to_jul_handlers_outcome(ctx, *logger, level, message, None, None, None, None)
        .map(JulPublish::disposed)
}

pub fn log_simple(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // This fallback channel predates the fix that made real
    // `ConsoleHandler.publish` actually produce visible output (a stale
    // `phases_early` stub silently discarded every record -- see
    // `apply_jul_config_entries`/the removed `ConsoleHandler` overrides);
    // it unconditionally surfaced every `info`/`warning`/`severe` call
    // regardless of the logger's configured level so *something* was
    // visible under `CapturedOutput`. Now that real delivery works and is
    // correctly level-gated, this unconditional emission leaks messages
    // tests explicitly assert are ABSENT (e.g.
    // `JavaLoggingSystemTests#noFile` logs "Hidden" while the root logger
    // is muted to SEVERE via `beforeInitialize`, then asserts the captured
    // output does NOT contain it). Gate on the same ancestor-aware
    // effective level so this channel's visibility matches what real JUL
    // would actually deliver.
    let jul_level_name = match level {
        "WARN" => "WARNING",
        "ERROR" => "SEVERE",
        // The fine-grained levels must be gated as THEMSELVES, not collapsed
        // into INFO — otherwise a `finest()` call is level-checked at 800 and
        // sails through any threshold at or below INFO.
        "SEVERE" | "WARNING" | "CONFIG" | "FINE" | "FINER" | "FINEST" => level,
        _ => "INFO",
    };
    // gc-common w12-b: the message and the logger's name are read BEFORE
    // `resolve_standard_level`, which can run `Level.<clinit>`, and the logger
    // is pinned across it (both used to be read afterwards, from `args`).
    let message_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let message = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    if let Some(logger) = this {
        let logger_pin = ctx.pin_native_root(logger);
        let level_obj = resolve_standard_level(ctx, jul_level_name);
        let logger = ctx.read_native_pin(logger_pin, logger);
        ctx.unpin_native_roots(logger_pin);
        if let Some(level_obj) = level_obj {
            let loggable = matches!(
                native_jul_logger_is_loggable(
                    ctx,
                    &[Value::Object(Some(logger)), Value::Object(Some(level_obj))]
                ),
                Ok(Some(Value::Int(1)))
            );
            if !loggable {
                return;
            }
        }
    }
    // `eprintln!` writes to the process's raw OS stderr, entirely bypassing
    // the Java-level `System.out`/`System.err` `PrintStream` objects — so
    // JUnit5's `OutputCaptureExtension` (which substitutes those objects via
    // `System.setOut`/`setErr`) never sees these `java.util.logging.Logger`
    // convenience-method records, even though a human watching the console
    // (or this process's raw stderr) sees them fine. Same bug class as the
    // Logback/commons-logging `emit_framework_log` fix — route through the
    // live (possibly test-substituted) stream instead.
    // See propertiesmigration-logfactory-oom-residual-FIXED.md.
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
}

pub fn native_jboss_logger_detach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments(vm).lock().unwrap_or_else(|e| e.into_inner());
    let prev = map.remove(&(this, key));
    Ok(Some(match prev {
        Some(addr) if addr != 0 => Value::Object(Some(unsafe { object_from_u64(addr) })),
        _ => Value::Object(None),
    }))
}

/// Lazily seed `parsed_log_properties` from the configuration file the
/// application itself named in `java.util.logging.config.file`.
///
/// WHY THIS IS NEEDED. `getProperty` is not only an application-facing API —
/// the JDK's own logging bytecode calls it. `StreamHandler.configure()` (run
/// from `new ConsoleHandler()`) does
/// `setLevel(manager.getLevelProperty(cname + ".level", Level.INFO))`. That
/// inner lookup lands on THIS native, which consulted a map that only
/// `readConfiguration(InputStream)` ever populated — so it returned null and
/// the handler silently fell back to the `Level.INFO` default. Under Tomcat,
/// whose JULI `ClassLoaderLogManager` reads the file itself and so never calls
/// `readConfiguration(InputStream)`, that meant a `ConsoleHandler` configured
/// `= ALL` came up at `INFO` and dropped every FINE/FINER record — the
/// "`org.apache.coyote.http2.level = FINEST` has no effect" symptom.
/// (`getProperty` called from *application* code returned the right value all
/// along, because that dispatches to JULI's own override — which is exactly
/// what made this so confusing to pin down.)
///
/// SCOPE. This only fills the property MAP, from the path the application
/// explicitly configured. It deliberately does NOT apply the entries
/// (`apply_jul_config_entries`) — instantiating handler/formatter classes
/// named by the file is the part worth being conservative about, and under a
/// real `LogManager` implementation that work is the manager's own job. So the
/// original "never instantiate from a filesystem config" posture is kept while
/// the JDK's own property reads start answering correctly.
pub fn ensure_config_file_properties_loaded(ctx: &mut dyn NativeContext) {
    // gc-common w13-b: the latch is per VM (`config_file_properties_loaded`);
    // it was a process-wide `OnceLock`, so a second VM never read its file.
    let vm = ctx.vm_identity();
    if *config_file_properties_loaded(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        return;
    }
    // Only the app-supplied path; no `$java.home/conf/logging.properties`
    // fallback, so a process that configures nothing keeps today's behaviour
    // exactly (empty map, CratonVM's own tracing sink governs visibility).
    //
    // Do NOT latch `LOADED` when the property isn't visible yet: JUL bootstraps
    // early and the very first `getProperty` can land before the system
    // properties are populated. Latching there would cache "no configuration"
    // forever and silently defeat the whole fix (which is exactly what the
    // first cut of this function did — `ConsoleHandler` still came up at INFO).
    let Some(path) = ctx
        .get_system_property("java.util.logging.config.file")
        .filter(|p| !p.trim().is_empty())
    else {
        return;
    };
    // A path exists: this is our one real attempt, success or not. The flag is
    // re-checked under its lock so two racing threads read the file once.
    {
        let mut loaded = config_file_properties_loaded(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *loaded {
            return;
        }
        *loaded = true;
    }
    let Ok(bytes) = std::fs::read(path.trim()) else {
        return;
    };
    let entries = crate::properties_sidetable::parse_properties_pub(&bytes);
    let mut props = parsed_log_properties(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (k, v) in entries {
        // A value already present came from an explicit
        // `readConfiguration(InputStream)` call, which is more specific than
        // the startup file — don't clobber it.
        props.entry(k).or_insert(v);
    }
}

pub fn native_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ensure_config_file_properties_loaded(ctx);
    // We never load a filesystem-backed configuration (see
    // `native_read_configuration_no_arg`), but a prior `readConfiguration
    // (InputStream)` call (Spring Boot's `JavaLoggingSystem`) does populate
    // `parsed_log_properties` -- consult it so e.g. a `Handler` subclass's
    // own constructor bytecode querying `LogManager.getProperty(cname +
    // ".level")` observes the same config `apply_jul_config_entries`
    // already applied directly.
    // A null key is an NPE from the backing map, not a miss. `getProperty` of
    // an ABSENT key still answers null on both VMs — that arm is below and is
    // measured; only the null key changes.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_KEY);
    }
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(key) = key else {
        return Ok(Some(Value::Object(None)));
    };
    let value = parsed_log_properties(ctx.vm_identity())
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    match value {
        Some(v) => Ok(Some(Value::Object(Some(ctx.create_string(&v))))),
        None => Ok(Some(Value::Object(None))),
    }
}

// ---------------------------------------------------------------------------
// GC integration — root scan + post-move remap for the cached ObjectRefs
// ---------------------------------------------------------------------------
//
// B4 fix: every side-table in this module stores Java object addresses as raw
// `u64` (singleton `LogManager`, the JUL + JBoss logger registries, the JBoss
// `LogContext` singleton, and the attachment side-table — whose *keys* are
// themselves `(receiver-addr, key-addr)` pairs). The old safety comments
// conflated "we never free these" with "these never move": a moving young GC
// relocates the underlying objects, after which `object_from_u64` rebuilds an
// `ObjectRef` over a stale (recycled) address — a use-after-free / wrong-object
// hazard, the exact bug class MEMORY.md documents for the classloader and
// lang_math caches.
//
// The fix mirrors `jboss_msc::{gc_scan_msc_service_roots,
// gc_update_msc_service_refs}`:
//   * `gc_scan_logmanager_roots` reports every cached object as a GC root so a
//     moving collector pins + relocates (rather than reclaims) it; and
//   * `gc_update_logmanager_refs` repoints every stored address (including BOTH
//     halves of each `attachments` key) to the relocated address afterwards.
//
// REGISTRATION REQUIRED (call sites are in files this agent does not own — see
// the existing MSC wiring for the exact shape):
//   * `vm/src/memory/roots.rs` (alongside step 19, after
//     `gc_scan_msc_service_roots`):
//         cratonvm_native_builtins::logmanager::gc_scan_logmanager_roots(&mut roots);
//   * `vm/src/memory/gc.rs` (alongside step 19, after
//     `gc_update_msc_service_refs`):
//         cratonvm_native_builtins::logmanager::gc_update_logmanager_refs(pointer_map);
// Until both are wired the scan/remap are inert (no behavior change) but the
// stale-pointer hazard remains — they MUST be registered to close B4.

/// GC root scan for every Java object cached by this module's side-tables.
/// Companion remap is [`gc_update_logmanager_refs`]. Reports the singleton
/// `LogManager`, both logger registries, the JBoss `LogContext` singleton,
/// every registered configuration listener, and every attachment receiver /
/// key / value so a moving collector relocates
/// (rather than reclaims) them. Uses blocking locks that are never held across
/// a Java allocation, so the allocating thread cannot self-deadlock here.
pub fn gc_scan_logmanager_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    scan_logmanager_registry_roots(vm, out);
    // gc-common w12-b / gci i24/lk: the handlers of the name-keyed
    // `addHandler` rows. `publish_jul_handlers_src` dereferences them, so they
    // are roots while their row exists; a row goes with its owner Logger
    // (`gc_sweep_logging_rows`), as a `config.handlers` element would.
    for (_, handler) in named_handler_row_pairs(vm) {
        // SAFETY: a row's handler address was stored from a live ObjectRef and
        // is re-addressed by `gc_update_logmanager_refs` after every move.
        out.push(unsafe { object_from_u64(handler as u64) });
    }
    // gc-common w14-d: the VALUES of the per-object JUL rows in
    // `logging_shims.rs` (handler lists, parents, filters, ErrorManagers).
    // They used to be JNI global roots; their keys are weak and swept by
    // `gc_sweep_logging_rows`.
    crate::logging_shims::gc_scan_jul_side_rows(vm, out);
}

/// gci i23/ep (`common-w14d-proposal-ephemeron-native-side-tables`, the
/// Generational stop-the-world stage; switch `CRATONVM_GEN_SIDE_ROW_EPHEMERON`,
/// default OFF, read by the caller, `vm/src/memory/native_roots.rs`
/// `scan_logmanager`): [`gc_scan_logmanager_roots`] with the per-object JUL
/// rows as ephemerons (`logging_shims::gc_scan_jul_side_rows_ephemeron`).
/// Every registry root goes to `out` unchanged; a per-object value
/// `deferrable` accepts becomes the pin `(key, value)` in `pins`; every other
/// per-object value goes to `row_values`, which the caller roots.
///
/// The Tomcat JULI registries (loader-keyed) are NOT made ephemerons: they
/// stand for `ClassLoaderLogManager.classLoaderLoggers`, a `WeakHashMap`,
/// whose value is strong while its loader lives and which HotSpot itself
/// does not collect when the value reaches its key. JULI's undeploy path
/// (`removeHandler`) empties them, as it does on HotSpot.
pub fn gc_scan_logmanager_roots_ephemeron(
    vm: usize,
    out: &mut Vec<ObjectRef>,
    row_values: &mut Vec<ObjectRef>,
    deferrable: &dyn Fn(usize) -> bool,
    pins: &mut Vec<(usize, usize)>,
) {
    scan_logmanager_registry_roots(vm, out);
    // gci i24/lk: a name-keyed handler row stands for an element of its
    // owner Logger's `config.handlers`, so it is an ephemeron of the owner
    // exactly like the identity-keyed handler list below: a deferrable
    // handler is pinned to its owner, any other is rooted.
    for (owner, handler) in named_handler_row_pairs(vm) {
        if deferrable(handler) {
            pins.push((owner, handler));
        } else {
            // SAFETY: as in `gc_scan_logmanager_roots`.
            row_values.push(unsafe { object_from_u64(handler as u64) });
        }
    }
    crate::logging_shims::gc_scan_jul_side_rows_ephemeron(vm, out, row_values, deferrable, pins);
}

/// Every `(owner, handler)` row of `vm`'s name-keyed [`logger_handlers`]
/// table, as addresses, copied out under its lock (gci i24/lk). A row with a
/// 0 half (never filed) is skipped.
pub fn named_handler_row_pairs(vm: usize) -> Vec<(usize, usize)> {
    logger_handlers(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .flat_map(|rows| rows.iter().copied())
        .filter(|&(owner, handler)| owner != 0 && handler != 0)
        .map(|(owner, handler)| (owner as usize, handler as usize))
        .collect()
}

/// Every root of [`gc_scan_logmanager_roots`] except the per-object JUL rows
/// (gci i23/ep: split out unchanged, so the ephemeron scan shares it).
pub fn scan_logmanager_registry_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    // SAFETY (all `object_from_u64` calls below): the addresses were produced
    // by `as_ptr()` on live ObjectRefs allocated by this process's heap and
    // stored under these locks; reporting them as roots is exactly what keeps
    // them live across a moving collection.
    let mut push_addr = |addr: u64| {
        if addr != 0 {
            out.push(unsafe { object_from_u64(addr) });
        }
    };

    if let Some(addr) = *singleton_cell(vm).lock().unwrap_or_else(|e| e.into_inner()) {
        push_addr(addr);
    }
    if let Some(addr) = *jboss_log_context_singleton(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        push_addr(addr);
    }
    for &addr in logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        // gci i25/jl: a HANDLE row (weak registry) is no root; the VM's
        // handle table tracks it.
        if !registry_row_is_handle(addr) {
            push_addr(addr);
        }
    }
    for &addr in tomcat_juli_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        push_addr(addr);
    }
    for handlers in tomcat_juli_root_handler_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        for &addr in handlers {
            push_addr(addr);
        }
    }
    for &addr in jboss_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        push_addr(addr);
    }
    // gc-common w12-b's name-keyed `addHandler` table is no longer scanned
    // here: since gci i24/lk its rows belong to their owner Logger (weak), and
    // the two public scans report their handlers (`named_handler_row_pairs`).
    // Configuration listeners are held ONLY by this table between
    // `addConfigurationListener` and the next `readConfiguration` — a caller
    // that registers a lambda inline keeps no other reference to it.
    for &addr in config_listeners(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        push_addr(addr);
    }
    // The attachment table keys ON object addresses (receiver, AttachmentKey)
    // and stores the attached value address — all three are live Java objects
    // and must be rooted (the keys too, else the AttachmentKey decays and the
    // post-move key remap cannot find its new address).
    for (&(this, key), &value) in attachments(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        push_addr(this);
        push_addr(key);
        push_addr(value);
    }
}

/// Post-GC remap for every cached ObjectRef (companion to
/// [`gc_scan_logmanager_roots`]). After a moving collection the cached objects
/// relocate; repoint every stored address — including BOTH halves of each
/// `attachments` key — to its new location so later `object_from_u64`
/// reconstructions resolve to the live object instead of a recycled slot.
pub fn gc_update_logmanager_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    // Map an old address to its relocated address, leaving it untouched when the
    // collector did not move it (not every object relocates in a young GC).
    let remap = |addr: u64| -> u64 {
        if addr == 0 {
            return 0;
        }
        match pointer_map.get(&(addr as usize)) {
            Some(&new_addr) => {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                new_addr as u64
            }
            None => addr,
        }
    };
    let remap_slot = |slot: &mut Option<u64>| {
        if let Some(addr) = slot.as_mut() {
            *addr = remap(*addr);
        }
    };

    remap_slot(&mut singleton_cell(vm).lock().unwrap_or_else(|e| e.into_inner()));
    remap_slot(
        &mut jboss_log_context_singleton(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    for addr in logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        // gci i25/jl: a HANDLE row is remapped by the VM's handle table.
        if !registry_row_is_handle(*addr) {
            *addr = remap(*addr);
        }
    }
    // gc-common w14-d: the Tomcat JULI registries are keyed by the context
    // loader's address, so a moved loader's rows are re-keyed as well.
    {
        let mut registry = tomcat_juli_logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        rekey_moved_address_rows(
            &mut *registry,
            pointer_map,
            |key: &(usize, String)| key.0,
            |key: &(usize, String), new_loader: usize| (new_loader, key.1.clone()),
        );
        for addr in registry.values_mut() {
            *addr = remap(*addr);
        }
    }
    {
        let mut registry = tomcat_juli_root_handler_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        rekey_moved_address_rows(
            &mut *registry,
            pointer_map,
            |key: &usize| *key,
            |_: &usize, new: usize| new,
        );
        for handlers in registry.values_mut() {
            for addr in handlers {
                *addr = remap(*addr);
            }
        }
    }
    for addr in jboss_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
    }
    // gci i24/lk: both halves of every name-keyed handler row (the owner
    // Logger and the handler).
    for handlers in logger_handlers(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        for (owner, handler) in handlers.iter_mut() {
            *owner = remap(*owner);
            *handler = remap(*handler);
        }
    }
    for addr in config_listeners(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter_mut()
    {
        *addr = remap(*addr);
    }
    // Rebuild the attachment table: both the `(this, key)` key addresses and
    // the value address can relocate, so we cannot mutate values in place —
    // collect, remap key+value, and reinsert under the relocated key.
    {
        let mut map = attachments(vm).lock().unwrap_or_else(|e| e.into_inner());
        if !map.is_empty() {
            let rebuilt: HashMap<AttachKey, u64> = map
                .drain()
                .map(|((this, key), value)| ((remap(this), remap(key)), remap(value)))
                .collect();
            *map = rebuilt;
        }
    }
    // gc-common w14-d: the per-object JUL rows (keys re-keyed, values
    // re-addressed).
    crate::logging_shims::gc_update_jul_side_rows(vm, pointer_map);
}

/// Drop `vm`'s logging rows whose KEY object did not survive a collection,
/// and return how many went (gc-common w14-d,
/// `common-w13b-jul-identity-keyed-rows-outlive-their-keys-FIXED-20260923`):
///
/// * the per-object JUL rows of `logging_shims.rs` (a Logger's handler list,
///   parent and filter, a Handler's `ErrorManager`, a FileHandler's
///   filename), keyed by the Logger / Handler, whose values are roots; and
/// * the Tomcat JULI logger and root-handler registries, keyed by the
///   context class loader, so an undeployed webapp's rows go with its loader;
/// * (gci i24/lk) the name-keyed `addHandler` rows, each judged by its OWNER
///   Logger (`sweep_dead_owner_rows`), and the name-keyed filter rows of
///   `logging_shims.rs`, likewise.
///
/// Other VMs' rows are never judged. The `is_live` contract is
/// `lib.rs::gc_sweep_locale_rows`'s: `is_live(addr)` answers for a key's
/// address as the table holds it -- the PRE-remap address inside a
/// stop-the-world epilogue (the caller folds the collection's pointer map in,
/// and [`gc_update_logmanager_refs`] re-keys the survivors afterwards), or a
/// pure "not in a span this reclamation freed" range test. It is evaluated
/// with no table lock held ([`sweep_dead_address_keys`]).
///
/// Called by the VM next to `gc_sweep_locale_rows` / `gc_sweep_tls_rows`:
/// after every stop-the-world collection
/// (`gc_and_alloc::process_references_after_gc`) and from both out-of-pause
/// reclamation hooks in `vm/src/memory/addr_keyed.rs`. Without it a dead key's
/// row outlived it -- rooting its value for the life of the VM -- and a later
/// object at the same address inherited the row.
pub fn gc_sweep_logging_rows(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    let loggers = sweep_dead_address_keys(
        tomcat_juli_logger_registry(vm),
        |key: &(usize, String)| key.0,
        is_live,
    );
    let root_handlers = sweep_dead_address_keys(
        tomcat_juli_root_handler_registry(vm),
        |key: &usize| *key,
        is_live,
    );
    // gci i24/lk: a name-keyed `addHandler` row goes with its owner Logger.
    let named_handlers = sweep_dead_owner_rows(logger_handlers(vm), is_live);
    loggers
        + root_handlers
        + named_handlers
        + crate::logging_shims::gc_sweep_jul_side_rows(vm, is_live)
}

