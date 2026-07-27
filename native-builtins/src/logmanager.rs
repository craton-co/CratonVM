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

use std::collections::{BTreeMap, HashMap};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Mutex, OnceLock,
};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
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
pub(crate) const LOGGER_FIELD_NAME: usize = 0;
pub(crate) const LOGGER_FIELD_LEVEL: usize = 1;
pub(crate) const LOGGER_FIELD_PARENT: usize = 2;

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

/// JULI's `ClassLoaderLogManager` deliberately permits the same logger name
/// in independent web application class loaders. Keep those synthetic loggers
/// out of the default process-wide registry and key them by the stable identity
/// hash of the current thread context class loader instead.
fn tomcat_juli_logger_registry() -> &'static Mutex<HashMap<(i32, String), u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<(i32, String), u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Root handlers must be scoped by the thread context class loader as well:
/// every web application uses the JUL root name `""`, but each has an
/// independent FileHandler configuration.
fn tomcat_juli_root_handler_registry() -> &'static Mutex<HashMap<i32, Vec<u64>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, Vec<u64>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Explicit handlers installed on synthetic JUL loggers. The JDK normally
/// keeps this state in `Logger.ConfigurationData`; our compact Logger mirror
/// deliberately does not model that private layout, so keep the Java-visible
/// handler references here instead. This matters for framework log capture
/// (Tomcat's `LogCapture` is one such user), not only console output.
fn logger_handlers() -> &'static Mutex<HashMap<String, Vec<u64>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, Vec<u64>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Explicit JUL levels for real Logger objects. Their private configuration
/// layout is not always materialized by the VM, but callers such as Tomcat's
/// LogCapture rely on `setLevel(FINE)` taking effect immediately.
fn logger_explicit_levels() -> &'static Mutex<HashMap<String, i32>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Message payloads for minimally-constructed LogRecord mirrors. The real
/// private LogRecord layout is not initialized on this native-only path, but
/// `getMessage()` remains a required public contract for JUL handlers.
fn log_record_messages() -> &'static Mutex<BTreeMap<i64, String>> {
    static INSTANCE: OnceLock<Mutex<BTreeMap<i64, String>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn next_log_record_id() -> i64 {
    static NEXT: AtomicI64 = AtomicI64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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

/// Resolve one of the 9 standard `java.util.logging.Level` singletons
/// (`INFO`, `ALL`, `FINE`, ...) by name. Shared by the root-level default,
/// the convenience-method publishers, and `readConfiguration` parsing.
pub(crate) fn resolve_standard_level(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let level_class = ctx.ensure_class_initialized(CLS_JUL_LEVEL).ok()?;
    let idx = ctx.static_field_index_by_name(level_class, name)?;
    match ctx.get_static_field(level_class, idx) {
        Value::Object(Some(level)) => Some(level),
        _ => None,
    }
}

/// Allocate a `Logger` object, populate its name field, and register
/// it in the process-wide registry.
///
/// Every non-root logger gets a real parent link (the logger for its
/// dotted-name prefix, recursively demand-created) so `getParent()` /
/// `getEffectiveLevel()`-style ancestor walks terminate correctly instead
/// of chasing a permanently-null parent. The root logger ("") has no
/// parent but is seeded with the JDK-default `Level.INFO` so those same
/// walks stop at the root instead of dereferencing a null level.
fn allocate_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    // Ensure the wildfly_core registry also learns about this name so
    // log-level overrides and credential redaction apply uniformly. The
    // wildfly layer returns an `Arc<LoggerMirror>` — we don't need the
    // Arc itself here, just the side-effect of interning the name.
    let _mirror = crate::wildfly_core::get_logger(name);
    let obj = alloc_concurrent_synthetic(ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS);
    // GC SAFETY: every step below (`create_string`, `Level.<clinit>` via
    // `resolve_standard_level`, the recursive parent demand-creation) can
    // allocate and therefore move `obj`. Keep the fresh Logger rooted and
    // re-derive it after each of those boundaries.
    let obj_pin = ctx.pin_native_root(obj);
    let mut obj = obj;
    obj = populate_real_logger_bundle(ctx, obj_pin, obj);
    let name_obj = ctx.create_string(name);
    let name_pin = ctx.pin_native_root(name_obj);
    obj = ctx.read_native_pin(obj_pin, obj);
    let name_obj = ctx.read_native_pin(name_pin, name_obj);
    ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
    // NB: deliberately NOT also written by name. On a real-JDK Logger the
    // by-name `name` field is slot 2 — the very slot this module uses for the
    // parent link — so writing both aliases one over the other, and a String
    // landing in the parent slot is what `resolve_jul_handler_list`'s legacy
    // fallback would then mistake for a handler list
    // (`NoSuchMethodError: java/lang/String.size()I`).
    // `read_jul_logger_name` already falls back to slot 0 for this shape.
    ctx.unpin_native_roots(name_pin);
    if name.is_empty() {
        ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(None));
        let default_level = resolve_standard_level(ctx, "INFO");
        obj = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field(obj, LOGGER_FIELD_LEVEL, Value::Object(default_level));
    } else {
        ctx.set_field(obj, LOGGER_FIELD_LEVEL, Value::Object(None));
        let parent_name = match name.rfind('.') {
            Some(idx) => &name[..idx],
            None => "",
        };
        let parent = get_or_create_logger(ctx, parent_name);
        let parent_pin = ctx.pin_native_root(parent);
        obj = ctx.read_native_pin(obj_pin, obj);
        let parent = ctx.read_native_pin(parent_pin, parent);
        ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(Some(parent)));
        ctx.unpin_native_roots(parent_pin);
    }
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    obj
}

/// Resolve `java.util.logging.Logger.NO_RESOURCE_BUNDLE` — the shared
/// `Logger$LoggerBundle` sentinel the real constructor assigns to every
/// `Logger`'s `loggerBundle` field.
///
/// Returns `None` when the real class body isn't the one loaded (the compact
/// synthetic `Logger` has neither the static nor the nested class), which is
/// the signal for the caller to skip the field write entirely.
fn real_logger_no_resource_bundle(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let logger_class = ctx.ensure_class_initialized(CLS_JUL_LOGGER).ok()?;
    if let Some(idx) = ctx.static_field_index_by_name(logger_class, "NO_RESOURCE_BUNDLE") {
        if let Value::Object(Some(bundle)) = ctx.get_static_field(logger_class, idx) {
            return Some(bundle);
        }
    }
    // The static is present but still null (clinit ordering): mint an
    // equivalent empty bundle. `LoggerBundle`'s two fields
    // (`resourceBundleName`, `userBundle`) are BOTH null for the no-bundle
    // case, and `isSystemBundle()` is an identity comparison against a
    // different singleton — so a zero-initialized instance answers every read
    // the JUL bytecode performs exactly as the sentinel does. Guard on the
    // class actually being loaded so we never synthesize a bogus one.
    let bundle_class = "java/util/logging/Logger$LoggerBundle";
    ctx.class_id_by_name(bundle_class)?;
    match ctx.new_object(bundle_class) {
        Ok(Some(Value::Object(Some(bundle)))) => Some(bundle),
        _ => None,
    }
}

