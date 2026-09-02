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
//! 4. `readConfiguration()` / `readConfiguration(InputStream)` — both parse
//!    the configuration and apply it (`apply_jul_config_entries`): install
//!    `handlers=` on the root logger and record `<logger>.level` entries.
//!    The no-arg overload follows the JDK's resolution order
//!    (`java.util.logging.config.class`, then `.config.file`, then
//!    `$java.home/conf/logging.properties`).
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

use crate::try_alloc_concurrent_synthetic;

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

// Slot layouts for `java.util.logging.LogManager` and `Logger`.
//
// These are REAL JDK indices, not a private numbering. In real-JDK mode these
// objects carry the real class's layout (they are allocated with the real class
// id by `alloc_concurrent_synthetic`), so every index below addresses whatever
// the JDK declares there. The previous numbering — a compact 0/1/2/3 for the
// four fields this implementation cares about — was measured writing a `String`
// onto `Logger.config`, a `Logger` onto `Logger.name`, and an `Int` onto
// `LogManager.rootLogger`, a reference field. Found 2026-08-05 by running the
// L4 shadow-layout census under three real workloads (commons-lang, Jackson,
// Tomcat) instead of three small probes, which is what the L4 record's standing
// caveat asked for.
//
// Derived from `javap -p --module java.logging` on Temurin 25.0.3:
//
//   LogManager: props(0) systemContext(1) userContext(2) rootLogger(3)
//               readPrimordialConfiguration(4) globalHandlersState(5)
//               configurationLock(6) closeOnResetLoggers(7) listeners(8)
//               initializedCalled(9) initializationDone(10) loggerRefQueue(11)
//
//   Logger:     config(0) manager(1) name(2) loggerBundle(3) anonymous(4)
//               catalogRef(5) catalogName(6) catalogLocale(7) parent(8)
//               kids(9) callerModuleRef(10) isSystemLogger(11)
//
// Two of the four values this implementation keeps have NO real field to live
// in — `loggerRegistry` is a placeholder for Rust-side state, and `ready` is our
// own initialisation bit; a real `Logger`'s level lives inside `config`, not in
// a field of its own. Those are anchored PAST the real field count, where they
// land on padding `define_class_with_options` adds and can corrupt nothing.
// That is the shape the L4 record calls the target for a kind-3 overlay, and
// the shadow-layout diff reports such a slot as a harmless `pad` rather than a
// finding. The models in `ClassManager::synthetic_stub_fields` declare the same
// widths, so the padding is there in both run modes.

/// Real `LogManager` instance-field count on JDK 21–25.
const LM_REAL_FIELDS: usize = 12;
const LM_NUM_FIELDS: usize = LM_REAL_FIELDS + 2;
/// Real: `props`. Same slot as before — and the only one of the four that was
/// already right, which is why it read as a NAME-only disagreement.
const LM_FIELD_PROPERTIES: usize = 0;
const LM_FIELD_ROOT_LOGGER: usize = 3;
/// VM-internal: a placeholder for the Rust-side `LoggerRegistry`. No real field.
const LM_FIELD_LOGGER_REGISTRY: usize = LM_REAL_FIELDS;
/// VM-internal: our own "initialised" bit. No real field — and it used to sit
/// on `rootLogger`, writing an `Int` where the collector's reference map says a
/// `Logger` lives.
const LM_FIELD_READY: usize = LM_REAL_FIELDS + 1;

/// Real `Logger` instance-field count on JDK 21–25.
const LOGGER_REAL_FIELDS: usize = 12;
/// The width `ClassManager::synthetic_stub_fields` declares for
/// `java/util/logging/Logger` (`class_manager.rs:14191`): the 12 real fields
/// plus the one VM-internal slot [`LOGGER_FIELD_LEVEL`] is anchored on.
///
/// `pub(crate)` because it is the width every JUL Logger producer in the tree
/// must ask for. `logging_shims` used to ask 2 or 3 and write its own
/// name/level slots at 0/1 — a second slot map for a class that already had a
/// declaration, which put the name on `config` and the level on `manager`.
pub(crate) const LOGGER_NUM_FIELDS: usize = LOGGER_REAL_FIELDS + 1;
pub(crate) const LOGGER_FIELD_NAME: usize = 2;
pub(crate) const LOGGER_FIELD_PARENT: usize = 8;
/// VM-internal: a real `Logger` has no `level` field at all — the effective
/// level lives inside `config` (`Logger$ConfigurationData`). This used to sit
/// on `manager`.
pub(crate) const LOGGER_FIELD_LEVEL: usize = LOGGER_REAL_FIELDS;

/// What `org.jboss.logmanager.Logger.getEffectiveLevel()` reports for a logger
/// with no explicit level anywhere up its chain.
///
/// `LoggerNode.<init>` seeds `effectiveLevel` with `Logger.INFO_INT` whenever
/// `LogContextInitializer.getInitialLevel(name)` answers null, which is what
/// the interface's `DEFAULT` does — so an unconfigured jboss-logmanager logger
/// stops at INFO exactly like an unconfigured JUL one.
///
/// **This constant was `i32::MIN` for one session, and that was a misreading.**
/// The probe that produced `-2147483648` ran with `quarkus-bootstrap-runner`
/// on the classpath, whose `InitialConfigurator.getInitialLevel("")` returns
/// `Level.ALL` for the ROOT — every logger then inherits it. Re-running the
/// same probe against jboss-logmanager 3.2.2 with NO
/// `LogContextInitializer` provider on the classpath reads
/// `root.effective=800`, `freshControl.effective=800`,
/// `freshControl.isLoggableTrace=false`. The MIN was one application's
/// configuration being read as the library's default. Deleting the provider
/// from the classpath is the control that separates them; see
/// `apply_log_context_initializer`.
const JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL: i32 = 800;

/// `java.util.logging.Level.ALL.intValue()`, and the default
/// `LoggerNode.effectiveMinLevel` — `LoggerNode.<init>` computes
/// `requireNonNullElse(initializer.getMinimumLevel(name), Level.ALL)`.
const JBOSS_LEVEL_ALL_INT: i32 = i32::MIN;

/// `java.util.logging.Level.OFF.intValue()`. `LoggerNode.isLoggableLevel`
/// rejects it outright — `level != OFF_INT && ...` — so `isLoggable(OFF)` is
/// false however low the thresholds are, which no comparison against a
/// threshold can reproduce (OFF is `Integer.MAX_VALUE`, i.e. above every one).
const JBOSS_LEVEL_OFF_INT: i32 = i32::MAX;

// ---------------------------------------------------------------------------
// Process-wide singleton state
// ---------------------------------------------------------------------------

/// Per-VM instance of a process-global side table.
///
/// Every table below caches raw heap addresses, and a heap address only means
/// anything inside the VM that produced it. The process may hold several: the
/// `cratonvm-vm` inline test module stands up an independent `SharedVm` per
/// test, and `NativeContext::vm_identity`'s contract spells the rule out —
/// "native side caches that store heap `ObjectRef`s must scope entries to this
/// value".
///
/// Before this, `logger_registry` and friends were shared across all of them
/// and commented "SAFETY: singleton-style lifetime". That holds for one VM and
/// fails for two: `get_or_create_logger` handed a second VM the first VM's
/// logger address, and `jul_logger_handlers_clear` faulted taking its identity
/// hash — an intermittent EXCEPTION_ACCESS_VIOLATION that killed roughly two
/// of every three full `--features synthetic-jdk` runs. The GC root scan read
/// the same tables, so foreign addresses were being handed to the collector as
/// roots as well.
///
/// The inner tables are leaked rather than reclaimed on VM teardown. That
/// matches what the process already does (a `SharedVm` outlives its test) and
/// keeps the returned `&'static Mutex<T>` shape the ~90 call sites expect, so
/// scoping them cost a `vm` argument and nothing else.
pub(crate) fn per_vm_table<T: Send + Default + 'static>(
    slot: &'static OnceLock<Mutex<HashMap<usize, &'static Mutex<T>>>>,
    vm: usize,
) -> &'static Mutex<T> {
    let outer = slot.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = outer.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(vm)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(T::default()))))
}

/// Holds the `LogManager` singleton's raw ObjectRef address, per VM. Pointer
/// identity is stable for the lifetime of its VM — we never free this object.
fn singleton_cell(vm: usize) -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<Option<u64>>>>> = OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// Whether this VM has performed the primordial configuration read, per VM.
///
/// The JDK's `LogManager.ensureLogManagerInitialized()` finishes by calling
/// `readPrimordialConfiguration()`, which is what puts the `ConsoleHandler`
/// from `$java.home/conf/logging.properties` on the root logger. Ours never
/// did, so a fresh VM had `root.handlers=0` where HotSpot has 1 — and
/// `Logger.getLogger("x").info("hello")` printed NOTHING, silently, because a
/// record with no handler anywhere up the parent chain is simply dropped.
///
/// Latched so the read happens exactly once per VM, and latched BEFORE the
/// read runs: `read_configuration_no_arg_impl` instantiates handler classes,
/// which can re-enter `getLogManager()`.
fn primordial_config_done(vm: usize) -> &'static Mutex<bool> {
    static DONE: OnceLock<Mutex<HashMap<usize, &'static Mutex<bool>>>> = OnceLock::new();
    per_vm_table(&DONE, vm)
}

/// Name -> `Logger` ObjectRef (as raw u64), per VM. Populated on first
/// `getLogger`/`addLogger`. Reads are cheap; a lock is acquired only
/// during mutation.
fn logger_registry(vm: usize) -> &'static Mutex<HashMap<String, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<String, u64>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// JULI's `ClassLoaderLogManager` deliberately permits the same logger name
/// in independent web application class loaders. Keep those synthetic loggers
/// out of the default process-wide registry and key them by the stable identity
/// hash of the current thread context class loader instead.
fn tomcat_juli_logger_registry(vm: usize) -> &'static Mutex<HashMap<(i32, String), u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<(i32, String), u64>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// Root handlers must be scoped by the thread context class loader as well:
/// every web application uses the JUL root name `""`, but each has an
/// independent FileHandler configuration.
fn tomcat_juli_root_handler_registry(vm: usize) -> &'static Mutex<HashMap<i32, Vec<u64>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<i32, Vec<u64>>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// `LogManager.addConfigurationListener(Runnable)` registrations, in
/// registration order, as raw ObjectRef addresses.
///
/// WAVE-4 (2026-07-28): `addConfigurationListener` used to return `this` and
/// silently DROP the listener, and `removeConfigurationListener` was a no-op
/// justified as "consistent with the add". That justification was wrong at the
/// root: the JDK invokes these listeners after every successful
/// `readConfiguration()` / `readConfiguration(InputStream)` /
/// `updateConfiguration(...)`, and this module implements all three for real
/// (`native_read_configuration_no_arg`, `native_read_configuration_with_stream`,
/// `native_update_configuration_with_stream`) — so there WAS an observable
/// effect being thrown away. Frameworks that re-derive their logger levels from
/// such a listener (Spring Boot's `JavaLoggingSystem` reconfiguration, Tomcat
/// JULI users, Log4j's JUL bridge) never learned that the configuration had
/// changed.
///
/// Rooted/remapped by `gc_scan_logmanager_roots` / `gc_update_logmanager_refs`
/// like every other ObjectRef side-table in this module.
fn config_listeners(vm: usize) -> &'static Mutex<Vec<u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<Vec<u64>>>>> = OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// Explicit handlers installed on synthetic JUL loggers. The JDK normally
/// keeps this state in `Logger.ConfigurationData`; our compact Logger mirror
/// deliberately does not model that private layout, so keep the Java-visible
/// handler references here instead. This matters for framework log capture
/// (Tomcat's `LogCapture` is one such user), not only console output.
fn logger_handlers(vm: usize) -> &'static Mutex<HashMap<String, Vec<u64>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<String, Vec<u64>>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// Explicit JUL levels for real Logger objects, keyed by logger name.
///
/// Their private configuration layout is not always materialized by the VM,
/// and the synthetic Logger's own level slot holds one of several shapes
/// (`Level` object or raw int) depending on which of the several competing
/// `setLevel` registrations won the registry slot. This name-keyed table is the
/// one representation every `setLevel` writes and `isLoggable` reads, so the
/// answer no longer depends on that registration race.
///
/// Per VM: logger names are not unique across VMs (every test VM has a
/// `""` root), so a process-wide table let one VM's `setLevel` silently
/// re-level another's logger.
fn logger_explicit_levels(vm: usize) -> &'static Mutex<HashMap<String, i32>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<String, i32>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
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
///
/// # This is the state that blocks `java/util/logging`'s retirement
///
/// The singleton is ALLOCATED here and never CONSTRUCTED: `<init>` does not
/// run, and only four of its fourteen slots are written. Measured against
/// HotSpot 25.0.3 on 2026-08-11 (`--add-opens
/// java.logging/java.util.logging=ALL-UNNAMED`, reflecting the object
/// `LogManager.getLogManager()` returns), eight reference fields are null here
/// and real there: `props`, `systemContext`, `userContext`, `rootLogger`,
/// `configurationLock`, `closeOnResetLoggers`, `listeners`, `loggerRefQueue`.
///
/// That was survivable for as long as every accessor was a native, and it is
/// still what this function produces. What it is NOT is the cause of the
/// strict-mode `Logger.getLogger` regression, and the correction matters
/// because the obvious repair here — "run `<init>` on the singleton" — is
/// inert.
///
/// # This object is not on the `--jdk-only` path at all
///
/// Measured 2026-08-11 against `target/release/cratonvm.exe` with
/// `--explain-jdk-only --dump-native-registry`, reading the `registered_by` /
/// `overwrote` chain the schema-4 census exists to expose:
///
/// ```text
/// LogManager.getLogManager()  Compatible          --jdk-only
///   phases_early.rs:20272     intrinsic           intrinsic   <- SURVIVES, wins
///   lib.rs:16964              overwrote intrinsic  (refused)
///   logmanager.rs:5472        overwrote s-stub, WINS  (refused)
/// ```
///
/// Three registrars hold that one triple. In `Compatible` the last one — this
/// file's, which caches through `ensure_singleton` — overwrites the other two
/// and wins. Under `--jdk-only` the retirement refuses this file's and
/// `lib.rs`'s, because both are `Bridge`s over real bytecode; **a refusal does
/// not remove the earlier registration it was going to overwrite**, so what is
/// left holding the triple is `phases_early.rs`'s, which was registered
/// `Intrinsic` and is therefore exempt from the retirement. Its body is a bare
/// `try_alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0)`
/// with no cache, so strict-mode `getLogManager()` mints a FRESH,
/// unconstructed manager on every call — measured, `identityHashCode` 11, 12,
/// 13 on three successive calls, versus a stable value in `Compatible` and on
/// HotSpot.
///
/// So `Logger.getLogger` does not NPE because the singleton below is
/// unconstructed. It NPEs because it never sees the singleton below.
///
/// # And the state the retirement wanted is ALREADY REAL
///
/// `LogManager.<init>()V` is itself a retired shadow, so under `--jdk-only`
/// the real ctor runs and the static `LogManager.manager` — the object real
/// `getLogManager()` bytecode returns — comes out fully built. Measured with
/// `--add-opens java.logging/java.util.logging=ALL-UNNAMED`, reflecting the
/// static rather than the value `getLogManager()` handed back:
///
/// ```text
///   props Properties · systemContext SystemLoggerContext · userContext
///   LoggerContext · configurationLock ReentrantLock · closeOnResetLoggers
///   CopyOnWriteArrayList · listeners SynchronizedMap · loggerRefQueue
///   ReferenceQueue · rootLogger null
/// ```
///
/// Seven of the eight, and `rootLogger` is null on purpose: the JDK writes it
/// only from `ensureLogManagerInitialized`, which no-ops unless the receiver
/// is the static `manager` — which this one IS, so the null is instead the
/// `initializationDone` handshake not having run, and every consumer on the
/// `getLogger` path is guarded for it (`requiresDefaultLoggers`,
/// `ensureDefaultLogger`, `processParentHandlers`, all read off `javap`).
///
/// **Therefore the §1.4-correct fix adds no state-building code at all**: stop
/// `phases_early.rs`'s `Intrinsic` from shadowing the triple, and strict mode
/// returns the already-real static. Adding an `<init>` call to this function
/// was written, measured to be inert in BOTH modes — never reached under
/// `--jdk-only`, and in `Compatible` the `<init>` triple resolves to
/// `native_jboss_init`, a bare `Ok(None)` — and reverted rather than landed as
/// a fix that moves nothing. The patch, and why the interim hold-back is also
/// out of reach from this file, are in
/// docs/known-issues/jdk-only/W7-25-jul-getlogger-regression.md.
///
/// Do not "simplify" the nulls away without reading that: they are what this
/// file's own `Compatible`-mode natives are written around.
fn allocate_log_manager(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, class_name, LM_NUM_FIELDS)?;
    // Leave slot 0 as `null` — a properly-initialized `Properties` would
    // round-trip through synthetic HashMap natives, but most Quarkus/JBoss
    // code reads it via accessors we no-op, so null is safe. That argument
    // holds because this object is only ever reached in COMPATIBLE mode,
    // where every accessor is still a native; under `--jdk-only` this
    // function is not on the path at all. See the doc comment — the
    // difference matters, because "run `<init>` here" reads like the fix and
    // was measured to change nothing.
    ctx.set_field(obj, LM_FIELD_PROPERTIES, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_LOGGER_REGISTRY, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_ROOT_LOGGER, Value::Object(None));
    ctx.set_field(obj, LM_FIELD_READY, Value::Int(1));
    Ok(obj)
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
fn try_allocate_property_log_manager(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let Some(prop) = ctx.get_system_property("java.util.logging.manager") else {
        return Ok(None);
    };
    let dotted = prop.trim();
    if dotted.is_empty() {
        return Ok(None);
    }
    // Built-in aliases use our synthetic singleton layout; calling
    // `<init>` on them through the bytecode path would either re-enter
    // this function or trip the `native_jboss_init` no-op contract.
    let internal = dotted.replace('.', "/");
    if internal == CLS_JUL_LOG_MANAGER {
        return Ok(None);
    }
    if internal == CLS_JBOSS_LOG_MANAGER {
        return Ok(Some(allocate_log_manager(ctx, CLS_JBOSS_LOG_MANAGER)?));
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
        return Ok(None);
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
        return Ok(None);
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
            return Ok(None);
        }
    };
    if let Err(e) = ctx.invoke(&internal, "<init>", "()V", &[Value::Object(Some(obj))]) {
        tracing::warn!(
            class = %dotted,
            error = ?e,
            "java.util.logging.manager: <init> threw, falling back"
        );
        return Ok(None);
    }
    Ok(Some(obj))
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
fn ensure_singleton(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    // Fast path: already cached.
    {
        let guard = singleton_cell(vm).lock().unwrap_or_else(|e| e.into_inner());
        if let Some(addr) = *guard {
            if addr != 0 {
                // SAFETY: the cached address was produced by `alloc_object`
                // earlier in this process. Singleton lifetime == process lifetime.
                return unsafe { Ok(object_from_u64(addr)) };
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
        Ok(None)
    };
    let obj = match chose_property_class? {
        Some(o) => o,
        None => allocate_log_manager(ctx, class_name)?,
    };
    let mut guard = singleton_cell(vm).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(addr) = *guard {
        if addr != 0 {
            // Another thread beat us; drop our allocation on the floor
            // (we have no external references to it yet) and adopt
            // theirs.
            return unsafe { Ok(object_from_u64(addr)) };
        }
    }
    *guard = Some(obj.as_ptr() as u64);
    Ok(obj)
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
/// Every non-root logger gets a real parent link -- the NEAREST ALREADY
/// EXISTING dotted-name ancestor, or the root logger when there is none -- so
/// `getParent()` /
/// `getEffectiveLevel()`-style ancestor walks terminate correctly instead
/// of chasing a permanently-null parent. The root logger ("") has no
/// parent but is seeded with the JDK-default `Level.INFO` so those same
/// walks stop at the root instead of dereferencing a null level.
fn allocate_logger(ctx: &mut dyn NativeContext, name: &str) -> Result<ObjectRef, MethodCallFailed> {
    // Ensure the wildfly_core registry also learns about this name so
    // log-level overrides and credential redaction apply uniformly. The
    // wildfly layer returns an `Arc<LoggerMirror>` — we don't need the
    // Arc itself here, just the side-effect of interning the name.
    let _mirror = crate::wildfly_core::get_logger(name);
    let obj = try_alloc_concurrent_synthetic(ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS)?;
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
        // Real JUL does NOT materialise the intermediate namespace nodes:
        // `LogManager.addLogger` records the name in a `LogNode` tree but only
        // ever links the new `Logger` to the nearest ancestor that ALREADY has
        // a Logger object, falling back to the root. Demand-creating the
        // immediate dotted prefix instead made
        // `Logger.getLogger("com.example.Foo").getParent()` answer with a
        // `com.example` logger HotSpot never creates (it answers with the root).
        let parent_name = nearest_existing_ancestor_name(ctx.vm_identity(), name);
        let parent = get_or_create_logger(ctx, &parent_name)?;
        let parent_pin = ctx.pin_native_root(parent);
        obj = ctx.read_native_pin(obj_pin, obj);
        let parent = ctx.read_native_pin(parent_pin, parent);
        ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(Some(parent)));
        ctx.unpin_native_roots(parent_pin);
    }
    obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

/// The name of the nearest ancestor of `name` that already has a `Logger`
/// object in the registry, or `""` (the root) when there is none.
///
/// This is JUL's `LogManager.LogNode.getParentLogger` rule. Walking the whole
/// dotted prefix chain matters: `getLogger("a.b.c.d")` with only `a` present
/// must parent to `a`, not to a freshly fabricated `a.b.c`.
fn nearest_existing_ancestor_name(vm: usize, name: &str) -> String {
    let reg = logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut cur = name;
    while let Some(idx) = cur.rfind('.') {
        cur = &cur[..idx];
        if reg.get(cur).copied().unwrap_or(0) != 0 {
            return cur.to_string();
        }
    }
    String::new()
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

/// Build a `Logger$LoggerBundle` that carries `bundle_name` (and, when it can
/// be loaded, the resolved `ResourceBundle`).
///
/// Returns `None` when the real nested class isn't the one loaded — the same
/// signal `real_logger_no_resource_bundle` uses for the compact synthetic
/// `Logger` shape, which has no `loggerBundle` field to stamp.
///
/// A `MissingResourceException` from `ResourceBundle.getBundle` is deliberately
/// swallowed rather than propagated: real JUL throws it out of
/// `Logger.getLogger(name, bundle)`, but this native is also the path CratonVM's
/// own internal logger creation takes, and failing logger creation outright
/// would be a far larger behaviour change than reporting the requested NAME
/// with an unresolved bundle. `getResourceBundleName()` therefore answers
/// truthfully even when `getResourceBundle()` cannot.
fn real_logger_named_bundle(ctx: &mut dyn NativeContext, bundle_name: &str) -> Option<ObjectRef> {
    let bundle_class = "java/util/logging/Logger$LoggerBundle";
    ctx.class_id_by_name(bundle_class)?;
    // Allocate the LoggerBundle FIRST so there is exactly one long-lived
    // reference to keep pinned across the allocating calls that follow.
    let lb = match ctx.new_object(bundle_class) {
        Ok(Some(Value::Object(Some(lb)))) => lb,
        _ => return None,
    };
    let lb_pin = ctx.pin_native_root(lb);
    let name_obj = ctx.create_string(bundle_name);
    let name_pin = ctx.pin_native_root(name_obj);
    let name_arg = ctx.read_native_pin(name_pin, name_obj);
    let user_bundle = match ctx.invoke(
        "java/util/ResourceBundle",
        "getBundle",
        "(Ljava/lang/String;)Ljava/util/ResourceBundle;",
        &[Value::Object(Some(name_arg))],
    ) {
        Ok(Some(Value::Object(Some(bundle)))) => Some(bundle),
        _ => None,
    };
    // `set_field_by_name` neither allocates nor safepoints, so the references
    // re-derived here (and `user_bundle`, returned by the call just above)
    // stay valid for the whole write sequence.
    let lb = ctx.read_native_pin(lb_pin, lb);
    let name_obj = ctx.read_native_pin(name_pin, name_obj);
    ctx.set_field_by_name(lb, "resourceBundleName", Value::Object(Some(name_obj)));
    ctx.set_field_by_name(lb, "userBundle", Value::Object(user_bundle));
    ctx.unpin_native_roots(lb_pin);
    Some(lb)
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
pub(crate) fn get_or_create_logger(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if !is_valid_logger_name(name) {
        tracing::warn!(
            rejected_name = %name,
            "LogManager.getLogger: rejected suspicious logger name, returning anonymous logger"
        );
        return Ok(allocate_logger(ctx, "")?);
    }
    {
        let reg = logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                // SAFETY: singleton-style lifetime.
                return unsafe { Ok(object_from_u64(addr)) };
            }
        }
    }
    let obj = allocate_logger(ctx, name)?;
    // Descendants that were parented to a HIGHER ancestor (or to the root)
    // before this node existed must now point at it -- JUL does the same in
    // `LogNode.walkAndSetParent` when `addLogger` inserts an intermediate node.
    let mut reparent: Vec<u64> = Vec::new();
    {
        let mut reg = logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Check again under the lock (TOCTOU); if another thread beat
        // us, return their logger and drop ours on the floor (it has
        // no external references yet).
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                return unsafe { Ok(object_from_u64(addr)) };
            }
        }
        reg.insert(name.to_string(), obj.as_ptr() as u64);
        let prefix = format!("{name}.");
        for (other, &addr) in reg.iter() {
            if addr == 0 || !other.starts_with(&prefix) {
                continue;
            }
            // Recompute `other`'s nearest existing ancestor against the
            // registry as it now stands; only the ones this insert actually
            // took over get re-linked.
            let mut cur: &str = other.as_str();
            let mut nearest: &str = "";
            while let Some(i) = cur.rfind('.') {
                cur = &cur[..i];
                if reg.get(cur).copied().unwrap_or(0) != 0 {
                    nearest = cur;
                    break;
                }
            }
            if nearest == name {
                reparent.push(addr);
            }
        }
    }
    // `set_field` neither allocates nor safepoints, so `obj` and every address
    // read out of the registry above stay valid across this loop.
    for addr in reparent {
        let child = unsafe { object_from_u64(addr) };
        ctx.set_field(child, LOGGER_FIELD_PARENT, Value::Object(Some(obj)));
    }
    Ok(obj)
}

