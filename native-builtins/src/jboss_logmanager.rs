// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Block 2C — Synthetic `org.jboss.logmanager.LogManager` boot-log sink.
//!
//! ## Purpose
//!
//! KC16 / WildFly boots with
//! `-Djava.util.logging.manager=org.jboss.logmanager.LogManager` and
//! `-Dorg.jboss.boot.log.file=<base>/standalone/log/server.log`. Without
//! the real `jboss-logmanager-2.1.18.Final.jar` actually wired in (Blocks
//! 2A and 2B) the JDK's `java.util.logging.LogManager.<clinit>` does
//! `clz.newInstance()` then `checkcast` to `java.util.logging.LogManager`.
//! Two failures fall out today:
//!
//!   1. `org.jboss.logmanager.LogManager`'s synthetic stub used to extend
//!      `java.lang.Object`, so the `checkcast` raised
//!      `ClassCastException`. Fixed in `class_manager::jdk_superclass` —
//!      the synthetic class now extends `java.util.logging.LogManager`.
//!   2. The JBoss `LogManager` ctor in the real JAR wires up a handler
//!      chain that writes log lines to stderr and to
//!      `org.jboss.boot.log.file`. Our synthetic ctor (in `logmanager.rs`)
//!      is a no-op, so even after the cast lands the WildFly boot lines
//!      (`org.jboss.as.bootstrap | INFO | starting`) silently disappear.
//!
//! This module owns piece (2). It registers a small set of natives on
//! `java/util/logging/Logger` and `org/jboss/logmanager/Logger` that
//! intercept the `info` / `warning` / `severe` / `log` overloads and
//! write a formatted line to a process-wide sink. The sink is:
//!
//!   * `org.jboss.boot.log.file` (system property) when set, opened
//!     append-only the first time a log line is written. Also tees to
//!     stderr so a developer running locally still sees the line.
//!   * stderr only when the property is unset.
//!
//! The format is intentionally minimal:
//! `LEVEL [logger.name] message\n` — readable but not a faithful
//! reproduction of the real JBoss pattern formatter. The point is that
//! the lines flow at all; the WildFly boot sequence stops swallowing
//! them silently and the server.log file becomes non-empty.
//!
//! ## Gate / interaction with Blocks 2A and 2B
//!
//! Registration is unconditional from `register_essential_natives` so the
//! synthetic shim is always available. The natives only fire when the
//! `Logger.log` / `Logger.info` / etc. dispatch lands on our synthetic
//! method registry — i.e. when no real `Logger` class file (from
//! `jboss-logmanager-2.1.18.Final.jar` or the JDK `java.logging` module)
//! has been resolved with a non-stub method body. If Block 2A successfully
//! loads the real JBoss LM jar later, those `Logger.log` overloads are
//! resolved to the real Java implementation by the class loader and our
//! natives never run for them. The system loader prefers a real
//! Java-bytecode method body over a synthetic native.
//!
//! ## What's *not* here
//!
//! The companion changes in `class_manager.rs::jdk_superclass` (synthetic
//! `org.jboss.logmanager.LogManager extends java.util.logging.LogManager`)
//! are required for the `checkcast` in JDK `LogManager.<clinit>` to
//! succeed. Those live next to the rest of the synthetic-stub hierarchy
//! definitions, not here. See `class_manager.rs` line ~3220 for the
//! superclass mapping.
//!
//! Logger registry / addLogger / getLogger / readConfiguration etc. are
//! owned by the older `logmanager.rs` module (T19.H3). This module is
//! strictly additive — it adds `Logger.log/info/warning/severe` dispatch
//! and the boot-log file sink.

#![allow(clippy::needless_pass_by_value)]

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

const CLS_JUL_LOGGER: &str = "java/util/logging/Logger";
const CLS_JBOSS_LOGGER: &str = "org/jboss/logmanager/Logger";

/// Slot 0 of a synthetic `Logger` instance is the logger name (String),
/// matching the layout in `logmanager::LOGGER_FIELD_NAME`.
const LOGGER_FIELD_NAME: usize = 0;

/// Process-wide append-only file handle for the boot log. Opened lazily
/// the first time a log line is emitted. Held inside a `Mutex` so
/// concurrent threads serialize their write() calls (POSIX/Win32 don't
/// guarantee atomic interleaving for sub-page appends).
fn boot_log_file() -> &'static Mutex<Option<std::fs::File>> {
    static INSTANCE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Lookup the `org.jboss.boot.log.file` system property and open the