/// Populate the real-JDK `Logger.loggerBundle` field on a Logger this module
/// allocated natively.
///
/// In real-JDK mode `alloc_concurrent_synthetic` hands back an instance with
/// the REAL `java.util.logging.Logger` layout but no constructor run, so every
/// reference field is null — including `loggerBundle`. Real JUL bytecode that
/// still executes over such an instance (`throwing`, `logrb`, `doLog`,
/// `getEffectiveLoggerBundle`) dereferences it unconditionally and dies with
/// `NullPointerException: Cannot invoke
/// "java.util.logging.Logger$LoggerBundle.isSystemBundle()" because "lb" is
/// null`. Seed it with the same sentinel the real constructor uses.
///
/// `logger_pin` must be a live pin for `logger`; the (possibly relocated)
/// Logger is returned.
fn populate_real_logger_bundle(
    ctx: &mut dyn NativeContext,
    logger_pin: usize,
    logger: ObjectRef,
) -> ObjectRef {
    let Some(bundle) = real_logger_no_resource_bundle(ctx) else {
        return ctx.read_native_pin(logger_pin, logger);
    };
    let bundle_pin = ctx.pin_native_root(bundle);
    let logger = ctx.read_native_pin(logger_pin, logger);
    let bundle = ctx.read_native_pin(bundle_pin, bundle);
    // No-op on the compact synthetic shape, which declares no such field.
    ctx.set_field_by_name(logger, "loggerBundle", Value::Object(Some(bundle)));
    ctx.unpin_native_roots(bundle_pin);
    logger
}

/// Look up a cached logger by name; if absent, allocate one and cache
/// it. Rejected names (via `is_valid_logger_name`) allocate an
/// anonymous Logger that isn't registered so the caller still receives
/// a non-null Logger for the `.info()` / `.warning()` fallback but the
/// bad name never enters the registry.
pub(crate) fn get_or_create_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
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

pub(crate) fn read_jul_logger_name(ctx: &dyn NativeContext, logger: ObjectRef) -> String {
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

fn tomcat_context_loader_key(ctx: &mut dyn NativeContext) -> i32 {
    let thread = match ctx
        .invoke(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .ok()
        .flatten()
    {
        Some(Value::Object(Some(thread))) => thread,
        _ => return 0,
    };
    let thread_pin = ctx.pin_native_root(thread);
    let loader = ctx.invoke(
        "java/lang/Thread",
        "getContextClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[Value::Object(Some(thread))],
    );
    ctx.unpin_native_roots(thread_pin);
    match loader.ok().flatten() {
        Some(Value::Object(Some(loader))) => ctx.identity_hash_code(loader),
        _ => 0,
    }
}

/// Return the root logger already configured by Tomcat for the current thread
/// context class loader.  Calling `getLogger("")` is safe here: JULI creates
/// and configures that root as part of its own class-loader-info bootstrap;
/// unlike `addLogger(child)`, it does not recurse through parent logger names.
fn tomcat_juli_root_logger(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let manager = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER);
    if ctx
        .class_name_of_id(ctx.class_id_of_object(manager))
        .as_deref()
        != Some("org/apache/juli/ClassLoaderLogManager")
    {
        return None;
    }
    let manager_pin = ctx.pin_native_root(manager);
    let root_name = ctx.create_string("");
    let manager = ctx.read_native_pin(manager_pin, manager);
    let root = ctx
        .invoke_virtual_bytecode_only(
            manager,
            "getLogger",
            "(Ljava/lang/String;)Ljava/util/logging/Logger;",
            &[Value::Object(Some(root_name))],
        )
        .ok()
        .flatten();
    ctx.unpin_native_roots(manager_pin);
    match root {
        Some(Value::Object(Some(root))) => Some(root),
        _ => None,
    }
}

fn get_or_create_tomcat_juli_logger(ctx: &mut dyn NativeContext, name: &str) -> ObjectRef {
    if !is_valid_logger_name(name) {
        return allocate_logger(ctx, "");
    }
    // The JUL root is a real logger installed by ClassLoaderLogManager while
    // it reads the current webapp's logging.properties. Returning it directly
    // preserves the configured root level instead of manufacturing a second,
    // unconfigured synthetic root.
    if name.is_empty() {
        if let Some(root) = tomcat_juli_root_logger(ctx) {
            return root;
        }
    }
    let context_loader_key = tomcat_context_loader_key(ctx);
    let key = (context_loader_key, name.to_string());
    if let Some(&address) = tomcat_juli_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        if address != 0 {
            return unsafe { object_from_u64(address) };
        }
    }
    let logger = allocate_logger(ctx, name);
    // This factory re-enters real JULI bytecode and allocates handler state.
    // Keep its new Logger rooted throughout, refreshing it after every
    // GC-capable boundary before it is stored or returned.
    let logger_pin = ctx.pin_native_root(logger);
    let mut logger = logger;
    // Do not merely cache the child: Tomcat's addLogger bytecode applies the
    // current context-class-loader configuration, wires its parent chain and
    // instantiates any per-logger handlers. Bypassing this path was why the
    // per-webapp FileHandler and root level disappeared.
    let manager = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER);
    logger = ctx.read_native_pin(logger_pin, logger);
    if ctx
        .class_name_of_id(ctx.class_id_of_object(manager))
        .as_deref()
        == Some("org/apache/juli/ClassLoaderLogManager")
    {
        let manager_pin = ctx.pin_native_root(manager);
        let manager = ctx.read_native_pin(manager_pin, manager);
        let logger_arg = ctx.read_native_pin(logger_pin, logger);
        let _ = ctx.invoke_virtual_bytecode_only(
            manager,
            "addLogger",
            "(Ljava/util/logging/Logger;)Z",
            &[Value::Object(Some(logger_arg))],
        );
        logger = ctx.read_native_pin(logger_pin, logger);
        ctx.unpin_native_roots(manager_pin);
    }
    if let Some(root) = tomcat_juli_root_logger(ctx) {
        logger = ctx.read_native_pin(logger_pin, logger);
        if let Some(handlers) = crate::jul_logger_handlers_get(ctx, root) {
            // `publish_to_jul_handlers` is intentionally compact and does not
            // walk a Java parent chain.  Share JULI's already-filtered root
            // handler list with the context-local child so it observes the
            // same per-webapp FileHandler configuration.
            crate::jul_logger_handlers_set(ctx, logger, handlers);
            logger = ctx.read_native_pin(logger_pin, logger);
        } else {
            // Older JULI setup paths register a root handler through the
            // name-keyed compatibility table. Snapshot that current root list
            // into a distinct identity-keyed ArrayList so two webapps with
            // root name "" cannot subsequently overwrite one another.
            let root_handlers = tomcat_juli_root_handler_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&context_loader_key)
                .cloned()
                .unwrap_or_default();
            if !root_handlers.is_empty() {
                let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
                let _ =
                    cratonvm_native_collections::native_al_init(ctx, &[Value::Object(Some(list))]);
                for address in root_handlers {
                    let handler = unsafe { object_from_u64(address) };
                    let _ = cratonvm_native_collections::native_al_add(
                        ctx,
                        &[Value::Object(Some(list)), Value::Object(Some(handler))],
                    );
                }
                logger = ctx.read_native_pin(logger_pin, logger);
                crate::jul_logger_handlers_set(ctx, logger, list);
                logger = ctx.read_native_pin(logger_pin, logger);
            }
        }
    }
    logger = ctx.read_native_pin(logger_pin, logger);
    let mut registry = tomcat_juli_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&address) = registry.get(&key) {
        if address != 0 {
            ctx.unpin_native_roots(logger_pin);
            return unsafe { object_from_u64(address) };
        }
    }
    registry.insert(key, logger.as_ptr() as u64);
    ctx.unpin_native_roots(logger_pin);
    logger
}