/// The one place that knows where a `java.util.logging.Logger` keeps its name.
///
/// **THREE layouts reach this VM's JUL surface**, which is why this is a
/// function and not a slot constant:
///
/// * a **real-JDK** `Logger` keeps it in the `name` field. Slot 0 there is
///   `Logger$ConfigurationData`, so an unconditional slot-0 read hands the
///   caller a `ConfigurationData` and the next `getName().lastIndexOf('.')`
///   dies with `NoSuchMethodError`;
/// * the **`logmanager` synthetic** (`allocate_logger`) keeps it at
///   [`LOGGER_FIELD_NAME`] and deliberately does NOT also write it by name,
///   because on a real layout that name resolves to the slot this module uses
///   for the PARENT link;
/// * the **legacy 2/3-field shim synthetic** (`logging_shims`) keeps it at
///   slot 0.
///
/// The type check on the slot reads is what makes the three separable: a real
/// `Logger`'s slot 0 is never a `String`.
///
/// Returns the Java `String` object itself, so `getName()` can hand back the
/// one the Logger already holds instead of minting a copy per call.
///
/// # Why this is shared rather than inlined
///
/// It was inlined, three times, and the copies disagreed. `logging_shims`'s
/// `Logger.getName` read slot 0 unconditionally and -- being the LAST
/// registration for the triple -- won the slot, so on the 13-field
/// `logmanager` Logger that `Logger.getLogger(name)` actually returns,
/// `getName()` answered the raw contents of slot 0. In the synthetic-JDK build
/// that is `Int(0)`: not a String, not null, and not something any Java caller
/// can use. Registration order was deciding which layout the accessor believed
/// in.
pub(crate) fn jul_logger_name_object(
    ctx: &dyn NativeContext,
    logger: ObjectRef,
) -> Option<ObjectRef> {
    // The legacy shim layout first, and only when slot 0 really holds a
    // String -- that is the discriminator against a real `Logger`'s
    // `ConfigurationData`.
    if let Value::Object(Some(name_obj)) = ctx.get_field(logger, 0) {
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(name_obj))
            .as_deref()
            == Some("java/lang/String")
        {
            return Some(name_obj);
        }
    }
    // The real-JDK layout.
    if let Value::Object(Some(name_obj)) = ctx.get_field_by_name(logger, "name") {
        if ctx.read_string(name_obj).is_some() {
            return Some(name_obj);
        }
    }
    // The `logmanager` synthetic layout.
    if let Value::Object(Some(name_obj)) = ctx.get_field(logger, LOGGER_FIELD_NAME) {
        if ctx
            .class_name_arc_of_id(ctx.class_id_of_object(name_obj))
            .as_deref()
            == Some("java/lang/String")
        {
            return Some(name_obj);
        }
    }
    None
}

/// [`jul_logger_name_object`] as a Rust string, empty when the Logger has no
/// name in any of the three layouts.
pub(crate) fn read_jul_logger_name(ctx: &dyn NativeContext, logger: ObjectRef) -> String {
    jul_logger_name_object(ctx, logger)
        .and_then(|name_obj| ctx.read_string(name_obj))
        .unwrap_or_default()
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
fn tomcat_juli_root_logger(
    ctx: &mut dyn NativeContext,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let manager = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER)?;
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(manager))
        .as_deref()
        != Some("org/apache/juli/ClassLoaderLogManager")
    {
        return Ok(None);
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
        Some(Value::Object(Some(root))) => Ok(Some(root)),
        _ => Ok(None),
    }
}

fn get_or_create_tomcat_juli_logger(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if !is_valid_logger_name(name) {
        return Ok(allocate_logger(ctx, "")?);
    }
    // The JUL root is a real logger installed by ClassLoaderLogManager while
    // it reads the current webapp's logging.properties. Returning it directly
    // preserves the configured root level instead of manufacturing a second,
    // unconfigured synthetic root.
    if name.is_empty() {
        if let Ok(Some(root)) = tomcat_juli_root_logger(ctx) {
            return Ok(root);
        }
    }
    let context_loader_key = tomcat_context_loader_key(ctx);
    let key = (context_loader_key, name.to_string());
    if let Some(&address) = tomcat_juli_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        if address != 0 {
            return unsafe { Ok(object_from_u64(address)) };
        }
    }
    let logger = allocate_logger(ctx, name)?;
    // This factory re-enters real JULI bytecode and allocates handler state.
    // Keep its new Logger rooted throughout, refreshing it after every
    // GC-capable boundary before it is stored or returned.
    let logger_pin = ctx.pin_native_root(logger);
    let mut logger = logger;
    // Do not merely cache the child: Tomcat's addLogger bytecode applies the
    // current context-class-loader configuration, wires its parent chain and
    // instantiates any per-logger handlers. Bypassing this path was why the
    // per-webapp FileHandler and root level disappeared.
    let manager = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER)?;
    logger = ctx.read_native_pin(logger_pin, logger);
    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(manager))
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
    if let Ok(Some(root)) = tomcat_juli_root_logger(ctx) {
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
            let root_handlers = tomcat_juli_root_handler_registry(vm)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&context_loader_key)
                .cloned()
                .unwrap_or_default();
            if !root_handlers.is_empty() {
                let list = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
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
    let mut registry = tomcat_juli_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&address) = registry.get(&key) {
        if address != 0 {
            ctx.unpin_native_roots(logger_pin);
            return unsafe { Ok(object_from_u64(address)) };
        }
    }
    registry.insert(key, logger.as_ptr() as u64);
    ctx.unpin_native_roots(logger_pin);
    Ok(logger)
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
    // STATIC: args[0] is the name, not a receiver. Same null rule, and the
    // same message, as `LogManager.getLogger` — both land in the same map.
    // This also covers the two-arg overload, which delegates here: measured,
    // `Logger.getLogger(null, "bundle")` NPEs on the NAME before the bundle is
    // looked at, while `Logger.getLogger("more.a", null)` RETURNS a logger.
    // The bundle half of that pair is the "legal null" and must stay legal.
    if jul_arg_is_null(args, 0) {
        return jul_throw_npe(JUL_NPE_NULL_KEY);
    }
    let name = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if jboss_log_manager_requested(ctx) {
        let logger = get_or_create_jboss_logger(ctx, &name);
        return Ok(Some(Value::Object(Some(logger?))));
    }
    if tomcat_classloader_log_manager_requested(ctx) {
        let logger = get_or_create_tomcat_juli_logger(ctx, &name);
        return Ok(Some(Value::Object(Some(logger?))));
    }
    let logger = get_or_create_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger?))))
}

/// `Logger.getLogger(name, resourceBundleName)`.
///
/// This used to drop `resourceBundleName` on the floor and hand back the same
/// bundle-less Logger as the one-arg overload — which is precisely what made
/// `getResourceBundleName()`/`getResourceBundle()` unimplementable and left
/// them as constant-null stubs. Record the request on the Logger the way the
/// real constructor does, by stamping its `loggerBundle` field, so the two
/// accessors below have something true to report.
fn native_jul_static_get_logger_with_bundle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = native_jul_static_get_logger(ctx, args)?;
    let logger = match result {
        Some(Value::Object(Some(logger))) => logger,
        _ => return Ok(result),
    };
    let bundle_name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    if bundle_name.is_empty() {
        return Ok(result);
    }
    // GC SAFETY: `real_logger_named_bundle` allocates (a String, the
    // LoggerBundle, and possibly a whole ResourceBundle graph), so the Logger
    // can move underneath us. Pin it and re-derive before the field write.
    let logger_pin = ctx.pin_native_root(logger);
    let bundle = real_logger_named_bundle(ctx, &bundle_name);
    let logger = ctx.read_native_pin(logger_pin, logger);
    if let Some(bundle) = bundle {
        // No allocation between `real_logger_named_bundle` returning and this
        // write, so `bundle` cannot have been relocated since.
        ctx.set_field_by_name(logger, "loggerBundle", Value::Object(Some(bundle)));
    }
    ctx.unpin_native_roots(logger_pin);
    Ok(Some(Value::Object(Some(logger))))
}

/// Read one field of a Logger's `loggerBundle` sentinel, null-safely.
///
/// Every step tolerates absence: the compact synthetic `Logger` shape declares
/// no `loggerBundle` at all, and the shared `NO_RESOURCE_BUNDLE` sentinel that
/// `allocate_logger` installs has both of its fields null. That is what keeps
/// the WildFly `SystemExiter.logBeforeExit` path (which is what the old
/// constant-null stubs were written for) free of the
/// `"lb" is null` NPE, while a Logger that genuinely HAS a bundle now reports
/// it instead of always answering null.
fn jul_logger_bundle_field(ctx: &mut dyn NativeContext, args: &[Value], field: &str) -> Value {
    let logger = match args.first() {
        Some(Value::Object(Some(logger))) => *logger,
        _ => return Value::Object(None),
    };
    let bundle = match ctx.get_field_by_name(logger, "loggerBundle") {
        Value::Object(Some(bundle)) => bundle,
        _ => return Value::Object(None),
    };
    match ctx.get_field_by_name(bundle, field) {
        Value::Object(Some(value)) => Value::Object(Some(value)),
        _ => Value::Object(None),
    }
}

fn native_jul_logger_get_resource_bundle_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(jul_logger_bundle_field(
        ctx,
        args,
        "resourceBundleName",
    )))
}

fn native_jul_logger_get_resource_bundle(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(jul_logger_bundle_field(ctx, args, "userBundle")))
}

/// Test-only helper: wipe the singleton + logger registry so tests
/// don't see state bleed between parallel threads.
/// VM identity the crate's own tests run under.
///
/// The side tables below are per-VM (see `per_vm_table`), keyed by
/// `NativeContext::vm_identity`. These tests drive mock contexts, which take
/// the trait's default identity of 0, so that is the scope their assertions
/// and `reset_state_for_tests` must look at.
#[cfg(test)]
const TEST_VM: usize = 0;

#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
    if let Ok(mut g) = singleton_cell(TEST_VM).lock() {
        *g = None;
    }
    if let Ok(mut r) = logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_root_handler_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut h) = logger_handlers(TEST_VM).lock() {
        h.clear();
    }
    // The explicit-level table was NOT reset here, so one test's
    // `setLevel("com.example.App", WARNING)` was still the threshold every
    // LATER test's `com.example.App.*` logger inherited — order-dependent
    // results in the one table whose whole purpose is to be consulted by name.
    if let Ok(mut l) = logger_explicit_levels(TEST_VM).lock() {
        l.clear();
    }
    // Both level tables and the resolved-initializer cache, for the same
    // reason: they are process-wide and name-keyed, so anything left in them
    // is the next test's inherited threshold or the next test's provider.
    if let Ok(mut l) = logger_minimum_levels(TEST_VM).lock() {
        l.clear();
    }
    if let Ok(mut c) = jboss_initializer_cell(TEST_VM).lock() {
        *c = None;
    }
    if let Ok(mut l) = config_listeners(TEST_VM).lock() {
        l.clear();
    }
    if let Ok(mut m) = log_record_messages().lock() {
        m.clear();
    }
    if let Ok(mut g) = jboss_log_context_singleton(TEST_VM).lock() {
        *g = None;
    }
    if let Ok(mut r) = jboss_logger_registry(TEST_VM).lock() {
        r.clear();
    }
    if let Ok(mut m) = attachments(TEST_VM).lock() {
        m.clear();
    }
    if let Ok(mut d) = primordial_config_done(TEST_VM).lock() {
        *d = false;
    }
}

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

fn native_get_log_manager(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = ensure_singleton(ctx, CLS_JUL_LOG_MANAGER)?;
    // Real `LogManager.ensureLogManagerInitialized()` finishes by adding TWO
    // loggers to the root context: the root logger `""` and `Logger.global`.
    // Ours demand-created the root on first use and never registered `global`
    // at all, so `getLoggerNames()` came back one short of HotSpot —
    // `[, namesprobe.one]` against `[, global, namesprobe.one]` (measured).
    //
    // `Logger.getGlobal()` still ANSWERED, because our `getLogger` demand-
    // creates any name; what was missing is the REGISTRATION, and only an
    // enumeration of the registry can see the difference. That is the whole
    // reason regression-suite RJdkLogging prints `loggerNames=` instead of
    // only asserting `contains(...)`: a registry that fabricates on demand
    // satisfies every membership test and still has the wrong contents.
    let _ = get_or_create_logger(ctx, "");
    let _ = get_or_create_logger(ctx, "global");
    // ...and then reads the configuration. `readPrimordialConfiguration` is
    // the step that installs the `ConsoleHandler` named by
    // `$java.home/conf/logging.properties`; without it `root.handlers` was 0
    // against HotSpot's 1, and every `Logger.info(..)` on a default-configured
    // VM was dropped on the floor with no handler to publish it.
    //
    // Latch FIRST. `read_configuration_no_arg_impl` instantiates the handler
    // classes named by the file, and a handler constructor is free to call
    // `LogManager.getLogManager()` — which is this function.
    let first_time = {
        let mut done = primordial_config_done(ctx.vm_identity())
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = !*done;
        *done = true;
        first
    };
    if first_time {
        // A missing or unreadable file is not fatal in the JDK either, and
        // `read_configuration_no_arg_impl` already returns `Ok(None)` for it.
        // Swallow a hard error too rather than failing `getLogManager()`: the
        // JDK's own primordial read is wrapped so that a broken config file
        // leaves you with a usable (if unconfigured) LogManager.
        let _ = read_configuration_no_arg_impl(ctx, &[]);
    }
    Ok(Some(Value::Object(Some(obj))))
}

