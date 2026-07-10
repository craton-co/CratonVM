// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.H3 — `java.util.logging.LogManager` singleton + `org.jboss.logmanager.LogManager` subclass.
//!
//! KC26 (Quarkus/Keycloak) fails early because
//! `java.util.logging.LogManager.getLogManager()` in our pre-T19.H3 code was
//! registered multiple times with subtly different field counts, and the
//! post-clinit fixup in `vm_util.rs` already allocates a `LogManager.manager`
//! static — but the subsequent Java-side bytecode in `LogManager.<clinit>` /
//! `java.util.logging.LogManager.getLogManager0()` does a
//! `Class.newInstance()` on the class named by the
//! `java.util.logging.manager` system property (Quarkus sets it to
//! `org.jboss.logmanager.LogManager`) and then casts the result to
//! `java.util.logging.LogManager`. When our VM returns a `Class` mirror
//! instead of a fresh instance — e.g. because `newInstance()` was
//! mis-routed or the ctor blew up and left a `Class` on the stack — the
//! checked-cast raises `ClassCastException: java/lang/Class cannot be cast
//! to java/util/logging/LogManager`.
//!
//! This module takes ownership of the whole `LogManager` surface:
//!
//! 1. `LogManager.getLogManager()` (and its `org.jboss.logmanager.LogManager`
//!    subclass equivalent) returns a **process-wide singleton instance
//!    ObjectRef** allocated once and cached in a `OnceLock`. Subsequent
//!    calls return the same ObjectRef so pointer-identity comparisons
//!    inside Quarkus/JBoss bytecode still observe a stable manager.
//! 2. `getLogger(String)` — idempotently returns the same `Logger` mirror
//!    for the same name, backed by the `LoggerRegistry`. This complements
//!    the existing `wildfly_core::get_logger` registry by also producing
//!    a heap ObjectRef visible to JDK bytecode.
//! 3. `addLogger(Logger)` — returns `true` on first successful add, `false`
//!    if a logger with the same name is already registered (matches spec).
//!    Rejects names containing `../`, `\\`, `:`, or any ASCII control char
//!    so malicious `loadConfiguration` calls can't traverse the filesystem
//!    via logger-name injection.
//! 4. `readConfiguration()` / `readConfiguration(InputStream)` — no-op that
//!    returns null. We never parse untrusted logging configuration; all
//!    levels are inherited from the process-wide tracing subscriber.
//! 5. `reset()` — clears the logger registry (leaves the singleton in
//!    place; JDK spec allows the manager instance to be kept while the
//!    logger-name set is flushed).
//! 6. `getLoggerNames()` — returns an `Enumeration<String>` over the
//!    currently-registered logger names, with a snapshot taken at call
//!    time so subsequent registrations don't ConcurrentModify the
//!    enumeration.
//!
//! ## Interaction with the `vm_util.rs` post-clinit fixup
//!
//! `vm_util.rs::post_clinit_fixup` allocates a `LogManager` object and
//! stashes it in the static `LogManager.manager` field. When our
//! `getLogManager()` native runs AFTER that fixup, we read
//! `LogManager.manager` first and honour it; if it's null (fixup ran but
//! allocator failed, or we're in a mode where it didn't run), we fall
//! back to allocating our own singleton via `alloc_object`. This way
//! the DELAYED_HANDLER registered against the fixup-time manager is
//! still reachable from the later native-returned instance.
//!
//! ## Interaction with `wildfly_core::get_logger`
//!
//! `wildfly_core::get_logger(name)` maintains a Rust-side
//! `Arc<LoggerMirror>` registry keyed by name. We delegate to it so
//! tracing redaction + level-override state stays unified across JBoss
//! and JUL code paths.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

// ---------------------------------------------------------------------------
// Class / field name constants
// ---------------------------------------------------------------------------

const CLS_JUL_LOG_MANAGER: &str = "java/util/logging/LogManager";
const CLS_JBOSS_LOG_MANAGER: &str = "org/jboss/logmanager/LogManager";
const CLS_JUL_LOGGER: &str = "java/util/logging/Logger";
const CLS_LOGGER_ENUMERATION: &str = "java/util/logging/LogManager$StringEnumeration";
const CLS_JUL_LEVEL: &str = "java/util/logging/Level";
const CLS_JBOSS_LEVEL: &str = "org/jboss/logmanager/Level";

/// The 9 standard `java.util.logging.Level` public static constants.
const STANDARD_LEVEL_NAMES: [&str; 9] = [
    "OFF", "SEVERE", "WARNING", "INFO", "CONFIG", "FINE", "FINER", "FINEST", "ALL",
];

/// `org.jboss.logmanager.Level`'s extended constants (JBoss config files —
/// including WildFly's own `host.xml`/`domain.xml` — use these names, e.g.
/// `<level name="WARN"/>`). `INFO` is intentionally omitted: it aliases the
/// standard `java.util.logging.Level.INFO` constant, which is already
/// checked first.
const JBOSS_LEVEL_NAMES: [&str; 5] = ["FATAL", "ERROR", "WARN", "DEBUG", "TRACE"];

/// Synthetic slot layout for the singleton `LogManager` instance.  The
/// real JDK `LogManager` has many more fields but our native-only
/// implementation only uses:
///   * slot 0 — `properties` (Properties / HashMap — null-ok).
///   * slot 1 — `loggerRegistry` (opaque placeholder; the real state
///              lives in the Rust-side `LoggerRegistry`).
///   * slot 2 — `rootLogger` (Logger object for "" — populated lazily).
///   * slot 3 — `ready` (int flag — always 1; honours "initialised" bit).
const LM_NUM_FIELDS: usize = 4;
const LM_FIELD_PROPERTIES: usize = 0;
const LM_FIELD_LOGGER_REGISTRY: usize = 1;
const LM_FIELD_ROOT_LOGGER: usize = 2;
const LM_FIELD_READY: usize = 3;

/// Synthetic slot layout for a `Logger` object allocated by us.
///   * slot 0 — `name` (String).
///   * slot 1 — `level` (Level object; null = inherit from parent).
///   * slot 2 — `parent` (Logger object; null for root).
const LOGGER_NUM_FIELDS: usize = 3;
const LOGGER_FIELD_NAME: usize = 0;
const LOGGER_FIELD_LEVEL: usize = 1;
const LOGGER_FIELD_PARENT: usize = 2;

// ---------------------------------------------------------------------------
// Process-wide singleton state
// ---------------------------------------------------------------------------

/// Holds the `LogManager` singleton's raw ObjectRef address. Pointer
/// identity is stable for the lifetime of the process — we never free
/// this object. Stored as a `u64` so we can atomically swap through a
/// single `OnceLock<Mutex<u64>>` without re-allocation across tests.
fn singleton_cell() -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Name -> `Logger` ObjectRef (as raw u64). Populated on first
/// `getLogger`/`addLogger`. Reads are cheap; a lock is acquired only
/// during mutation.
fn logger_registry() -> &'static Mutex<HashMap<String, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Validate a logger name. Reject anything that looks like a filesystem
/// traversal or control-character injection so `loadConfiguration`-style
/// code can't accidentally write to arbitrary paths via `addLogger` +
/// `getHandlers` side-channels.
///
/// Criteria (all of these → reject):
///   * contains `"../"` or `"..\\"` (path traversal).
///   * contains a `'\\'` or `':'` (Windows path separator / drive letter
///     injection — real logger names never use them).
///   * contains any ASCII control char (codepoints < 0x20 or == 0x7F).
///   * exceeds 512 chars — guardrail against resource-exhaustion log
///     names that would bloat the registry.
fn is_valid_logger_name(name: &str) -> bool {
    if name.len() > 512 {
        return false;
    }
    if name.contains("../") || name.contains("..\\") {
        return false;
    }
    for ch in name.chars() {
        if ch == '\\' || ch == ':' {
            return false;
        }
        if (ch as u32) < 0x20 || ch == '\u{7f}' {
            return false;
        }
    }
    true
}

/// SECURITY FIX: Validate a JVM *internal* (binary) class name in its
/// slash-separated form (e.g. `com/example/MyConfig`). This is distinct
/// from `is_valid_logger_name`, which validates dotted logger names and
/// therefore cannot be reused here: by the time the LogManager property
/// has been converted to internal form, every `.` (including the dots of
/// a `..` traversal token) has already become `/`, so a path-traversal /
/// absolute-path payload would slip past the dotted-form `../` check.
///
/// A name is accepted only if ALL hold:
///   * non-empty and <= 512 chars (resource guardrail),
///   * contains no ASCII control char (< 0x20 or == 0x7F) and no NUL,
///   * contains no `\` or `:` (Windows separator / drive-letter / URL),
///   * splits on `/` into one or more segments where every segment is
///     non-empty (rejects leading/trailing `/` and empty `//` segments,
///     i.e. absolute paths like `/////////etc/passwd`) and no segment is
///     `.` or `..` (rejects `../../../etc/passwd`-style traversal).
fn is_valid_internal_class_name(internal: &str) -> bool {
    if internal.is_empty() || internal.len() > 512 {
        return false;
    }
    for ch in internal.chars() {
        if ch == '\\' || ch == ':' {
            return false;
        }
        if (ch as u32) < 0x20 || ch == '\u{7f}' {
            return false;
        }
    }
    // Every `/`-separated segment must be a real identifier-ish token:
    // non-empty (no leading/trailing/double slash) and not a `.`/`..`
    // filesystem relative-path component.
    for segment in internal.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
    }
    true
}

/// Rebuild an `ObjectRef` from a raw u64 address.
///
/// # Safety
///
/// The caller must have obtained `addr` from a live ObjectRef allocated
/// by the same process's heap. We never free LogManager / Logger
/// singletons once registered, so the address remains valid for the
/// lifetime of the process. Field read/writes go through the
/// `NativeContext` trait, which performs its own bounds-checking.
unsafe fn object_from_u64(addr: u64) -> ObjectRef {
    ObjectRef::from_raw(addr as *mut u8)
}

/// Allocate a fresh `LogManager` instance using the specified concrete
/// class (either `java.util.logging.LogManager` or
/// `org.jboss.logmanager.LogManager`). The two share the same synthetic
/// field layout — the difference is purely the `getClass()` mirror the
/// bytecode observes.
fn allocate_log_manager(ctx: &mut dyn NativeContext, class_name: &str) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, class_name, LM_NUM_FIELDS);
    // Leave slot 0 as `null` — a properly-initialized `Properties` would
    // round-trip through synthetic HashMap natives, but most Quarkus/JBoss
    // code reads it via accessors we no-op, so null is safe.
    ctx.set_field(obj, LM_FIELD_PROPERTIES, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_LOGGER_REGISTRY, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_ROOT_LOGGER, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_READY, Value::Int(1));
    obj
}

