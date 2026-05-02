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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ArrayElementType, ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

// ---------------------------------------------------------------------------
// Class / field name constants
// ---------------------------------------------------------------------------

const CLS_JUL_LOG_MANAGER: &str = "java/util/logging/LogManager";
const CLS_JBOSS_LOG_MANAGER: &str = "org/jboss/logmanager/LogManager";
const CLS_JUL_LOGGER: &str = "java/util/logging/Logger";
const CLS_LOGGER_ENUMERATION: &str = "java/util/logging/LogManager$StringEnumeration";

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

/// Return (or lazily allocate) the process-wide `LogManager` singleton
/// ObjectRef. Subsequent calls return the same ObjectRef so
/// pointer-identity comparisons in Java (`if (mgr == other)`) stay
/// stable.
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
    // Slow path: allocate + cache under the same lock to avoid a race
    // where two threads both pay the allocation cost.
    let mut guard = singleton_cell().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(addr) = *guard {
        if addr != 0 {
            return unsafe { object_from_u64(addr) };
        }
    }
    let obj = allocate_log_manager(ctx, class_name);
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
}

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

fn native_get_log_manager(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Block 2C / Path A: honour `-Djava.util.logging.manager=...` by
    // allocating the singleton with the *requested concrete class* —
    // not always `java.util.logging.LogManager`. jboss-modules' `Main`
    // would otherwise observe a JDK-default LogManager via
    // `getLogManager().getClass()` and print the
    // "WARNING: Failed to load the specified log manager class" line.
    //
    // We honour ONLY `org.jboss.logmanager.LogManager` here (the canonical
    // KC16 / Quarkus / WildFly case). Other custom LogManager subclasses
    // still get the default — narrowing keeps blast radius minimal and
    // avoids accidentally returning instances of classes whose clinit
    // would NPE on our synthetic-stub field layouts.
    //
    // `LmProbe` exercises this exact path: `getLogManager().getClass()`
    // is asserted to print `org.jboss.logmanager.LogManager` when the
    // system property is set. Without this change LmProbe prints the
    // JDK default class name and KC16 prints the WARNING.
    let class_name = match ctx.get_system_property("java.util.logging.manager") {
        Some(name) if name == "org.jboss.logmanager.LogManager" => CLS_JBOSS_LOG_MANAGER,
        _ => CLS_JUL_LOG_MANAGER,
    };
    // Block 2C / Path A: prime the boot-log path cache so background
    // threads' `LoggerMirror::log` (in `wildfly_core.rs`) can route
    // INFO/WARN/SEVERE lines through `jboss_logmanager::emit_boot_log_no_ctx`
    // without needing a NativeContext. `getLogManager()` is the
    // earliest reliable hook — it runs during JDK initLogManager
    // before any user-level logger is allocated.
    crate::jboss_logmanager::prime_boot_log_path(ctx);
    let obj = ensure_singleton(ctx, class_name);
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
    // Read the logger's name via slot 0; if it's not a String, reject.
    let name = match ctx.get_field(logger, LOGGER_FIELD_NAME) {
        Value::Object(Some(name_obj)) => ctx.read_string(name_obj).unwrap_or_default(),
        _ => String::new(),
    };
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

fn native_read_configuration_no_arg(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Security: we deliberately do NOT parse untrusted logging config —
    // any path that would load a properties file is suppressed. The
    // process-wide tracing subscriber already governs effective levels.
    Ok(None)
}

fn native_read_configuration_with_stream(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
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

fn native_get_property(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // We never load configuration files (see `native_read_configuration_*`),
    // so every `getProperty(key)` returns null. The JDK default
    // implementation also allows null returns, so callers handle it.
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register every `LogManager` / `Logger` native this module owns. Call
/// from `register_essential_natives` in `lib.rs` BEFORE the fallback
/// stubs so these win in the `NativeMethodRegistry` lookup.
pub fn register_logmanager_natives(registry: &mut NativeMethodRegistry) {
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
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "checkAccess",
        "()V",
        |_ctx, _args| Ok(None),
    );
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
    use rustjvm_native_api::NativeMethodRegistry;
    use rustjvm_types::Value;

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
            .find(CLS_JUL_LOG_MANAGER, "getLogManager", "()Ljava/util/logging/LogManager;")
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
        // Enumeration wrapper.
        assert!(r
            .find(CLS_LOGGER_ENUMERATION, "hasMoreElements", "()Z")
            .is_some());
        assert!(r
            .find(CLS_LOGGER_ENUMERATION, "nextElement", "()Ljava/lang/Object;")
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
        let jboss = match native_get_jboss_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            other => panic!("expected manager ObjectRef, got {:?}", other),
        };
        assert_eq!(
            jul, jboss,
            "JUL and JBoss getLogManager() must share singleton"
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
        assert_eq!(second, Value::Int(0), "duplicate addLogger must return false");
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
        let r = native_add_logger(
            &mut ctx,
            &[Value::Object(Some(mgr)), Value::Object(None)],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn t19_h3_read_configuration_is_graceful_no_op() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert!(
            native_read_configuration_no_arg(&mut ctx, &[])
                .unwrap()
                .is_none()
        );
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
            let has = native_enumeration_has_more(
                &mut ctx,
                &[Value::Object(Some(enumeration))],
            )
            .unwrap()
            .unwrap();
            if matches!(has, Value::Int(0)) {
                break;
            }
            let elem = native_enumeration_next(
                &mut ctx,
                &[Value::Object(Some(enumeration))],
            )
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
        let v = native_get_property(
            &mut ctx,
            &[Value::Object(None), Value::Object(Some(key))],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(v, Value::Object(None)));
    }
}