fn native_get_jboss_log_manager(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Same singleton — the JBoss subclass in JUL resolves via
    // Class.forName(property) + newInstance(), and the JDK code at
    // `LogManager.getLogManager()` ultimately returns the manager
    // singleton regardless of the concrete class. Using the same slot
    // keeps pointer-identity stable.
    let obj = ensure_singleton(ctx, CLS_JBOSS_LOG_MANAGER);
    Ok(Some(Value::Object(Some(obj?))))
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

/// `LogManager.getLogger(String)` when the active singleton is
/// `org/jboss/logmanager/LogManager` (`java.util.logging.manager` was set to
/// it — see [`jboss_log_manager_requested`]).
///
/// `getLogger` is inherited unchanged from `java.util.logging.LogManager`,
/// so before this fix `CLS_JBOSS_LOG_MANAGER`'s `getLogger` was registered
/// to the same [`native_get_logger`] as the plain-JUL manager, which always
/// allocates a `CLS_JUL_LOGGER`-shaped object. Callers that cast the result
/// to `org.jboss.logmanager.Logger` (as JBoss LogManager's own API contract
/// promises once its LogManager subclass is active — see
/// `AbstractQuarkusExtensionTest`'s `(Logger) LogManager.getLogManager()
/// .getLogger("")`) got a real `ClassCastException` even though the
/// singleton swap itself (`native_get_jboss_log_manager`) was correct.
///
/// [`get_or_create_jboss_logger`] already exists and is correct — it backs
/// `LogContext.getLogger`, JBoss's own internal entry point — so this just
/// wires the same allocation to the path real application/test code
/// actually calls. Mirrors [`native_get_logger`]'s null-name NPE contract.
fn native_get_jboss_manager_logger(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `getLogger(String)` — receiver in args[0], name in args[1].
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_KEY);
    }
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let logger = get_or_create_jboss_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger?))))
}

fn native_get_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `getLogger(String)` — receiver in args[0], name in args[1].
    //
    // A null name is NOT the empty name. HotSpot reaches
    // `ConcurrentHashMap.get(name)` and NPEs there; this body coerced null to
    // `""` and handed back the ROOT logger, so `LogManager.getLogger(null)`
    // returned a live object where the JDK throws.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_KEY);
    }
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let logger = get_or_create_logger(ctx, &name);
    Ok(Some(Value::Object(Some(logger?))))
}

fn native_add_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    // `addLogger(Logger)` — args[0]=this, args[1]=Logger.
    let Some(Value::Object(Some(logger))) = args.get(1).cloned() else {
        // The old comment here said "contract says NullPointerException, but
        // we prefer to swallow + return false so the caller's bootstrap path
        // keeps going". Measured, that preference protects nobody: HotSpot
        // throws, so any bootstrap that reached this line was already dead on
        // a real JDK and the swallow only moved the failure somewhere less
        // legible. Returning `false` for a DUPLICATE logger is a different
        // rule and is still correct — that arm is below, and it is measured
        // too (`addLogger(realLogger)` twice answers `false,false` on both
        // VMs, because the name is already registered by `getLogger`).
        return jul_throw_npe(JUL_NPE_NULL_LOGGER);
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
    if logger_registry(vm)
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
    let mut reg = logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if reg.contains_key(&name) {
        ctx.unpin_native_roots(logger_pin);
        return Ok(Some(Value::Int(0)));
    }
    reg.insert(name, logger.as_ptr() as u64);
    ctx.unpin_native_roots(logger_pin);
    Ok(Some(Value::Int(1)))
}

/// `LogManager.readConfiguration()` — the no-arg, startup-configuration
/// overload. Follows the documented JDK resolution order:
///
///   1. `java.util.logging.config.class` — instantiate it; that class's
///      constructor is responsible for calling `readConfiguration(InputStream)`
///      itself (that is the documented contract). If it cannot be constructed,
///      fall through, exactly as the JDK does.
///   2. `java.util.logging.config.file` — read and apply that file.
///   3. `$java.home/conf/logging.properties` — the JDK's own default.
///
/// This used to be a hard no-op justified as "we never parse untrusted logging
/// configuration". That reasoning does not survive contact with the actual
/// threat model: every path here is named by the application itself (a `-D`
/// system property, or the JDK's own install file), which is no more
/// attacker-controlled than the classpath we already load code from. The cost
/// of the no-op was that `LogManager.getLogManager().readConfiguration()` —
/// the documented way to (re)load logging configuration, and what a plain
/// `java.util.logging` program relies on — silently did nothing: no handlers,
/// no levels.
///
/// Note this is only reachable when a caller explicitly asks for it, or via a
/// manager that does not override it (Tomcat's JULI and JBoss LogManager both
/// override `readConfiguration` in their own bytecode and never reach here).
fn native_read_configuration_no_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = read_configuration_no_arg_impl(ctx, args);
    // The JDK notifies configuration listeners after EVERY readConfiguration
    // call, including the ones that found nothing to read (no config.file, an
    // unreadable file) — the contract is "the configuration was re-read", not
    // "the configuration changed". Fire outside the impl so its early returns
    // cannot skip the notification.
    fire_configuration_listeners(ctx);
    result
}

fn read_configuration_no_arg_impl(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // NOTE on manager subclasses: a `LogManager` subclass that overrides
    // `readConfiguration` (Tomcat's `ClassLoaderLogManager`, JBoss's) runs its
    // OWN bytecode — verified: JULI's `ClassLoaderLogManager.readConfiguration`
    // frame appears in stack traces from a Tomcat run, and Tomcat's handler
    // chain and FINE output are byte-for-byte identical with and without this
    // implementation. So this native does not shadow such an override and must
    // NOT try to detect one: a subclass that deliberately calls
    // `super.readConfiguration()` to pick up the standard configuration is a
    // legitimate pattern, and short-circuiting it would silently break it.

    // 1. config.class — the JDK instantiates it and expects its ctor to call
    //    readConfiguration(InputStream); we honour the same contract.
    if let Some(cls) = ctx
        .get_system_property("java.util.logging.config.class")
        .filter(|c| !c.trim().is_empty())
    {
        let internal = cls.trim().replace('.', "/");
        if let Ok(Some(_)) = ctx.new_object_initialized(&internal, "()V", &[]) {
            return Ok(None);
        }
        // Unconstructible: the JDK logs and falls back to the file path.
        tracing::warn!(
            config_class = %cls.trim(),
            "LogManager.readConfiguration: java.util.logging.config.class could not be \
             instantiated; falling back to java.util.logging.config.file"
        );
    }

    // 2. config.file, else 3. the JDK's own $java.home/conf/logging.properties.
    //
    // Step 3 is the JDK LogManager's fallback, and ONLY the JDK's. When
    // `java.util.logging.manager` names `org.jboss.logmanager.LogManager` the
    // primordial read runs that subclass's OVERRIDE, whose `doConfigure`
    // resolves a `ConfiguratorFactory` through `ServiceLoader` and never looks
    // at `$java.home/conf/logging.properties` at all. Taking the JDK fallback
    // there imported two settings HotSpot does not have: `.level=INFO` on the
    // root — which then became the inherited threshold for EVERY logger, so an
    // unconfigured JBoss logger refused FINE/TRACE where HotSpot allows it
    // (measured: `getEffectiveLevel()` -2147483648 vs 800, `isLoggable(TRACE)`
    // true vs false) — and a `java.util.logging.ConsoleHandler` on the root.
    // That INFO floor is exactly what silences Quarkus's
    // `traceCategories(...)`/`overrideLoggerLevel` support.
    //
    // An EXPLICITLY designated config file is still honoured: naming one is a
    // deliberate act by the application, and step 3 is the only implicit one.
    let jboss_manager = jboss_log_manager_requested(ctx);
    let path = ctx
        .get_system_property("java.util.logging.config.file")
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().to_string())
        .or_else(|| {
            if jboss_manager {
                return None;
            }
            ctx.get_system_property("java.home")
                .filter(|h| !h.trim().is_empty())
                .map(|h| {
                    format!(
                        "{}/conf/logging.properties",
                        h.trim_end_matches(['/', '\\'])
                    )
                })
        });
    let Some(path) = path else {
        return Ok(None);
    };
    let Ok(bytes) = std::fs::read(&path) else {
        // A missing/unreadable file is not fatal in the JDK either.
        return Ok(None);
    };
    let entries = crate::properties_sidetable::parse_properties_pub(&bytes);
    apply_jul_config_entries(ctx, &entries)
}

/// `LogManager.updateConfiguration(InputStream, Function)`.
///
/// This was a no-op, so a caller handing the manager a fully-formed
/// configuration stream — the very thing `readConfiguration(InputStream)`
/// already accepts and applies — silently got no level, handler or formatter
/// change at all. Route it through the same parser: `args[1]` is the stream in
/// both descriptors.
///
/// LIMITATION: the `Function<String, BiFunction<String,String,String>>` mapper
/// (`args[2]`) is not applied. `apply_jul_config_entries` implements the
/// null-mapper contract exactly — the new configuration replaces the old
/// wholesale — which is both the documented default and the only behaviour a
/// caller can get today. A non-null mapper is therefore honoured as if it were
/// null rather than being silently dropped along with the whole update.
fn native_update_configuration_with_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_read_configuration_with_stream(ctx, args)
}

fn native_read_configuration_with_stream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let result = read_configuration_with_stream_impl(ctx, args);
    // Same contract as the no-arg overload — see `native_read_configuration_no_arg`.
    fire_configuration_listeners(ctx);
    result
}

fn read_configuration_with_stream_impl(
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

/// `LogManager.addConfigurationListener(Runnable)` — record the listener and
/// return `this` (the JDK returns the manager to allow chaining).
///
/// Real JDK semantics honoured here: a `null` listener throws NPE; a listener
/// already registered is a no-op (identity comparison, exactly like the JDK's
/// `IdentityHashMap`-backed set).
fn native_add_configuration_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let this = args.first().cloned().unwrap_or(Value::Object(None));
    let Some(Value::Object(Some(listener))) = args.get(1).copied() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("LogManager.addConfigurationListener: listener is null".to_string()),
        }
        .into());
    };
    let addr = listener.as_ptr() as u64;
    let mut listeners = config_listeners(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !listeners.contains(&addr) {
        listeners.push(addr);
    }
    Ok(Some(this))
}

/// `LogManager.removeConfigurationListener(Runnable)` — drop the listener.
///
/// Real JDK semantics: removing a listener that was never added is a no-op
/// (not an error), and a `null` argument throws NPE.
fn native_remove_configuration_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let Some(Value::Object(Some(listener))) = args.get(1).copied() else {
        return Err(RuntimeError::NullPointerException {
            message: Some("LogManager.removeConfigurationListener: listener is null".to_string()),
        }
        .into());
    };
    let addr = listener.as_ptr() as u64;
    let mut listeners = config_listeners(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    listeners.retain(|&a| a != addr);
    Ok(None)
}

/// Invoke every registered configuration listener, in registration order.
///
/// The JDK swallows whatever a listener throws (it reports it to the logging
/// ErrorManager and carries on) so that one bad listener cannot abort the
/// configuration read for the others — mirrored here by discarding the result
/// of each `run()`.
fn fire_configuration_listeners(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    // Re-entrancy guard: a listener whose `run()` itself calls
    // `readConfiguration`/`updateConfiguration` would otherwise recurse until
    // the stack blows. One notification per outermost read is the observable
    // contract either way.
    if CONFIG_LISTENERS_FIRING.with(|f| f.get()) {
        return;
    }
    // Snapshot under the lock and release it before any Java call: a listener
    // is free to call add/removeConfigurationListener, which would re-enter.
    let listeners: Vec<u64> = {
        let guard = config_listeners(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.is_empty() {
            return;
        }
        guard.clone()
    };
    CONFIG_LISTENERS_FIRING.with(|f| f.set(true));
    for addr in listeners {
        // SAFETY / GC: the addresses come from `as_ptr()` on live ObjectRefs
        // held by this module's side-table, which `gc_scan_logmanager_roots`
        // reports as roots and `gc_update_logmanager_refs` repoints after a
        // move — the same convention `publish_to_jul_handlers` uses for
        // `logger_handlers`. Pin each one before the GC-capable `run()` call.
        let listener = unsafe { object_from_u64(addr) };
        let pin = ctx.pin_native_root(listener);
        let listener = ctx.read_native_pin(pin, listener);
        let _ = ctx.invoke_virtual(listener, "run", "()V", &[]);
        ctx.unpin_native_roots(pin);
    }
    CONFIG_LISTENERS_FIRING.with(|f| f.set(false));
}

thread_local! {
    /// Set while `fire_configuration_listeners` is walking the chain on this
    /// thread. Nothing between the set and the clear can return early — the
    /// only Java call in the loop has its result discarded — so a `Drop` guard
    /// would buy nothing here.
    static CONFIG_LISTENERS_FIRING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
/// Reached from BOTH `readConfiguration` overloads: the `InputStream` one
/// (a stream the caller's own Java code produced) and, since the no-arg
/// overload was implemented, the startup file named by
/// `java.util.logging.config.file` / `$java.home/conf/logging.properties`.
/// Both are application-designated inputs — see
/// `native_read_configuration_no_arg` for why the earlier
/// "never touch a filesystem config" stance was dropped.
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
    logger_explicit_levels(ctx.vm_identity())
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

    let root = get_or_create_logger(ctx, "")?;
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
                logger_explicit_levels(ctx.vm_identity())
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

/// `LogManager.reset()`.
///
/// # It must NOT drop the loggers
///
/// This used to `clear()` the logger registry, on the reading that "reset"
/// means "forget everything". `java.util.logging.LogManager.reset()` says
/// otherwise, and so does the JDK, measured on HotSpot 25:
///
/// ```text
///   before: names=[, a.b.c, bar, baz, d.e.f, foo, global]
///           a.level=FINEST  a.handlers=1  root.level=INFO  root.handlers=1
///   after : names=[, a.b.c, bar, baz, d.e.f, foo, global]   <-- UNCHANGED
///           a.level=null    a.handlers=0  root.level=INFO   root.handlers=0
///           Logger.getLogger("reset.probe.a") == the pre-reset object -> true
/// ```
///
/// So `reset()` resets CONFIGURATION — handlers off, levels back to
/// "inherit", root pinned at INFO — and the registry itself is untouched.
///
/// Dropping the registry was not merely an over-broad `getLoggerNames()`. Our
/// `getLogger` demand-creates any name, so after a `reset()` the next
/// `Logger.getLogger("x")` minted a DIFFERENT object from the one the
/// application was still holding. Two live `Logger`s for one name is a silent
/// split: `setLevel`/`addHandler` on either is invisible to the other, and the
/// JDK guarantees the identity that makes them the same object. Nothing failed
/// loudly, which is why a unit test asserting the registry came back EMPTY sat
/// green over it.
fn native_reset(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    // Resolve INFO BEFORE snapshotting: `resolve_standard_level` can run
    // `Level.<clinit>` and therefore allocate, and the snapshot below holds
    // raw logger addresses that a collection could invalidate. Nothing in the
    // loop allocates, so one resolve up front is also the only one needed.
    let info_level = resolve_standard_level(ctx, "INFO");
    let loggers: Vec<(String, ObjectRef)> = {
        let reg = logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .filter(|(_, &addr)| addr != 0)
            // SAFETY: same singleton-style lifetime `get_or_create_logger`
            // relies on when it hands these addresses back out.
            .map(|(name, &addr)| (name.clone(), unsafe { object_from_u64(addr) }))
            .collect()
    };
    for (name, logger) in loggers {
        // "removes ... all Handlers" — the handler list lives in an
        // identity-keyed side table, not a slot, so dropping the row is the
        // removal. NOT ALSO CLOSED: the JDK closes them here, and doing that
        // means invoking Java `Handler.close()` per handler from inside the
        // manager, which allocates and re-enters logging. Consequence, stated
        // rather than hidden: a buffered `StreamHandler` is not flushed by
        // `reset()` alone. Callers that need the flush call `Handler.close()`
        // or `flush()` themselves, which is what the JULI and jboss paths in
        // this module already do.
        crate::logging_shims::jul_logger_handlers_clear(ctx, logger);
        // "(except for the root logger) sets the level to null. The root
        // logger's level is set to Level.INFO."
        let level = if name.is_empty() { info_level } else { None };
        ctx.set_field(logger, LOGGER_FIELD_LEVEL, Value::Object(level));
    }
    // The Tomcat JULI mirrors are per-classloader caches of the above rather
    // than a separate namespace, so they are rebuilt on next use and clearing
    // them costs no identity.
    if let Ok(mut r) = tomcat_juli_logger_registry(vm).lock() {
        r.clear();
    }
    if let Ok(mut r) = tomcat_juli_root_handler_registry(vm).lock() {
        r.clear();
    }
    Ok(None)
}

fn native_get_logger_names(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    // Snapshot the names and pack into a synthetic Enumeration<String>.
    //
    // Layout (slot 0 = Object[] backing array, slot 1 = cursor int).
    // Our enumeration natives (`hasMoreElements`, `nextElement`) are
    // registered against `CLS_LOGGER_ENUMERATION` so the standard JDK
    // Enumeration API works on this shape.
    // BOTH registries. This read only ever saw `logger_registry`, so under
    // `java.util.logging.manager=org.jboss.logmanager.LogManager` — where every
    // factory call mints into `jboss_logger_registry` instead — the JBoss
    // `LogManager.getLoggerNames()` enumerated the loggers nobody had created
    // and omitted every one that existed. A name can be in both; dedupe.
    let names: Vec<String> = {
        let mut seen: Vec<String> = logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for name in jboss_logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
        {
            if !seen.iter().any(|existing| existing == name) {
                seen.push(name.clone());
            }
        }
        seen
    };

    let arr = ctx.new_array(ArrayElementType::Reference, names.len());
    for (i, name) in names.iter().enumerate() {
        let s = ctx.create_string(name);
        ctx.set_array_element(arr, i, Value::Object(Some(s)));
    }

    let enumeration = try_alloc_concurrent_synthetic(ctx, CLS_LOGGER_ENUMERATION, 2)?;
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
fn attachments(vm: usize) -> &'static Mutex<HashMap<AttachKey, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<AttachKey, u64>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

fn obj_addr(v: &Value) -> u64 {
    if let Value::Object(Some(o)) = v {
        o.as_ptr() as u64
    } else {
        0
    }
}

fn native_jboss_logger_get_attachment(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let map = attachments(vm).lock().unwrap_or_else(|e| e.into_inner());
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

fn native_jboss_logger_attach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    let value = args.get(2).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments(vm).lock().unwrap_or_else(|e| e.into_inner());
    let prev = map.insert((this, key), value);
    Ok(Some(match prev {
        Some(addr) if addr != 0 => Value::Object(Some(unsafe { object_from_u64(addr) })),
        _ => Value::Object(None),
    }))
}

fn native_jboss_logger_attach_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let this = args.first().map(obj_addr).unwrap_or(0);
    let key = args.get(1).map(obj_addr).unwrap_or(0);
    let value = args.get(2).map(obj_addr).unwrap_or(0);
    if this == 0 || key == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut map = attachments(vm).lock().unwrap_or_else(|e| e.into_inner());
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
fn jboss_log_context_singleton(vm: usize) -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<Option<u64>>>>> = OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

fn ensure_jboss_log_context(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    {
        let g = jboss_log_context_singleton(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(addr) = *g {
            if addr != 0 {
                return unsafe { Ok(object_from_u64(addr)) };
            }
        }
    }
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/LogContext", 1)?;
    // Real LogContext.addCloseHandler synchronizes on treeLock. Most LogContext
    // methods are native-overridden below, but initializing the monitor keeps
    // any remaining real bytecode null-safe.
    let tree_lock = try_alloc_concurrent_synthetic(ctx, "java/lang/Object", 0)?;
    ctx.set_field_by_name(obj, "treeLock", Value::Object(Some(tree_lock)));
    if ctx
        .resolve_field_index("org/jboss/logmanager/LogContext", "treeLock")
        .is_none()
    {
        ctx.set_field(obj, 0, Value::Object(Some(tree_lock)));
    }
    let mut g = jboss_log_context_singleton(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(addr) = *g {
        if addr != 0 {
            return unsafe { Ok(object_from_u64(addr)) };
        }
    }
    *g = Some(obj.as_ptr() as u64);
    Ok(obj)
}

fn native_jboss_logger_get_log_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ensure_jboss_log_context(ctx)?))))
}