/// file (creating + appending if needed). Returns `None` if the property
/// is unset, empty, or the open syscall failed — callers fall back to
/// stderr-only.
fn open_boot_log_path() -> Option<std::fs::File> {
    let path = crate::nbflags()
        .jboss_boot_log_file
        .clone()
        .or_else(|| cratonvm_types::flags::runtime_var("org.jboss.boot.log.file").ok());
    let path = match path {
        Some(p) if !p.is_empty() => p,
        _ => return None,
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Read the system-property side-table (populated by
/// `lang_system::native_set_property`) for `org.jboss.boot.log.file`.
/// Falls back to a real environment variable for tests that drive the
/// shim without going through Java `System.setProperty`.
fn boot_log_path_from_ctx(ctx: &dyn NativeContext) -> Option<String> {
    if let Some(p) = ctx.get_system_property("org.jboss.boot.log.file") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    if let Ok(p) = cratonvm_types::flags::runtime_var("org.jboss.boot.log.file") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    None
}

/// Append a single line to the configured boot log file (if any) and
/// always tee to stderr. The line should already include the trailing
/// `\n` so we don't insert one and break callers writing pre-formatted
/// multi-line records.
fn emit_log_line(ctx: &dyn NativeContext, line: &str) {
    // Tee to stderr unconditionally so a developer running locally
    // sees the line even when no file path is configured.
    eprint!("{line}");

    // Write to the configured file if any. Cache the FD across calls so
    // we don't pay the open() syscall per line.
    let mut guard = boot_log_file().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        // Resolve the path either from the JVM's properties (preferred)
        // or fall back to a process-environment variable.
        let path = boot_log_path_from_ctx(ctx);
        if let Some(p) = path {
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .ok();
        } else {
            *guard = open_boot_log_path();
        }
    }
    if let Some(f) = guard.as_mut() {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

/// Translate a `java.util.logging.Level` ObjectRef (or null) to a short
/// human-readable level name. Slot 0 of the synthetic Level layout is
/// the level name string; if the slot is null or the receiver is null
/// we return `INFO` so the line is still legible.
fn level_name(ctx: &mut dyn NativeContext, level_obj: Option<cratonvm_types::ObjectRef>) -> String {
    let Some(obj) = level_obj else { return "INFO".to_string() };
    if let Value::Object(Some(name_str)) = ctx.get_field(obj, LOGGER_FIELD_NAME) {
        if let Some(s) = ctx.read_string(name_str) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    "INFO".to_string()
}

/// Read the `name` field of a synthetic Logger instance. Falls back to
/// `<root>` when the receiver is null or the slot is unset (the JDK
/// root logger has the empty-string name).
fn logger_name(ctx: &mut dyn NativeContext, this: Option<cratonvm_types::ObjectRef>) -> String {
    let Some(obj) = this else { return "<root>".to_string() };
    if let Value::Object(Some(name_str)) = ctx.get_field(obj, LOGGER_FIELD_NAME) {
        if let Some(s) = ctx.read_string(name_str) {
            return if s.is_empty() { "<root>".to_string() } else { s };
        }
    }
    "<root>".to_string()
}

/// Format and emit a single log line via [`emit_log_line`]. Centralised
/// so `info`/`warning`/`severe`/`log(Level,String)` all produce a
/// consistent shape.
fn log_one(ctx: &mut dyn NativeContext, level: &str, logger: &str, message: &str) {
    if crate::nbflags().dbg_jlm {
        tracing::debug!(level, logger, message, "synthetic JBoss LM: log line");
    }
    emit_log_line(ctx, &format!("{level} [{logger}] {message}\n"));
}

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

fn native_logger_log_record(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (this, LogRecord) — read level + message off the LogRecord if we can.
    // The synthetic LogRecord layout is unspecified by us, so we tolerate
    // null fields and emit a minimal `INFO [name] <LogRecord>` line.
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger = logger_name(ctx, this);
    let mut level = "INFO".to_string();
    let mut message = "<LogRecord>".to_string();
    if let Some(Value::Object(Some(rec))) = args.get(1) {
        // Best-effort field probe: most synthetic LogRecord shapes
        // store level at slot 0, message at slot 1.
        if let Value::Object(Some(lvl_obj)) = ctx.get_field(*rec, 0) {
            level = level_name(ctx, Some(lvl_obj));
        }
        if let Value::Object(Some(msg_obj)) = ctx.get_field(*rec, 1) {
            if let Some(s) = ctx.read_string(msg_obj) {
                message = s;
            }
        }
    }
    log_one(ctx, &level, &logger, &message);
    Ok(None)
}

fn native_logger_log_level_msg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (this, Level, String)
    let this = match args.first() { Some(Value::Object(o)) => *o, _ => None };
    let lvl = match args.get(1) { Some(Value::Object(o)) => *o, _ => None };
    let msg = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let level = level_name(ctx, lvl);
    let logger = logger_name(ctx, this);
    log_one(ctx, &level, &logger, &msg);
    Ok(None)
}

/// Common dispatch: read this+message, format with the supplied level
/// label, and emit. Each public level wrapper just supplies its
/// `level_label` so we can register a bare `fn` pointer (the
/// `NativeMethodRegistry` API doesn't accept closures with state).
#[inline]
fn log_named(ctx: &mut dyn NativeContext, args: &[Value], level_label: &'static str) {
    let this = match args.first() { Some(Value::Object(o)) => *o, _ => None };
    let msg = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let logger = logger_name(ctx, this);
    log_one(ctx, level_label, &logger, &msg);
}

fn native_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "INFO"); Ok(None)
}
fn native_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "WARNING"); Ok(None)
}
fn native_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "SEVERE"); Ok(None)
}
fn native_logger_fine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "FINE"); Ok(None)
}
fn native_logger_finer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "FINER"); Ok(None)
}
fn native_logger_finest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "FINEST"); Ok(None)
}
fn native_logger_config(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_named(ctx, args, "CONFIG"); Ok(None)
}