/// Block 2B — try to allocate a custom subclass instance based on the
/// `java.util.logging.manager` system property.
///
/// Returns `Some(instance)` if:
///   * the property is set to a non-empty class name,
///   * the named class is loadable through the unified class loader
///     (which sees `-c` classpath entries — bypassing the bootstrap-
///     loader caller-class detection that vanilla
///     `Class.forName(name)` from inside `j.u.l.LogManager.<clinit>`
///     stumbles over),
///   * the class has an accessible no-arg constructor that completes
///     without throwing.
///
/// Returns `None` otherwise so the caller can fall back to the JDK
/// default `java/util/logging/LogManager` singleton (preserving the
/// no-`-Djava.util.logging.manager` baseline).
///
/// The JDK alias (`java.util.logging.LogManager`) short-circuits to
/// `None` so the existing default singleton remains the source of truth.
/// The JBoss alias (`org.jboss.logmanager.LogManager`) is special: WildFly's
/// logging extension checks that the active singleton's concrete class is the
/// JBoss manager, so allocate our synthetic JBoss-classed singleton directly
/// instead of invoking the real constructor.
fn try_allocate_property_log_manager(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let prop = ctx.get_system_property("java.util.logging.manager")?;
    let dotted = prop.trim();
    if dotted.is_empty() {
        return None;
    }
    // Built-in aliases use our synthetic singleton layout; calling
    // `<init>` on them through the bytecode path would either re-enter
    // this function or trip the `native_jboss_init` no-op contract.
    let internal = dotted.replace('.', "/");
    if internal == CLS_JUL_LOG_MANAGER {
        return None;
    }
    if internal == CLS_JBOSS_LOG_MANAGER {
        return Some(allocate_log_manager(ctx, CLS_JBOSS_LOG_MANAGER));
    }

    // SECURITY FIX: validate the *class name* with a dedicated
    // class-name validator, NOT `is_valid_logger_name`. The previous code
    // ran `is_valid_logger_name(&internal)` against the already
    // slash-converted form, so a traversal payload like
    // `../../../etc/passwd` had its dots rewritten to slashes
    // (`/////////etc/passwd`) *before* the `../` substring check ran —
    // defeating the check and letting an absolute filesystem-like path
    // through as a "class name". A valid binary/internal class name has no
    // leading/trailing/empty segments and no `.`/`..` segments, so
    // `is_valid_internal_class_name` rejects such payloads outright and we
    // fall back to the JDK default.
    if !is_valid_internal_class_name(&internal) {
        tracing::warn!(
            class = %dotted,
            "java.util.logging.manager: rejecting suspicious class name"
        );
        return None;
    }

    // Step 1: load + init via the unified loader. This is the path that
    // sees `-c` classpath entries and JBoss-Modules-injected jars; the
    // vanilla `Class.forName(String)` 1-arg form invoked from inside the
    // JDK's `LogManager.<clinit>` resolves with the bootstrap loader
    // (caller-class detection) and so misses `-c apps/...` classes
    // entirely. Bypassing that path is exactly what the override exists
    // for.
    if ctx.ensure_class_initialized(&internal).is_err() {
        tracing::warn!(
            class = %dotted,
            "java.util.logging.manager: class not loadable via unified loader, \
             falling back to JDK default"
        );
        return None;
    }

    // Step 2: allocate without invoking `<init>` so we control the
    // ordering — `new_object` reserves a slot, then `invoke(<init>()V)`
    // runs the user-defined ctor (which may itself call `super()` into
    // `java.util.logging.LogManager.<init>`, satisfied by the
    // `native_jboss_init` no-op we register on the parent class).
    let obj = match ctx.new_object(&internal) {
        Ok(Some(Value::Object(Some(obj)))) => obj,
        _ => {
            tracing::warn!(
                class = %dotted,
                "java.util.logging.manager: new_object failed, falling back"
            );
            return None;
        }
    };
    if let Err(e) = ctx.invoke(&internal, "<init>", "()V", &[Value::Object(Some(obj))]) {
        tracing::warn!(
            class = %dotted,
            error = ?e,
            "java.util.logging.manager: <init> threw, falling back"
        );
        return None;
    }
    Some(obj)
}

/// Return (or lazily allocate) the process-wide `LogManager` singleton
/// ObjectRef. Subsequent calls return the same ObjectRef so
/// pointer-identity comparisons in Java (`if (mgr == other)`) stay
/// stable.
///
/// Block 2B: when called for the JDK-side `java.util.logging.LogManager`
/// entry-point and `-Djava.util.logging.manager=<X>` is set, the
/// resolved instance's concrete class is `X` (loaded through the
/// unified system loader so `-c` paths are visible). Without the
/// property, the JDK-default class is used as before — see
/// `try_allocate_property_log_manager` for the loader-bypass rationale.
fn ensure_singleton(ctx: &mut dyn NativeContext, class_name: &str) -> ObjectRef {
    // Fast path: already cached.
    {
        let guard = singleton_cell().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(addr) = *guard {
            if addr != 0 {
                // SAFETY: the cached address was produced by `alloc_object`
                // earlier in this process. Singleton lifetime == process lifetime.
                return unsafe { object_from_u64(addr) };
            }
        }
    }
    // Slow path: try the property-driven subclass first (only relevant
    // when the JDK's own `getLogManager` is the entry point). For
    // direct `org.jboss.logmanager.LogManager.getLogManager()` calls
    // the caller already chose the concrete class, so honour that.
    let chose_property_class = if class_name == CLS_JUL_LOG_MANAGER {
        try_allocate_property_log_manager(ctx)
    } else {
        None
    };
    let obj = match chose_property_class {
        Some(o) => o,
        None => allocate_log_manager(ctx, class_name),
    };
    let mut guard = singleton_cell().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(addr) = *guard {
        if addr != 0 {
            // Another thread beat us; drop our allocation on the floor
            // (we have no external references to it yet) and adopt
            // theirs.
            return unsafe { object_from_u64(addr) };
        }
    }
    *guard = Some(obj.as_ptr() as u64);
    obj
}

/// Allocate a `Logger` object, populate its name field, and register
/// it in the process-wide registry.
fn allocate_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    // Ensure the wildfly_core registry also learns about this name so
    // log-level overrides and credential redaction apply uniformly. The
    // wildfly layer returns an `Arc<LoggerMirror>` — we don't need the
    // Arc itself here, just the side-effect of interning the name.
    let _mirror = crate::wildfly_core::get_logger(name);
    let obj = alloc_concurrent_synthetic(ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS);
    let name_obj = ctx.create_string(name);
    ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(obj, LOGGER_FIELD_LEVEL, Value::Object(None));
    ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(None));
    obj
}

/// Look up a cached logger by name; if absent, allocate one and cache
/// it. Rejected names (via `is_valid_logger_name`) allocate an
/// anonymous Logger that isn't registered so the caller still receives
/// a non-null Logger for the `.info()` / `.warning()` fallback but the
/// bad name never enters the registry.
fn get_or_create_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    if !is_valid_logger_name(name) {
        tracing::warn!(
            rejected_name = %name,
            "LogManager.getLogger: rejected suspicious logger name, returning anonymous logger"
        );
        return allocate_logger(ctx, "");
    }
    {
        let reg = logger_registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                // SAFETY: singleton-style lifetime.
                return unsafe { object_from_u64(addr) };
            }
        }
    }
    let obj = allocate_logger(ctx, name);
    {
        let mut reg = logger_registry().lock().unwrap_or_else(|e| e.into_inner());
        // Check again under the lock (TOCTOU); if another thread beat
        // us, return their logger and drop ours on the floor (it has
        // no external references yet).
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                return unsafe { object_from_u64(addr) };
            }
        }
        reg.insert(name.to_string(), obj.as_ptr() as u64);
    }
    obj
}

fn read_jul_logger_name(ctx: &dyn NativeContext, logger: ObjectRef) -> String {
    // Real-JDK Logger instances keep their name in the `name` field; slot 0 is
    // `Logger$ConfigurationData`. CratonVM synthetic Logger instances keep the
    // name in slot 0. Prefer the real layout, then fall back only if slot 0 is
    // actually a String.
    if let Value::Object(Some(name_obj)) = ctx.get_field_by_name(logger, "name") {
        if let Some(name) = ctx.read_string(name_obj) {
            return name;
        }
    }
    if let Value::Object(Some(name_obj)) = ctx.get_field(logger, LOGGER_FIELD_NAME) {
        if ctx
            .class_name_of_id(ctx.class_id_of_object(name_obj))
            .as_deref()
            == Some("java/lang/String")
        {
            if let Some(name) = ctx.read_string(name_obj) {
                return name;
            }
        }
    }
    String::new()
}

fn jboss_log_manager_requested(ctx: &dyn NativeContext) -> bool {
    matches!(
        ctx.get_system_property("java.util.logging.manager")
            .as_deref()
            .map(str::trim),
        Some("org.jboss.logmanager.LogManager" | "org/jboss/logmanager/LogManager")
    )
}

fn native_jul_static_get_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if jboss_log_manager_requested(ctx) {
        let logger = get_or_create_jboss_logger(ctx, &name);
        return Ok(Some(Value::Object(Some(logger))));
    }
    let logger = get_or_create_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger))))
}

fn native_jul_static_get_logger_with_bundle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_jul_static_get_logger(ctx, args)
}

/// Test-only helper: wipe the singleton + logger registry so tests
/// don't see state bleed between parallel threads.
#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
    if let Ok(mut g) = singleton_cell().lock() {
        *g = None;
    }
    if let Ok(mut r) = logger_registry().lock() {
        r.clear();
    }
    if let Ok(mut g) = jboss_log_context_singleton().lock() {
        *g = None;
    }
    if let Ok(mut r) = jboss_logger_registry().lock() {
        r.clear();
    }
    if let Ok(mut m) = attachments().lock() {
        m.clear();
    }
}

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

fn native_get_log_manager(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_get_jboss_log_manager(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Same singleton — the JBoss subclass in JUL resolves via
    // Class.forName(property) + newInstance(), and the JDK code at
    // `LogManager.getLogManager()` ultimately returns the manager
    // singleton regardless of the concrete class. Using the same slot
    // keeps pointer-identity stable.
    let obj = ensure_singleton(ctx, CLS_JBOSS_LOG_MANAGER);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_jboss_init(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `org.jboss.logmanager.LogManager.<init>` is invoked by Java's
    // `Class.newInstance()` inside `java.util.logging.LogManager.getLogManager()`
    // when `java.util.logging.manager` is set to
    // `org.jboss.logmanager.LogManager`. The real JBoss ctor reads a
    // `readConfiguration()`-driven handler chain — we short-circuit it
    // to a no-op so the singleton we hand back later isn't half-init'd.
    Ok(None)
}

fn native_get_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `getLogger(String)` — receiver in args[0], name in args[1].
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let logger = get_or_create_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger))))
}

fn native_add_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `addLogger(Logger)` — args[0]=this, args[1]=Logger.
    let Some(Value::Object(Some(logger))) = args.get(1).cloned() else {
        // null Logger -> contract says NullPointerException, but we
        // prefer to swallow + return false so the caller's bootstrap
        // path keeps going; this is consistent with the permissive
        // behaviour T19.H1 established elsewhere.
        return Ok(Some(Value::Int(0)));
    };
    let name = read_jul_logger_name(ctx, logger);
    if !is_valid_logger_name(&name) {
        tracing::warn!(
            rejected_name = %name,
            "LogManager.addLogger: rejected suspicious logger name"
        );
        return Ok(Some(Value::Int(0)));
    }
    let mut reg = logger_registry().lock().unwrap_or_else(|e| e.into_inner());
    if reg.contains_key(&name) {
        // Already registered — spec says return false.
        return Ok(Some(Value::Int(0)));
    }
    // Also track in wildfly_core so tracing redaction picks this up.
    let _mirror = crate::wildfly_core::get_logger(&name);
    reg.insert(name, logger.as_ptr() as u64);
    Ok(Some(Value::Int(1)))
}

fn native_read_configuration_no_arg(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Security: we deliberately do NOT parse untrusted logging config —
    // any path that would load a properties file is suppressed. The
    // process-wide tracing subscriber already governs effective levels.
    Ok(None)
}

fn native_read_configuration_with_stream(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_reset(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Drop all logger bindings but keep the manager singleton alive.
    if let Ok(mut r) = logger_registry().lock() {
        r.clear();
    }
    Ok(None)
}

fn native_get_logger_names(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Snapshot the names and pack into a synthetic Enumeration<String>.
    //
    // Layout (slot 0 = Object[] backing array, slot 1 = cursor int).
    // Our enumeration natives (`hasMoreElements`, `nextElement`) are
    // registered against `CLS_LOGGER_ENUMERATION` so the standard JDK
    // Enumeration API works on this shape.
    let names: Vec<String> = {
        let r = logger_registry().lock().unwrap_or_else(|e| e.into_inner());
        r.keys().cloned().collect()
    };

    let arr = ctx.new_array(ArrayElementType::Reference, names.len());
    for (i, name) in names.iter().enumerate() {
        let s = ctx.create_string(name);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }

    let enumeration = alloc_concurrent_synthetic(ctx, CLS_LOGGER_ENUMERATION, 2);
    ctx.set_field(enumeration, 0, Value::Object(Some(arr)));
    ctx.set_field(enumeration, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(enumeration))))
}

fn native_enumeration_has_more(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().cloned() else {
        return Ok(Some(Value::Int(0)));
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if cursor < len { 1 } else { 0 })))
}

fn native_enumeration_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().cloned() else {
        return Ok(Some(Value::Object(None)));
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    if cursor < 0 || cursor >= len {
        return Ok(Some(Value::Object(None)));
    }
    let elem = ctx.get_array_element(arr, cursor as usize);
    ctx.set_field(this, 1, Value::Int(cursor + 1));
    Ok(Some(elem))
}