fn native_jboss_log_context_get_log_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ensure_jboss_log_context(ctx)?))))
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
/// instances keyed by name. Distinct from `logger_registry(vm)` (which
/// holds `java/util/logging/Logger` mirrors) so the JBoss-side overrides
/// for `getAttachment` etc. dispatch on receivers whose concrete class
/// is `org/jboss/logmanager/Logger`.
fn jboss_logger_registry(vm: usize) -> &'static Mutex<HashMap<String, u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<HashMap<String, u64>>>>> =
        OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

/// Every logger object ALREADY registered under `name`, JBoss shape first.
///
/// One logger name can name two objects. `logger_registry` holds the
/// `java/util/logging/Logger` mirrors; `jboss_logger_registry` holds the
/// `org/jboss/logmanager/Logger` ones, and which of the two a factory call
/// mints depends on whether `java.util.logging.manager` was set to
/// `org.jboss.logmanager.LogManager` at the moment it ran (see
/// [`jboss_log_manager_requested`]) — a property Quarkus's
/// `AbstractQuarkusExtensionTest` sets from its own `<clinit>`, i.e. part-way
/// through a run.
///
/// That matters because the per-logger handler side table is keyed by the
/// OBJECT's identity hash, so an ancestor walk that demand-creates the
/// JUL-shaped logger for a name whose handlers were installed on the
/// JBoss-shaped one finds an empty list and silently drops the record. This is
/// the lookup those walks must use instead: it CREATES NOTHING (a name nobody
/// has asked for cannot have handlers) and reports both shapes so the caller
/// can take whichever one actually carries the state it wants.
/// Returns a fixed-size array rather than a `Vec` because this sits inside the
/// per-publish ancestor walk, once per dotted-name level.
fn existing_loggers_for_name(ctx: &dyn NativeContext, name: &str) -> [Option<ObjectRef>; 2] {
    let vm = ctx.vm_identity();
    let mut out = [None, None];
    for (slot, table) in out
        .iter_mut()
        .zip([jboss_logger_registry(vm), logger_registry(vm)])
    {
        let reg = table.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                // SAFETY: singleton-style lifetime, the same contract as every
                // other read out of these two registries.
                *slot = Some(unsafe { object_from_u64(addr) });
            }
        }
    }
    out
}

/// The dotted-name parent of `name`, or `None` for the root.
/// `"a.b.c"` -> `"a.b"`, `"a"` -> `""`, `""` -> `None`.
fn logger_name_parent(name: &str) -> Option<&str> {
    if name.is_empty() {
        return None;
    }
    Some(match name.rfind('.') {
        Some(idx) => &name[..idx],
        None => "",
    })
}

/// Pins `logger` across [`attach_minimal_jboss_logger_node_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `logger` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn attach_minimal_jboss_logger_node(
    ctx: &mut dyn NativeContext,
    logger: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*logger);
    let w5_out = attach_minimal_jboss_logger_node_body(ctx, *logger);
    *logger = ctx.read_native_pin(w5_pin, *logger);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

fn attach_minimal_jboss_logger_node_body(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Result<(), MethodCallFailed> {
    // Some JIT/real-bytecode paths still execute JBoss Logger methods directly
    // before the native override gate can short-circuit them. Those methods all
    // start by dereferencing `this.loggerNode`. We do not model the full
    // LoggerNode graph, but a tiny node with INFO effective level is enough for
    // getEffectiveLevel/isLoggable-style reads to be null-safe and conservative.
    let node = try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/LoggerNode", 16)?;
    ctx.set_field_by_name(node, "effectiveLevel", Value::Int(800));
    ctx.set_field_by_name(node, "effectiveMinLevel", Value::Int(i32::MIN));
    ctx.set_field_by_name(node, "useParentHandlers", Value::Int(1));
    ctx.set_field_by_name(node, "useParentFilter", Value::Int(1));
    ctx.set_field_by_name(logger, "loggerNode", Value::Object(Some(node)));
    Ok(())
}

fn get_or_create_jboss_logger(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if !is_valid_logger_name(name) {
        let mut obj =
            try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS)?;
        let name_obj = ctx.create_string("");
        ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
        attach_minimal_jboss_logger_node(ctx, &mut obj)?;
        return Ok(obj);
    }
    {
        let reg = jboss_logger_registry(vm)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(&addr) = reg.get(name) {
            if addr != 0 {
                return unsafe { Ok(object_from_u64(addr)) };
            }
        }
    }
    let mut obj =
        try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Logger", LOGGER_NUM_FIELDS)?;
    let name_obj = ctx.create_string(name);
    ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(obj, LOGGER_FIELD_LEVEL, Value::Object(None));
    ctx.set_field(obj, LOGGER_FIELD_PARENT, Value::Object(None));
    attach_minimal_jboss_logger_node(ctx, &mut obj)?;
    let mut reg = jboss_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(&addr) = reg.get(name) {
        if addr != 0 {
            return unsafe { Ok(object_from_u64(addr)) };
        }
    }
    reg.insert(name.to_string(), obj.as_ptr() as u64);
    // Drop the registry lock BEFORE the SPI call below: it runs application
    // bytecode that is free to demand another logger, and that call comes
    // straight back here.
    drop(reg);
    // The node is in the registry first and initialised second, on purpose —
    // a re-entrant `getLogger(name)` from inside the initializer then finds
    // this object instead of building a second one and recursing.
    let obj = apply_log_context_initializer(ctx, obj, name)?;
    Ok(obj)
}

// ---------------------------------------------------------------------------
// org.jboss.logmanager.LogContextInitializer (the per-node SPI)
// ---------------------------------------------------------------------------

const CLS_JBOSS_LOG_CONTEXT_INITIALIZER: &str = "org/jboss/logmanager/LogContextInitializer";

/// Kill switch for the `LogContextInitializer` consultation below.
/// `CRATONVM_JBOSS_LOG_CONTEXT_INITIALIZER=0` restores the previous behaviour
/// (no provider is ever asked, every node is born bare). Default ON.
///
/// It exists because this is the one place in the logging natives that runs
/// APPLICATION bytecode from inside a logger allocator — a provider's
/// `<clinit>` constructs handlers and is free to log — and every
/// jboss-logmanager consumer in the corpus (WildFly, Keycloak, Quarkus) reaches
/// it. A kill switch makes "is this the initializer?" a same-binary question.
fn jboss_log_context_initializer_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var_os("CRATONVM_JBOSS_LOG_CONTEXT_INITIALIZER")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("0")
        )
    })
}

/// Per-VM cache of the resolved provider: absent = not yet looked for,
/// `Some(0)` = looked for and there is none, `Some(addr)` = the provider.
///
/// Real `LogContext.discoverDefaultInitializer0` resolves once and stores the
/// result in a static; the negative answer has to be cached too, or every
/// logger creation in a process with no provider pays a full `ServiceLoader`
/// scan of the classpath.
fn jboss_initializer_cell(vm: usize) -> &'static Mutex<Option<u64>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, &'static Mutex<Option<u64>>>>> = OnceLock::new();
    per_vm_table(&INSTANCE, vm)
}

thread_local! {
    /// Set while this thread is resolving or calling the initializer.
    ///
    /// A provider's static initialiser builds handlers (Quarkus's
    /// `InitialConfigurator.<clinit>` constructs a `QuarkusDelayedHandler`),
    /// and anything on that path may log — which demands a logger, which
    /// re-enters `get_or_create_jboss_logger`. The re-entrant node is created
    /// and registered normally; it just does not itself consult the SPI, which
    /// is what makes the recursion finite.
    static JBOSS_INITIALIZER_BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Resolve the single `LogContextInitializer` the way
/// `LogContext.discoverDefaultInitializer0` does: `ServiceLoader.load` over
/// the interface's own class loader, first provider wins, and `DEFAULT` (which
/// answers null / null / `NO_HANDLERS` — i.e. nothing) when there is none.
///
/// Returns `None` for "no provider", including every failure: a missing
/// jboss-logmanager, an unloadable provider class, a `ServiceLoader` that
/// throws. Real jboss-logmanager also swallows provider failures here
/// (`discoverDefaultInitializer` catches and falls back to `DEFAULT`), and a
/// VM that refused to hand out loggers because a logging SPI misbehaved would
/// be worse than one that logs a little less.
fn resolve_jboss_log_context_initializer(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if !jboss_log_context_initializer_enabled() {
        return None;
    }
    let vm = ctx.vm_identity();
    if let Some(addr) = *jboss_initializer_cell(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        return if addr == 0 {
            None
        } else {
            // SAFETY: the provider is held by a global root (added below), so
            // the address stays valid and current for the life of the VM.
            Some(unsafe { object_from_u64(addr) })
        };
    }
    let resolved = discover_jboss_log_context_initializer(ctx);
    let addr = resolved.map(|o| o.as_ptr() as u64).unwrap_or(0);
    jboss_initializer_cell(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(addr);
    resolved
}

fn discover_jboss_log_context_initializer(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    // No jboss-logmanager on the classpath: there is no SPI to consult, and
    // this is the gate that keeps every non-JBoss program out of the code
    // below. Note it is deliberately NOT gated on
    // `java.util.logging.manager` — real `LogContext` consults the
    // initializer whenever it builds a node, whether or not the JBoss
    // LogManager was installed as the JUL manager.
    let cid = ctx
        .ensure_class_initialized(CLS_JBOSS_LOG_CONTEXT_INITIALIZER)
        .ok()?;
    let mirror = ctx.get_class_mirror(cid);
    let mirror_pin = ctx.pin_native_root(mirror);
    let found = (|| -> Option<ObjectRef> {
        let loader = match ctx
            .invoke_virtual(mirror, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])
            .ok()?
        {
            Some(v @ Value::Object(_)) => v,
            _ => Value::Object(None),
        };
        let mirror = ctx.read_native_pin(mirror_pin, mirror);
        let loader_arg = loader;
        let service_loader = match ctx
            .invoke(
                "java/util/ServiceLoader",
                "load",
                "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
                &[Value::Object(Some(mirror)), loader_arg],
            )
            .ok()?
        {
            Some(Value::Object(Some(sl))) => sl,
            _ => return None,
        };
        let sl_pin = ctx.pin_native_root(service_loader);
        let first = (|| -> Option<ObjectRef> {
            let iterator = match ctx
                .invoke_virtual(service_loader, "iterator", "()Ljava/util/Iterator;", &[])
                .ok()?
            {
                Some(Value::Object(Some(it))) => it,
                _ => return None,
            };
            let it_pin = ctx.pin_native_root(iterator);
            let out = (|| -> Option<ObjectRef> {
                if !matches!(
                    ctx.invoke_virtual(iterator, "hasNext", "()Z", &[]).ok()?,
                    Some(Value::Int(1))
                ) {
                    return None;
                }
                let iterator = ctx.read_native_pin(it_pin, iterator);
                match ctx
                    .invoke_virtual(iterator, "next", "()Ljava/lang/Object;", &[])
                    .ok()?
                {
                    Some(Value::Object(Some(provider))) => Some(provider),
                    _ => None,
                }
            })();
            ctx.unpin_native_roots(it_pin);
            out
        })();
        ctx.unpin_native_roots(sl_pin);
        first
    })();
    ctx.unpin_native_roots(mirror_pin);
    let provider = found?;
    // Hold it: the cache stores a raw address, and this is the object every
    // later node creation calls back into.
    ctx.add_global_root(provider);
    tracing::debug!(
        provider = %ctx
            .class_name_arc_of_id(ctx.class_id_of_object(provider))
            .as_deref()
            .unwrap_or("<unknown>"),
        "resolved org.jboss.logmanager.LogContextInitializer provider"
    );
    Some(provider)
}

/// Apply the initializer to a freshly created node, mirroring
/// `LoggerNode.<init>`:
///
/// ```text
/// effectiveMinLevel = requireNonNullElse(initializer.getMinimumLevel(name), Level.ALL).intValue()
/// level             = initializer.getInitialLevel(name)      // null => inherit
/// handlers          = safeCloneHandlers(initializer.getInitialHandlers(name))
/// ```
///
/// This is the ONLY thing that puts a handler on the root logger of a Quarkus
/// process: `io.quarkus.bootstrap.logging.InitialConfigurator` answers
/// `[QuarkusDelayedHandler]` and `Level.ALL` for the empty name, and nothing
/// for every other name. Measured, HotSpot + jboss-logmanager 3.2.2:
/// `root.handlers.initialCount=1` with the provider on the classpath, `0`
/// (from this SPI — the default `ConsoleHandler` there comes from the separate
/// `ConfiguratorFactory` chain) without it.
///
/// Returns the (possibly relocated) logger.
fn apply_log_context_initializer(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    if JBOSS_INITIALIZER_BUSY.with(|b| b.get()) {
        return Ok(logger);
    }
    let logger_pin = ctx.pin_native_root(logger);
    JBOSS_INITIALIZER_BUSY.with(|b| b.set(true));
    let result = apply_log_context_initializer_body(ctx, logger, name);
    JBOSS_INITIALIZER_BUSY.with(|b| b.set(false));
    let logger = ctx.read_native_pin(logger_pin, logger);
    ctx.unpin_native_roots(logger_pin);
    result?;
    Ok(logger)
}

fn apply_log_context_initializer_body(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
    name: &str,
) -> Result<(), MethodCallFailed> {
    let Some(initializer) = resolve_jboss_log_context_initializer(ctx) else {
        return Ok(());
    };
    let init_pin = ctx.pin_native_root(initializer);
    let logger_pin = ctx.pin_native_root(logger);
    let vm = ctx.vm_identity();
    let result = (|| -> Result<(), MethodCallFailed> {
        // 1. `getMinimumLevel(name)`, defaulting to `Level.ALL`. This is the
        //    floor `LoggerNode.isLoggableLevel` checks ALONGSIDE the effective
        //    level (`level >= effectiveMinLevel && level >= effectiveLevel`),
        //    so it is a second, independent threshold and not a synonym.
        let name_obj = ctx.create_string(name);
        let initializer = ctx.read_native_pin(init_pin, initializer);
        if let Ok(Some(Value::Object(level))) = ctx.invoke_virtual(
            initializer,
            "getMinimumLevel",
            "(Ljava/lang/String;)Ljava/util/logging/Level;",
            &[Value::Object(Some(name_obj))],
        ) {
            let value = level
                .and_then(|l| jul_requested_level_value(ctx, Some(l)))
                .unwrap_or(JBOSS_LEVEL_ALL_INT);
            set_name_keyed_level(logger_minimum_levels(vm), name, Some(value));
        }

        // 2. `getInitialLevel(name)`. Null means "inherit", which in this
        //    name-keyed model means recording nothing and letting the
        //    ancestor walk answer.
        let name_obj = ctx.create_string(name);
        let initializer = ctx.read_native_pin(init_pin, initializer);
        if let Ok(Some(Value::Object(Some(level)))) = ctx.invoke_virtual(
            initializer,
            "getInitialLevel",
            "(Ljava/lang/String;)Ljava/util/logging/Level;",
            &[Value::Object(Some(name_obj))],
        ) {
            let value = jul_requested_level_value(ctx, Some(level));
            set_name_keyed_level(logger_explicit_levels(vm), name, value);
            // Keep the node's own `getLevel()` consistent with the table,
            // exactly as `setLevel` does. Re-derive both references: the
            // invoke above ran application bytecode and could have moved them.
            let logger = ctx.read_native_pin(logger_pin, logger);
            if ctx.object_num_fields(logger) > LOGGER_FIELD_LEVEL {
                ctx.set_field(logger, LOGGER_FIELD_LEVEL, Value::Object(Some(level)));
            }
        }

        // 3. `getInitialHandlers(name)`.
        let name_obj = ctx.create_string(name);
        let initializer = ctx.read_native_pin(init_pin, initializer);
        let handlers = match ctx.invoke_virtual(
            initializer,
            "getInitialHandlers",
            "(Ljava/lang/String;)[Ljava/util/logging/Handler;",
            &[Value::Object(Some(name_obj))],
        ) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            _ => return Ok(()),
        };
        let arr_pin = ctx.pin_native_root(handlers);
        let len = ctx.array_length(handlers);
        for index in 0..len {
            let handlers = ctx.read_native_pin(arr_pin, handlers);
            let element = ctx.get_array_element(handlers, index);
            if !matches!(element, Value::Object(Some(_))) {
                continue;
            }
            let logger = ctx.read_native_pin(logger_pin, logger);
            native_jul_logger_add_handler(ctx, &[Value::Object(Some(logger)), element])?;
        }
        ctx.unpin_native_roots(arr_pin);
        Ok(())
    })();
    ctx.unpin_native_roots(init_pin);
    result
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
    Ok(Some(Value::Object(Some(logger?))))
}