fn tomcat_classloader_log_manager_requested(ctx: &dyn NativeContext) -> bool {
    matches!(
        ctx.get_system_property("java.util.logging.manager")
            .as_deref()
            .map(str::trim),
        Some("org.apache.juli.ClassLoaderLogManager" | "org/apache/juli/ClassLoaderLogManager")
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
    if tomcat_classloader_log_manager_requested(ctx) {
        let logger = get_or_create_tomcat_juli_logger(ctx, &name);
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
    if let Ok(mut r) = tomcat_juli_logger_registry().lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_root_handler_registry().lock() {
        r.clear();
    }
    if let Ok(mut h) = logger_handlers().lock() {
        h.clear();
    }
    if let Ok(mut m) = log_record_messages().lock() {
        m.clear();
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
    // `wildfly_core::get_logger` below can allocate and trigger a moving GC.
    // Keep the real-JDK Logger receiver rooted through that call before
    // storing its address in the cross-call registry.
    let logger_pin = ctx.pin_native_root(logger);
    let name = read_jul_logger_name(ctx, logger);
    if !is_valid_logger_name(&name) {
        tracing::warn!(
            rejected_name = %name,
            "LogManager.addLogger: rejected suspicious logger name"
        );
        ctx.unpin_native_roots(logger_pin);
        return Ok(Some(Value::Int(0)));
    }
    if logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&name)
    {
        // Already registered — spec says return false.
        ctx.unpin_native_roots(logger_pin);
        return Ok(Some(Value::Int(0)));
    }
    // Also track in wildfly_core so tracing redaction picks this up. This is
    // deliberately outside the registry lock because it may allocate.
    let _mirror = crate::wildfly_core::get_logger(&name);
    let logger = ctx.read_native_pin(logger_pin, logger);
    let mut reg = logger_registry().lock().unwrap_or_else(|e| e.into_inner());
    if reg.contains_key(&name) {
        ctx.unpin_native_roots(logger_pin);
        return Ok(Some(Value::Int(0)));
    }
    reg.insert(name, logger.as_ptr() as u64);
    ctx.unpin_native_roots(logger_pin);
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
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this (LogManager), args[1] = the InputStream.
    let Some(Value::Object(Some(stream))) = args.get(1).copied() else {
        return Ok(None);
    };
    let Some(bytes) = crate::properties_sidetable::drain_input_stream_pub(ctx, stream) else {
        return Ok(None);
    };
    let entries = crate::properties_sidetable::parse_properties_pub(&bytes);
    apply_jul_config_entries(ctx, &entries)
}

pub(crate) fn parsed_log_properties() -> &'static Mutex<HashMap<String, String>> {
    static T: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Apply a parsed `java.util.logging` properties file (Spring Boot's
/// bundled `logging.properties`/`logging-file.properties` in this
/// cluster): install `handlers=` on the root logger, apply
/// `.level=`/`<logger-name>.level=` entries into `logger_explicit_levels`,
/// and forward per-handler `<HandlerClass>.level`/`.formatter` keys to the
/// handler instances just created.
///
/// This intentionally stays reachable ONLY from the `InputStream` overload
/// (never the no-arg, filesystem-backed one — see
/// `native_read_configuration_no_arg`), so the only content ever parsed
/// here is a stream the caller's own Java code already produced (a
/// packaged classpath `Resource`), not arbitrary filesystem/environment
/// input.
fn apply_jul_config_entries(
    ctx: &mut dyn NativeContext,
    entries: &[(String, String)],
) -> MethodCallResult {
    // A fresh `readConfiguration` call replaces the whole prior config
    // snapshot, matching real JUL semantics.
    {
        let mut props = parsed_log_properties()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        props.clear();
        for (k, v) in entries {
            props.insert(k.clone(), v.clone());
        }
    }
    // Real `LogManager.readConfiguration` calls `reset()` first, which sets
    // every known logger's level back to null before the new config is
    // applied. Our `logger_explicit_levels` side table has no equivalent of
    // real JUL's per-Logger weak-reference lifecycle (a real Logger with no
    // external strong ref is eventually collected and recreated fresh by
    // `demandLogger`, silently dropping stale explicit levels); ours is a
    // permanent, process-wide map, so without this clear, an explicit level
    // set in one test (e.g. `JavaLoggingSystemTests`'s
    // `@AfterEach resetLogger` calling `this.logger.setLevel(Level.OFF)`)
    // would leak into every subsequent test sharing this process and
    // permanently mute that logger.
    logger_explicit_levels()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();

    let handler_class_names: Vec<String> = entries
        .iter()
        .find(|(k, _)| k == "handlers")
        .map(|(_, v)| {
            v.split(|c: char| c == ',' || c.is_whitespace())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.replace('.', "/"))
                .collect()
        })
        .unwrap_or_default();

    let root = get_or_create_logger(ctx, "");
    let root_pin = ctx.pin_native_root(root);
    // A fresh config replaces whatever handlers a previous
    // `readConfiguration` call (or explicit `addHandler`) installed on the
    // root logger -- otherwise repeated `beforeInitialize`/`initialize`
    // cycles (one per @Test method sharing this process) would stack up
    // duplicate `ConsoleHandler`s and double-print every message.
    crate::jul_logger_handlers_clear(ctx, root);

    let mut created: Vec<(String, ObjectRef)> = Vec::new();
    for cls in &handler_class_names {
        if let Ok(Some(Value::Object(Some(handler)))) = ctx.new_object_initialized(cls, "()V", &[])
        {
            let root = ctx.read_native_pin(root_pin, root);
            let _ = native_jul_logger_add_handler(
                ctx,
                &[Value::Object(Some(root)), Value::Object(Some(handler))],
            );
            created.push((cls.replace('/', "."), handler));
        }
        // Handler class missing/uninstantiable -- skip it rather than fail
        // the whole config load (mirrors real JUL's per-handler try/catch
        // in `LogManager.readConfiguration`).
    }
    ctx.unpin_native_roots(root_pin);

    let mut formatter_configured = vec![false; created.len()];
    for (k, v) in entries {
        if k == "handlers" {
            continue;
        }
        let Some(dot) = k.rfind('.') else { continue };
        let (prefix, suffix) = (&k[..dot], &k[dot + 1..]);
        if let Some(idx) = created.iter().position(|(name, _)| name == prefix) {
            let handler = created[idx].1;
            match suffix {
                "level" => {
                    if let Some(level) = resolve_standard_level(ctx, v.trim()) {
                        let _ = ctx.invoke_virtual(
                            handler,
                            "setLevel",
                            "(Ljava/util/logging/Level;)V",
                            &[Value::Object(Some(level))],
                        );
                    }
                }
                "formatter" => {
                    if let Ok(Some(Value::Object(Some(fmt)))) =
                        ctx.new_object_initialized(&v.trim().replace('.', "/"), "()V", &[])
                    {
                        let _ = ctx.invoke_virtual(
                            handler,
                            "setFormatter",
                            "(Ljava/util/logging/Formatter;)V",
                            &[Value::Object(Some(fmt))],
                        );
                        formatter_configured[idx] = true;
                    }
                }
                _ => {}
            }
            continue;
        }
        // Not a handler-instance key -- treat `<logger-name>.level` (the
        // root is the empty-prefix case, key literally ".level") as a
        // per-logger explicit level.
        if suffix == "level" {
            if let Some(level) = jul_standard_level_value(v.trim()) {
                logger_explicit_levels()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(prefix.to_string(), level);
            }
        }
    }
    // Handlers with no explicit `.formatter=` config fall back to real
    // JDK's own `new java.util.logging.SimpleFormatter()`. That
    // constructor computes its default pattern via `invokedynamic` +
    // `jdk/internal/logger/SurrogateLogger.getSimpleFormat` (a lambda
    // metafactory call into a JDK-internal helper), which under CratonVM
    // leaves the formatter's private `format` field null instead of the
    // documented default pattern -- `String.format(null, ...)` then either
    // throws (silently swallowed by `Handler.publish`'s own
    // format-failure error path) or produces no usable text, so every
    // handler built this way is publish-silent. Detect the unset field
    // and patch in the documented default pattern directly, sidestepping
    // the broken `invokedynamic` path without reimplementing it.
    for (idx, (_, handler)) in created.iter().enumerate() {
        if formatter_configured[idx] {
            continue;
        }
        if let Ok(Some(Value::Object(Some(fmt)))) =
            ctx.new_object_initialized("java/util/logging/SimpleFormatter", "()V", &[])
        {
            // Always overwrite: the broken constructor doesn't reliably
            // leave `format` exactly null (observed non-null-but-wrong
            // values too), so a null-only guard under-detects.
            let pattern = ctx.create_string("%1$tc%n%4$s: %5$s%n%6$s%n");
            ctx.set_field_by_name(fmt, "format", Value::Object(Some(pattern)));
            let _ = ctx.invoke_virtual(
                *handler,
                "setFormatter",
                "(Ljava/util/logging/Formatter;)V",
                &[Value::Object(Some(fmt))],
            );
        }
    }
    Ok(None)
}

fn native_reset(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Drop all logger bindings but keep the manager singleton alive.
    if let Ok(mut r) = logger_registry().lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_logger_registry().lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_root_handler_registry().lock() {
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
                            if let Value::Int(v) = ctx.get_field_by_name(level_obj, "value") {
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
    crate::emit_framework_log(ctx, &format!("{tag} [{logger}] {message}"));
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
            Some(m) if !m.is_empty() => {
                crate::emit_framework_log(ctx, &format!("{indent}{prefix}{cls}: {m}"))
            }
            _ => crate::emit_framework_log(ctx, &format!("{indent}{prefix}{cls}")),
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
                crate::emit_framework_log(
                    ctx,
                    &format!(
                        "{indent}    at {}.{}{where_}",
                        f.class_name.replace('/', "."),
                        f.method_name
                    ),
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
    crate::emit_framework_log(ctx, &format!("{level_name} [{logger_name}] {message}"));
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
    let format = format_pin
        .and_then(|(pin, object)| {
            let object = ctx.read_native_pin(pin, object);
            ctx.read_string(object)
        })
        .unwrap_or_default();
    // Substitute %s/%% /%n from the params Object[] so structured messages
    // (e.g. WFLYCTL0013 failure description) are visible in the output.
    let message = if let Some((params_pin, params)) = params_pin {
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
    // `eprintln!` writes to the process's raw OS stderr, bypassing the
    // Java-level `System.out`/`System.err` `PrintStream` that JUnit5's
    // `OutputCaptureExtension` substitutes — same bug class as
    // `log_simple`'s fix below; route through the live stream instead.
    // See docs/known-issues/springboot/propertiesmigration-logfactory-oom-residual.md.
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
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
    publish_jul_handlers(ctx, this, level_obj, message_obj);
    crate::emit_framework_log(ctx, &format!("{level_name} [{logger_name}] {message}"));
    if let (Some(logger), Some(level), Some(message)) = (this, level_obj, message_obj) {
        publish_to_jul_handlers(ctx, logger, level, message)?;
    }
    Ok(None)
}

/// `java/util/logging/Logger.log(Level, String, Object)` — single-param
/// sibling of `log(Level, String, Object[])`. The JDK wraps `param1` in a
/// one-element `Object[]` before building the LogRecord, and
/// `LogRecord.getParameters()` must report it exactly that way, so do the same
/// here rather than pre-formatting the text.
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
    jul_log_parameterized(ctx, this, level_obj, message_obj, params_arr, None)
}

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
fn jul_log_parameterized(
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
    notify_jul_logger_filter(ctx, this, level_obj, message_obj, thrown);
    let this = this_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let level_obj = level_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let message_obj = message_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let params = params_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let thrown = thrown_pin.map(|(p, o)| ctx.read_native_pin(p, o));
    let delivered = match (this, level_obj, message_obj) {
        (Some(logger), Some(level), Some(message)) => {
            publish_to_jul_handlers_full(ctx, logger, level, message, None, None, params, thrown)
                // A handler that threw must not turn into an application-visible
                // exception raised from the logging call itself; fall back to the
                // console sink instead, exactly as if there had been no handler.
                .unwrap_or(false)
        }
        _ => false,
    };
    if jul_dbg_enabled() {
        eprintln!(
            "[JUL-DBG] jul_log_parameterized: params={} thrown={} delivered_to_handler={delivered}",
            params.is_some(),
            thrown.is_some()
        );
    }
    if !delivered {
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

fn native_jul_log_record_get_message(
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
    let message = log_record_messages()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&record_id)
        .cloned()
        .map(|text| ctx.create_string_uninterned_gc_safe(&text));
    Ok(Some(Value::Object(message)))
}

/// Store an explicit handler without relying on the private JDK Logger layout.
fn native_jul_logger_add_handler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let mut all = logger_handlers().lock().unwrap_or_else(|e| e.into_inner());
    let handlers = all.entry(name).or_default();
    if !handlers.iter().any(|&addr| addr == handler.as_ptr() as u64) {
        handlers.push(handler.as_ptr() as u64);
    }
    drop(all);
    if is_root && tomcat_classloader_log_manager_requested(ctx) {
        let loader_key = tomcat_context_loader_key(ctx);
        let mut roots = tomcat_juli_root_handler_registry()
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
    let side_list = match crate::jul_logger_handlers_get(ctx, logger) {
        Some(list) => list,
        None => {
            let list = alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2);
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
        let _ = cratonvm_native_collections::native_al_add(
            ctx,
            &[Value::Object(Some(side_list)), Value::Object(Some(handler))],
        )?;
    }
    ctx.unpin_native_roots(logger_pin);
    Ok(None)
}

fn native_jul_logger_remove_handler(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(handler)))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    let name = read_jul_logger_name(ctx, *logger);
    let mut all = logger_handlers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handlers) = all.get_mut(&name) {
        handlers.retain(|&addr| addr != handler.as_ptr() as u64);
    }
    Ok(None)
}

/// Publish a real LogRecord to every explicit handler. Console emission alone
/// is insufficient: JUL users legitimately install in-process handlers to
/// collect records, including Tomcat's standalone startup test.
fn publish_jul_handlers(
    ctx: &mut dyn NativeContext,
    logger: Option<ObjectRef>,
    level: Option<ObjectRef>,
    message: Option<ObjectRef>,
) {
    publish_jul_handlers_src(ctx, logger, level, message, None, None)
}

/// Deliver a JUL record to the logger-local Filter before the compact logging
/// bridge emits it. `Logger.setFilter` cannot use the real JDK field layout:
/// compact and real loggers have different shapes, so the filter itself lives
/// in a rooted side table maintained by `lib.rs`.
fn notify_jul_logger_filter(
    ctx: &mut dyn NativeContext,
    logger: Option<ObjectRef>,
    level: Option<ObjectRef>,
    message: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
) {
    let (Some(logger), Some(level), Some(message)) = (logger, level, message) else {
        return;
    };
    let Some(filter) = crate::jul_logger_filter_get(ctx, logger) else {
        return;
    };
    let level_pin = ctx.pin_native_root(level);
    let message_pin = ctx.pin_native_root(message);
    let filter_pin = ctx.pin_native_root(filter);
    let thrown_pin = thrown.map(|o| (ctx.pin_native_root(o), o));
    // The compact JUL bridge only needs the observable LogRecord fields.
    // Its real-JDK constructor may walk unmaterialized time/sequence state
    // before a filter gets to inspect the record, so use the same compact
    // allocation strategy as the established handler bridge below.
    let record = match ctx.new_object("java/util/logging/LogRecord") {
        Ok(Some(Value::Object(Some(record)))) => record,
        _ => {
            ctx.unpin_native_roots(level_pin);
            return;
        }
    };
    let record_pin = ctx.pin_native_root(record);
    let record = ctx.read_native_pin(record_pin, record);
    let level = ctx.read_native_pin(level_pin, level);
    let message = ctx.read_native_pin(message_pin, message);
    ctx.set_field_by_name(record, "level", Value::Object(Some(level)));
    ctx.set_field_by_name(record, "message", Value::Object(Some(message)));
    let _ = ctx.invoke_virtual(
        record,
        "setLevel",
        "(Ljava/util/logging/Level;)V",
        &[Value::Object(Some(level))],
    );
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
    let filter = ctx.read_native_pin(filter_pin, filter);
    let record = ctx.read_native_pin(record_pin, record);
    let filter = ctx.read_native_pin(filter_pin, filter);
    let record = ctx.read_native_pin(record_pin, record);
    let _ = ctx.invoke_virtual(
        filter,
        "isLoggable",
        "(Ljava/util/logging/LogRecord;)Z",
        &[Value::Object(Some(record))],
    );
    ctx.unpin_native_roots(level_pin);
}

/// `publish_jul_handlers` with the caller-provided source class/method pair
/// (`Logger.logp` args) stamped into each record so JULI's OneLineFormatter
/// prints the real source instead of "null.null".
fn publish_jul_handlers_src(
    ctx: &mut dyn NativeContext,
    logger: Option<ObjectRef>,
    level: Option<ObjectRef>,
    message: Option<ObjectRef>,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
) {
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
    let handlers = logger_handlers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&read_jul_logger_name(ctx, logger))
        .cloned()
        .unwrap_or_default();
    // Resolve the producing thread's Java tid once, BEFORE any record
    // allocation below can move freshly-created objects.
    let producer_tid = crate::current_java_thread_tid(ctx);
    let producer_short_tid = crate::short_thread_id(producer_tid);
    for addr in handlers {
        // SAFETY: handlers are strongly referenced by Java-side LogCapture for
        // the whole interval they are registered; this is the same stable-ref
        // convention used by the existing synthetic logger registry above.
        // The reconstructed `ObjectRef` itself is still subject to the usual
        // moving-GC staleness once any GC-capable call runs below, so pin it
        // like every other live reference in this loop.
        let handler = unsafe { object_from_u64(addr) };
        let handler_pin = ctx.pin_native_root(handler);
        let level = ctx.read_native_pin(level_pin, level);
        let message = ctx.read_native_pin(message_pin, message);
        // The real LogRecord constructor reaches private JDK state that is not
        // materialized on our compact JUL path. Handlers require the public
        // record fields, in particular `message`, so initialize that stable
        // surface directly.
        let record = match ctx.new_object("java/util/logging/LogRecord") {
            Ok(Some(Value::Object(Some(record)))) => record,
            _ => continue,
        };
        let record_pin = ctx.pin_native_root(record);
        ctx.set_field_by_name(record, "level", Value::Object(Some(level)));
        ctx.set_field_by_name(record, "message", Value::Object(Some(message)));
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
        let mut messages = log_record_messages()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        messages.insert(record_id, ctx.read_string(message).unwrap_or_default());
        // A long-lived handler must not turn the bridge into an unbounded
        // message cache. Records are ephemeral; retain a generous recent
        // window for in-flight handler delivery only.
        while messages.len() > 4096 {
            let oldest = messages.keys().next().copied();
            if let Some(oldest) = oldest {
                messages.remove(&oldest);
            } else {
                break;
            }
        }
        drop(messages);
        let _ = ctx.invoke_virtual(
            handler,
            "publish",
            "(Ljava/util/logging/LogRecord;)V",
            &[Value::Object(Some(record))],
        );
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
    // Map JUL level names to the same compact tags log_simple uses so
    // grep-able output is consistent across the JUL native surface.
    let tag = match level_name.as_str() {
        "SEVERE" => "ERROR",
        "WARNING" => "WARN",
        "INFO" => "INFO",
        "CONFIG" => "INFO",
        // Keep fine-grained messages off the console, but do publish them to
        // explicitly installed handlers. Tomcat's LogCapture sets a logger to
        // FINE specifically to assert a recoverable handshake underflow.
        "FINE" | "FINER" | "FINEST" => {
            publish_jul_handlers_src(ctx, this, level_obj, message_obj, src_cls_obj, src_mth_obj);
            let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let throwable_obj = throwable_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
            let result = if let (Some(logger), Some(level), Some(message)) =
                (this, level_obj, message_obj)
            {
                // The 5-arg `logp` overload's Throwable belongs ON the record
                // (`LogRecord.getThrown()`), not only in a console detail
                // line: `Logger.throwing` is defined in terms of exactly this
                // call, and a handler that reports the throwable saw null.
                publish_to_jul_handlers_full(
                    ctx,
                    logger,
                    level,
                    message,
                    src_cls_obj,
                    src_mth_obj,
                    None,
                    throwable_obj,
                )
                .map(|_| None)
            } else {
                Ok(None)
            };
            if let Some(base) = base_pin {
                ctx.unpin_native_roots(base);
            }
            return result;
        }
        other => other,
    };
    let message = message_obj
        .and_then(|o| ctx.read_string(o))
        .unwrap_or_default();
    notify_jul_logger_filter(ctx, this, level_obj, message_obj, throwable_obj);
    publish_jul_handlers_src(ctx, this, level_obj, message_obj, src_cls_obj, src_mth_obj);
    let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let throwable_obj = throwable_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
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
            crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message} ({cls})"));
        } else {
            crate::emit_framework_log(
                ctx,
                &format!("{tag} [{logger_name}] {message} ({cls}: {detail})"),
            );
        }
    } else {
        crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message}"));
    }
    // `emit_framework_log` dispatches `println` into (overridable) Java
    // bytecode and allocates the argument String, so every reference below has
    // to come off its pin again.
    let this = this_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let level_obj = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let message_obj = message_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let throwable_obj = throwable_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_cls_obj = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_mth_obj = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let result = if let (Some(logger), Some(level), Some(message)) = (this, level_obj, message_obj)
    {
        // Same as the trace-level arm above: keep the Throwable on the record.
        publish_to_jul_handlers_full(
            ctx,
            logger,
            level,
            message,
            src_cls_obj,
            src_mth_obj,
            None,
            throwable_obj,
        )
        .map(|_| None)
    } else {
        Ok(None)
    };
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    result
}

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
fn jul_trace_marker(ctx: &mut dyn NativeContext, args: &[Value], marker: &str) -> MethodCallResult {
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
    notify_jul_logger_filter(ctx, Some(this), level, Some(message), thrown);
    let this = ctx.read_native_pin(base_pin, this);
    let message = ctx.read_native_pin(message_pin, message);
    let level = level_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_cls = src_cls_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let src_mth = src_mth_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    let thrown = thrown_pin.map(|(pin, o)| ctx.read_native_pin(pin, o));
    if let Some(level) = level {
        // No console fallback: FINER is trace-level and `logp` deliberately
        // keeps FINE/FINER/FINEST off the console sink, so a logger with no
        // handler chain stays as quiet here as it is there.
        let _ =
            publish_to_jul_handlers_full(ctx, this, level, message, src_cls, src_mth, None, thrown);
    }
    ctx.unpin_native_roots(base_pin);
    Ok(None)
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
fn jul_use_parent_handlers(ctx: &dyn NativeContext, logger: ObjectRef) -> bool {
    match ctx.get_field_by_name(logger, "config") {
        Value::Object(Some(config)) => !matches!(
            ctx.get_field_by_name(config, "useParentHandlers"),
            Value::Int(0)
        ),
        _ => true,
    }
}

/// Resolve the handler `ArrayList` that should receive a record published for
/// `logger`: the logger's own handlers first, then (when `useParentHandlers`
/// allows it) the nearest dotted-name ancestor that has any.
///
/// GC: the ancestor walk demand-creates loggers and therefore allocates, so
/// callers MUST re-derive every reference they still hold — including `logger`
/// itself — from its pin after this returns.
fn resolve_jul_handler_list(ctx: &mut dyn NativeContext, logger: ObjectRef) -> Option<ObjectRef> {
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
            if ctx.class_name_of_id(ctx.class_id_of_object(name)).as_deref()
                == Some("java/lang/String"));
    if synthetic_layout {
        // `allocate_logger` now populates this same slot with a real parent
        // `Logger` (see ancestor walk below) rather than a handlers list --
        // don't misread it as one.
        if let Value::Object(Some(list)) = ctx.get_field(logger, LOGGER_FIELD_PARENT) {
            if ctx
                .class_name_of_id(ctx.class_id_of_object(list))
                .as_deref()
                != Some(CLS_JUL_LOGGER)
            {
                return Some(list);
            }
        }
    }
    // No handlers on the exact logger (and it isn't the legacy slot-2 layout
    // above): walk dotted-name ancestors up to the root, mirroring real JUL's
    // parent-handler propagation (`useParentHandlers`, on by default). Our
    // loggers have no real object parent chain to traverse here, so walk by
    // name instead -- covers the common case of a single ConsoleHandler
    // installed on the root logger by `readConfiguration`.
    let logger_name = read_jul_logger_name(ctx, logger);
    if !jul_use_parent_handlers(ctx, logger) {
        return None;
    }
    let mut candidate: &str = &logger_name;
    loop {
        if candidate.is_empty() {
            return None;
        }
        candidate = match candidate.rfind('.') {
            Some(idx) => &candidate[..idx],
            None => "",
        };
        let ancestor = get_or_create_logger(ctx, candidate);
        if let Some(h) = crate::jul_logger_handlers_get(ctx, ancestor) {
            return Some(h);
        }
    }
}