// ---------------------------------------------------------------------------
// KC16: org.jboss.logmanager.Logger attachment side-table
// ---------------------------------------------------------------------------
//
// Real-JDK `org.jboss.logmanager.Logger.getAttachment(AttachmentKey)` reads
// `this.loggerNode` and forwards to `LoggerNode.getAttachment`. When the
// `LogManager` failed to install (the JDK warning "Failed to load the
// specified log manager class org.jboss.logmanager.LogManager" fires during
// boot), `LogContext.getLogger("")` returns a Logger whose `loggerNode` field
// is null. The bytecode then NPEs at pc=8.
//
// Override the four attachment methods on `org/jboss/logmanager/Logger` with
// natives that stash attachments in a process-wide side table keyed by
// `(receiver-ObjectRef-addr, AttachmentKey-ObjectRef-addr)`. The JBoss
// `JBossLogManagerFacade.getLoggerRepository` PrivilegedAction is happy with
// any non-NPE behavior — it null-checks the result of getAttachment and
// allocates a fresh Hierarchy/RootLogger when null. `attachIfAbsent` semantics
// (return previous value, null if newly attached) are honored so the facade's
// race-free initialisation path matches the JDK contract.

type AttachKey = (u64, u64);
fn attachments() -> &'static Mutex<HashMap<AttachKey, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<AttachKey, u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn obj_addr(v: &Value) -> u64 {
    if let Value::Object(Some(o)) = v {
        o.as_ptr() as u64
    } else {
        0
    }
}

fn native_jboss_logger_get_attachment(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let map = attachments().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&addr) = map.get(&(this, key)) {
        if addr != 0 {
            // SAFETY: addresses produced by attach/attachIfAbsent are
            // ObjectRefs alive for the lifetime of the process (JBoss
            // facade attachments are static-singletons).
            return Ok(Some(Value::Object(Some(unsafe { object_from_u64(addr) }))));
        }
    }
    Ok(Some(Value::Object(None)))
}

fn native_jboss_logger_attach(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    let value = args.get(2).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments().lock().unwrap_or_else(|e| e.into_inner());
    let prev = map.insert((this, key), value);
    Ok(Some(match prev {
        Some(addr) if addr != 0 => Value::Object(Some(unsafe { object_from_u64(addr) })),
        _ => Value::Object(None),
    }))
}

fn native_jboss_logger_attach_if_absent(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    let value = args.get(2).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&addr) = map.get(&(this, key)) {
        if addr != 0 {
            return Ok(Some(Value::Object(Some(unsafe { object_from_u64(addr) }))));
        }
    }
    map.insert((this, key), value);
    // Per JDK contract, attachIfAbsent returns null when newly attached.
    Ok(Some(Value::Object(None)))
}

/// Process-wide synthetic `org.jboss.logmanager.LogContext` singleton.
/// Returned by `Logger.getLogContext()` and `LogContext.getLogContext()`
/// natives. The real class has many fields; we only need a non-null
/// receiver so JBoss bytecode that walks the parent chain
/// (`JBossLogManagerFacade.updateParents`) can call methods on it
/// without NPE. All overridden methods on this class return null/empty.
fn jboss_log_context_singleton() -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

fn ensure_jboss_log_context(ctx: &mut dyn NativeContext) -> ObjectRef {
    {
        let g = jboss_log_context_singleton()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(addr) = *g {
            if addr != 0 {
                return unsafe { object_from_u64(addr) };
            }
        }
    }
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/LogContext", 1);
    // Real LogContext.addCloseHandler synchronizes on treeLock. Most LogContext
    // methods are native-overridden below, but initializing the monitor keeps
    // any remaining real bytecode null-safe.
    let tree_lock = alloc_concurrent_synthetic(ctx, "java/lang/Object", 0);
    ctx.set_field_by_name(obj, "treeLock", Value::Object(Some(tree_lock)));
    if ctx
        .resolve_field_index("org/jboss/logmanager/LogContext", "treeLock")
        .is_none()
    {
        ctx.set_field(obj, 0, Value::Object(Some(tree_lock)));
    }
    let mut g = jboss_log_context_singleton()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(addr) = *g {
        if addr != 0 {
            return unsafe { object_from_u64(addr) };
        }
    }
    *g = Some(obj.as_ptr() as u64);
    obj
}

fn native_jboss_logger_get_log_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ensure_jboss_log_context(ctx)))))
}

fn native_jboss_log_context_get_log_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ensure_jboss_log_context(ctx)))))
}

fn native_jboss_log_context_get_logger_if_exists(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Returning null is contractually fine — callers null-check before
    // dereferencing (see JBossLogManagerFacade.getLoggers / updateParents).
    Ok(Some(Value::Object(None)))
}

/// Process-wide registry of synthetic `org/jboss/logmanager/Logger`
/// instances keyed by name. Distinct from `logger_registry()` (which
/// holds `java/util/logging/Logger` mirrors) so the JBoss-side overrides
/// for `getAttachment` etc. dispatch on receivers whose concrete class
/// is `org/jboss/logmanager/Logger`.
fn jboss_logger_registry() -> &'static Mutex<HashMap<String, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn attach_minimal_jboss_logger_node(ctx: &mut dyn NativeContext, logger: ObjectRef) {
    // Some JIT/real-bytecode paths still execute JBoss Logger methods directly
    // before the native override gate can short-circuit them. Those methods all
    // start by dereferencing `this.loggerNode`. We do not model the full
    // LoggerNode graph, but a tiny node with INFO effective level is enough for
    // getEffectiveLevel/isLoggable-style reads to be null-safe and conservative.
    let node = alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/LoggerNode", 16);
    ctx.set_field_by_name(node, "effectiveLevel", Value::Int(800));
    ctx.set_field_by_name(node, "effectiveMinLevel", Value::Int(i32::MIN));
    ctx.set_field_by_name(node, "useParentHandlers", Value::Int(1));
    ctx.set_field_by_name(node, "useParentFilter", Value::Int(1));
    ctx.set_field_by_name(logger, "loggerNode", Value::Object(Some(node)));
}

fn get_or_create_jboss_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    if !is_valid_logger_name(name) {
        let obj = alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS);
        let name_obj = ctx.create_string("");
        ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
        attach_minimal_jboss_logger_node(ctx, obj);
        return obj;
    }
    {
        let reg = jboss_logger_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                return unsafe { object_from_u64(addr) };
            }
        }
    }
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS);
    let name_obj = ctx.create_string(name);
    ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(obj, LOGGER_FIELD_LEVEL, Value::Object(None));
    ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(None));
    attach_minimal_jboss_logger_node(ctx, obj);
    let mut reg = jboss_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&addr) = reg.get(name) {
        if addr != 0 {
            return unsafe { object_from_u64(addr) };
        }
    }
    reg.insert(name.to_string(), obj.as_ptr() as u64);
    obj
}

fn native_jboss_log_context_get_logger(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Mirror `LogManager.getLogger(String)` semantics — return a stable
    // synthetic JBoss Logger keyed by name. Used by JBoss
    // `JBossLogManagerFacade.getJBossLogger(LogContext, name)`. The
    // concrete class is `org/jboss/logmanager/Logger` so subsequent
    // calls to `getAttachment` etc. resolve to our native overrides
    // registered on that class name.
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let logger = get_or_create_jboss_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger))))
}

/// `java.util.logging.Level.parse(String)` — static factory that resolves a
/// level name (or its decimal `intValue()`) to the canonical `Level` object.
///
/// Real JDK 25 bytecode resolves this through `KnownLevel.findByName`, which
/// (per `docs/internal/gaps/kc16-blocker-map.md`'s KC16 investigation) walks
/// a `ClassLoaderValue`-keyed cache that needs a non-null `Module` for a
/// class/classloader CratonVM's module-system synthesis doesn't fully cover
/// — the lookup throws `NullPointerException: Cannot invoke "isNamed" on
/// null` internally, which real `Level.parse`'s own catch-all then reports as
/// `IllegalArgumentException: Bad level "<name>"` regardless of whether the
/// name is a genuine standard constant (`WARNING`) or a JBoss LogManager
/// extension (`WARN`). This broke WildFly's own `host.xml`/`domain.xml`
/// parsing, which resolves `<level name="WARN"/>` via this exact method.
///
/// Bypass the broken registry lookup entirely (matching the same
/// static-field-by-name technique already used by
/// [`native_jboss_log_context_get_level_for_name`] for JBoss's
/// `LogContext.getLevelForName`): check the 9 standard `java.util.logging.
/// Level` constants, then JBoss LogManager's extended constants if that
/// class is already resolvable, then fall back to a numeric parse — only
/// throwing the real `IllegalArgumentException` when none of those match,
/// same as the genuine JDK contract.
fn native_level_parse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Name cannot be null".to_string()),
            }
            .into());
        }
    };
    let name = ctx.read_string(name_obj).unwrap_or_default();
    let upper = name.to_uppercase();

    if let Ok(level_cid) = ctx.ensure_class_initialized(CLS_JUL_LEVEL) {
        if STANDARD_LEVEL_NAMES.contains(&upper.as_str()) {
            if let Some(idx) = ctx.static_field_index_by_name(level_cid, &upper) {
                let v = ctx.get_static_field(level_cid, idx);
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));
                }
            }
        }
    }
    if JBOSS_LEVEL_NAMES.contains(&upper.as_str()) {
        if let Ok(jb_cid) = ctx.ensure_class_initialized(CLS_JBOSS_LEVEL) {
            if let Some(idx) = ctx.static_field_index_by_name(jb_cid, &upper) {
                let v = ctx.get_static_field(jb_cid, idx);
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));
                }
            }
        }
    }
    // Numeric fallback, matching `Level.parse`'s own integer-name path: scan
    // every known constant for an exact `intValue()` match before giving up.
    if let Ok(target) = upper.parse::<i32>() {
        for (cls, names) in [
            (CLS_JUL_LEVEL, STANDARD_LEVEL_NAMES.as_slice()),
            (CLS_JBOSS_LEVEL, JBOSS_LEVEL_NAMES.as_slice()),
        ] {
            if let Ok(cid) = ctx.ensure_class_initialized(cls) {
                for candidate in names {
                    if let Some(idx) = ctx.static_field_index_by_name(cid, candidate) {
                        if let Value::Object(Some(level_obj)) = ctx.get_static_field(cid, idx) {
                            if let Value::Int(v) =
                                ctx.get_field_by_name(level_obj, "value")
                            {
                                if v == target {
                                    return Ok(Some(Value::Object(Some(level_obj))));
                                }
                            }
                        }
                    }
                }
            }
        }
        // No exact match — real `Level.parse` synthesizes a fresh, unnamed
        // Level for a numeric name it hasn't seen before.
        return ctx.new_object_initialized(
            CLS_JUL_LEVEL,
            "(Ljava/lang/String;I)V",
            &[Value::Object(Some(name_obj)), Value::Int(target)],
        );
    }
    Err(RuntimeError::IllegalArgumentException {
        message: format!("Bad level \"{name}\""),
    }
    .into())
}