/// `java.util.logging.Level.parse(String)` — static factory that resolves a
/// level name (or its decimal `intValue()`) to the canonical `Level` object.
///
/// Real JDK 25 bytecode resolves this through `KnownLevel.findByName`, which
/// (per `gaps/kc16-blocker-map.md`'s KC16 investigation) walks
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
/// `java/util/logging/Level.findLevel(String)` — `parse`'s non-throwing
/// sibling. Same resolution as `parse`, but an unknown name yields `null`
/// instead of an `IllegalArgumentException`, which is precisely the contract
/// `LogManager.getLevelProperty` relies on to fall back to its default.
fn native_level_find_level(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match native_level_parse(ctx, args) {
        Ok(v) => Ok(v),
        // `parse` throws for an unresolvable name (and NPEs on null); the
        // findLevel contract is to report that as `null`.
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

fn native_level_parse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name_obj = match args.first() {
        Some(Value::Object(Some(s))) => *s,
        // The CLASS was already right; the MESSAGE was invented. HotSpot's
        // `Level.parse` opens `name.length()`, so the helpful-NPE names
        // `String.length()`. See `JUL_NPE_NULL_LEVEL_NAME`.
        _ => return jul_throw_npe(JUL_NPE_NULL_LEVEL_NAME),
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
    let set = try_alloc_concurrent_synthetic(ctx, "java/util/Collections$EmptySet", 0)?;
    Ok(Some(Value::Object(Some(set))))
}

/// `org.jboss.logmanager.Logger.getLevel()`.
///
/// Used to answer a hardcoded null, justified as "spec-legal (inherit from
/// parent)". It is spec-legal only for a logger nobody has called `setLevel`
/// on — and `AbstractQuarkusExtensionTest.overrideLoggerLevel` does exactly
/// `getLevel()` (stash), `setLevel(TRACE)`, then `setLevel(stashed)` to
/// restore, so a constant null made the override unobservable AND its restore
/// a lie. The level lives in the same VM-internal [`LOGGER_FIELD_LEVEL`] slot
/// the `java/util/logging/Logger.getLevel` native reads, because the JBoss
/// synthetic logger is allocated at exactly the same width and field map
/// (`get_or_create_jboss_logger` -> `LOGGER_NUM_FIELDS`).
fn native_jboss_logger_get_level(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    if ctx.object_num_fields(this) <= LOGGER_FIELD_LEVEL {
        return Ok(Some(Value::Object(None)));
    }
    match ctx.get_field(this, LOGGER_FIELD_LEVEL) {
        v @ Value::Object(_) => Ok(Some(v)),
        // A raw int can reach the slot through `logging_shims`' `setLevel`
        // shape; there is no `Level` object to hand back for it.
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `org.jboss.logmanager.Logger.getParent()`.
///
/// Used to answer a hardcoded null "because returning the root would loop".
/// It does not loop: the walk terminates because the ROOT logger (name `""`)
/// still answers null here, which is also what HotSpot answers for it. What
/// the constant null did instead was make every logger look like a root, so
/// nothing that walks the chain — `getEffectiveLevel`, `useParentHandlers`
/// propagation, and `AbstractQuarkusExtensionTest`'s own handler restore —
/// could see an ancestor.
///
/// Mirrors real JBoss LogManager, whose `LoggerNode` tree is built by splitting
/// the dotted name, so EVERY intermediate node exists whether or not anyone
/// asked for it: the parent is the immediate dotted-name predecessor.
/// Measured on Temurin 25 + jboss-logmanager 3.2.2: `probe.fresh`, created
/// alone, reports parent `probe` — a name no caller ever requested. Hence
/// `get_or_create_jboss_logger` here rather than a registry lookup.
fn native_jboss_logger_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    let name = read_jul_logger_name(ctx, this);
    if name.is_empty() {
        // The root logger has no parent — this is the terminating case that
        // makes a caller's parent walk finite, and it is what HotSpot answers
        // for the root too.
        return Ok(Some(Value::Object(None)));
    }
    let parent_name = match name.rfind('.') {
        Some(idx) => name[..idx].to_string(),
        None => String::new(),
    };
    // The JBoss shape specifically: this accessor is declared to return
    // `org/jboss/logmanager/Logger`, so handing back a JUL mirror would give
    // the caller a value its own `checkcast` rejects — the very bug this doc
    // family started from.
    let parent = get_or_create_jboss_logger(ctx, &parent_name)?;
    Ok(Some(Value::Object(Some(parent))))
}

/// `org.jboss.logmanager.Logger.setLevel(Level)`.
///
/// Used to be a no-op justified as "level filtering happens in the
/// process-wide tracing subscriber". The subscriber governs what CratonVM's
/// own console sink emits; it cannot answer `getLevel()`, it cannot make
/// `isLoggable` disagree with the default, and it is invisible to a Java
/// caller. Route through the same name-keyed table the JUL `setLevel` native
/// uses ([`record_jul_logger_level`], which every `setLevel` registration in
/// the tree is required to funnel through) and stamp the slot, so `getLevel`
/// round-trips and [`native_jul_logger_is_loggable`]'s ancestor walk sees it.
fn native_jboss_logger_set_level(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let level = args.get(1).copied().unwrap_or(Value::Object(None));
    record_jul_logger_level(ctx, this, level);
    if ctx.object_num_fields(this) > LOGGER_FIELD_LEVEL {
        ctx.set_field(this, LOGGER_FIELD_LEVEL, level);
    }
    Ok(None)
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

/// Null-safe no-op for the `org.jboss.logmanager.Logger` setters whose real
/// bytecode dereferences `this.loggerNode` and whose value this VM does not
/// model (`setUseParentHandlers` / `setUseParentFilters`). It is NOT for the
/// handler mutators any more — those now maintain the real side list.
fn native_jboss_logger_handler_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// Allocate an empty `java.util.logging.Handler[]`, typed when the element
/// class resolves.
fn empty_handler_array(ctx: &mut dyn NativeContext) -> ObjectRef {
    match ctx.ensure_class_initialized("java/util/logging/Handler") {
        Ok(handler_cid) => ctx.new_ref_array(handler_cid, 0),
        Err(_) => ctx.new_array(ArrayElementType::Reference, 0),
    }
}

/// `org.jboss.logmanager.Logger.getHandlers()`.
///
/// Used to answer a freshly allocated EMPTY array on every call, whatever had
/// been added. That is the shape of an answer, not an answer:
/// `AbstractQuarkusExtensionTest.beforeAll` does
/// `originalHandlers = rootLogger.getHandlers()` and `afterAll` restores that
/// array, so an always-empty read silently discarded whatever the surrounding
/// application had configured on the root logger.
///
/// Reads the identity-keyed side list `addHandler` maintains — this logger's
/// OWN handlers only, which is what both JUL and JBoss `getHandlers()` return
/// (ancestor propagation is a publish-time concern, see
/// [`resolve_jul_handler_list`]).
fn native_jboss_logger_get_handlers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        let arr = empty_handler_array(ctx);
        return Ok(Some(Value::Object(Some(arr))));
    };
    let Some(side_list) = crate::jul_logger_handlers_get(ctx, this) else {
        let arr = empty_handler_array(ctx);
        return Ok(Some(Value::Object(Some(arr))));
    };
    let side_list_pin = ctx.pin_native_root(side_list);
    let result = (|| -> Result<ObjectRef, MethodCallFailed> {
        let size = match ctx.invoke_virtual(side_list, "size", "()I", &[])? {
            Some(Value::Int(size)) if size > 0 => size as usize,
            _ => 0,
        };
        let arr = match ctx.ensure_class_initialized("java/util/logging/Handler") {
            Ok(handler_cid) => ctx.new_ref_array(handler_cid, size),
            Err(_) => ctx.new_array(ArrayElementType::Reference, size),
        };
        let arr_pin = ctx.pin_native_root(arr);
        for index in 0..size {
            let side_list = ctx.read_native_pin(side_list_pin, side_list);
            let element = ctx.invoke_virtual(
                side_list,
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Int(index as i32)],
            )?;
            // `get` runs real `ArrayList` bytecode and can allocate, so the
            // array we are filling may have moved: re-derive it from its pin
            // before every store.
            let arr = ctx.read_native_pin(arr_pin, arr);
            if let Some(value @ Value::Object(Some(_))) = element {
                ctx.set_array_element(arr, index, value);
            }
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        Ok(arr)
    })();
    ctx.unpin_native_roots(side_list_pin);
    Ok(Some(Value::Object(Some(result?))))
}

/// `org.jboss.logmanager.Logger.setHandlers(Handler[])` — JBoss-only, and the
/// exact call `AbstractQuarkusExtensionTest.afterAll` uses to put the root
/// logger back the way it found it. Replaces the whole handler set.
fn native_jboss_logger_set_handlers(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let replacement = match args.get(1).copied() {
        Some(Value::Object(Some(arr))) => Some(arr),
        _ => None,
    };
    let this_pin = ctx.pin_native_root(this);
    let replacement_pin = replacement.map(|a| (ctx.pin_native_root(a), a));
    let result = (|| -> Result<(), MethodCallFailed> {
        // Clear first: the side list is the authority every publish reads, and
        // `setHandlers` is a REPLACE, not an add.
        clear_jul_handler_side_list(ctx, this)?;
        let Some((pin, arr)) = replacement_pin else {
            return Ok(());
        };
        let arr = ctx.read_native_pin(pin, arr);
        let len = ctx.array_length(arr);
        for index in 0..len {
            let arr = ctx.read_native_pin(pin, arr);
            let element = ctx.get_array_element(arr, index);
            let this = ctx.read_native_pin(this_pin, this);
            if let Value::Object(Some(_)) = element {
                native_jul_logger_add_handler(ctx, &[Value::Object(Some(this)), element])?;
            }
        }
        Ok(())
    })();
    ctx.unpin_native_roots(this_pin);
    result?;
    Ok(None)
}

/// Empty `logger`'s handler side list AND its name-keyed compatibility entry.
fn clear_jul_handler_side_list(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let name = read_jul_logger_name(ctx, logger);
    logger_handlers(ctx.vm_identity())
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&name);
    let Some(side_list) = crate::jul_logger_handlers_get(ctx, logger) else {
        return Ok(());
    };
    ctx.invoke_virtual(side_list, "clear", "()V", &[])?;
    Ok(())
}

fn native_jboss_logger_get_use_parent_filters(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `org.jboss.logmanager.Logger.getEffectiveLevel()I`.
///
/// Returned a hardcoded 800 (INFO) — "the JBoss LoggerNode default before any
/// setLevel call". The words "before any setLevel call" were the whole bug:
/// the constant kept being the answer AFTER one too, so
/// `setLevel(Level.TRACE)` (Quarkus's `traceCategories(...)` support) could
/// not be observed through the accessor whose entire job is to report it.
///
/// The unconfigured default is [`JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL`] — INFO,
/// the `Logger.INFO_INT` `LoggerNode.<init>` seeds `effectiveLevel` with when
/// the initializer answers no initial level. A Quarkus process reads
/// `Integer.MIN_VALUE` instead, and that is not the library's default: it is
/// `InitialConfigurator.getInitialLevel("")` returning `Level.ALL` for the
/// ROOT, which every logger then inherits. See that constant's doc for the
/// control run that separates the two.
///
/// `jul_ancestor_explicit_level` is the same name-keyed table with a
/// nearest-ancestor walk that [`native_jboss_logger_is_loggable`] consults, so
/// the two agree by construction rather than by coincidence.
fn native_jboss_logger_get_effective_level(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL)));
    };
    // Same per-call name-read cost as `isLoggable`, same skip — jboss-logging
    // gates on `getEffectiveLevel()` as readily as on `isLoggable`.
    if no_explicit_logger_levels(vm) {
        return Ok(Some(Value::Int(JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL)));
    }
    let name = read_jul_logger_name(ctx, this);
    Ok(Some(Value::Int(
        jul_ancestor_explicit_level(vm, &name).unwrap_or(JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL),
    )))
}