/// Publish an ALREADY-CONSTRUCTED `LogRecord` through `logger`'s handler
/// chain — the `Logger.log(LogRecord)` path, and the tail of the real-JDK
/// `throwing`/`entering` bytecode when it reaches `doLog`.
///
/// Returns `true` when at least one handler accepted the record so the caller
/// can keep its console-sink fallback for handler-less loggers.
fn publish_existing_record_to_jul_handlers(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    record: ObjectRef,
) -> bool {
    let logger_pin = ctx.pin_native_root(logger);
    let record_pin = ctx.pin_native_root(record);
    let Some(handlers) = resolve_jul_handler_list(ctx, logger) else {
        ctx.unpin_native_roots(logger_pin);
        return false;
    };
    let handlers_pin = ctx.pin_native_root(handlers);
    // The resolution above allocates (ancestor demand-creation); re-derive
    // both pinned references before touching them again.
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
    let handlers = ctx.read_native_pin(handlers_pin, handlers);
    let size = match ctx.invoke_virtual(handlers, "size", "()I", &[]) {
        Ok(Some(Value::Int(size))) if size > 0 => size as usize,
        _ => {
            ctx.unpin_native_roots(logger_pin);
            return false;
        }
    };
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
        if ctx
            .invoke_virtual(
                handler,
                "publish",
                "(Ljava/util/logging/LogRecord;)V",
                &[Value::Object(Some(record))],
            )
            .is_ok()
        {
            delivered = true;
        }
        let handler = ctx.read_native_pin(handler_pin, handler);
        let _ = ctx.invoke_virtual(handler, "flush", "()V", &[]);
    }
    ctx.unpin_native_roots(logger_pin);
    delivered
}