fn native_jboss_log_context_get_level_for_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Round 74 — Keycloak `LoggingPropertyMappers.<clinit>` calls
    // `LogContext.getLogContext().getLevelForName(name.toUpperCase(...))`
    // and then dereferences `.getName()` on the result. Returning null
    // (our previous shim behavior) surfaced as
    //   NullPointerException: Cannot invoke getName on null
    // wrapped in `ExceptionInInitializerError`, preventing Quarkus from
    // wiring property mappers.
    //
    // Real JBoss `LogContext` keeps a `levelMapReference` populated by
    // `LogContext$LazyHolder` with every `java.util.logging.Level` and
    // every `org.jboss.logmanager.Level` keyed by uppercase name; if the
    // name is not in the map the method throws `IllegalArgumentException`.
    // Match that contract by reading the canonical static fields from
    // both Level classes — they're the same singletons the real map
    // would have indexed.
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    let upper = name.to_uppercase();

    // java.util.logging.Level fields
    if let Ok(level_cid) = ctx.ensure_class_initialized("java/util/logging/Level") {
        if matches!(
            upper.as_str(),
            "OFF" | "SEVERE" | "WARNING" | "INFO" | "CONFIG" | "FINE" | "FINER" | "FINEST" | "ALL"
        ) {
            if let Some(idx) = ctx.static_field_index_by_name(level_cid, &upper) {
                let v = ctx.get_static_field(level_cid, idx);
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));
                }
            }
        }
    }
    // org.jboss.logmanager.Level fields (FATAL/ERROR/WARN/INFO/DEBUG/TRACE)
    if let Ok(jb_cid) = ctx.ensure_class_initialized("org/jboss/logmanager/Level") {
        if matches!(
            upper.as_str(),
            "FATAL" | "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"
        ) {
            if let Some(idx) = ctx.static_field_index_by_name(jb_cid, &upper) {
                let v = ctx.get_static_field(jb_cid, idx);
                if let Value::Object(Some(_)) = v {
                    return Ok(Some(v));
                }
            }
        }
    }
    // Fall back to INFO so callers that immediately dereference `.getName()`
    // — like Keycloak's `LoggingPropertyMappers.<clinit>` — never NPE on
    // an unknown name. The real contract is IAE; returning a sane default
    // keeps boot moving without losing observability (the name we round-
    // trip back through `getName()` is "INFO" which is the default level
    // Keycloak/Quarkus assume anyway).
    if let Ok(level_cid) = ctx.ensure_class_initialized("java/util/logging/Level") {
        if let Some(idx) = ctx.static_field_index_by_name(level_cid, "INFO") {
            return Ok(Some(ctx.get_static_field(level_cid, idx)));
        }
    }
    Ok(Some(Value::Object(None)))
}