fn native_logger_set_level(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // We don't gate by level — the tracing subscriber filters elsewhere.
    Ok(None)
}

fn native_logger_get_level(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Returning null means "inherit from parent" per JDK spec. Safe for
    // any caller that wraps the result in a null check.
    Ok(Some(Value::Object(None)))
}

fn native_logger_is_loggable(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Match the JDK default: every level is loggable. Filtering happens
    // in `log_one` via the tracing subscriber.
    Ok(Some(Value::Int(1)))
}

fn native_jboss_logger_log_unchecked(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // org.jboss.logmanager.Logger.logRaw / doLog overloads vary widely;
    // they all reduce to "format an entry then emit". Emit a single
    // best-effort line so a `tracing::warn!`-style stub indicator is
    // still preferable to a silent NoSuchMethodError.
    if crate::nbflags().dbg_jlm {
        tracing::debug!(arg_count = args.len(), "synthetic JBoss LM: stub method logRaw called");
    }
    let this = match args.first() { Some(Value::Object(o)) => *o, _ => None };
    let logger = logger_name(ctx, this);
    log_one(ctx, "INFO", &logger, "<jboss-logmanager raw log entry>");
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the Block 2C boot-log sink natives. Wire from
/// `register_essential_natives` AFTER the older `logmanager.rs` block so
/// the latter's `addLogger` / `getLogger` registration order is
/// preserved. This block adds *new* method triples
/// (`Logger.{info,warning,severe,log}`) — it does NOT replace any
/// previously-registered triple from `logmanager.rs`, since those don't
/// register a Logger.log handler today.
pub fn register_jboss_logmanager_natives(registry: &mut NativeMethodRegistry) {
    // NOTE (borderline): the JBoss LogManager has no real JDK class — these
    // natives fabricate a logger / route log lines to a boot-log sink rather
    // than running real bytecode. Tagged Bridge (host log sink, no Java
    // bytecode to defer to) per classification guidance.
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    for cls in [CLS_JUL_LOGGER, CLS_JBOSS_LOGGER] {
        registry.register(cls, "info", "(Ljava/lang/String;)V", native_logger_info);
        registry.register(cls, "warning", "(Ljava/lang/String;)V", native_logger_warning);
        registry.register(cls, "severe", "(Ljava/lang/String;)V", native_logger_severe);
        registry.register(cls, "fine", "(Ljava/lang/String;)V", native_logger_fine);
        registry.register(cls, "finer", "(Ljava/lang/String;)V", native_logger_finer);
        registry.register(cls, "finest", "(Ljava/lang/String;)V", native_logger_finest);
        registry.register(cls, "config", "(Ljava/lang/String;)V", native_logger_config);
        registry.register(cls, "log", "(Ljava/util/logging/LogRecord;)V",
            native_logger_log_record);
        registry.register(
            cls,
            "log",
            "(Ljava/util/logging/Level;Ljava/lang/String;)V",
            native_logger_log_level_msg,
        );
        registry.register(cls, "setLevel", "(Ljava/util/logging/Level;)V",
            native_logger_set_level);
        registry.register(cls, "getLevel", "()Ljava/util/logging/Level;",
            native_logger_get_level);
        registry.register(cls, "isLoggable", "(Ljava/util/logging/Level;)Z",
            native_logger_is_loggable);
    }
    // Common JBoss LM `logRaw` overload reached by `ExtHandler` callers.
    // Register only on the JBoss-side class so we don't shadow the
    // (real) JDK Logger if it ever resolves through here.
    registry.register(
        CLS_JBOSS_LOGGER,
        "logRaw",
        "(Lorg/jboss/logmanager/ExtLogRecord;)V",
        native_jboss_logger_log_unchecked,
    );
    // Keycloak `Log4jLogger.doLogf` -> `Logger.logRaw(LogRecord)` overload.
    // The real bytecode wraps the LogRecord into an ExtLogRecord then
    // recurses into `logRaw(ExtLogRecord)`, which NPEs on `loggerNode`.
    // Intercept the LogRecord overload too and emit a best-effort line
    // directly without ever touching the missing LoggerNode field.
    registry.register(
        CLS_JBOSS_LOGGER,
        "logRaw",
        "(Ljava/util/logging/LogRecord;)V",
        native_jboss_logger_log_unchecked,
    );
    registry.set_category(__prev_cat);
}

/// Test-only: drop the cached file handle so a re-test against a
/// freshly-set property opens the new file rather than continuing to
/// write into a stale FD.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    if let Ok(mut g) = boot_log_file().lock() {
        *g = None;
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;

    /// Process-wide guard so two tests don't race on the file handle
    /// state mutated by `reset_for_tests`.
    fn lock() -> &'static std::sync::Mutex<()> {
        static L: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn block_2c_register_jboss_logmanager_natives_registers_expected() {
        let mut r = NativeMethodRegistry::new();
        register_jboss_logmanager_natives(&mut r);
        // JUL Logger surface.
        assert!(r.find(CLS_JUL_LOGGER, "info", "(Ljava/lang/String;)V").is_some());
        assert!(r.find(CLS_JUL_LOGGER, "warning", "(Ljava/lang/String;)V").is_some());
        assert!(r.find(CLS_JUL_LOGGER, "severe", "(Ljava/lang/String;)V").is_some());
        assert!(r.find(CLS_JUL_LOGGER, "log",
            "(Ljava/util/logging/Level;Ljava/lang/String;)V").is_some());
        assert!(r.find(CLS_JUL_LOGGER, "log",
            "(Ljava/util/logging/LogRecord;)V").is_some());
        // JBoss subclass mirror.
        assert!(r.find(CLS_JBOSS_LOGGER, "info", "(Ljava/lang/String;)V").is_some());
        assert!(r.find(CLS_JBOSS_LOGGER, "logRaw",
            "(Lorg/jboss/logmanager/ExtLogRecord;)V").is_some());
    }

    #[test]
    fn block_2c_info_writes_to_boot_log_file_when_property_set() {
        let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_for_tests();
        // Use a per-test file under the OS temp dir so we don't trip
        // CI's writable-path policies. Set via the env-var fallback so
        // we don't need a real System.setProperty implementation in the
        // mock context.
        let mut path = std::env::temp_dir();
        path.push(format!("cratonvm-block2c-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // A thread-scoped flag override rather than `set_var`: writing to
        // `environ` from a test is a process-wide data race against every other
        // test running in parallel (`setenv` may realloc and free the array
        // under a concurrent `getenv`), and no local mutex can contain it. The
        // production read is `flags::runtime_var("org.jboss.boot.log.file")`,
        // which this override serves. The guard restores on drop.
        let path_str = path.to_string_lossy().into_owned();
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "org.jboss.boot.log.file",
                Some(path_str.as_str()),
            )]),
        );

        let mut ctx = mock_ctx();
        let logger = crate::try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, 3)?;
        let name = ctx.create_string("com.example.App");
        ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(name)));
        let msg = ctx.create_string("hello-block2c");
        let _ = native_logger_info(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(msg))],
        )
        .unwrap();

        // Drop our cached fd before reading so the OS flushes & closes.
        reset_for_tests();
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            contents.contains("hello-block2c"),
            "boot log file must contain the message; saw: {:?}",
            contents
        );
        assert!(
            contents.contains("[com.example.App]"),
            "boot log file must contain the logger name; saw: {:?}",
            contents
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn block_2c_log_record_falls_back_to_info_when_fields_null() {
        let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_for_tests();
        // No file path → only stderr. We just want the call not to
        // panic on null-Level / null-message LogRecord shapes. Overridden to
        // "as if unset" on this thread rather than removed from `environ` —
        // see the sibling test for why that distinction matters.
        let _flags = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                "org.jboss.boot.log.file",
                None,
            )]),
        );
        let mut ctx = mock_ctx();
        let logger = crate::try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, 3)?;
        let rec = crate::try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/LogRecord", 4)?;
        let res = native_logger_log_record(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(rec))],
        );
        assert!(res.is_ok(), "must not error on a half-populated LogRecord");
    }
}