/// Deliver a native-intercepted JUL call to handlers added to the synthetic
/// logger. The LogManager bridge owns `logp` and `log(Level, String)`, so
/// printing those calls alone bypassed JULI's `AsyncFileHandler` entirely.
fn publish_to_jul_handlers(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    level: ObjectRef,
    message: ObjectRef,
) -> MethodCallResult {
    publish_to_jul_handlers_src(ctx, logger, level, message, None, None)
}

/// `publish_to_jul_handlers` with the caller-provided source class/method
/// pair (`Logger.logp` args) stamped into the bridged record.
fn publish_to_jul_handlers_src(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    level: ObjectRef,
    message: ObjectRef,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
) -> MethodCallResult {
    publish_to_jul_handlers_full(ctx, logger, level, message, src_cls, src_mth, None, None)?;
    Ok(None)
}

/// `publish_to_jul_handlers_src` plus the two record payloads the
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
#[allow(clippy::too_many_arguments)]
fn publish_to_jul_handlers_full(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    level: ObjectRef,
    message: ObjectRef,
    src_cls: Option<ObjectRef>,
    src_mth: Option<ObjectRef>,
    params: Option<ObjectRef>,
    thrown: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    let base_pin = ctx.pin_native_root(logger);
    let level_pin = ctx.pin_native_root(level);
    let message_pin = ctx.pin_native_root(message);
    // Released with base_pin below (stack discipline).
    let src_cls_pin = src_cls.map(|o| (ctx.pin_native_root(o), o));
    let src_mth_pin = src_mth.map(|o| (ctx.pin_native_root(o), o));
    let params_pin = params.map(|o| (ctx.pin_native_root(o), o));
    let thrown_pin = thrown.map(|o| (ctx.pin_native_root(o), o));
    let result: Result<bool, MethodCallFailed> = (|| {
        let logger = ctx.read_native_pin(base_pin, logger);
        let Some(handlers) = resolve_jul_handler_list(ctx, logger) else {
            return Ok(false);
        };
        let handlers_pin = ctx.pin_native_root(handlers);
        // The ancestor walk inside `resolve_jul_handler_list` demand-creates
        // loggers (and so allocates): re-derive every reference used below
        // from its pin before touching it again.
        let logger = ctx.read_native_pin(base_pin, logger);
        let level = ctx.read_native_pin(level_pin, level);
        let message = ctx.read_native_pin(message_pin, message);
        let record = match ctx.new_object_initialized(
            "java/util/logging/LogRecord",
            "(Ljava/util/logging/Level;Ljava/lang/String;)V",
            &[Value::Object(Some(level)), Value::Object(Some(message))],
        )? {
            Some(Value::Object(Some(record))) => record,
            _ => return Ok(false),
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
        // (docs/internal/tomcat-08-07/dohead-post-fix-sporadic-residuals-FIXED.md).
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
        let message = ctx.read_native_pin(message_pin, message);
        // The compact VM may not materialize the JDK's private LogRecord
        // layout through its constructor. FileHandler.isLoggable() and its
        // formatter consume the public level/message surface, so make that
        // surface explicit just as the direct-handler bridge does.
        ctx.set_field_by_name(record, "level", Value::Object(Some(level)));
        ctx.set_field_by_name(record, "message", Value::Object(Some(message)));
        ctx.set_field(record, 4, Value::Object(Some(message)));
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
        let message = ctx.read_native_pin(message_pin, message);
        let _ = ctx.invoke_virtual(
            record,
            "setMessage",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(message))],
        );
        // `setMessage` above can GC; refresh both `record` and `message`
        // (the latter is also read again below, past the same call) before
        // touching either again.
        let record = ctx.read_native_pin(record_pin, record);
        let message = ctx.read_native_pin(message_pin, message);
        let record_id = next_log_record_id();
        ctx.set_field(record, 1, Value::Long(record_id));
        {
            let mut messages = log_record_messages()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            messages.insert(record_id, ctx.read_string(message).unwrap_or_default());
            // Mirror the sibling registry's retention cap. Without it this
            // identity-keyed delivery path -- the ONLY one that fires for a
            // real `Logger.getLogger(...)` logger (the name-keyed sibling's
            // lookup misses there, so its in-loop eviction never runs) --
            // grows the process-global side table by one entry per log call,
            // forever.
            while messages.len() > 4096 {
                let oldest = messages.keys().next().copied();
                if let Some(oldest) = oldest {
                    messages.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let handlers = ctx.read_native_pin(handlers_pin, handlers);
        let size = match ctx.invoke_virtual(handlers, "size", "()I", &[])? {
            Some(Value::Int(size)) if size > 0 => size as usize,
            _ => return Ok(false),
        };
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
            if ctx
                .invoke_virtual(
                    handler,
                    "publish",
                    "(Ljava/util/logging/LogRecord;)V",
                    &[Value::Object(Some(record))],
                )
                .is_ok()
            {
                delivered = true;
            }
            // FileHandler buffers output. The native publication bridge is
            // synchronous, so preserve JUL's observable completion contract
            // before the caller inspects its per-webapp log file.
            //
            // GC SAFETY: `publish` above is arbitrary, overridable Java
            // bytecode that can GC; refresh `handler` from its pin before
            // reusing it for `flush` below (same hazard class as the
            // `record`/`message` fix above this loop).
            let handler = ctx.read_native_pin(handler_pin, handler);
            let _ = ctx.invoke_virtual(handler, "flush", "()V", &[]);
        }
        Ok(delivered)
    })();
    ctx.unpin_native_roots(base_pin);
    result
}

/// `CRATONVM_DBG_JUL=1`: trace which JUL native each probe call actually
/// reaches and what it does with the record. Same shape (and rationale) as
/// `CRATONVM_DBG_DROPPED_STUBS` in `native-api::registry` — the JUL surface is
/// registered from six different registrars across three crates with
/// last-registration-wins semantics, so "which implementation ran" is not
/// answerable by reading the source alone. Cheap/no-op when unset.
fn jul_dbg_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JUL").is_some())
}

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
fn jul_is_throwable(ctx: &dyn NativeContext, o: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(o);
    // Depth guard: a corrupt or self-referential chain must not spin here.
    for _ in 0..64 {
        match ctx.class_name_of_id(cid).as_deref() {
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
fn jul_resolve_msg(ctx: &mut dyn NativeContext, o: ObjectRef) -> String {
    if let Some(s) = ctx.read_string(o) {
        return s;
    }
    let supplier_class = ctx.class_id_by_name("java/util/function/Supplier");
    let object_class = ctx.class_id_of_object(o);
    if supplier_class
        .is_some_and(|supplier| object_class == supplier || ctx.is_subclass(object_class, supplier))
    {
        if let Ok(Some(Value::Object(Some(r)))) =
            ctx.invoke_virtual(o, "get", "()Ljava/lang/Object;", &[])
        {
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
pub(crate) fn native_jul_logger_log_throwable(
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
    // `jul_resolve_msg` may invoke Java supplier code and allocate. Keep the
    // receiver/level rooted while identifying the throwable so the later
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
                let text = jul_resolve_msg(ctx, o);
                Some(ctx.create_string(&text))
            }
        }
        None => None,
    };
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
    crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {msg}"));
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
            if delivered {
                return Ok(None);
            }
            (Some(logger), rec)
        }
        None => (None, rec),
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
                crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {r}"));
            } else {
                crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message}\n{r}"));
            }
        }
        None => crate::emit_framework_log(ctx, &format!("{tag} [{logger_name}] {message}")),
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
pub(crate) fn native_jul_logger_is_loggable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
                .and_then(|n| jul_standard_level_value(&n))
        })
        .unwrap_or(800);
    let configured_threshold = logger
        .and_then(|logger| match ctx.get_field_by_name(logger, "config") {
            Value::Object(Some(config)) => match ctx.get_field_by_name(config, "levelObject") {
                Value::Object(Some(level)) => match ctx.get_field_by_name(level, "value") {
                    Value::Int(value) => Some(value),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(800);
    let threshold = logger
        .map(|logger| read_jul_logger_name(ctx, logger))
        .map(|name| jul_ancestor_explicit_level(&name).unwrap_or(configured_threshold))
        .unwrap_or(configured_threshold);
    Ok(Some(Value::Int(if level_value >= threshold {
        1
    } else {
        0
    })))
}

/// Map one of the 9 standard `java.util.logging.Level` names to its `int`
/// value. Mirrors `java.util.logging.Level`'s built-in constants.
fn jul_standard_level_value(name: &str) -> Option<i32> {
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
fn jul_ancestor_explicit_level(logger_name: &str) -> Option<i32> {
    let levels = logger_explicit_levels()
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

pub(crate) fn record_jul_logger_level(ctx: &dyn NativeContext, logger: ObjectRef, level: Value) {
    let name = read_jul_logger_name(ctx, logger);
    if name.is_empty() {
        return;
    }
    let value = match level {
        Value::Object(Some(level)) => match ctx.get_field_by_name(level, "value") {
            Value::Int(value) => Some(value),
            _ => None,
        },
        _ => None,
    };
    let mut levels = logger_explicit_levels()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(value) = value {
        levels.insert(name, value);
    } else {
        levels.remove(&name);
    }
}

fn native_jul_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "INFO");
    publish_jul_convenience(ctx, args, "INFO")?;
    Ok(None)
}
fn native_jul_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "WARN");
    publish_jul_convenience(ctx, args, "WARNING")?;
    Ok(None)
}
fn native_jul_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    log_simple(ctx, args, "ERROR");
    publish_jul_convenience(ctx, args, "SEVERE")?;
    Ok(None)
}
fn native_jul_logger_fine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Keep fine/finer/finest quiet on the console by default (real JUL's
    // root level is INFO), but honor an ancestor level raised to
    // DEBUG/FINE-or-finer (e.g. Spring Boot's `LoggingSystem.setLogLevel`)
    // and explicit JUL handlers (for example Tomcat's LogCapture) must
    // still receive the record -- see `publish_jul_convenience`'s
    // `isLoggable` gate.
    publish_jul_convenience(ctx, args, "FINE")?;
    Ok(None)
}