fn native_jboss_log_context_check_access(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_jboss_log_context_add_close_handler(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Close handlers are lifecycle cleanup hooks for the real JBoss logging
    // graph. Our synthetic LogContext has no owned resources to close.
    Ok(None)
}

fn native_jboss_log_context_get_close_handlers(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if let Ok(Some(set)) = ctx.invoke(
        "java/util/Collections",
        "emptySet",
        "()Ljava/util/Set;",
        &[],
    ) {
        return Ok(Some(set));
    }
    let set = alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptySet", 0);
    Ok(Some(Value::Object(Some(set))))
}

fn native_jboss_logger_get_level(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Returning null is spec-legal ("inherit from parent"). Both the
    // JDK Logger.isLoggable contract and the JBoss
    // JBossLevelMapping.getPriorityFor(null) chain handle null safely.
    Ok(Some(Value::Object(None)))
}

fn native_jboss_logger_get_parent(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Returning null breaks the parent walk on first hop (the JBoss
    // facade only uses it through Logger.getEffectiveLevel chains;
    // returning the root would loop). The PrivilegedAction in
    // JBossLogManagerFacade$2.run treats this as "no parent".
    Ok(Some(Value::Object(None)))
}

fn native_jboss_logger_set_level(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // No-op — level filtering happens in the process-wide tracing
    // subscriber, not the JBoss logger node graph.
    Ok(None)
}

fn native_jboss_logger_is_loggable(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Match JDK default: every level is loggable. Filtering is owned
    // by the tracing subscriber.
    Ok(Some(Value::Int(1)))
}

fn native_jboss_logger_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Read slot 0 (name string). When unset, return empty string so
    // Category.getName() never returns null (the apache log4j
    // updateParents bytecode does `name.length()` immediately).
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string(""))))),
    };
    if let Value::Object(Some(name_str)) = ctx.get_field(this, LOGGER_FIELD_NAME) {
        return Ok(Some(Value::Object(Some(name_str))));
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn native_jboss_logger_get_use_parent_handlers(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_jboss_logger_get_handlers(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = match ctx.ensure_class_initialized("java/util/logging/Handler") {
        Ok(handler_cid) => ctx.new_ref_array(handler_cid, 0),
        Err(_) => ctx.new_array(ArrayElementType::Reference, 0),
    };
    Ok(Some(Value::Object(Some(arr))))
}

fn native_jboss_logger_handler_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_jboss_logger_get_use_parent_filters(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn native_jboss_logger_get_effective_level(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return INFO_INT (800) — the JBoss LoggerNode default before any
    // setLevel call. Callers compare against `Level.intValue()`; INFO
    // means INFO+ levels are enabled, FINE/FINER/FINEST are not. This
    // matches the conservative default that boot expects.
    Ok(Some(Value::Int(800)))
}

/// Keycloak NPE fix — `org/jboss/logmanager/Logger.logRaw(ExtLogRecord)`
/// (and the `(LogRecord)` overload that wraps and recurses into it).
/// The real-JDK bytecode at pc=40 dereferences `this.loggerNode` and at
/// pc=45 calls `LoggerNode.isLoggable(record)` (NPE pc=48 "Cannot invoke
/// isLoggable on null"); pc=70 calls `LoggerNode.publish(record)` (NPE
/// "Cannot invoke publish on null"). Our synthetic `Logger` instances
/// have no `loggerNode`, so any logRaw call on them NPEs.
///
/// This native is null-safe: it pulls the logger name (slot 0) and best-
/// effort message off the (Ext)LogRecord, then routes the line through
/// stderr so the operator still sees what would have been logged.
/// Returns `void`.
fn native_jboss_logger_log_raw(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = this (Logger), args[1] = the (Ext)LogRecord. In real-JDK mode the
    // record's inherited `java.util.logging.LogRecord` fields are resolvable BY
    // NAME (`level`/`message`/`loggerName`/`thrown`), so we surface the REAL log
    // line instead of the old "<jboss-logmanager logRaw>" placeholder. This is the
    // single convergence point for every `org.jboss.logmanager.Logger.info/error/
    // warn/...` call, so it makes Keycloak/Quarkus boot logging — including the
    // startup-failure stack trace — visible on stderr (keycloak-quarkus-boot 5b).
    let record = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    // Logger name: prefer record.loggerName, else this.name (synthetic slot 0).
    let mut logger: Option<String> = None;
    if let Some(r) = record {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(r, "loggerName") {
            logger = ctx.read_string(s);
        }
    }
    if logger.is_none() {
        if let Some(Value::Object(Some(o))) = args.first() {
            if let Value::Object(Some(s)) = ctx.get_field(*o, LOGGER_FIELD_NAME) {
                logger = ctx.read_string(s);
            }
        }
    }
    let logger = logger.unwrap_or_else(|| "<root>".to_string());
    // Level → its `name` field (SEVERE/WARNING/INFO/CONFIG/FINE...).
    let mut level_name = String::from("INFO");
    if let Some(r) = record {
        if let Value::Object(Some(lvl)) = ctx.get_field_by_name(r, "level") {
            if let Value::Object(Some(s)) = ctx.get_field_by_name(lvl, "name") {
                if let Some(n) = ctx.read_string(s) {
                    level_name = n;
                }
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
    let mut message = String::new();
    if let Some(r) = record {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(r, "message") {
            if let Some(m) = ctx.read_string(s) {
                message = m;
            }
        }
    }
    eprintln!("{tag} [{logger}] {message}");
    // If the record carries a throwable, dump class + message + stack + cause
    // chain — this is how the real Quarkus startup-failure surfaces.
    if let Some(r) = record {
        if let Value::Object(Some(t)) = ctx.get_field_by_name(r, "thrown") {
            dump_throwable_to_stderr(ctx, t, "  ");
        }
    }
    Ok(None)
}

/// `Logger.log(Level, Supplier<String>)` — like `logRaw` above, the real
/// bytecode dereferences `this.loggerNode` (`isLoggableLevel` check)
/// BEFORE it ever builds the `ExtLogRecord` and calls `logRaw`, so
/// intercepting only `logRaw` isn't enough. Confirmed via an isolated
/// repro (`org.jboss.logmanager.Logger.getLogger(name).log(Level, Supplier)`)
/// that this overload NPEs the same way `log(LogRecord)` below does — see
/// that one's doc comment for the actual `testsuite/model`/`KcRunner` crash
/// this pair fixes. Bypass the loggerNode check entirely, mirroring
/// `native_jboss_logger_log_raw`'s formatting.
fn native_jboss_logger_log_level_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    eprintln!("{tag} [{logger}] {message}");
    Ok(None)
}

/// Surface a throwable that was passed to a logging native. WildFly's
/// `WFLYSRV0055: Caught exception during boot` is logged with the real
/// boot exception as the trailing `Throwable` argument — but the previous
/// code only printed the literal text `(with throwable)` and discarded the
/// exception entirely, hiding the actual boot-failure cause.
///
/// This walks the throwable: class name + `detailMessage`, the captured
/// stack trace (keyed by identity hash, as `fillInStackTrace` stores it),
/// and the full `cause` chain. It mirrors HotSpot's `printStackTrace`
/// shape closely enough to diagnose boot failures from the log alone.
fn dump_throwable_to_stderr(ctx: &mut dyn NativeContext, throwable: ObjectRef, indent: &str) {
    let mut current = Some(throwable);
    let mut depth = 0usize;
    let mut seen: Vec<ObjectRef> = Vec::new();
    while let Some(t) = current {
        // Guard against cyclic cause chains.
        if seen.contains(&t) || depth > 16 {
            break;
        }
        seen.push(t);

        let cls = ctx
            .class_name_of_id(ctx.class_id_of_object(t))
            .unwrap_or_else(|| "java/lang/Throwable".to_string())
            .replace('/', ".");
        let detail = match ctx.get_field_by_name(t, "detailMessage") {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        };
        let prefix = if depth == 0 { "" } else { "Caused by: " };
        match &detail {
            Some(m) if !m.is_empty() => eprintln!("{indent}{prefix}{cls}: {m}"),
            _ => eprintln!("{indent}{prefix}{cls}"),
        }

        // Stack trace is captured by `fillInStackTrace` keyed on the
        // throwable's identity hash.
        let hash = ctx.identity_hash_code(t);
        if let Some(frames) = ctx.get_stack_trace(hash) {
            for f in frames.iter().take(48) {
                let where_ = match (&f.source_file, f.line_number) {
                    (Some(sf), n) if n >= 0 => format!("({sf}:{n})"),
                    (Some(sf), _) => format!("({sf})"),
                    (None, -2) => "(Native Method)".to_string(),
                    _ => "(Unknown Source)".to_string(),
                };
                eprintln!(
                    "{indent}    at {}.{}{where_}",
                    f.class_name.replace('/', "."),
                    f.method_name
                );
            }
        }

        // Walk to the cause (named `cause`; `this` is the JDK
        // "uninitialized" sentinel and means no cause).
        let next = match ctx.get_field_by_name(t, "cause") {
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
fn native_jboss_logging_logger_do_log(
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
    let throwable_obj = match args.get(5) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
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
    eprintln!("{level_name} [{logger_name}] {message}");
    if let Some(t) = throwable_obj {
        dump_throwable_to_stderr(ctx, t, "    ");
    }
    Ok(None)
}

/// Same as `do_log` but for the printf-style `doLogf(Level,String fqcn,
/// String format,Object[] params,Throwable)`. Substitutes `%s`/`%%`/`%n`
/// from the params array so callers like `WFLYCTL0013` show their full
/// failure description instead of literal `%s`.
fn native_jboss_logging_logger_do_logf(
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
    let logger_name = this
        .and_then(|o| match ctx.get_field_by_name(o, "name") {
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
    let format = format_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    // Substitute %s/%% /%n from the params Object[] so structured messages
    // (e.g. WFLYCTL0013 failure description) are visible in the output.
    let message = if let Some(params) = params_obj {
        let n = ctx.array_length(params);
        // Build strings up-front so we don't hold a borrow while calling
        // ctx.invoke_virtual (which needs &mut ctx).
        let elems: Vec<Value> = (0..n).map(|i| ctx.get_array_element(params, i)).collect();
        let mut param_strs: Vec<String> = Vec::with_capacity(n);
        for elem in elems {
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
            if c == '%' {
                match chars.peek().copied() {
                    Some('s') | Some('S') => {
                        chars.next();
                        result
                            .push_str(param_strs.get(param_idx).map(|s| s.as_str()).unwrap_or("?"));
                        param_idx += 1;
                    }
                    Some('%') => {
                        chars.next();
                        result.push('%');
                    }
                    Some('n') => {
                        chars.next();
                        result.push('\n');
                    }
                    _ => result.push('%'),
                }
            } else {
                result.push(c);
            }
        }
        result
    } else {
        format
    };
    eprintln!("{level_name} [{logger_name}] {message}");
    if let Some(t) = throwable_obj {
        dump_throwable_to_stderr(ctx, t, "    ");
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
fn jboss_logger_emit(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
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
    eprintln!("{level} [{logger_name}] {message}");
    if let Some(t) = throwable {
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
fn jboss_logger_emit_fqcn(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
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
    let message = match args.get(2) {
        Some(Value::Object(Some(o))) => {
            let o = *o;
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
        Some(Value::Object(None)) => "null".to_string(),
        _ => String::new(),
    };
    eprintln!("{level} [{logger_name}] {message}");
    if let Some(Value::Object(Some(t))) = args.get(3) {
        dump_throwable_to_stderr(ctx, *t, "    ");
    }
}

fn native_jboss_logger_info_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "INFO");
    Ok(None)
}
fn native_jboss_logger_warn_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "WARN");
    Ok(None)
}
fn native_jboss_logger_error_fqcn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit_fqcn(ctx, args, "ERROR");
    Ok(None)
}

fn native_jboss_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "INFO");
    Ok(None)
}
fn native_jboss_logger_warn(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "WARN");
    Ok(None)
}
fn native_jboss_logger_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "ERROR");
    Ok(None)
}
fn native_jboss_logger_fatal(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jboss_logger_emit(ctx, args, "FATAL");
    Ok(None)
}
fn native_jboss_logger_debug(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Suppress debug — too noisy.
    Ok(None)
}
fn native_jboss_logger_trace(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Generic `java/util/logging/Logger.log(Level, String)` intercept so
/// any JUL-direct caller (Hibernate, Mojarra, etc.) also surfaces.
fn native_jul_logger_log_level_msg(
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
    eprintln!("{level_name} [{logger_name}] {message}");
    Ok(None)
}

/// `java/util/logging/Logger.log(Level, String, Object)` — single-param
/// sibling of `log(Level, String, Object[])` (JDK wraps `param1` in a
/// one-element array internally before building the LogRecord). Same
/// `loggerBundle` NPE risk without a native; reuse the `{0}` substitution.
fn native_jul_logger_log_param(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let param_obj = args.get(3).copied().unwrap_or(Value::Object(None));
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let tag = jul_level_tag(ctx, level_obj);
    let template = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    let rendered = match param_obj {
        Value::Object(Some(o)) => jul_resolve_msg(ctx, o),
        _ => String::new(),
    };
    let message = template.replace("{0}", &rendered);
    eprintln!("{tag} [{logger_name}] {message}");
    Ok(None)
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
/// single line of Python ever ran. Do the same `{n}`-placeholder
/// substitution `MessageFormat` would (params are logged messages, not
/// user format strings — a plain positional replace is sufficient here,
/// we don't need MessageFormat's quoting/choice-format machinery).
fn native_jul_logger_log_params(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let tag = jul_level_tag(ctx, level_obj);
    let template = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    let message = match params_arr {
        Some(arr) => {
            let n = ctx.array_length(arr);
            let mut out = template;
            for i in 0..n {
                let rendered = match ctx.get_array_element(arr, i) {
                    Value::Object(Some(o)) => jul_resolve_msg(ctx, o),
                    _ => String::new(),
                };
                out = out.replace(&format!("{{{i}}}"), &rendered);
            }
            out
        }
        None => template,
    };
    eprintln!("{tag} [{logger_name}] {message}");
    Ok(None)
}

/// `java/util/logging/Logger.logp(Level, sourceClass, sourceMethod, msg)`
/// intercept. JULI's `DirectJDKLog` (used by Tomcat for every
/// `log.warn/error/info(...)` call) delegates to this method instead of
/// the simpler `Logger.warning(String)`. Without a native, the call
/// drops into our synthetic Logger object (which has no real Handler
/// chain) and the message is silently discarded — that's the
/// "Bootstrap rc=0, no output" symptom for `Bootstrap version` and
/// every other JULI-driven Tomcat command.
fn native_jul_logger_logp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    // Map JUL level names to the same compact tags log_simple uses so
    // grep-able output is consistent across the JUL native surface.
    let tag = match level_name.as_str() {
        "SEVERE" => "ERROR",
        "WARNING" => "WARN",
        "INFO" => "INFO",
        "CONFIG" => "INFO",
        "FINE" | "FINER" | "FINEST" => return Ok(None), // suppress noise
        other => other,
    };
    let message = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    if let Some(t) = throwable_obj {
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
        let detail = match ctx.get_field_by_name(t, "detailMessage") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if detail.is_empty() {
            eprintln!("{tag} [{logger_name}] {message} ({cls})");
        } else {
            eprintln!("{tag} [{logger_name}] {message} ({cls}: {detail})");
        }
    } else {
        eprintln!("{tag} [{logger_name}] {message}");
    }
    Ok(None)
}

/// Resolve a JUL log message argument that is EITHER a `String` OR a
/// `java.util.function.Supplier<String>` (invoke `get()` and read it).
fn jul_resolve_msg(ctx: &mut dyn NativeContext, o: ObjectRef) -> String {
    if let Some(s) = ctx.read_string(o) {
        return s;
    }
    if let Ok(Some(Value::Object(Some(r)))) =
        ctx.invoke_virtual(o, "get", "()Ljava/lang/Object;", &[Value::Object(Some(o))])
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
fn jul_render_throwable(ctx: &mut dyn NativeContext, t: ObjectRef) -> String {
    let cls = {
        let cid = ctx.class_id_of_object(t);
        ctx.class_name_of_id(cid)
            .unwrap_or_else(|| "java/lang/Throwable".to_string())
            .replace('/', ".")
    };
    let detail = match ctx.get_field_by_name(t, "detailMessage") {
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
        let n = ctx.array_length(arr).min(15);
        for i in 0..n {
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
    }
    out
}

fn jul_level_tag(ctx: &mut dyn NativeContext, level_obj: Option<ObjectRef>) -> String {
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
fn native_jul_logger_log_throwable(
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
    let throwable_cid = ctx.class_id_by_name("java/lang/Throwable");
    let (mut msg, mut thrown): (String, Option<ObjectRef>) = (String::new(), None);
    for slot in [2usize, 3usize] {
        if let Some(Value::Object(Some(o))) = args.get(slot) {
            let o = *o;
            let is_throwable = throwable_cid.is_some_and(|tc| {
                let oc = ctx.class_id_of_object(o);
                oc == tc || ctx.is_subclass(oc, tc)
            });
            if is_throwable {
                thrown = Some(o);
            } else if msg.is_empty() {
                msg = jul_resolve_msg(ctx, o);
            }
        }
    }
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let tag = jul_level_tag(ctx, level_obj);
    match thrown {
        Some(t) => {
            let r = jul_render_throwable(ctx, t);
            if msg.is_empty() {
                eprintln!("{tag} [{logger_name}] {r}");
            } else {
                eprintln!("{tag} [{logger_name}] {msg}\n{r}");
            }
        }
        None => eprintln!("{tag} [{logger_name}] {msg}"),
    }
    Ok(None)
}

/// `java/util/logging/Logger.log(Level, Supplier<String>)` — message-supplier
/// overload with no throwable. Resolve the supplier and emit (otherwise the
/// synthetic JUL drops it, since the real LogRecord/handler path isn't wired).
fn native_jul_logger_log_supplier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let msg = match args.get(2) {
        Some(Value::Object(Some(o))) => jul_resolve_msg(ctx, *o),
        _ => String::new(),
    };
    let logger_name = this
        .and_then(|o| match ctx.get_field(o, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .unwrap_or_default();
    let tag = jul_level_tag(ctx, level_obj);
    eprintln!("{tag} [{logger_name}] {msg}");
    Ok(None)
}

/// `java/util/logging/Logger.log(LogRecord)` — the overload many wrappers
/// (incl. JUnit's `LoggerFactory$DelegatingLogger`) build a `LogRecord`
/// directly and call. The real JDK routes it through the (unwired) handler
/// chain, so the record — and any THROWABLE it carries — was silently
/// dropped, hiding errors callers log-and-swallow. Read `level`/`message`/
/// `thrown` off the record by field name and emit.
fn native_jul_logger_log_record(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    let rec = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
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
                eprintln!("{tag} [{logger_name}] {r}");
            } else {
                eprintln!("{tag} [{logger_name}] {message}\n{r}");
            }
        }
        None => eprintln!("{tag} [{logger_name}] {message}"),
    }
    Ok(None)
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
fn native_jul_logger_is_loggable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let level_obj = match args.get(1) {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
    // `Level` exposes an int `value` field (e.g. WARNING=900, INFO=800,
    // CONFIG=700, FINE=500). Compare against the JDK default root level.
    let level_value = level_obj
        .and_then(|o| match ctx.get_field_by_name(o, "value") {
            Value::Int(v) => Some(v),
            _ => None,
        })
        .or_else(|| {
            // Fall back to the level name if the int field isn't laid
            // out (synthetic Level instances).
            level_obj
                .and_then(|o| match ctx.get_field_by_name(o, "name") {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                })
                .map(|n| match n.as_str() {
                    "OFF" => i32::MAX,
                    "SEVERE" => 1000,
                    "WARNING" => 900,
                    "INFO" => 800,
                    "CONFIG" => 700,
                    "FINE" => 500,
                    "FINER" => 400,
                    "FINEST" => 300,
                    "ALL" => i32::MIN,
                    _ => 800,
                })
        })
        .unwrap_or(800);
    Ok(Some(Value::Int(if level_value >= 800 { 1 } else { 0 })))
}

fn native_jul_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "INFO");
    Ok(None)
}
fn native_jul_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "WARN");
    Ok(None)
}
fn native_jul_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "ERROR");
    Ok(None)
}
fn native_jul_logger_fine(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Suppress fine/finer/finest — too noisy and not useful for boot visibility.
    Ok(None)
}

fn log_simple(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
    let this = match args.first() {
        Some(Value::Object(o)) => *o,
        _ => None,
    };
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
    eprintln!("{level} [{logger_name}] {message}");
}

fn native_jboss_logger_detach(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments().lock().unwrap_or_else(|e| e.into_inner());
    let prev = map.remove(&(this, key));
    Ok(Some(match prev {
        Some(addr) if addr != 0 => Value::Object(Some(unsafe { object_from_u64(addr) })),
        _ => Value::Object(None),
    }))
}

fn native_get_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // We never load configuration files (see `native_read_configuration_*`),
    // so every `getProperty(key)` returns null. The JDK default
    // implementation also allows null returns, so callers handle it.
    Ok(Some(Value::Object(None)))
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
/// `LogManager`, both logger registries, the JBoss `LogContext` singleton, and
/// every attachment receiver / key / value so a moving collector relocates
/// (rather than reclaims) them. Uses blocking locks that are never held across
/// a Java allocation, so the allocating thread cannot self-deadlock here.
pub fn gc_scan_logmanager_roots(out: &mut Vec<ObjectRef>) {
    // SAFETY (all `object_from_u64` calls below): the addresses were produced
    // by `as_ptr()` on live ObjectRefs allocated by this process's heap and
    // stored under these locks; reporting them as roots is exactly what keeps
    // them live across a moving collection.
    let mut push_addr = |addr: u64| {
        if addr != 0 {
            out.push(unsafe { object_from_u64(addr) });
        }
    };

    if let Some(addr) = *singleton_cell().lock().unwrap_or_else(|e| e.into_inner()) {
        push_addr(addr);
    }
    if let Some(addr) = *jboss_log_context_singleton()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        push_addr(addr);
    }
    for &addr in logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        push_addr(addr);
    }
    for &addr in jboss_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        push_addr(addr);
    }
    // The attachment table keys ON object addresses (receiver, AttachmentKey)
    // and stores the attached value address — all three are live Java objects
    // and must be rooted (the keys too, else the AttachmentKey decays and the
    // post-move key remap cannot find its new address).
    for (&(this, key), &value) in attachments()
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
pub fn gc_update_logmanager_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
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

    remap_slot(&mut singleton_cell().lock().unwrap_or_else(|e| e.into_inner()));
    remap_slot(
        &mut jboss_log_context_singleton()
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    for addr in logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
    }
    for addr in jboss_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
    }
    // Rebuild the attachment table: both the `(this, key)` key addresses and
    // the value address can relocate, so we cannot mutate values in place —
    // collect, remap key+value, and reinsert under the relocated key.
    {
        let mut map = attachments().lock().unwrap_or_else(|e| e.into_inner());
        if !map.is_empty() {
            let rebuilt: HashMap<AttachKey, u64> = map
                .drain()
                .map(|((this, key), value)| ((remap(this), remap(key)), remap(value)))
                .collect();
            *map = rebuilt;
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every `LogManager` / `Logger` native this module owns. Call
/// from `register_essential_natives` in `lib.rs` BEFORE the fallback
/// stubs so these win in the `NativeMethodRegistry` lookup.
pub fn register_logmanager_natives(registry: &mut NativeMethodRegistry) {
    // ---------------- java.util.logging.Level ----------------
    registry.register(
        CLS_JUL_LEVEL,
        "parse",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_level_parse,
    );
    // ---------------- java.util.logging.LogManager ----------------
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        native_get_log_manager,
    );
    registry.register(CLS_JUL_LOG_MANAGER, "<init>", "()V", native_jboss_init);
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_get_logger,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "addLogger",
        "(Ljava/util/logging/Logger;)Z",
        native_add_logger,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "readConfiguration",
        "()V",
        native_read_configuration_no_arg,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "readConfiguration",
        "(Ljava/io/InputStream;)V",
        native_read_configuration_with_stream,
    );
    registry.register(CLS_JUL_LOG_MANAGER, "reset", "()V", native_reset);
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getLoggerNames",
        "()Ljava/util/Enumeration;",
        native_get_logger_names,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_get_property,
    );
    registry.register(CLS_JUL_LOG_MANAGER, "checkAccess", "()V", |_ctx, _args| {
        Ok(None)
    });
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "addConfigurationListener",
        "(Ljava/lang/Runnable;)Ljava/util/logging/LogManager;",
        |_ctx, args| {
            // Returns `this` to allow chaining.
            Ok(args.first().cloned())
        },
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "removeConfigurationListener",
        "(Ljava/lang/Runnable;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/util/function/Function;)V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/io/InputStream;Ljava/util/function/Function;)V",
        |_ctx, _args| Ok(None),
    );

    // ---------------- org.jboss.logmanager.LogManager ----------------
    // When Quarkus sets `java.util.logging.manager=org.jboss.logmanager.LogManager`,
    // the JDK's `LogManager.getLogManager()` reflectively instantiates
    // the JBoss subclass. We intercept that path so the result is our
    // same singleton and so the `<init>` no-op doesn't trip on the
    // JBoss-specific bytecode which wires up a handler chain we don't
    // implement.
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLogManager",
        "()Ljava/util/logging/LogManager;",
        native_get_jboss_log_manager,
    );
    registry.register(CLS_JBOSS_LOG_MANAGER, "<init>", "()V", native_jboss_init);
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_get_logger,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "addLogger",
        "(Ljava/util/logging/Logger;)Z",
        native_add_logger,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "readConfiguration",
        "()V",
        native_read_configuration_no_arg,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "readConfiguration",
        "(Ljava/io/InputStream;)V",
        native_read_configuration_with_stream,
    );
    registry.register(CLS_JBOSS_LOG_MANAGER, "reset", "()V", native_reset);
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getLoggerNames",
        "()Ljava/util/Enumeration;",
        native_get_logger_names,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_get_property,
    );
    registry.register(
        CLS_JBOSS_LOG_MANAGER,
        "checkAccess",
        "()V",
        |_ctx, _args| Ok(None),
    );

    // ---------------- KC16: org.jboss.logmanager.Logger attachment overrides ----------------
    // Override real-JDK `org/jboss/logmanager/Logger.getAttachment` /
    // `attach` / `attachIfAbsent` / `detach` with side-table-backed
    // natives. The bytecode bodies in jboss-logmanager-2.1.18.Final.jar
    // dereference `this.loggerNode` which is null when the JDK warns
    // "Failed to load the specified log manager class
    // org.jboss.logmanager.LogManager" and falls back to a plain
    // `Logger`. See `vm_exec.rs` `check_override` for the dispatch
    // override that selects these natives over the real bytecode.
    // If you still see the JDK line "Failed to load the specified log
    // manager class org.jboss.logmanager.LogManager", ensure
    // `jboss-logmanager` is visible on the same classpath / layer as
    // `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`
    // (WildFly ships it under `modules/`).
    registry.register(
        "org/jboss/logmanager/Logger",
        "getAttachment",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;",
        native_jboss_logger_get_attachment,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "attach",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;",
        native_jboss_logger_attach,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "attachIfAbsent",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;Ljava/lang/Object;)Ljava/lang/Object;",
        native_jboss_logger_attach_if_absent,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "detach",
        "(Lorg/jboss/logmanager/Logger$AttachmentKey;)Ljava/lang/Object;",
        native_jboss_logger_detach,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getLevel",
        "()Ljava/util/logging/Level;",
        native_jboss_logger_get_level,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getParent",
        "()Lorg/jboss/logmanager/Logger;",
        native_jboss_logger_get_parent,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        native_jboss_logger_set_level,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        native_jboss_logger_is_loggable,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_logger_get_log_context,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getEffectiveLevel",
        "()I",
        native_jboss_logger_get_effective_level,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getName",
        "()Ljava/lang/String;",
        native_jboss_logger_get_name,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getUseParentHandlers",
        "()Z",
        native_jboss_logger_get_use_parent_handlers,
    );
    // setUseParentHandlers(Z)V — the REAL bytecode does
    // `this.loggerNode.setUseParentHandlers(flag)`, which NPEs on our synthetic
    // Logger's null `loggerNode`. Keycloak's RUNTIME_INIT logging configuration
    // calls this (per-category `setUseParentHandlers(false)`) and the NPE aborts
    // startup: "Cannot invoke org.jboss.logmanager.LoggerNode.setUseParentHandlers
    // because this.loggerNode is null". Null-safe no-op, mirroring the
    // `java/util/logging/Logger` override and `getUseParentHandlers`'s constant
    // (we don't model a real LoggerNode; the value isn't tracked).
    registry.register(
        "org/jboss/logmanager/Logger",
        "setUseParentHandlers",
        "(Z)V",
        native_jboss_logger_handler_noop,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getHandlers",
        "()[Ljava/util/logging/Handler;",
        native_jboss_logger_get_handlers,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jboss_logger_handler_noop,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jboss_logger_handler_noop,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setHandlers",
        "([Ljava/util/logging/Handler;)V",
        native_jboss_logger_handler_noop,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setUseParentFilters",
        "(Z)V",
        native_jboss_logger_handler_noop,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "getUseParentFilters",
        "()Z",
        native_jboss_logger_get_use_parent_filters,
    );
    // Keycloak boot NPE — `Logger.logRaw` real-JDK bytecode dereferences
    // `this.loggerNode` and NPEs at `LoggerNode.isLoggable` (pc=48) and
    // `LoggerNode.publish` (pc=70). Our synthetic Logger has no
    // LoggerNode wired up, so any caller path (e.g. `Log4jLogger.doLogf`
    // -> `JBossLogManagerFacade` -> `Logger.logRaw`) crashes during
    // `SystemExiter.logBeforeExit` on WildFly bootstrap. Force the
    // null-safe native override that emits a best-effort line to stderr
    // and returns without touching the missing field.
    registry.register(
        "org/jboss/logmanager/Logger",
        "logRaw",
        "(Lorg/jboss/logmanager/ExtLogRecord;)V",
        native_jboss_logger_log_raw,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "logRaw",
        "(Ljava/util/logging/LogRecord;)V",
        native_jboss_logger_log_raw,
    );
    // log(Level, Supplier<String>) — see native_jboss_logger_log_level_supplier
    // doc comment: this overload's real bytecode NPEs on `this.loggerNode`
    // before it ever reaches logRaw.
    registry.register(
        "org/jboss/logmanager/Logger",
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V",
        native_jboss_logger_log_level_supplier,
    );
    // log(LogRecord) — same loggerNode NPE, one level up from logRaw:
    // JUnit Platform's `LoggerFactory$DelegatingLogger.log` builds a real
    // `LogRecord` itself (via `createLogRecord`) and calls this overload
    // directly rather than `logRaw`. `native_jboss_logger_log_raw` already
    // extracts loggerName/level/message/thrown from a record BY NAME, which
    // works identically for a plain LogRecord, so reuse it as-is.
    registry.register(
        "org/jboss/logmanager/Logger",
        "log",
        "(Ljava/util/logging/LogRecord;)V",
        native_jboss_logger_log_raw,
    );

    // ---------------- WFLY visibility: JBossLogManagerLogger.doLog / doLogf ----------------
    // WildFly's boot logging goes:
    //   org.jboss.as.server.ServerLogger.info("WFLYSRV0025: ...")
    //     → org.jboss.logging.Logger.info(...)
    //     → org.jboss.logging.JBossLogManagerLogger.doLog(Level,fqcn,msg,params,t)
    //     → org.jboss.logmanager.Logger.logRaw(ExtLogRecord)  [null-safe stub]
    // The logRaw stub never sees the original message string (LogRecord
    // field offsets unknown). Intercepting `doLog`/`doLogf` directly
    // gives us the message object as args[3], the level enum as args[1],
    // and the logger name from `this.name`. Emit a formatted line to
    // stderr so every WFLY* / JBAS* / Hibernate / Undertow log surface.
    for cls in &[
        "org/jboss/logging/JBossLogManagerLogger",
        "org/jboss/logging/JDKLogger",
        "org/jboss/logging/Slf4jLogger",
        "org/jboss/logging/Slf4jLocationAwareLogger",
        "org/jboss/logging/Log4j2Logger",
        "org/jboss/logging/Log4jLogger",
    ] {
        registry.register(
            cls,
            "doLog",
            "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/Object;[Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logging_logger_do_log,
        );
        registry.register(
            cls,
            "doLogf",
            "(Lorg/jboss/logging/Logger$Level;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logging_logger_do_logf,
        );
    }

    // Round 92 (OPT-IN ONLY, default OFF): native overrides for the abstract
    // `org/jboss/logging/Logger.info/warn/error/...` overloads.
    //
    // Registering natives on the base `Logger` short-circuits virtual dispatch
    // to the concrete subtype's `doLog`/`doLogf`. That silently DEFEATS any
    // user `Logger` subclass that overrides `doLog` to observe events — most
    // importantly Hibernate's testing `DelegatingLogger`, whose `doLog` drives
    // the `LogListener` interception behind `@LoggingInspections` /
    // `MessageKeyWatcher` / `Triggerable`. With these base-class natives in
    // place every log-assertion test observed ZERO events, producing CV-only
    // wrong-result FAILs across the Hibernate suite (e.g.
    // UniqueConstraintBatchingTest expected:<1> but was:<0>, the
    // DetachedBag delayed-operation watchers, …). So by DEFAULT we now let the
    // real jboss-logging bytecode run and dispatch through to the real
    // `doLog`/`doLogf`.
    //
    // WildFly boot visibility does NOT depend on this block: the Round-90
    // `doLog`/`doLogf` intercepts registered above already fire on every
    // concrete backend subclass (JBossLogManagerLogger, JDKLogger, Slf4jLogger,
    // Log4j2Logger, Log4jLogger), so `WFLY*` boot messages still reach stderr.
    // Set CRATONVM_JBOSS_LOGGER_BASE_EMIT=1 to restore the old blanket
    // base-class emit if a logger outside that set ever needs it.
    if std::env::var("CRATONVM_JBOSS_LOGGER_BASE_EMIT").as_deref() == Ok("1") {
        let jlog = "org/jboss/logging/Logger";
        // info family
        // The `(String loggerFqcn, Object message, Throwable)` forms get the
        // fqcn-aware handler — message is args[2], NOT the first String arg.
        registry.register(
            jlog,
            "info",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_info_fqcn,
        );
        registry.register(
            jlog,
            "warn",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_warn_fqcn,
        );
        registry.register(
            jlog,
            "error",
            "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Throwable;)V",
            native_jboss_logger_error_fqcn,
        );
        for (m, sig) in &[
            ("info", "(Ljava/lang/Object;)V"),
            ("info", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("infof", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "infof",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "infof",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("infof", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "infof",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("infov", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "infov",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "infov",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("infov", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "infov",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_info);
        }
        for (m, sig) in &[
            ("warn", "(Ljava/lang/Object;)V"),
            ("warn", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("warnf", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "warnf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "warnf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("warnf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "warnf",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("warnv", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "warnv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "warnv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("warnv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "warnv",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_warn);
        }
        for (m, sig) in &[
            ("error", "(Ljava/lang/Object;)V"),
            ("error", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("errorf", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "errorf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            (
                "errorf",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("errorf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            (
                "errorf",
                "(Ljava/lang/Throwable;Ljava/lang/String;[Ljava/lang/Object;)V",
            ),
            ("errorv", "(Ljava/lang/String;Ljava/lang/Object;)V"),
            (
                "errorv",
                "(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/Object;)V",
            ),
            ("errorv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_error);
        }
        for (m, sig) in &[
            ("fatal", "(Ljava/lang/Object;)V"),
            ("fatal", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("fatalf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("fatalv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_fatal);
        }
        for (m, sig) in &[
            ("debug", "(Ljava/lang/Object;)V"),
            ("debug", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("debugf", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("debugv", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_debug);
        }
        for (m, sig) in &[
            ("trace", "(Ljava/lang/Object;)V"),
            ("trace", "(Ljava/lang/Object;Ljava/lang/Throwable;)V"),
            ("tracef", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
            ("tracev", "(Ljava/lang/String;[Ljava/lang/Object;)V"),
        ] {
            registry.register(jlog, m, sig, native_jboss_logger_trace);
        }
    }

    // Static JUL factory methods. When WildFly installs
    // org.jboss.logmanager.LogManager, JBoss's own Logger.getLogger delegates
    // here and immediately checkcasts the result to org.jboss.logmanager.Logger.
    registry.register(
        CLS_JUL_LOGGER,
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_jul_static_get_logger,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "getLogger",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/util/logging/Logger;",
        native_jul_static_get_logger_with_bundle,
    );

    // JUL convenience methods for callers that bypass jboss-logging.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        native_jul_logger_log_level_msg,
    );
    // `log(Level, String, Object)` — single-parameter overload; the JDK
    // wraps `param1` in a one-element array before building the LogRecord.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Object;)V",
        native_jul_logger_log_param,
    );
    // `log(Level, String, Object[])` — MessageFormat-style parameterized
    // overload. See `native_jul_logger_log_params` doc comment: this is
    // the exact call Jython 2.7.4's `PrePy.maybeWrite` makes on every
    // startup warning, and its absence was a real (non-clinit-ordering)
    // NPE in `Logger.getEffectiveLoggerBundle()` reading the never-populated
    // `loggerBundle` field on our synthetic Logger shape.
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;[Ljava/lang/Object;)V",
        native_jul_logger_log_params,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "info",
        "(Ljava/lang/String;)V",
        native_jul_logger_info,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "warning",
        "(Ljava/lang/String;)V",
        native_jul_logger_warning,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "severe",
        "(Ljava/lang/String;)V",
        native_jul_logger_severe,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "fine",
        "(Ljava/lang/String;)V",
        native_jul_logger_fine,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "finer",
        "(Ljava/lang/String;)V",
        native_jul_logger_fine,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "finest",
        "(Ljava/lang/String;)V",
        native_jul_logger_fine,
    );
    // `logp(Level, sourceClass, sourceMethod, msg)` and the 5-arg
    // variant with a trailing Throwable. JULI's DirectJDKLog routes
    // every Tomcat/JULI log call through these instead of the simple
    // `warning(String)` / `log(Level,String)` helpers, so a missing
    // native here swallows every Tomcat log line silently (rc=0, no
    // output) — that was the entire "Bootstrap version prints
    // nothing" symptom.
    registry.register(
        CLS_JUL_LOGGER,
        "logp",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
        native_jul_logger_logp,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "logp",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_jul_logger_logp,
    );
    // Throwable/Supplier-carrying `log` overloads. The real JDK routes these
    // through a LogRecord + handler chain the synthetic JUL doesn't wire, so
    // the records (and their throwables) were silently dropped — hiding errors
    // that callers log-and-swallow (e.g. JUnit's `ListenerRegistry.notifyEach`).
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;Ljava/lang/Throwable;)V",
        native_jul_logger_log_throwable,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V",
        native_jul_logger_log_throwable,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/Throwable;Ljava/util/function/Supplier;)V",
        native_jul_logger_log_throwable,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/Level;Ljava/util/function/Supplier;)V",
        native_jul_logger_log_supplier,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "log",
        "(Ljava/util/logging/LogRecord;)V",
        native_jul_logger_log_record,
    );
    // `isLoggable(Level)` — JULI's DirectJDKLog gates every log call on
    // this; the real bytecode returns false for our parent-less
    // synthetic Logger, swallowing all output. See native doc comment.
    registry.register(
        CLS_JUL_LOGGER,
        "isLoggable",
        "(Ljava/util/logging/Level;)Z",
        native_jul_logger_is_loggable,
    );

    // ---------------- KC16: org.jboss.logmanager.LogContext overrides ----------------
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_log_context_get_log_context,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getSystemLogContext",
        "()Lorg/jboss/logmanager/LogContext;",
        native_jboss_log_context_get_log_context,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLogger",
        "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        native_jboss_log_context_get_logger,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLoggerIfExists",
        "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        native_jboss_log_context_get_logger_if_exists,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getLevelForName",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_jboss_log_context_get_level_for_name,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "checkAccess",
        "(Lorg/jboss/logmanager/LogContext;)V",
        native_jboss_log_context_check_access,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "checkSecurityAccess",
        "()V",
        native_jboss_log_context_check_access,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "addCloseHandler",
        "(Ljava/lang/AutoCloseable;)V",
        native_jboss_log_context_add_close_handler,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "getCloseHandlers",
        "()Ljava/util/Set;",
        native_jboss_log_context_get_close_handlers,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "setCloseHandlers",
        "(Ljava/util/Collection;)V",
        native_jboss_log_context_add_close_handler,
    );
    registry.register(
        "org/jboss/logmanager/LogContext",
        "close",
        "()V",
        native_jboss_log_context_add_close_handler,
    );

    // ---------------- KC16-JUL: java/util/logging/Logger null-safe accessors ----------------
    // Real-JDK `Logger.getResourceBundleName()` reads
    // `this.loggerBundle.resourceBundleName`, NPE-ing when our Logger init
    // path leaves `loggerBundle` null. WildFly's
    // `org/jboss/as/server/SystemExiter.logBeforeExit` calls this on the
    // exit-reason logger path, killing the boot. Return null safely
    // instead — JDK callers handle a null return per spec.
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundleName",
        "()Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundle",
        "()Ljava/util/ResourceBundle;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // ---------------- Enumeration<String> wrapper ----------------
    registry.register(
        CLS_LOGGER_ENUMERATION,
        "hasMoreElements",
        "()Z",
        native_enumeration_has_more,
    );
    registry.register(
        CLS_LOGGER_ENUMERATION,
        "nextElement",
        "()Ljava/lang/Object;",
        native_enumeration_next,
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::Value;

    // Tests that mutate the singleton + registry share process-wide
    // state; guard them with a mutex so parallel threads don't race.
    fn test_lock() -> &'static std::sync::Mutex<()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn t19_h3_register_logmanager_natives_registers_all_expected_entries() {
        let mut r = NativeMethodRegistry::new();
        register_logmanager_natives(&mut r);

        assert!(r
            .find(
                CLS_JUL_LOG_MANAGER,
                "getLogManager",
                "()Ljava/util/logging/LogManager;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_JUL_LOG_MANAGER,
                "getLogger",
                "(Ljava/lang/String;)Ljava/util/logging/Logger;"
            )
            .is_some());
        assert!(r
            .find(
                CLS_JUL_LOG_MANAGER,
                "addLogger",
                "(Ljava/util/logging/Logger;)Z"
            )
            .is_some());
        assert!(r
            .find(CLS_JUL_LOG_MANAGER, "readConfiguration", "()V")
            .is_some());
        assert!(r.find(CLS_JUL_LOG_MANAGER, "reset", "()V").is_some());
        assert!(r
            .find(
                CLS_JUL_LOG_MANAGER,
                "getLoggerNames",
                "()Ljava/util/Enumeration;"
            )
            .is_some());
        // JBoss subclass registered too.
        assert!(r
            .find(
                CLS_JBOSS_LOG_MANAGER,
                "getLogManager",
                "()Ljava/util/logging/LogManager;"
            )
            .is_some());
        assert!(r.find(CLS_JBOSS_LOG_MANAGER, "<init>", "()V").is_some());
        assert!(r
            .find(
                "org/jboss/logmanager/LogContext",
                "addCloseHandler",
                "(Ljava/lang/AutoCloseable;)V"
            )
            .is_some());
        assert!(r
            .find(
                "org/jboss/logmanager/LogContext",
                "getCloseHandlers",
                "()Ljava/util/Set;"
            )
            .is_some());
        // Enumeration wrapper.
        assert!(r
            .find(CLS_LOGGER_ENUMERATION, "hasMoreElements", "()Z")
            .is_some());
        assert!(r
            .find(
                CLS_LOGGER_ENUMERATION,
                "nextElement",
                "()Ljava/lang/Object;"
            )
            .is_some());
    }

    #[test]
    fn t19_h3_get_log_manager_returns_singleton_identity() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let a = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        let b = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        assert_eq!(a, b, "getLogManager() must return the same singleton");
        // Singleton fields initialised.
        assert!(matches!(ctx.get_field(a, LM_FIELD_READY), Value::Int(1)));
    }

    #[test]
    fn t19_h3_jboss_get_log_manager_returns_same_singleton() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let jul = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        let jboss = match native_get_jboss_log_manager(&mut ctx, &[])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        assert_eq!(
            jul, jboss,
            "JUL and JBoss getLogManager() must share singleton"
        );
    }

    #[test]
    fn jboss_log_context_singleton_has_tree_lock_for_real_bytecode() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let log_context = ensure_jboss_log_context(&mut ctx);
        assert!(
            matches!(ctx.get_field(log_context, 0), Value::Object(Some(_))),
            "real LogContext.addCloseHandler synchronizes on treeLock"
        );
    }

    #[test]
    fn t19_h3_get_logger_is_idempotent_by_name() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let name = ctx.create_string("com.example.App");
        let a = match native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected logger, got {:?}", other),
        };
        let b = match native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(a, b, "getLogger(same name) must return the same Logger");
        // Name field round-trips.
        let stored_name = match ctx.get_field(a, LOGGER_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert_eq!(stored_name, "com.example.App");
    }

    #[test]
    fn t19_h3_static_jul_get_logger_defaults_to_jul_logger() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let name = ctx.create_string("com.example.App");
        let logger = match native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected logger, got {:?}", other),
        };
        let cid = ctx.class_id_of_object(logger);
        let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(class_name, CLS_JUL_LOGGER);
    }

    #[test]
    fn t19_h3_static_jul_get_logger_honors_jboss_logmanager_property() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        let name = ctx.create_string("com.example.App");
        let logger = match native_jul_static_get_logger(&mut ctx, &[Value::Object(Some(name))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected logger, got {:?}", other),
        };
        let cid = ctx.class_id_of_object(logger);
        let class_name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(class_name, "org/jboss/logmanager/Logger");
    }

    #[test]
    fn t19_h3_jboss_logger_get_handlers_returns_empty_array() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = get_or_create_jboss_logger(&mut ctx, "com.example.App");
        let handlers =
            match native_jboss_logger_get_handlers(&mut ctx, &[Value::Object(Some(logger))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(arr)) => arr,
                other => panic!("expected handler array, got {:?}", other),
            };
        assert_eq!(ctx.array_length(handlers), 0);
    }

    #[test]
    fn t19_h3_add_logger_returns_true_then_false_on_duplicate() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        // Build a Logger manually.
        let logger = alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS);
        let name_obj = ctx.create_string("dup.logger");
        ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));

        let first = native_add_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(first, Value::Int(1));
        let second = native_add_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            second,
            Value::Int(0),
            "duplicate addLogger must return false"
        );
    }

    #[test]
    fn tomcat0807_juli_add_logger_indexes_real_jdk_logger_name_field() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };

        // Tomcat JULI's ClassLoaderLogManager.addLogger receives real-JDK
        // Logger instances. On that layout slot 0 is `config`; the logger name
        // lives in the `name` field. A later LogManager.getLogger(name) must
        // find this same logger instead of creating a separate entry.
        let logger_cid = ctx.ensure_class_initialized(CLS_JUL_LOGGER).unwrap();
        let logger = ctx.alloc_object(logger_cid, LOGGER_NUM_FIELDS);
        let config_cid = ctx
            .ensure_class_initialized("java/util/logging/Logger$ConfigurationData")
            .unwrap();
        let config = ctx.alloc_object(config_cid, 1);
        let logger_name = "org.apache.catalina.core.AsyncContextImpl";
        let name_obj = ctx.create_string(logger_name);
        ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(config)));
        ctx.set_field_by_name(logger, "name", Value::Object(Some(name_obj)));

        let added = native_add_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(added, Value::Int(1));

        let lookup_name = ctx.create_string(logger_name);
        let looked_up = match native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(lookup_name))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected logger, got {:?}", other),
        };
        assert_eq!(
            looked_up, logger,
            "LogManager.getLogger(name) must see the real-JDK logger registered by addLogger"
        );
    }

    #[test]
    fn t19_h3_add_logger_rejects_path_traversal_name() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        for bad in &[
            "../../../etc/passwd",
            "..\\windows\\system32",
            "C:\\evil",
            "foo\tbar",
            "foo\u{0000}bar",
        ] {
            let logger = alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS);
            let name_obj = ctx.create_string(bad);
            ctx.set_field(logger, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
            let r = native_add_logger(
                &mut ctx,
                &[Value::Object(Some(mgr)), Value::Object(Some(logger))],
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                r,
                Value::Int(0),
                "addLogger must reject suspicious name: {bad}"
            );
        }
    }

    #[test]
    fn t19_h3_add_logger_rejects_null_logger_argument() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let r = native_add_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn t19_h3_read_configuration_is_graceful_no_op() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert!(native_read_configuration_no_arg(&mut ctx, &[])
            .unwrap()
            .is_none());
        assert!(
            native_read_configuration_with_stream(&mut ctx, &[Value::Object(None)])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn t19_h3_reset_clears_logger_registry_but_keeps_singleton() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        // Register two loggers.
        for name in &["a.b.c", "d.e.f"] {
            let name_obj = ctx.create_string(name);
            let _ = native_get_logger(
                &mut ctx,
                &[Value::Object(Some(mgr)), Value::Object(Some(name_obj))],
            )
            .unwrap();
        }
        assert_eq!(
            logger_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            2
        );
        native_reset(&mut ctx, &[Value::Object(Some(mgr))]).unwrap();
        assert!(
            logger_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "reset() must clear the logger registry"
        );
        // Singleton identity preserved.
        let again = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(mgr, again, "singleton must survive reset()");
    }

    #[test]
    fn t19_h3_get_logger_names_returns_snapshot_enumeration() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        for name in &["foo", "bar", "baz"] {
            let n = ctx.create_string(name);
            let _ = native_get_logger(
                &mut ctx,
                &[Value::Object(Some(mgr)), Value::Object(Some(n))],
            )
            .unwrap();
        }
        let enumeration = match native_get_logger_names(&mut ctx, &[Value::Object(Some(mgr))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let mut observed = Vec::new();
        loop {
            let has = native_enumeration_has_more(&mut ctx, &[Value::Object(Some(enumeration))])
                .unwrap()
                .unwrap();
            if matches!(has, Value::Int(0)) {
                break;
            }
            let elem = native_enumeration_next(&mut ctx, &[Value::Object(Some(enumeration))])
                .unwrap()
                .unwrap();
            match elem {
                Value::Object(Some(s)) => {
                    observed.push(ctx.read_string(s).unwrap_or_default());
                }
                _ => break,
            }
        }
        observed.sort();
        let mut expected = vec!["bar".to_string(), "baz".to_string(), "foo".to_string()];
        expected.sort();
        assert_eq!(observed, expected);
    }

    #[test]
    fn t19_h3_is_valid_logger_name_accepts_and_rejects() {
        assert!(is_valid_logger_name(""));
        assert!(is_valid_logger_name("com.example.App"));
        assert!(is_valid_logger_name("my-app.subsystem_1"));
        assert!(!is_valid_logger_name("../etc"));
        assert!(!is_valid_logger_name("foo\\bar"));
        assert!(!is_valid_logger_name("C:drive"));
        assert!(!is_valid_logger_name("has\rcr"));
        assert!(!is_valid_logger_name(&"a".repeat(513)));
    }

    #[test]
    fn t19_h3_get_logger_with_bad_name_returns_anonymous_not_cached() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let bad = ctx.create_string("../bad");
        let l1 = match native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(bad))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let l2 = match native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(bad))],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_ne!(
            l1, l2,
            "bad names must not be cached (each call allocates a throw-away Logger)"
        );
        // Registry untouched.
        assert!(logger_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[test]
    fn t19_h3_get_property_returns_null_for_any_key() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let key = ctx.create_string("foo");
        let v = native_get_property(&mut ctx, &[Value::Object(None), Value::Object(Some(key))])
            .unwrap()
            .unwrap();
        assert!(matches!(v, Value::Object(None)));
    }

    // -- Block 2B: -Djava.util.logging.manager honoured by getLogManager --

    #[test]
    fn block_2b_property_unset_returns_default_class_singleton() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        // Property absent — expected to return the default JDK class.
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(
            name, CLS_JUL_LOG_MANAGER,
            "no property set => default LogManager class"
        );
    }

    #[test]
    fn block_2b_property_set_to_subclass_returns_named_class() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.util.logging.manager", "LmSubclass$MyLm");
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(
            name, "LmSubclass$MyLm",
            "property set => getLogManager returns instance of named class"
        );
    }

    #[test]
    fn block_2b_property_built_in_alias_falls_through_to_singleton() {
        // The JDK class name short-circuits through the pre-allocated default
        // singleton path. This pins the contract that
        // `try_allocate_property_log_manager` returns `None` for the JDK alias
        // so the existing synthetic field layout is preserved.
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.util.logging.manager", "java.util.logging.LogManager");
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(name, CLS_JUL_LOG_MANAGER);
        // Singleton identity preserved across calls.
        let obj2 = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(obj, obj2);
    }

    #[test]
    fn block_2b_property_jboss_alias_returns_jboss_class_singleton() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(
            name, CLS_JBOSS_LOG_MANAGER,
            "WildFly's logging extension requires the active manager class to be JBoss LogManager"
        );

        let jboss = match native_get_jboss_log_manager(&mut ctx, &[])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("expected JBoss manager ObjectRef, got {:?}", other),
        };
        assert_eq!(obj, jboss, "JUL and JBoss entry points share singleton");
    }

    #[test]
    fn block_2b_property_empty_or_whitespace_returns_default() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.util.logging.manager", "   ");
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        assert_eq!(name, CLS_JUL_LOG_MANAGER);
    }

    #[test]
    fn block_2b_property_path_traversal_class_name_rejected() {
        // Hostile property values must not coax the unified loader
        // into probing arbitrary disk paths via the class-name string.
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.util.logging.manager", "../../../etc/passwd");
        let obj = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let cid = ctx.class_id_of_object(obj);
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        // Falls through to the default LogManager class on rejection.
        assert_eq!(name, CLS_JUL_LOG_MANAGER);
    }

    // -- B4: GC root scan + post-move remap for the cached ObjectRefs --

    /// Snapshot the raw addresses currently held in this module's
    /// side-tables (singleton, both logger registries, the JBoss
    /// `LogContext` singleton, and every attachment receiver/key/value).
    fn cached_addrs_snapshot() -> Vec<u64> {
        let mut v = Vec::new();
        if let Some(a) = *singleton_cell().lock().unwrap_or_else(|e| e.into_inner()) {
            v.push(a);
        }
        if let Some(a) = *jboss_log_context_singleton()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            v.push(a);
        }
        v.extend(
            logger_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .copied(),
        );
        v.extend(
            jboss_logger_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .copied(),
        );
        for (&(this, key), &value) in attachments()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            v.extend_from_slice(&[this, key, value]);
        }
        v
    }

    #[test]
    fn b4_gc_scan_reports_every_cached_object_as_root() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();

        // Populate every side-table: singleton, a JUL logger, a JBoss
        // logger, the JBoss LogContext singleton, and one attachment.
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let jul_name = ctx.create_string("scan.jul.logger");
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(jul_name))],
        )
        .unwrap();
        let jb_name = ctx.create_string("scan.jboss.logger");
        let _ = native_jboss_log_context_get_logger(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(jb_name))],
        )
        .unwrap();
        let _ = ensure_jboss_log_context(&mut ctx);
        let recv =
            alloc_concurrent_synthetic(&mut ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS);
        let key = alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1);
        let val = alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1);
        let _ = native_jboss_logger_attach(
            &mut ctx,
            &[
                Value::Object(Some(recv)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .unwrap();

        let expected = cached_addrs_snapshot();
        assert!(!expected.is_empty(), "side-tables must be populated");

        let mut roots = Vec::new();
        gc_scan_logmanager_roots(&mut roots);
        let root_addrs: std::collections::HashSet<u64> =
            roots.iter().map(|o| o.as_ptr() as u64).collect();
        for a in expected {
            assert!(
                root_addrs.contains(&a),
                "cached object {a:#x} must be reported as a GC root"
            );
        }
    }

    #[test]
    fn b4_gc_update_repoints_every_cached_address() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();

        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let jul_name = ctx.create_string("remap.jul.logger");
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(jul_name))],
        )
        .unwrap();
        let jb_name = ctx.create_string("remap.jboss.logger");
        let _ = native_jboss_log_context_get_logger(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(jb_name))],
        )
        .unwrap();
        let _ = ensure_jboss_log_context(&mut ctx);
        let recv =
            alloc_concurrent_synthetic(&mut ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS);
        let key = alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1);
        let val = alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1);
        let _ = native_jboss_logger_attach(
            &mut ctx,
            &[
                Value::Object(Some(recv)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .unwrap();

        // Simulate a moving GC: every cached old address maps to a fresh,
        // non-overlapping synthetic "relocated" address.
        let old_addrs = cached_addrs_snapshot();
        assert!(!old_addrs.is_empty());
        let mut pointer_map: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        // Use a high base so the synthetic targets never collide with a
        // real old address (which would make the assertion ambiguous).
        let base: usize = 0x1_0000_0000_0000;
        for (i, &a) in old_addrs.iter().enumerate() {
            pointer_map.insert(a as usize, base + (i + 1) * 0x1000);
        }

        gc_update_logmanager_refs(&pointer_map);

        // Every stored address must now be the relocated target — no old
        // address may survive (that would be the use-after-free B4 flags).
        let new_addrs = cached_addrs_snapshot();
        assert_eq!(
            new_addrs.len(),
            old_addrs.len(),
            "remap must preserve table cardinality (attachment key rebuild intact)"
        );
        for a in &new_addrs {
            assert!(
                (*a as usize) >= base,
                "address {a:#x} was not remapped to its relocated slot"
            );
            assert!(
                pointer_map.values().any(|&v| v as u64 == *a),
                "address {a:#x} is not one of the synthetic relocated targets"
            );
        }
    }

    #[test]
    fn b4_gc_update_is_noop_for_empty_pointer_map() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let n = ctx.create_string("noop.logger");
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(n))],
        )
        .unwrap();
        let before = cached_addrs_snapshot();
        gc_update_logmanager_refs(&std::collections::HashMap::new());
        let after = cached_addrs_snapshot();
        assert_eq!(
            before, after,
            "empty pointer map must not mutate any address"
        );
    }

    #[test]
    fn b4_gc_update_leaves_unmoved_addresses_untouched() {
        // Objects the young collector did not relocate are absent from the
        // pointer map; their cached address must be preserved verbatim.
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let n = ctx.create_string("stable.logger");
        let _ = native_get_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(Some(n))],
        )
        .unwrap();
        let before = cached_addrs_snapshot();
        // A pointer map that mentions only some unrelated address.
        let mut pm: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        pm.insert(0xdead_beef, 0xfeed_face);
        gc_update_logmanager_refs(&pm);
        let after = cached_addrs_snapshot();
        assert_eq!(
            before, after,
            "addresses absent from the pointer map must be left unchanged"
        );
    }
}