/// `org.jboss.logmanager.Logger.isLoggable(Level)Z`.
///
/// Answered a constant `true` ("filtering is owned by the tracing
/// subscriber"), which was unfalsifiable: `setLevel(WARNING)` suppressed
/// nothing a Java caller could see.
///
/// Implements `LoggerNode.isLoggableLevel` as it is actually written —
/// `level != OFF_INT && level >= effectiveMinLevel && level >= effectiveLevel`
/// — over the two name-keyed tables. It cannot share
/// [`native_jul_logger_is_loggable`], which knows about neither the OFF rule
/// nor the minimum-level floor.
fn native_jboss_logger_is_loggable(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
    }
    let vm = ctx.vm_identity();
    let level_value = match args.get(1) {
        Some(Value::Object(level)) => jul_requested_level_value(ctx, *level).unwrap_or(800),
        _ => 800,
    };
    // `isLoggable(Level.OFF)` is false whatever the thresholds are, and no
    // comparison can express that: OFF is `Integer.MAX_VALUE`, i.e. at or
    // above every threshold there is. Checked before the fast path below,
    // which would otherwise answer `true` for it.
    if level_value == JBOSS_LEVEL_OFF_INT {
        return Ok(Some(Value::Int(0)));
    }
    // Nothing configured anywhere. Taken BEFORE the receiver's name is read,
    // because that read allocates and this is a per-log-site call — see
    // `no_explicit_logger_levels`.
    if no_explicit_logger_levels(vm) && logger_minimum_levels(vm).lock().is_ok_and(|m| m.is_empty())
    {
        return Ok(Some(Value::Int(i32::from(
            level_value >= JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL,
        ))));
    }
    let name = match args.first() {
        Some(Value::Object(Some(this))) => read_jul_logger_name(ctx, *this),
        _ => String::new(),
    };
    let effective =
        jul_ancestor_explicit_level(vm, &name).unwrap_or(JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL);
    let minimum = jboss_ancestor_minimum_level(vm, &name).unwrap_or(JBOSS_LEVEL_ALL_INT);
    Ok(Some(Value::Int(i32::from(
        level_value >= minimum && level_value >= effective,
    ))))
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
    let mut record = match args.get(1) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    };
    let mut this = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // Deliver to the REAL handler chain first — this is the single convergence
    // point for every `org.jboss.logmanager.Logger.info/warning/severe/...`
    // call, so it is also the only place an `InMemoryLogHandler` installed by
    // `AbstractQuarkusExtensionTest` (or any application handler) can be
    // reached from. Without this the whole method was a stderr sink: the
    // operator saw the line and the program never did.
    //
    // Ordering mirrors `jul_convenience_with_console_fallback` — publish, and
    // fall back to the console sink only when no handler took the record, so
    // a configured handler chain does not print every line twice.
    //
    // GC SAFETY: the publish allocates (a `LogRecord`, the ancestor walk, the
    // handler invocations), so `this` and `record` are pinned across it and
    // re-derived — every read below this point uses the refreshed pair rather
    // than `args`, whose contents are pre-call addresses.
    if let (Some(this_obj), Some(record_obj)) = (this, record) {
        let this_pin = ctx.pin_native_root(this_obj);
        let record_pin = ctx.pin_native_root(record_obj);
        let delivered = publish_existing_record_to_jul_handlers(ctx, this_obj, record_obj);
        this = Some(ctx.read_native_pin(this_pin, this_obj));
        record = Some(ctx.read_native_pin(record_pin, record_obj));
        ctx.unpin_native_roots(this_pin);
        if delivered {
            return Ok(None);
        }
    }
    // Logger name: prefer record.loggerName, else this.name (synthetic slot 0).
    let mut logger: Option<String> = None;
    if let Some(r) = record {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(r, "loggerName") {
            logger = ctx.read_string(s);
        }
    }
    if logger.is_none() {
        if let Some(o) = this {
            if let Value::Object(Some(s)) = ctx.get_field(o, LOGGER_FIELD_NAME) {
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
        let detail = match crate::lang_misc::throwable_field_get(ctx, t, "detailMessage") {
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
fn jboss_logger_level_filter() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JBOSS_LOGGER_LEVEL_FILTER").as_deref(),
            Ok("0")
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
fn jboss_record_suppressed_by_level(vm: usize, level_name: &str, logger_name: &str) -> bool {
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
fn jboss_printf_format(
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
fn jboss_message_format(
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
    // See fixed-suite-bugs/springboot/propertiesmigration-logfactory-oom-residual-FIXED.md.
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
    // Null `Level` throws; a null MESSAGE does not. Measured both ways:
    // `log(null, "msg")` NPEs, `log(SEVERE, (String) null)` returns. This
    // overload does not route through `native_jul_logger_is_loggable`, so the
    // check is restated here rather than inherited.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
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
    // Null `Level` throws. A null single PARAMETER does not — measured:
    // `log(SEVERE, "msg", (Object) null)` returns on HotSpot, and the JDK
    // still wraps that null in a one-element array.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
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
fn native_jul_logger_log_params(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Null `Level` throws. A null PARAMETER ARRAY does not — measured:
    // `log(SEVERE, "msg", (Object[]) null)` returns on HotSpot.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
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
fn native_jul_logger_add_handler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let handlers = all.entry(name).or_default();
    if !handlers.iter().any(|&addr| addr == handler.as_ptr() as u64) {
        handlers.push(handler.as_ptr() as u64);
    }
    drop(all);
    if is_root && tomcat_classloader_log_manager_requested(ctx) {
        let loader_key = tomcat_context_loader_key(ctx);
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
            handlers.retain(|&addr| addr != handler.as_ptr() as u64);
        }
    }
    // `addHandler` writes TWO tables — the name-keyed compatibility map above
    // AND the identity-keyed `ArrayList` side list that
    // `resolve_jul_handler_list` (and therefore every publish) actually reads.
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
fn remove_from_jul_handler_side_list(
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
                )?;
            }
        }
        Ok(())
    })();
    ctx.unpin_native_roots(side_list_pin);
    result
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
    stamp_inferred_caller(ctx, record);
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
    let handlers = logger_handlers(vm)
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
        stamp_inferred_caller(ctx, record);
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
    // Null `Level` throws. The SOURCE CLASS and SOURCE METHOD do not —
    // measured: `logp(SEVERE, null, null, "msg")` returns on HotSpot, and so
    // does `entering(null, null)` / `exiting(null, null)` / `throwing(null,
    // null, null)`. Three nulls in one call, one of which is fatal and two of
    // which are ordinary: this signature is the clearest single refutation of
    // a blanket JUL null rule in the whole package.
    if jul_arg_is_null(args, 1) {
        return jul_throw_npe(JUL_NPE_NULL_LEVEL);
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
        let detail = match crate::lang_misc::throwable_field_get(ctx, t, "detailMessage") {
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
    } else {
        Ok(false)
    };
    // `console_line` is a plain Rust String, so emitting it here (after the
    // publish) needs no further pin refresh.
    let delivered = matches!(result, Ok(true));
    if !delivered {
        crate::emit_framework_log(ctx, &console_line);
    }
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    result.map(|_| None)
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
fn stamp_inferred_caller(ctx: &mut dyn NativeContext, record: ObjectRef) {
    let frames = ctx.capture_stack_trace(0);
    // Outermost-first, so the innermost frame — the direct caller of the log
    // method — is the LAST one. Skip any logging/reflection frame a re-entrant
    // log call could have left on top.
    let Some(frame) = frames.iter().rev().find(|f| {
        let c = f.class_name.as_ref();
        !c.starts_with("java/util/logging/")
            && !c.starts_with("sun/util/logging/")
            && !c.starts_with("java/lang/reflect/")
            && !c.starts_with("jdk/internal/reflect/")
    }) else {
        return;
    };
    let class = frame.class_name.replace('/', ".");
    let method = frame.method_name.as_ref().to_string();
    let record_pin = ctx.pin_native_root(record);
    let cls_obj = ctx.create_string(&class);
    let cls_pin = ctx.pin_native_root(cls_obj);
    let mth_obj = ctx.create_string(&method);
    let cls_obj = ctx.read_native_pin(cls_pin, cls_obj);
    let record = ctx.read_native_pin(record_pin, record);
    ctx.set_field_by_name(record, "sourceClassName", Value::Object(Some(cls_obj)));
    ctx.set_field_by_name(record, "sourceMethodName", Value::Object(Some(mth_obj)));
    // The real setters clear this; stamping the fields directly must too, or
    // the pair is written into a record that still believes it owes an
    // inference. That was inert while `getSourceClassName` was a shadow doing a
    // bare field read, but those four triples are retired under `--jdk-only`
    // since 2026-08-12 (native-api/src/retired_shadow.rs), so the REAL getter —
    // `if (needToInferCaller) inferCaller(); return sourceClassName;` — now
    // runs there and would overwrite this stamp with whatever its own walk
    // found. Every `java/util/logging/` entry point into this file is itself
    // retired in strict mode, so today that is reachable only via a non-JUL
    // receiver (e.g. the `org/jboss/logmanager/` bridges, which are not in the
    // retirement table); write the flag rather than rely on that staying true.
    //
    // Inert in `Compatible`, where the four accessors still dispatch as natives
    // and none of them reads this flag. W7-56-infercaller-strict.md
    if crate::log_record_real_layout(ctx, record) {
        ctx.set_field_by_name(record, "needToInferCaller", Value::Int(0));
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
/// FIRST CHOICE is `lib.rs`'s `jul_logger_use_parent_handlers_table`, which is
/// where `Logger.setUseParentHandlers(Z)V` records the flag. That write and
/// this read used to be in different modules and never met: the setter stored
/// into the side table, this walked `config.useParentHandlers`, and `config` is
/// null on every logger this bridge mints — so the `false` was accepted, read
/// back correctly by `getUseParentHandlers()`, and then ignored by the only
/// consumer that matters. An accessor agreeing with itself is not evidence the
/// consumer agrees with it. Measured in `Compatible` before this change:
/// `setUseParentHandlers(false)` then `info(...)` still reached the parent's
/// Handler (`[INFO:not-up-to-parent]`), where HotSpot delivers nothing; under
/// `--jdk-only` no native holds either triple, real bytecode runs, and the walk
/// already stopped. So this closes a `Compatible`-only divergence and leaves
/// strict mode byte-identical. See
/// docs/known-issues/jdk-only/W7-35-jul-supplier-and-payload-residuals.md.
///
/// An ABSENT entry is not `false`: it means nothing ever called the setter, so
/// fall through to the `config` read and its JDK default (`true`) rather than
/// letting a logger nobody configured silence its ancestors' handlers.
fn jul_use_parent_handlers(ctx: &dyn NativeContext, logger: ObjectRef) -> bool {
    let key = ctx.identity_hash_code(logger);
    if let Some(flag) = crate::jul_logger_use_parent_handlers_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied()
    {
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

/// Resolve the handler `ArrayList` that should receive a record published for
/// `logger`: the logger's own handlers first, then (when `useParentHandlers`
/// allows it) the nearest dotted-name ancestor that has any.
///
/// GC: the ancestor walk demand-creates loggers and therefore allocates, so
/// callers MUST re-derive every reference they still hold — including `logger`
/// itself — from its pin after this returns.
fn resolve_jul_handler_list(
    ctx: &mut dyn NativeContext,
    logger: ObjectRef,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if let Some(handlers) = crate::jul_logger_handlers_get(ctx, logger) {
        return Ok(Some(handlers));
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
                return Ok(Some(list));
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
        return Ok(None);
    }
    // This walk used to `get_or_create_logger(candidate)` — always the
    // `java/util/logging/Logger` shape. Under
    // `java.util.logging.manager=org.jboss.logmanager.LogManager` every
    // factory hands back the `org/jboss/logmanager/Logger` shape instead, so
    // the handler Quarkus installs on the ROOT logger lives on a JBoss object
    // while the walk demand-created a brand-new JUL object for `""` and read
    // ITS (empty) side list: every record published through an ancestor was
    // dropped. `existing_loggers_for_name` reports both shapes and creates
    // neither — a name nobody has asked for cannot have handlers, so the
    // demand-creation was only ever paying an allocation for a guaranteed miss.
    let mut candidate: &str = &logger_name;
    while let Some(parent) = logger_name_parent(candidate) {
        for ancestor in existing_loggers_for_name(ctx, parent).into_iter().flatten() {
            if let Some(h) = crate::jul_logger_handlers_get(ctx, ancestor) {
                return Ok(Some(h));
            }
        }
        candidate = parent;
    }
    Ok(None)
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
    let Ok(Some(handlers)) = resolve_jul_handler_list(ctx, logger) else {
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
fn infer_jul_caller_source(ctx: &mut dyn NativeContext) -> Option<(String, String)> {
    // Outermost-first: `frames[0]` is `main`, the last entry is the innermost
    // Java frame. Documented on `capture_stack_trace` and relied on the same
    // way by `locale_resources::caller_bundle_class_loader`.
    let frames = ctx.capture_stack_trace(0);
    frames
        .iter()
        .rev()
        .find(|f| {
            &*f.class_name != "java/util/logging/Logger"
                && !f.class_name.starts_with("sun/util/logging/PlatformLogger")
        })
        .map(|f| (f.class_name.replace('/', "."), f.method_name.to_string()))
}

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
        let Ok(Some(handlers)) = resolve_jul_handler_list(ctx, logger) else {
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
        // (fixed-suite-bugs/tomcat/dohead-post-fix-sporadic-residuals-FIXED.md).
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
        stamp_inferred_caller(ctx, record);
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
            cratonvm_native_api::delegated_close::vm_only_best_effort(&*ctx, flushed)?;
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
    let detail = match crate::lang_misc::throwable_field_get(ctx, t, "detailMessage") {
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
                            ctx.unpin_native_roots(base);
                        }
                        return Err(e);
                    }
                };
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
/// Recorded in docs/known-issues/jdk-only/W7-25-jul-getlogger-regression.md;
/// found by W7-22 §4.1, which measured the fan-out half only.
fn native_jul_logger_log_supplier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
                ctx.unpin_native_roots(base);
            }
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
/// docs/known-issues/jdk-only/W7-25-jul-getlogger-regression.md §7.
fn native_jul_logger_log_record(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

// ---------------------------------------------------------------------------
// The `java.util.logging` null axis
// ---------------------------------------------------------------------------
//
// MEASURED against HotSpot 25.0.3+9-LTS, one probe per method, printing the
// exception class AND its exact `getMessage()` — see
// docs/known-issues/jdk-only/G15-1-the-jul-null-axis-and-how-far-RJdkIntrinsics3-got-20260817.md
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
const JUL_NPE_NULL_LEVEL: &str =
    "Cannot invoke \"java.util.logging.Level.intValue()\" because \"level\" is null";

/// HotSpot's text for a null `Supplier` on `log(Level, Supplier)` /
/// `log(Level, Throwable, Supplier)`. The JDK's parameter is named
/// `msgSupplier`, and the helpful-NPE quotes the parameter name.
const JUL_NPE_NULL_SUPPLIER: &str =
    "Cannot invoke \"java.util.function.Supplier.get()\" because \"msgSupplier\" is null";

/// HotSpot's text for a null key reaching the `ConcurrentHashMap` behind
/// `Logger.getLogger` / `LogManager.getLogger` / `LogManager.getProperty`.
/// The NPE is raised by the map, not by JUL, which is why all three read
/// identically and why none of them names a `java.util.logging` type.
const JUL_NPE_NULL_KEY: &str = "Cannot invoke \"Object.hashCode()\" because \"key\" is null";

/// HotSpot's text for `Logger.log((LogRecord) null)`.
const JUL_NPE_NULL_RECORD: &str =
    "Cannot invoke \"java.util.logging.LogRecord.getLevel()\" because \"record\" is null";

/// HotSpot's text for `LogManager.addLogger(null)`.
const JUL_NPE_NULL_LOGGER: &str =
    "Cannot invoke \"java.util.logging.Logger.getName()\" because \"logger\" is null";

/// HotSpot's text for `Level.parse(null)`. `Level.parse` opens with
/// `name.length()`, so the helpful-NPE names `String.length()` — NOT a
/// hand-written "Name cannot be null", which is what this file answered
/// before and which no HotSpot build produces.
const JUL_NPE_NULL_LEVEL_NAME: &str = "Cannot invoke \"String.length()\" because \"name\" is null";

/// Raise the JUL null-argument NPE with HotSpot's own message text.
fn jul_throw_npe(message: &str) -> MethodCallResult {
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
fn jul_arg_is_null(args: &[Value], idx: usize) -> bool {
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
fn jul_resolve_msg_strict(
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
        return match ctx.invoke_virtual(o, "get", "()Ljava/lang/Object;", &[])? {
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
fn jul_requested_level_value(ctx: &dyn NativeContext, level: Option<ObjectRef>) -> Option<i32> {
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

pub(crate) fn native_jul_logger_is_loggable(
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
                    _ => None,
                },
                _ => None,
            },
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
    Ok(Some(Value::Int(if level_value >= threshold {
        1
    } else {
        0
    })))
}

/// Read a synthetic `Level`'s int value: slot 1 directly, or slot 0's name
/// mapped through [`jul_standard_level_value`]. Used only after the by-name
/// lookups fail, i.e. for a receiver with no field names at all.
fn synthetic_level_value(ctx: &dyn NativeContext, level: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(level) > 1 {
        if let Value::Int(v) = ctx.get_field(level, 1) {
            return Some(v);
        }
    }
    if ctx.object_num_fields(level) > 0 {
        if let Value::Object(Some(s)) = ctx.get_field(level, 0) {
            if let Some(n) = ctx.read_string(s) {
                return jul_standard_level_value(&n);
            }
        }
    }
    None
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
/// Write (or clear, on `None`) one name-keyed level table entry.
///
/// Shared by the two tables — the effective level and the minimum level — so
/// the "a `None` REMOVES the row" rule cannot drift between them. Unlike
/// [`record_jul_logger_level`] this takes the name directly, so it can record
/// the ROOT (`""`): that function refuses an empty name because it derives it
/// from a Logger object, where an empty name also means "could not read one".
/// Here the caller has the real name in hand.
pub(crate) fn set_name_keyed_level(
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
fn logger_minimum_levels(vm: usize) -> &'static Mutex<HashMap<String, i32>> {
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
fn jboss_ancestor_minimum_level(vm: usize, logger_name: &str) -> Option<i32> {
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
fn no_explicit_logger_levels(vm: usize) -> bool {
    logger_explicit_levels(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
}

fn jul_ancestor_explicit_level(vm: usize, logger_name: &str) -> Option<i32> {
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
pub(crate) fn record_jul_logger_level(ctx: &dyn NativeContext, logger: ObjectRef, level: Value) {
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
            _ => synthetic_level_value(ctx, level),
        },
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
fn jul_convenience_with_console_fallback(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    jul_level: &str,
    console_tag: &str,
) -> MethodCallResult {
    let delivered = publish_jul_convenience(ctx, args, jul_level)?;
    if !delivered {
        log_simple(ctx, args, console_tag);
    }
    Ok(None)
}

fn native_jul_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "INFO", "INFO")
}
fn native_jul_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "WARNING", "WARN")
}
fn native_jul_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_convenience_with_console_fallback(ctx, args, "SEVERE", "ERROR")
}
fn native_jul_logger_fine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_logger_fine_family(ctx, args, "FINE")
}
fn native_jul_logger_finer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jul_logger_fine_family(ctx, args, "FINER")
}
fn native_jul_logger_finest(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
fn jul_logger_fine_family(
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
fn publish_jul_convenience(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level_name: &str,
) -> Result<bool, MethodCallFailed> {
    let (Some(Value::Object(Some(logger))), Some(Value::Object(Some(message)))) =
        (args.first(), args.get(1))
    else {
        return Ok(false);
    };
    let Some(level) = resolve_standard_level(ctx, level_name) else {
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
    publish_to_jul_handlers_full(ctx, *logger, level, *message, None, None, None, None)
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
        // The fine-grained levels must be gated as THEMSELVES, not collapsed
        // into INFO — otherwise a `finest()` call is level-checked at 800 and
        // sails through any threshold at or below INFO.
        "SEVERE" | "WARNING" | "CONFIG" | "FINE" | "FINER" | "FINEST" => level,
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
    // See fixed-suite-bugs/springboot/propertiesmigration-logfactory-oom-residual-FIXED.md.
    crate::emit_framework_log(ctx, &format!("{level} [{logger_name}] {message}"));
}

fn native_jboss_logger_detach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
fn ensure_config_file_properties_loaded(ctx: &mut dyn NativeContext) {
    static LOADED: OnceLock<()> = OnceLock::new();
    if LOADED.get().is_some() {
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
    // A path exists: this is our one real attempt, success or not.
    let _ = LOADED.set(());
    let Ok(bytes) = std::fs::read(path.trim()) else {
        return;
    };
    let entries = crate::properties_sidetable::parse_properties_pub(&bytes);
    let mut props = parsed_log_properties()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (k, v) in entries {
        // A value already present came from an explicit
        // `readConfiguration(InputStream)` call, which is more specific than
        // the startup file — don't clobber it.
        props.entry(k).or_insert(v);
    }
}

fn native_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
/// `LogManager`, both logger registries, the JBoss `LogContext` singleton,
/// every registered configuration listener, and every attachment receiver /
/// key / value so a moving collector relocates
/// (rather than reclaims) them. Uses blocking locks that are never held across
/// a Java allocation, so the allocating thread cannot self-deadlock here.
pub fn gc_scan_logmanager_roots(vm: usize, out: &mut Vec<ObjectRef>) {
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
        push_addr(addr);
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
        *addr = remap(*addr);
    }
    for addr in tomcat_juli_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
    }
    for handlers in tomcat_juli_root_handler_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        for addr in handlers {
            *addr = remap(*addr);
        }
    }
    for addr in jboss_logger_registry(vm)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values_mut()
    {
        *addr = remap(*addr);
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
    // `Level.findLevel` is `parse`'s non-throwing, package-private sibling —
    // and it is the one the JDK's OWN logging bytecode uses. `LogManager
    // .getLevelProperty(name, default)` is literally
    // `getProperty(name)` -> `Level.findLevel(val.trim())` -> `?: default`,
    // and `Handler`'s `configure()` (run from `new ConsoleHandler()`) applies
    // its configured level through exactly that call.
    //
    // With `parse` overridden but `findLevel` left to real bytecode, the two
    // disagreed: the real `findLevel` resolves names through `Level`'s
    // internal `KnownLevel` registry, which our natively-created `Level`
    // instances are never entered into, so it returned null and every
    // `getLevelProperty` silently fell back to its default. Observable as
    // `java.util.logging.ConsoleHandler.level = ALL` in `conf/logging.properties`
    // producing a handler at `INFO`, which then dropped every FINE/FINER
    // record — while `getProperty` and `Level.parse` both returned `ALL`
    // when called directly, which is what made it look like a level-config
    // bug rather than a handler-config one.
    registry.register(
        CLS_JUL_LEVEL,
        "findLevel",
        "(Ljava/lang/String;)Ljava/util/logging/Level;",
        native_level_find_level,
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
    // KEEP (real JDK body has nothing left to do) — re-derived wave 4,
    // 2026-07-28. `LogManager.checkAccess()`'s entire body was
    // `SecurityManager sm = System.getSecurityManager(); if (sm == null)
    // return; sm.checkPermission(new LoggingPermission("control", null));`.
    // JEP 486 (JDK 24) permanently disabled the Security Manager:
    // `System.getSecurityManager()` now always returns null, so on a real JDK
    // 25 the method returns immediately on its first branch for every caller.
    // An empty body is therefore behaviourally identical, not a suppressed
    // check — and it must stay empty: `Logger.setLevel`/`addHandler`/`reset`
    // all call it, so throwing here would break configuration, not secure it.
    registry.register(CLS_JUL_LOG_MANAGER, "checkAccess", "()V", |_ctx, _args| {
        Ok(None)
    });
    // IMPLEMENTED wave 4 (2026-07-28). Both halves used to be constants: `add`
    // returned `this` and dropped the listener on the floor, `remove` was a
    // no-op justified as "consistent with the add". The justification was
    // wrong — `readConfiguration()`/`readConfiguration(InputStream)`/
    // `updateConfiguration(...)` are all really implemented in this module, and
    // the JDK fires the listener chain after each of them, so the drop WAS
    // observable. They are now backed by `config_listeners(vm)` and fired from
    // `fire_configuration_listeners`.
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "addConfigurationListener",
        "(Ljava/lang/Runnable;)Ljava/util/logging/LogManager;",
        native_add_configuration_listener,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "removeConfigurationListener",
        "(Ljava/lang/Runnable;)V",
        native_remove_configuration_listener,
    );
    // `updateConfiguration(Function)` re-reads the DEFAULT (filesystem /
    // `java.util.logging.config.file`) configuration — exactly the input
    // `native_read_configuration_no_arg` refuses to parse, and for exactly the
    // same reason. Sharing that handler keeps the two refusals from drifting
    // apart, and keeps the rationale in ONE place.
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/util/function/Function;)V",
        native_read_configuration_no_arg,
    );
    registry.register(
        CLS_JUL_LOG_MANAGER,
        "updateConfiguration",
        "(Ljava/io/InputStream;Ljava/util/function/Function;)V",
        native_update_configuration_with_stream,
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
        native_get_jboss_manager_logger,
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
        // KEEP — same reasoning as the `java.util.logging.LogManager.checkAccess`
        // registration above (JEP 486 leaves the inherited body with nothing to
        // do). `org.jboss.logmanager.LogManager` does not override it; this
        // registration exists only so the JBoss class name also resolves.
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
    // The three handler MUTATORS were no-ops (and `getHandlers` a constant
    // empty array) for as long as nothing but WildFly/Keycloak boot logging
    // reached this class — those callers only needed the methods not to NPE on
    // the synthetic Logger's null `loggerNode`. Repointing
    // `LogManager.getLogger` at the JBoss shape (the fix in
    // `julogger-cast-to-jbosslogmanager-logger-20260817.md`) made this the
    // shape ORDINARY application code gets back, and the first thing
    // `io.quarkus.test.AbstractQuarkusExtensionTest.beforeAll` does with it is
    // install an `InMemoryLogHandler` on the root logger and later assert on
    // what it collected. Against the no-ops it collected nothing, forever,
    // with no error anywhere — the JUL face has had a working, GC-safe,
    // ancestor-walking handler chain the whole time, so wire this one to it
    // rather than growing a second.
    registry.register(
        "org/jboss/logmanager/Logger",
        "addHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_add_handler,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "removeHandler",
        "(Ljava/util/logging/Handler;)V",
        native_jul_logger_remove_handler,
    );
    registry.register(
        "org/jboss/logmanager/Logger",
        "setHandlers",
        "([Ljava/util/logging/Handler;)V",
        native_jboss_logger_set_handlers,
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
        native_jul_logger_finer,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "finest",
        "(Ljava/lang/String;)V",
        native_jul_logger_finest,
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
    // exit-reason logger path, killing the boot.
    //
    // Both accessors used to answer a hardcoded null, which fixed that NPE by
    // making the resource-bundle feature invisible: `Logger.getLogger(name,
    // bundleName)` accepted a bundle name and no caller could ever read it
    // back, so localized JUL logging silently degraded to the raw message key
    // on every Logger, real or synthetic. `allocate_logger` now seeds
    // `loggerBundle` and `native_jul_static_get_logger_with_bundle` stamps a
    // named `LoggerBundle` when one was requested, so these read the real
    // field — still null-safe at every step, so the SystemExiter path keeps
    // its null answer and its no-NPE guarantee.
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundleName",
        "()Ljava/lang/String;",
        native_jul_logger_get_resource_bundle_name,
    );
    registry.register(
        CLS_JUL_LOGGER,
        "getResourceBundle",
        "()Ljava/util/ResourceBundle;",
        native_jul_logger_get_resource_bundle,
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
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
        let logger = allocate_logger(&mut ctx, "org.example.capture").unwrap();
        let handler =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/Handler", 0).unwrap();
        native_jul_logger_add_handler(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(handler))],
        )
        .unwrap();
        assert_eq!(
            logger_handlers(TEST_VM)
                .lock()
                .unwrap()
                .get("org.example.capture")
                .map(Vec::len),
            Some(1)
        );

        let record =
            try_alloc_concurrent_synthetic(&mut ctx, "java/util/logging/LogRecord", 5).unwrap();
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
        assert!(logger_handlers(TEST_VM)
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
        let log_context = ensure_jboss_log_context(&mut ctx).unwrap();
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
        let logger = get_or_create_jboss_logger(&mut ctx, "com.example.App").unwrap();
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

    /// The JDK's own `$java.home/conf/logging.properties` is the JDK
    /// `LogManager`'s implicit fallback and must not be taken for the JBoss
    /// subclass, whose `readConfiguration()` override resolves a
    /// `ConfiguratorFactory` through `ServiceLoader` instead and never reads
    /// that file.
    ///
    /// Importing it put `.level=INFO` on the ROOT, and since every logger
    /// inherits the nearest ancestor's explicit level, that one entry became a
    /// process-wide INFO floor — the thing that silenced
    /// `AbstractQuarkusExtensionTest.traceCategories(...)`. Measured on
    /// Temurin 25 + jboss-logmanager 3.2.2: `getEffectiveLevel()` on an
    /// unconfigured logger is `Integer.MIN_VALUE`, not 800.
    ///
    /// Both arms read the SAME file, so the difference this asserts is the
    /// manager property and nothing else.
    #[test]
    fn the_jdk_conf_logging_properties_is_read_for_jul_and_skipped_for_jboss() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "cratonvm-logmanager-conf-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::write(dir.join("conf/logging.properties"), b".level= INFO\n").unwrap();
        let java_home = dir.to_string_lossy().replace('\\', "/");

        // Arm 1: plain JUL — the fallback applies, so the root gets INFO and
        // every descendant inherits it.
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.home", &java_home);
        read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
        assert_eq!(
            jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
            Some(800),
            "the JDK fallback must still apply under the plain JUL manager"
        );

        // Arm 2: same file, same call, JBoss manager — nothing is imported.
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property("java.home", &java_home);
        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
        assert_eq!(
            jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
            None,
            "the JBoss LogManager never reads $java.home/conf/logging.properties"
        );

        // An EXPLICITLY named config file is a deliberate act and is still
        // honoured under either manager — only the implicit fallback is gone.
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        ctx.set_system_property(
            "java.util.logging.manager",
            "org.jboss.logmanager.LogManager",
        );
        ctx.set_system_property(
            "java.util.logging.config.file",
            &format!("{java_home}/conf/logging.properties"),
        );
        read_configuration_no_arg_impl(&mut ctx, &[]).unwrap();
        assert_eq!(
            jul_ancestor_explicit_level(ctx.vm_identity(), "any.logger"),
            Some(800),
            "an explicitly designated config file is still read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unconfigured JBoss logger stops at INFO, exactly like an
    /// unconfigured JUL one.
    ///
    /// **This test previously asserted the opposite**, on a measurement taken
    /// with `quarkus-bootstrap-runner` on the classpath — where
    /// `InitialConfigurator.getInitialLevel("")` hands the ROOT `Level.ALL`
    /// and every logger inherits it. Re-run against jboss-logmanager 3.2.2
    /// with NO `LogContextInitializer` provider on the classpath, the same
    /// probe reads `root.effective=800`, `freshControl.effective=800`,
    /// `freshControl.isLoggableTrace=false`. One application's configuration
    /// had been written down as the library's default, in a constant, a test
    /// name and a doc.
    ///
    /// The control that separates them is DELETING THE PROVIDER from the
    /// classpath, not reading harder: both arms are "jboss-logmanager under
    /// its own LogManager", and only the SPI differs.
    /// `the_initializers_root_level_is_inherited_by_every_logger` is the other
    /// half — the same assertion with a provider installed.
    #[test]
    fn an_unconfigured_jboss_logger_stops_at_info_exactly_like_a_jul_one() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let jboss = get_or_create_jboss_logger(&mut ctx, "probe.fresh").unwrap();
        let trace = make_level(&mut ctx, "TRACE", 400);
        let info = make_level(&mut ctx, "INFO", 800);
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(jboss)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "with no LogContextInitializer provider, TRACE is below the INFO seed"
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(jboss)), Value::Object(Some(info))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(
            native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(jboss))])
                .unwrap(),
            Some(Value::Int(800)),
            "LoggerNode.<init> seeds effectiveLevel with Logger.INFO_INT"
        );
        // The JUL face, same level object, the SAME answer. Built through the
        // real allocator, not `make_logger`: `allocate_logger` initialises the
        // level slot to a null REFERENCE, where a bare `alloc_object` leaves
        // it `Int(0)` and the threshold walk reads that as "level 0", i.e.
        // everything loggable — the mock would agree for the wrong reason.
        let jul = get_or_create_logger(&mut ctx, "probe.fresh.jul").unwrap();
        assert_eq!(
            native_jul_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(jul)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "an unconfigured JUL logger must NOT be loggable at TRACE either"
        );
    }

    /// `isLoggable(Level.OFF)` is false however low the thresholds are.
    ///
    /// `LoggerNode.isLoggableLevel` opens with `level != OFF_INT`, and no
    /// comparison against a threshold can express that: `Level.OFF.intValue()`
    /// is `Integer.MAX_VALUE`, at or above every threshold there is. A
    /// threshold-only implementation answers `true` — the exact opposite —
    /// and does so most confidently in the case where a caller has just
    /// switched a category OFF.
    #[test]
    fn is_loggable_off_is_false_even_when_everything_else_is_enabled() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = get_or_create_jboss_logger(&mut ctx, "probe.off").unwrap();
        let all = make_level(&mut ctx, "ALL", i32::MIN);
        let off = make_level(&mut ctx, "OFF", i32::MAX);
        // Open the logger as wide as it goes first, so the OFF answer cannot
        // come from a threshold that happened to be high.
        native_jboss_logger_set_level(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(all))],
        )
        .unwrap();
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(all))]
            )
            .unwrap(),
            Some(Value::Int(1)),
            "control: with the level at ALL, ALL is loggable"
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(off))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "OFF is refused by name, not by threshold"
        );
    }

    /// The initializer's ROOT level is what every other logger inherits.
    ///
    /// This is the mechanism behind the `Integer.MIN_VALUE` a Quarkus process
    /// reports for every logger: `InitialConfigurator.getInitialLevel("")`
    /// answers `Level.ALL` for the ROOT and null for everything else, and the
    /// nearest-ancestor walk carries it down. Asserted here through the table
    /// the SPI writes rather than through a mock ServiceLoader, so it holds
    /// whatever route a provider is discovered by.
    #[test]
    fn the_initializers_root_level_is_inherited_by_every_logger() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let deep = get_or_create_jboss_logger(&mut ctx, "a.b.c").unwrap();
        let trace = make_level(&mut ctx, "TRACE", 400);
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(deep)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "control: before the root is configured, TRACE is below INFO"
        );

        // What `apply_log_context_initializer` does for `getInitialLevel("")`.
        set_name_keyed_level(
            logger_explicit_levels(ctx.vm_identity()),
            "",
            Some(i32::MIN),
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(deep)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(1)),
            "a level on the ROOT reaches a.b.c — this is the whole mechanism"
        );
        assert_eq!(
            native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(deep))])
                .unwrap(),
            Some(Value::Int(i32::MIN))
        );
    }

    /// `getMinimumLevel` is a SECOND threshold, not a synonym for the level.
    ///
    /// `LoggerNode.isLoggableLevel` is
    /// `level != OFF && level >= effectiveMinLevel && level >= effectiveLevel`,
    /// so a provider that raises the minimum silences a category that
    /// `setLevel` alone would have enabled. Collapsing the two tables into one
    /// would pass every other test in this file and lose exactly this.
    #[test]
    fn the_minimum_level_floor_outranks_an_explicit_set_level() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = get_or_create_jboss_logger(&mut ctx, "floor.probe").unwrap();
        let trace = make_level(&mut ctx, "TRACE", 400);
        let all = make_level(&mut ctx, "ALL", i32::MIN);

        native_jboss_logger_set_level(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(all))],
        )
        .unwrap();
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(1)),
            "control: setLevel(ALL) alone enables TRACE"
        );

        // What `apply_log_context_initializer` does for `getMinimumLevel`.
        set_name_keyed_level(logger_minimum_levels(ctx.vm_identity()), "floor", Some(800));
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(trace))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "an ancestor's minimum level vetoes the logger's own setLevel(ALL)"
        );
        // ...and the effective level still reports what setLevel wrote: the
        // floor gates delivery, it does not rewrite the level.
        assert_eq!(
            native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(logger))])
                .unwrap(),
            Some(Value::Int(i32::MIN))
        );
    }

    /// The SPI must be asked ONCE, and its absence cached.
    ///
    /// Real `LogContext.discoverDefaultInitializer0` resolves into a static.
    /// Without a cached NEGATIVE, every logger creation in a process with no
    /// provider — which is most of them — pays a full `ServiceLoader` scan of
    /// the classpath. The mock cannot load `org.jboss.logmanager`, so this
    /// asserts the shape that matters: the answer is remembered, and asking
    /// again does not re-resolve.
    #[test]
    fn the_log_context_initializer_lookup_caches_its_negative() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert!(resolve_jboss_log_context_initializer(&mut ctx).is_none());
        assert_eq!(
            *jboss_initializer_cell(ctx.vm_identity())
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
            Some(0),
            "the negative must be cached, not re-resolved per logger"
        );
        assert!(resolve_jboss_log_context_initializer(&mut ctx).is_none());
    }

    /// `setLevel` must be OBSERVABLE — through `getLevel`, through
    /// `getEffectiveLevel`, and through `isLoggable`.
    ///
    /// All three used to be constants on this class (`null`, `800`, `true`),
    /// which is what made `AbstractQuarkusExtensionTest.overrideLoggerLevel`
    /// (stash `getLevel()`, `setLevel(TRACE)`, restore) a no-op whose restore
    /// also could not be checked.
    #[test]
    fn a_jboss_logger_level_round_trips_and_gates_is_loggable() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = get_or_create_jboss_logger(&mut ctx, "com.example.App").unwrap();
        let warning = make_level(&mut ctx, "WARNING", 900);
        let info = make_level(&mut ctx, "INFO", 800);
        let severe = make_level(&mut ctx, "SEVERE", 1000);

        assert_eq!(
            native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
            Some(Value::Object(None)),
            "a logger nobody configured has no level of its own"
        );
        native_jboss_logger_set_level(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(Some(warning))],
        )
        .unwrap();
        assert_eq!(
            native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
            Some(Value::Object(Some(warning))),
            "getLevel must hand back the very Level setLevel was given"
        );
        assert_eq!(
            native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(logger))])
                .unwrap(),
            Some(Value::Int(900))
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(info))]
            )
            .unwrap(),
            Some(Value::Int(0)),
            "INFO is below the configured WARNING threshold"
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(severe))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );

        // The restore half. `setLevel(null)` puts the logger back to
        // "inherits", which is what `afterAll` does with the stashed value.
        native_jboss_logger_set_level(
            &mut ctx,
            &[Value::Object(Some(logger)), Value::Object(None)],
        )
        .unwrap();
        assert_eq!(
            native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(logger))]).unwrap(),
            Some(Value::Object(None))
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(Some(info))]
            )
            .unwrap(),
            Some(Value::Int(1)),
            "with the override removed the logger is unconfigured again"
        );
    }

    /// A level set on an ancestor governs a descendant that has none of its
    /// own — `probe.tree` at WARNING makes `probe.tree.kid` refuse INFO.
    /// Measured: `tree.kid.effective=900`, `tree.kid.isLoggableInfo=false`.
    #[test]
    fn a_jboss_logger_inherits_its_nearest_ancestors_level() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let parent = get_or_create_jboss_logger(&mut ctx, "probe.tree").unwrap();
        let kid = get_or_create_jboss_logger(&mut ctx, "probe.tree.kid").unwrap();
        let warning = make_level(&mut ctx, "WARNING", 900);
        let info = make_level(&mut ctx, "INFO", 800);
        let severe = make_level(&mut ctx, "SEVERE", 1000);
        native_jboss_logger_set_level(
            &mut ctx,
            &[Value::Object(Some(parent)), Value::Object(Some(warning))],
        )
        .unwrap();

        assert_eq!(
            native_jboss_logger_get_level(&mut ctx, &[Value::Object(Some(kid))]).unwrap(),
            Some(Value::Object(None)),
            "the child has no level OF ITS OWN"
        );
        assert_eq!(
            native_jboss_logger_get_effective_level(&mut ctx, &[Value::Object(Some(kid))]).unwrap(),
            Some(Value::Int(900)),
            "but it inherits the ancestor's"
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(kid)), Value::Object(Some(info))]
            )
            .unwrap(),
            Some(Value::Int(0))
        );
        assert_eq!(
            native_jboss_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(kid)), Value::Object(Some(severe))]
            )
            .unwrap(),
            Some(Value::Int(1))
        );
    }

    /// `getParent` answered a constant null, so every logger looked like a
    /// root. It is the immediate DOTTED predecessor — jboss-logmanager builds
    /// the whole `LoggerNode` path, so `probe.fresh`'s parent is `probe` even
    /// though nobody ever asked for `probe` (measured:
    /// `level.freshControl.parentName=[probe]`) — and null only for the root,
    /// which is what terminates a caller's walk.
    #[test]
    fn a_jboss_loggers_parent_is_its_immediate_dotted_predecessor() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let kid = get_or_create_jboss_logger(&mut ctx, "probe.fresh").unwrap();
        let parent =
            match native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(kid))]).unwrap() {
                Some(Value::Object(Some(p))) => p,
                other => panic!("expected a parent logger, got {other:?}"),
            };
        assert_eq!(read_jul_logger_name(&ctx, parent), "probe");
        assert_eq!(
            ctx.class_name_arc_of_id(ctx.class_id_of_object(parent))
                .as_deref(),
            Some("org/jboss/logmanager/Logger"),
            "the declared return type is org.jboss.logmanager.Logger — handing \
             back a JUL mirror is the ClassCastException this doc family began with"
        );

        let grandparent =
            match native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(parent))]).unwrap()
            {
                Some(Value::Object(Some(p))) => p,
                other => panic!("expected the root logger, got {other:?}"),
            };
        assert_eq!(read_jul_logger_name(&ctx, grandparent), "");
        assert_eq!(
            native_jboss_logger_get_parent(&mut ctx, &[Value::Object(Some(grandparent))]).unwrap(),
            Some(Value::Object(None)),
            "the root has no parent — this is what makes a caller's walk finite"
        );
    }

    /// `getLoggerNames` read only the JUL registry, so under the JBoss manager
    /// — where every factory mints into the OTHER registry — it enumerated the
    /// loggers nobody had created and omitted every one that existed.
    #[test]
    fn get_logger_names_lists_jboss_shaped_loggers_too() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        get_or_create_jboss_logger(&mut ctx, "probe.a").unwrap();
        get_or_create_logger(&mut ctx, "plain.jul").unwrap();
        // A name in BOTH registries must appear once, not twice.
        get_or_create_jboss_logger(&mut ctx, "in.both").unwrap();
        get_or_create_logger(&mut ctx, "in.both").unwrap();

        let enumeration = match native_get_logger_names(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(e)) => e,
            other => panic!("expected an Enumeration, got {other:?}"),
        };
        let arr = match ctx.get_field(enumeration, 0) {
            Value::Object(Some(a)) => a,
            other => panic!("expected a backing array, got {other:?}"),
        };
        let mut names: Vec<String> = Vec::new();
        for i in 0..ctx.array_length(arr) {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                names.push(ctx.read_string(s).unwrap_or_default());
            }
        }
        assert!(
            names.iter().any(|n| n == "probe.a"),
            "a JBoss-shaped logger must be enumerated; got {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "plain.jul"),
            "and the JUL-shaped ones must not have been dropped; got {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| *n == "in.both").count(),
            1,
            "a name held by both registries is ONE logger name; got {names:?}"
        );
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
        let logger =
            try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS).unwrap();
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
            let logger =
                try_alloc_concurrent_synthetic(&mut ctx, CLS_JUL_LOGGER, LOGGER_NUM_FIELDS)
                    .unwrap();
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

    /// `addLogger(null)` rejects by THROWING, not by answering `false`.
    ///
    /// MEASURED on Temurin 25.0.4+7-LTS — `LogManager.getLogManager()
    /// .addLogger(null)` throws `NullPointerException: Cannot invoke
    /// "java.util.logging.Logger.getName()" because "logger" is null`, because
    /// `LogManager.addLogger` opens with an unguarded `logger.getName()`. The
    /// same run answers `false` for `addLogger(alreadyRegistered)`, which is
    /// the SEPARATE rule `t19_h3_add_logger_returns_true_then_false_on_duplicate`
    /// covers.
    ///
    /// This row used to assert `Int(0)` — the old deliberate swallow, whose
    /// own comment at `native_add_logger` conceded "the contract says NPE" and
    /// kept `false` to keep bootstraps alive. That swallow was removed once it
    /// was measured (it kept alive nothing a real JDK would have run), and this
    /// row was not updated with it, so it went on pinning the behaviour the fix
    /// deleted. `log_manager_add_logger_null_throws_instead_of_answering_false`
    /// is the test that landed WITH the fix; the two agree now, and this one
    /// stays because T19.H3 is a tracked family.
    #[test]
    fn t19_h3_add_logger_rejects_null_logger_argument() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_npe(
            native_add_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
            JUL_NPE_NULL_LOGGER,
            "LogManager.addLogger(null)",
        );
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

    /// `reset()` keeps every logger and resets their CONFIGURATION.
    ///
    /// This test used to assert the opposite — that `reset()` empties the
    /// registry — and passed, because the native did exactly that. HotSpot
    /// 25 disagrees (see `native_reset`'s doc for the measurement): the name
    /// list is byte-identical across `reset()`, and
    /// `Logger.getLogger(name)` still answers the SAME object.
    #[test]
    fn t19_h3_reset_keeps_loggers_and_resets_their_configuration() {
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
        // JUL does NOT materialise the intermediate namespace nodes:
        // `LogManager.addLogger` links a new Logger to the nearest ancestor
        // that ALREADY has a Logger object, falling back to the root, and
        // re-parents existing descendants when an intermediate appears
        // later. Registering "a.b.c" and "d.e.f" therefore creates exactly
        // three entries -- the two requested loggers plus the shared root
        // -- and `getLogger("a.b.c").getParent()` is the ROOT, which is
        // what HotSpot answers. (This used to demand-create "a", "a.b",
        // "d" and "d.e" as well, so the registry held 7 entries and
        // `getParent()` returned a logger HotSpot never creates.)
        {
            let reg = logger_registry(TEST_VM)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, ["", "a.b.c", "d.e.f", "global"]);
        }
        // Configure two of them, so "reset the configuration" has something
        // to reset — and configure the ROOT too, so "the root keeps a level"
        // is distinguishable from "the root was left alone".
        //
        // A marker object rather than a real `Level`: this crate's mock
        // context does not implement `static_field_index_by_name`, so
        // `resolve_standard_level` answers `None` for every name here. A
        // `Level.FINEST` assertion would be measuring the mock. Any object is
        // enough for the property under test, which is that the slot is
        // OVERWRITTEN.
        let marker = ctx.create_string("configured-level-marker");
        let a_before = get_or_create_logger(&mut ctx, "a.b.c").unwrap();
        ctx.set_field(a_before, LOGGER_FIELD_LEVEL, Value::Object(Some(marker)));
        let root_before = get_or_create_logger(&mut ctx, "").unwrap();
        ctx.set_field(root_before, LOGGER_FIELD_LEVEL, Value::Object(Some(marker)));

        native_reset(&mut ctx, &[Value::Object(Some(mgr))]).unwrap();

        {
            let reg = logger_registry(TEST_VM)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                ["", "a.b.c", "d.e.f", "global"],
                "reset() must NOT drop loggers — HotSpot's name list is unchanged"
            );
        }
        // Identity survives: the application's own reference is still the
        // registry's. This is the half that made the old behaviour a silent
        // config split rather than just a short `getLoggerNames()`.
        let a_after = get_or_create_logger(&mut ctx, "a.b.c").unwrap();
        assert_eq!(
            a_before, a_after,
            "getLogger(name) must answer the same object across reset()"
        );
        // A named logger's level goes back to null (inherit).
        assert_eq!(
            ctx.get_field(a_after, LOGGER_FIELD_LEVEL),
            Value::Object(None),
            "reset() sets a named logger's level to null"
        );
        // The root's is re-derived from `Level.INFO` rather than left as the
        // caller set it. Both halves are asserted: it is no longer the marker
        // (so reset() did touch the root), and it is exactly what
        // `resolve_standard_level("INFO")` yields on this context — which is
        // the same expression `native_reset` uses, so the row stays true on a
        // real VM where that resolves to the actual `Level.INFO` singleton.
        let root_after = get_or_create_logger(&mut ctx, "").unwrap();
        let root_level = ctx.get_field(root_after, LOGGER_FIELD_LEVEL);
        assert_ne!(
            root_level,
            Value::Object(Some(marker)),
            "reset() must re-derive the root logger's level, not leave it configured"
        );
        assert_eq!(
            root_level,
            Value::Object(resolve_standard_level(&mut ctx, "INFO")),
            "reset() sets the root logger's level from Level.INFO"
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
        //
        // `global` is present because `getLogManager()` registers it, which is
        // what the JDK does — `LogManager.ensureLogManagerInitialized` adds
        // both the root and `Logger.global`. Measured on HotSpot 25: a fresh
        // manager enumerates `[, global]`, and this set enumerates
        // `[, bar, baz, foo, global]`.
        let mut expected = vec![
            String::new(),
            "bar".to_string(),
            "baz".to_string(),
            "foo".to_string(),
            "global".to_string(),
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
        // Registry untouched — it still holds exactly what `getLogManager()`
        // put there (the root and `global`, as the JDK does), and the rejected
        // name was not added under any spelling.
        //
        // Asserting the CONTENTS rather than `is_empty()` is what keeps this
        // test meaning what it says now that `getLogManager()` registers
        // `global`: an emptiness check would have had to be RELAXED to keep
        // passing, while this one gets stricter.
        {
            let reg = logger_registry(TEST_VM)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut names: Vec<&str> = reg.keys().map(|k| k.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, ["", "global"]);
        }
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
        if let Some(a) = *singleton_cell(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            v.push(a);
        }
        if let Some(a) = *jboss_log_context_singleton(TEST_VM)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            v.push(a);
        }
        v.extend(
            logger_registry(TEST_VM)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .copied(),
        );
        v.extend(
            jboss_logger_registry(TEST_VM)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .copied(),
        );
        for (&(this, key), &value) in attachments(TEST_VM)
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
        let recv = try_alloc_concurrent_synthetic(
            &mut ctx,
            "org/jboss/logmanager/Logger",
            LOGGER_NUM_FIELDS,
        )
        .unwrap();
        let key = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
        let val = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
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
        gc_scan_logmanager_roots(TEST_VM, &mut roots);
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
        let recv = try_alloc_concurrent_synthetic(
            &mut ctx,
            "org/jboss/logmanager/Logger",
            LOGGER_NUM_FIELDS,
        )
        .unwrap();
        let key = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
        let val = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Object", 1).unwrap();
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
        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        // Use a high base so the synthetic targets never collide with a
        // real old address (which would make the assertion ambiguous).
        let base: usize = 0x1_0000_0000_0000;
        for (i, &a) in old_addrs.iter().enumerate() {
            pointer_map.insert(a as usize, base + (i + 1) * 0x1000);
        }

        gc_update_logmanager_refs(TEST_VM, &pointer_map);

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
        gc_update_logmanager_refs(TEST_VM, &cratonvm_types::PointerMap::default());
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
        let mut pm: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        pm.insert(0xdead_beef, 0xfeed_face);
        gc_update_logmanager_refs(TEST_VM, &pm);
        let after = cached_addrs_snapshot();
        assert_eq!(
            before, after,
            "addresses absent from the pointer map must be left unchanged"
        );
    }

    // ======================================================================
    // The JUL null axis
    //
    // Every expectation below is a transcription of a HotSpot 25.0.3+9-LTS
    // run, not a reading of the JDK source. The table is in
    // docs/known-issues/jdk-only/G15-1-the-jul-null-axis-and-how-far-RJdkIntrinsics3-got-20260817.md
    //
    // The tests come in PAIRS on purpose. A test file that only asserts the
    // throws would be passed by a blanket "JUL rejects null" rule — which is
    // the rule HANDOFF-20260814 §5 records as having broken working paths in
    // this exact family. The `..._is_legal_and_must_not_throw` half is what
    // fails if someone generalises.
    // ======================================================================

    fn runtime_error(failed: MethodCallFailed) -> RuntimeError {
        match failed {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(e)) => e,
            other => panic!("expected a RuntimeError, got {other:?}"),
        }
    }

    /// Assert an NPE carrying HotSpot's message VERBATIM. The message is the
    /// point: every row on this axis already threw the right CLASS somewhere,
    /// and `Level.parse` threw the right class with a message
    /// (`"Name cannot be null"`) that no HotSpot build produces.
    fn assert_npe(result: MethodCallResult, expected: &str, what: &str) {
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{what}: expected a throw, got a return"));
        match runtime_error(err) {
            RuntimeError::NullPointerException { message } => assert_eq!(
                message.as_deref(),
                Some(expected),
                "{what}: NPE message must be HotSpot's text verbatim"
            ),
            other => panic!("{what}: expected NullPointerException, got {other:?}"),
        }
    }

    /// The six messages, transcribed. If HotSpot's text ever changes these
    /// are the cells to re-measure; nothing else in this module hard-codes it.
    #[test]
    fn the_jul_null_messages_are_the_measured_hotspot_text() {
        assert_eq!(
            JUL_NPE_NULL_LEVEL,
            "Cannot invoke \"java.util.logging.Level.intValue()\" because \"level\" is null"
        );
        assert_eq!(
            JUL_NPE_NULL_SUPPLIER,
            "Cannot invoke \"java.util.function.Supplier.get()\" because \"msgSupplier\" is null"
        );
        assert_eq!(
            JUL_NPE_NULL_KEY,
            "Cannot invoke \"Object.hashCode()\" because \"key\" is null"
        );
        assert_eq!(
            JUL_NPE_NULL_RECORD,
            "Cannot invoke \"java.util.logging.LogRecord.getLevel()\" because \"record\" is null"
        );
        assert_eq!(
            JUL_NPE_NULL_LOGGER,
            "Cannot invoke \"java.util.logging.Logger.getName()\" because \"logger\" is null"
        );
        assert_eq!(
            JUL_NPE_NULL_LEVEL_NAME,
            "Cannot invoke \"String.length()\" because \"name\" is null"
        );
    }

    /// `Level.parse(null)` threw the right class with an invented message.
    #[test]
    fn level_parse_null_npe_carries_hotspots_message_not_an_invented_one() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert_npe(
            native_level_parse(&mut ctx, &[Value::Object(None)]),
            JUL_NPE_NULL_LEVEL_NAME,
            "Level.parse(null)",
        );
    }

    /// `Level.findLevel` is `parse`'s non-throwing sibling and MUST keep
    /// answering `null` for the same input — the message change above must not
    /// leak through it. `LogManager.getLevelProperty` depends on this.
    #[test]
    fn level_find_level_null_is_still_null_and_must_not_throw() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert!(
            matches!(
                native_level_find_level(&mut ctx, &[Value::Object(None)]),
                Ok(Some(Value::Object(None)))
            ),
            "findLevel must report an unresolvable name as null, never throw"
        );
    }

    /// A null name is not the empty name: this used to hand back the ROOT
    /// logger, so `getLogger(null)` returned a live object where HotSpot NPEs.
    #[test]
    fn log_manager_get_logger_null_key_throws() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("no LogManager"),
        };
        assert_npe(
            native_get_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
            JUL_NPE_NULL_KEY,
            "LogManager.getLogger(null)",
        );
    }

    /// The static factory takes the name in slot 0, not slot 1. Getting that
    /// wrong would make the check fire on the RECEIVER of a method that has
    /// none.
    #[test]
    fn logger_static_get_logger_null_name_throws() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        assert_npe(
            native_jul_static_get_logger(&mut ctx, &[Value::Object(None)]),
            JUL_NPE_NULL_KEY,
            "Logger.getLogger(null)",
        );
    }

    /// The other half of the same signature. MEASURED: `Logger.getLogger
    /// ("more.a", null)` returns a Logger — the BUNDLE may be null even
    /// though the NAME may not.
    #[test]
    fn logger_static_get_logger_null_bundle_is_legal_and_must_not_throw() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let name = ctx.create_string("axis.named");
        let got = native_jul_static_get_logger_with_bundle(
            &mut ctx,
            &[Value::Object(Some(name)), Value::Object(None)],
        );
        assert!(
            matches!(got, Ok(Some(Value::Object(Some(_))))),
            "a null resourceBundleName is legal and must still yield a Logger"
        );
    }

    #[test]
    fn log_manager_get_property_null_key_throws_but_a_missing_key_is_null() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("no LogManager"),
        };
        assert_npe(
            native_get_property(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
            JUL_NPE_NULL_KEY,
            "LogManager.getProperty(null)",
        );
        // The ABSENT key is a different rule and stays null on both VMs.
        let missing = ctx.create_string("no.such.key.at.all");
        assert!(
            matches!(
                native_get_property(
                    &mut ctx,
                    &[Value::Object(Some(mgr)), Value::Object(Some(missing))]
                ),
                Ok(Some(Value::Object(None)))
            ),
            "an absent key must still answer null, not throw"
        );
    }

    /// This one reverses a DELIBERATE swallow whose comment said the contract
    /// was NPE but that returning `false` kept bootstraps alive. It kept
    /// nothing alive that HotSpot would have run.
    #[test]
    fn log_manager_add_logger_null_throws_instead_of_answering_false() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let mgr = match native_get_log_manager(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("no LogManager"),
        };
        assert_npe(
            native_add_logger(&mut ctx, &[Value::Object(Some(mgr)), Value::Object(None)]),
            JUL_NPE_NULL_LOGGER,
            "LogManager.addLogger(null)",
        );
    }

    /// `isLoggable` is the level gate the supplier overloads share, so this
    /// single check is also what orders `log(null, supplier)` correctly: the
    /// level is reported and the supplier is never called.
    #[test]
    fn logger_is_loggable_null_level_throws() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = make_logger(&mut ctx, "axis.recv");
        assert_npe(
            native_jul_logger_is_loggable(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(None)],
            ),
            JUL_NPE_NULL_LEVEL,
            "Logger.isLoggable(null)",
        );
    }

    #[test]
    fn logger_log_record_null_throws() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = make_logger(&mut ctx, "axis.recv");
        assert_npe(
            native_jul_logger_log_record(
                &mut ctx,
                &[Value::Object(Some(logger)), Value::Object(None)],
            ),
            JUL_NPE_NULL_RECORD,
            "Logger.log((LogRecord) null)",
        );
    }

    /// Every `log`/`logp` overload that does NOT route through the shared gate
    /// restates the level check, so every one of them is asserted here. A
    /// missing restatement is invisible otherwise: the overload simply logs at
    /// the INFO default and returns.
    #[test]
    fn every_log_overload_refuses_a_null_level() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = make_logger(&mut ctx, "axis.recv");
        let msg = ctx.create_string("m");
        let recv = Value::Object(Some(logger));
        let text = Value::Object(Some(msg));
        let null = Value::Object(None);

        assert_npe(
            native_jul_logger_log_level_msg(&mut ctx, &[recv.clone(), null.clone(), text.clone()]),
            JUL_NPE_NULL_LEVEL,
            "log(null, String)",
        );
        assert_npe(
            native_jul_logger_log_param(
                &mut ctx,
                &[recv.clone(), null.clone(), text.clone(), null.clone()],
            ),
            JUL_NPE_NULL_LEVEL,
            "log(null, String, Object)",
        );
        assert_npe(
            native_jul_logger_log_params(
                &mut ctx,
                &[recv.clone(), null.clone(), text.clone(), null.clone()],
            ),
            JUL_NPE_NULL_LEVEL,
            "log(null, String, Object[])",
        );
        assert_npe(
            native_jul_logger_logp(
                &mut ctx,
                &[
                    recv.clone(),
                    null.clone(),
                    null.clone(),
                    null.clone(),
                    text.clone(),
                ],
            ),
            JUL_NPE_NULL_LEVEL,
            "logp(null, null, null, String)",
        );
        assert_npe(
            native_jul_logger_log_throwable(
                &mut ctx,
                &[recv.clone(), null.clone(), text.clone(), null.clone()],
            ),
            JUL_NPE_NULL_LEVEL,
            "log(null, String, Throwable)",
        );
        assert_npe(
            native_jul_logger_log_supplier(&mut ctx, &[recv, null.clone(), null]),
            JUL_NPE_NULL_LEVEL,
            "log(null, Supplier)",
        );
    }

    /// The half a blanket rule would break. `logp`'s SOURCE CLASS and SOURCE
    /// METHOD are nulls in the same call whose level null is fatal — measured,
    /// `logp(SEVERE, null, null, "msg")` returns on HotSpot. If someone
    /// "tidies" the level check into a loop over the reference arguments, this
    /// is the test that fails.
    #[test]
    fn logp_null_source_class_and_method_are_legal_and_must_not_throw() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = make_logger(&mut ctx, "axis.recv");
        let level = make_level(&mut ctx, "SEVERE", 1000);
        let msg = ctx.create_string("m");
        let got = native_jul_logger_logp(
            &mut ctx,
            &[
                Value::Object(Some(logger)),
                Value::Object(Some(level)),
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(msg)),
            ],
        );
        assert!(
            got.is_ok(),
            "a null source class/method pair is legal on HotSpot and must not throw: {got:?}"
        );
    }

    /// The other legal nulls in the same family, asserted together so a
    /// blanket rule cannot pass this module. MEASURED on HotSpot: all return.
    #[test]
    fn the_legal_nulls_in_the_log_family_must_not_throw() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        reset_state_for_tests();
        let mut ctx = mock_ctx();
        let logger = make_logger(&mut ctx, "axis.recv");
        let level = make_level(&mut ctx, "SEVERE", 1000);
        let recv = Value::Object(Some(logger));
        let lvl = Value::Object(Some(level));
        let null = Value::Object(None);

        // log(SEVERE, (String) null)
        let got =
            native_jul_logger_log_level_msg(&mut ctx, &[recv.clone(), lvl.clone(), null.clone()]);
        assert!(got.is_ok(), "log(level, null message) is legal: {got:?}");

        // log(SEVERE, "m", (Object[]) null)
        let msg = ctx.create_string("m");
        let got = native_jul_logger_log_params(
            &mut ctx,
            &[
                recv.clone(),
                lvl.clone(),
                Value::Object(Some(msg)),
                null.clone(),
            ],
        );
        assert!(got.is_ok(), "a null parameter array is legal: {got:?}");

        // log(SEVERE, (String) null, (Throwable) null) — TWO nulls in the same
        // two slots whose `(Level, Throwable, Supplier)` reading is fatal.
        let got = native_jul_logger_log_throwable(&mut ctx, &[recv, lvl, null.clone(), null]);
        assert!(
            got.is_ok(),
            "log(level, null message, null throwable) is legal and must not be \
             mistaken for a null Supplier: {got:?}"
        );
    }

    /// `jul_arg_is_null` is the whole discriminator; a primitive in the slot
    /// must not read as "null" (that would make a `(Z)V`-shaped sibling throw).
    #[test]
    fn jul_arg_is_null_distinguishes_absent_null_and_primitive() {
        assert!(jul_arg_is_null(&[], 0), "an absent slot reads as null");
        assert!(
            jul_arg_is_null(&[Value::Object(None)], 0),
            "an explicit null reference reads as null"
        );
        assert!(
            jul_arg_is_null(&[Value::Int(0)], 0),
            "a primitive is not a non-null REFERENCE"
        );
    }

    /// Build a `Logger`-shaped receiver. A `String` standing in for the
    /// receiver would be read at `LOGGER_FIELD_NAME` by half these natives,
    /// which is a field index a `String` does not have — the same raw-slot
    /// aliasing hazard `jul_logger_handlers_table`'s doc comment describes,
    /// reproduced in a test instead of in production.
    fn make_logger(ctx: &mut crate::test_utils::MockNativeContext, name: &str) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(CLS_JUL_LOGGER).unwrap();
        let obj = ctx.alloc_object(cid, LOGGER_NUM_FIELDS);
        let n = ctx.create_string(name);
        ctx.set_field(obj, LOGGER_FIELD_NAME, Value::Object(Some(n)));
        ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
        obj
    }

    /// Build a `Level`-shaped object the threshold walk can read.
    ///
    /// The `set_declared_fields` call is load-bearing and was MISSING. The
    /// mock resolves a field name through a per-class table plus whatever a
    /// test declared, and it has no entry for `java/util/logging/Level` — so
    /// the two `set_field_by_name` calls below silently wrote nowhere and the
    /// matching read answered `Value::Int(0)`, which
    /// `jul_requested_level_value`/`record_jul_logger_level` accept as a
    /// perfectly good level value of ZERO. Every level built here was
    /// therefore level 0 whatever the caller asked for; it went unnoticed
    /// because no test using this helper had ever asserted on the VALUE, only
    /// on "did not throw". Declaring the two fields makes the by-name path
    /// real, and slots 0/1 keep the synthetic-shape fallback agreeing.
    fn make_level(
        ctx: &mut crate::test_utils::MockNativeContext,
        name: &str,
        value: i32,
    ) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(CLS_JUL_LEVEL).unwrap();
        ctx.set_declared_fields(
            cid,
            vec![
                cratonvm_native_api::FieldMetadata {
                    name: "name".to_string(),
                    descriptor: "Ljava/lang/String;".to_string(),
                    access_flags: 0,
                    slot_index: 0,
                    declaring_class_id: cid,
                    is_static: false,
                },
                cratonvm_native_api::FieldMetadata {
                    name: "value".to_string(),
                    descriptor: "I".to_string(),
                    access_flags: 0,
                    slot_index: 1,
                    declaring_class_id: cid,
                    is_static: false,
                },
            ],
        );
        let obj = ctx.alloc_object(cid, 2);
        let n = ctx.create_string(name);
        ctx.set_field(obj, 0, Value::Object(Some(n)));
        ctx.set_field(obj, 1, Value::Int(value));
        ctx.set_field_by_name(obj, "name", Value::Object(Some(n)));
        ctx.set_field_by_name(obj, "value", Value::Int(value));
        obj
    }
}