fn publish_jul_convenience(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level_name: &str,
) -> MethodCallResult {
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(message)))) =
        (args.first(), args.get(1))
    else {
        return Ok(None);
    };
    let Some(level) = resolve_standard_level(ctx, level_name) else {
        return Ok(None);
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
        return Ok(None);
    }
    publish_to_jul_handlers(ctx, *logger, level, *message)
}

fn log_simple(ctx: &mut dyn NativeContext, args: &[Value], level: &str) {
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
        _ => "INFO",
    };
    if let Some(logger) = this {
        if let Some(level_obj) = resolve_standard_level(ctx, jul_level_name) {
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
    // `eprintln!` writes to the process's raw OS stderr, entirely bypassing
    // the Java-level `System.out`/`System.err` `PrintStream` objects — so
    // JUnit5's `OutputCaptureExtension` (which substitutes those objects via
    // `System.setOut`/`setErr`) never sees these `java.util.logging.Logger`
    // convenience-method records, even though a human watching the console
    // (or this process's raw stderr) sees them fine. Same bug class as the
    // Logback/commons-logging `emit_framework_log` fix — route through the
    // live (possibly test-substituted) stream instead.
    // See docs/known-issues/springboot/propertiesmigration-logfactory-oom-residual.md.
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
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

fn native_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // We never load a filesystem-backed configuration (see
    // `native_read_configuration_no_arg`), but a prior `readConfiguration
    // (InputStream)` call (Spring Boot's `JavaLoggingSystem`) does populate
    // `parsed_log_properties` -- consult it so e.g. a `Handler` subclass's
    // own constructor bytecode querying `LogManager.getProperty(cname +
    // ".level")` observes the same config `apply_jul_config_entries`
    // already applied directly.
    let key = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(key) = key else {
        return Ok(Some(Value::Object(None)));
    };
    let value = parsed_log_properties()
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
    for &addr in tomcat_juli_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        push_addr(addr);
    }
    for handlers in tomcat_juli_root_handler_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
    {
        for &addr in handlers {
            push_addr(addr);
        }
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
    for addr in tomcat_juli_logger_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
    }
    for handlers in tomcat_juli_root_handler_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        for addr in handlers {
            *addr = remap(*addr);
        }
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
    if crate::nbflags().jboss_logger_base_emit {
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

    registry.register(
        "java/util/logging/LogRecord",
        "getMessage",
        "()Ljava/lang/String;",
        native_jul_log_record_get_message,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_add_handler,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_remove_handler,
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
    // `entering`/`exiting`/`throwing` — the method-trace family. The real-JDK
    // bodies build their FINER record and push it through the private
    // `doLog`, which dereferences `Logger.loggerBundle`; that field is null on
    // the Logger instances `getLogger` mints here (no constructor ever ran),
    // which is what made `throwing` raise
    // `NullPointerException: ... "lb" is null`. `allocate_logger` now seeds
    // the field, and these natives additionally make the whole family behave
    // identically no matter which JUL class body is loaded — including
    // stamping `LogRecord.thrown`, which the console-only path dropped.
    // NOTE: `phases_late::register_p71_logging_extras` registers the same
    // three triples, but only along the synthetic-JDK path; this registrar is
    // the one that also runs in default real-JDK mode (via
    // `reflect_annotations::register_annotation_overrides`) and, being
    // registered later, wins in both.
    registry.register(
        CLS_JUL_LOGGER,
        "entering",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| jul_trace_marker(ctx, args, "ENTRY"),
    );
    registry.register(
        CLS_JUL_LOGGER,
        "exiting",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| jul_trace_marker(ctx, args, "RETURN"),
    );
    registry.register(
        CLS_JUL_LOGGER,
        "throwing",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Throwable;)V",
        |ctx, args| jul_trace_marker(ctx, args, "THROW"),
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::mock_ctx;
    use cratonvm_native_api::NativeMethodRegistry;
    use cratonvm_types::Value;

    static LOGF_SECOND_OLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LOGF_SECOND_NEW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LOGF_THROWABLE_OLD: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static LOGF_THROWABLE_NEW: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static LOGF_SECOND_SEEN: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static LOGF_TOSTRING_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn relocating_logf_to_string(
        ctx: &mut crate::test_utils::MockNativeContext,
        receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name != "toString" {
            return None;
        }
        use std::sync::atomic::Ordering;
        let call = LOGF_TOSTRING_CALLS.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            ctx.remap_native_pin_addr_for_test(
                LOGF_SECOND_OLD.load(Ordering::SeqCst),
                LOGF_SECOND_NEW.load(Ordering::SeqCst),
            );
            ctx.remap_native_pin_addr_for_test(
                LOGF_THROWABLE_OLD.load(Ordering::SeqCst),
                LOGF_THROWABLE_NEW.load(Ordering::SeqCst),
            );
        } else if call == 1 {
            LOGF_SECOND_SEEN.store(receiver.as_ptr() as usize, Ordering::SeqCst);
        }
        let rendered = ctx.create_string(if call == 0 { "first" } else { "second" });
        Some(Ok(Some(Value::Object(Some(rendered)))))
    }

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
    fn do_logf_refreshes_later_params_after_earlier_to_string_moves_them() {
        use std::sync::atomic::Ordering;

        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mut ctx = mock_ctx();
        let first = ctx.fresh_object_ref();
        let second_old = ctx.fresh_object_ref();
        let second_new = ctx.fresh_object_ref();
        let throwable_old = ctx.fresh_object_ref();
        let throwable_new = ctx.fresh_object_ref();
        let params = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
        ctx.set_array_element(params, 0, Value::Object(Some(first)));
        ctx.set_array_element(params, 1, Value::Object(Some(second_old)));
        let format = ctx.create_string("%s %s");

        LOGF_SECOND_OLD.store(second_old.as_ptr() as usize, Ordering::SeqCst);
        LOGF_SECOND_NEW.store(second_new.as_ptr() as usize, Ordering::SeqCst);
        LOGF_THROWABLE_OLD.store(throwable_old.as_ptr() as usize, Ordering::SeqCst);
        LOGF_THROWABLE_NEW.store(throwable_new.as_ptr() as usize, Ordering::SeqCst);
        LOGF_SECOND_SEEN.store(0, Ordering::SeqCst);
        LOGF_TOSTRING_CALLS.store(0, Ordering::SeqCst);
        ctx.set_invoke_virtual_hook(relocating_logf_to_string);

        native_jboss_logging_logger_do_logf(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(format)),
                Value::Object(Some(params)),
                Value::Object(Some(throwable_old)),
            ],
        )
        .unwrap();

        assert_eq!(LOGF_TOSTRING_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(
            LOGF_SECOND_SEEN.load(Ordering::SeqCst),
            second_new.as_ptr() as usize,
            "the second parameter must be re-read from its remapped native pin"
        );
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    #[test]
    fn jul_explicit_handler_and_log_record_message_bridge() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = allocate_logger(&mut ctx, "org.example.capture");
        let handler = alloc_concurrent_synthetic(&mut ctx, "java/util/logging/Handler", 0);
        native_jul_logger_add_handler(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(handler))],
        )
        .unwrap();
        assert_eq!(
            logger_handlers()
                .lock()
                .unwrap()
                .get("org.example.capture")
                .map(Vec::len),
            Some(1)
        );

        let record = alloc_concurrent_synthetic(&mut ctx, "java/util/logging/LogRecord", 5);
        ctx.set_field(record, 1, Value::Long(42));
        log_record_messages()
            .lock()
            .unwrap()
            .insert(42, "captured banner".to_string());
        let message = native_jul_log_record_get_message(&mut ctx, &[Value::Object(Some(record))])
            .unwrap()
            .unwrap();
        let Value::Object(Some(message)) = message else {
            panic!("expected LogRecord message");
        };
        assert_eq!(ctx.read_string(message).as_deref(), Some("captured banner"));

        native_jul_logger_remove_handler(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(handler))],
        )
        .unwrap();
        assert!(logger_handlers()
            .lock()
            .unwrap()
            .get("org.example.capture")
            .is_some_and(Vec::is_empty));
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
        // `get_or_create_logger` materialises a real Logger for every
        // dotted-name ancestor up to the root so `getEffectiveLevel()`-style
        // walks and JUL's parent-handler propagation terminate correctly
        // (5bfc73479). Registering "a.b.c" and "d.e.f" therefore also creates
        // "a", "a.b", "d", "d.e" and the shared root "" — 7 entries, not 2.
        // What matters here is that both requested loggers are present and
        // that every entry is an ancestor of one of them.
        {
            let reg = logger_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, ["", "a", "a.b", "a.b.c", "d", "d.e", "d.e.f"]);
        }
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
        // The root logger "" is always present: `get_or_create_logger` links
        // every logger to a real parent chain terminating at it (5bfc73479),
        // and real `LogManager.getLoggerNames()` likewise always enumerates
        // the root. These three names have no dots, so "" is the only
        // ancestor added.
        let mut expected = vec![
            String::new(),
            "bar".to_string(),
            "baz".to_string(),
            "foo".to_string(),
        ];
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
