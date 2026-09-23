// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real-JDK bridge for the `jdk.jfr.internal.JVM` native surface.
//!
//! OpenJDK intentionally keeps the public `jdk.jfr` classes in Java and puts
//! the VM boundary in this class.  Leaving even `isAvailable()` unregistered
//! makes `JVMSupport` permanently disable the whole module, which in turn
//! prevents `RecordingStream` users (including Micrometer's virtual-thread
//! binder) from being constructed.  These entry points provide the lifecycle
//! and monotonic-clock contract required by the Java streaming implementation;
//! event payload collection remains owned by CratonVM's `jfr` crate.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{
    AnnotationElementValue, NativeContext, NativeHandle, NativeHandleScope, NativeKind,
    NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

// There used to be a `static RECORDING: AtomicBool` here, written by
// `beginRecording`, `endRecording` and `destroyJFR` and **read by nothing**.
// `isRecording()` and `Event.isEnabled()`/`shouldCommit()` all answer
// `ctx.jfr_java_recording_active()`, which is the VM's real state, so the static
// was a second source of truth that could only ever drift from it. Removed
// rather than extended when the `RecordingStream` transitions were added: a
// write-only mirror of a state that already has an authoritative reader is a
// latent disagreement, not bookkeeping.

/// Mirrors HotSpot's `JfrRecorder::is_created()`: set by `createJFR`, cleared
/// by `destroyJFR`. Both natives are specified in terms of it (see their
/// registrations), so it cannot be replaced by a constant.
static CREATED: AtomicBool = AtomicBool::new(false);
static CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();

/// First JFR type id this bridge hands out, and the reason it is not 1.
///
/// `jdk.jfr.internal.JVM.RESERVED_CLASS_ID_LIMIT` is 500, and the JDK treats
/// every id below it as "defined by the JVM" (`Type.isDefinedByJVM`). More
/// concretely, `jdk/jfr/internal/types/metadata.bin` bakes FIXED ids into the
/// ~200 built-in types the JDK loads at startup, and `TypeLibrary.types` is a
/// map KEYED BY ID. So a counter starting at 1 does not merely look odd — it
/// collides: `TypeLibrary.createType` looks the app's event class up by the id
/// `JVM.getTypeId` returned and finds the JDK's built-in type sitting there.
///
/// Measured on this VM before the fix (JDK 25, Temurin 25.0.4):
/// `FlightRecorder.register(ProbeEvent.class)` reported success and
/// `EventType.getEventType(ProbeEvent.class)` then answered
/// `jdk.MetaspaceAllocationFailure` (id 44) — a completely unrelated event
/// type — while a second registration answered `jdk.MetaspaceOOM` (id 45).
/// Starting above the reserved limit keeps this bridge's ids in the range the
/// JDK reserves for VM-assigned application class ids, which is where HotSpot
/// puts them too (its own answers for app event classes are in the hundreds to
/// low thousands).
const FIRST_JFR_TYPE_ID: i64 = 500;
static NEXT_TYPE_ID: AtomicI64 = AtomicI64::new(FIRST_JFR_TYPE_ID);

/// The tick rate `counterTime()` counts in. `counterTime()` returns
/// nanoseconds, so this is 1e9 — and `getTicksFrequency()` /
/// `getTimeConversionFactor()` are both DERIVED from it below rather than
/// written out as independent literals, because OpenJDK's
/// `JVMSupport.nanosToTicks` multiplies one by the other and any drift between
/// the three would silently rescale every JFR timestamp.
const TICKS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

fn type_ids() -> &'static Mutex<HashMap<String, i64>> {
    static TYPE_IDS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    TYPE_IDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn type_id(name: &str) -> i64 {
    let mut ids = type_ids()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(id) = ids.get(name) {
        return *id;
    }
    let id = NEXT_TYPE_ID.fetch_add(1, Ordering::Relaxed);
    ids.insert(name.to_owned(), id);
    id
}

fn canonical_jfr_type_name(name: &str) -> String {
    match name {
        "Z" => "boolean".to_owned(),
        "B" => "byte".to_owned(),
        "C" => "char".to_owned(),
        "S" => "short".to_owned(),
        "I" => "int".to_owned(),
        "J" => "long".to_owned(),
        "F" => "float".to_owned(),
        "D" => "double".to_owned(),
        "V" => "void".to_owned(),
        _ => name.replace('/', "."),
    }
}

fn class_name_for_mirror(
    ctx: &mut dyn NativeContext,
    mirror: cratonvm_types::ObjectRef,
) -> Option<String> {
    ctx.class_id_from_mirror(mirror)
        .and_then(|id| ctx.class_name_of_id(id))
        .or_else(|| {
            match crate::lang_class::native_class_get_name(ctx, &[Value::Object(Some(mirror))]) {
                Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
                _ => None,
            }
        })
        .map(|name| canonical_jfr_type_name(&name))
}

/// JFR event names keyed by the type id `JVM.getTypeId(Class)` handed out.
///
/// `Recording.enable(Class)` does NOT store the class name in the recording's
/// settings map — `Recording$RecordingSettings` stores
/// `String.valueOf(Type.getTypeId(eventClass))`, i.e. the decimal id that came
/// out of this bridge. So a settings key can read `545#enabled`, and the only
/// way back to `io.netty.AllocateChunk` is a table this side keeps as it hands
/// the ids out. Recorded here rather than derived later because the `Class`
/// mirror — the one thing that knows the `@Name` — is only in hand at this call.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of both acquisition
/// sites: `type_id_from_class_mirror` inserts and drops, and the settings
/// walk reads a name out with `.get(..).cloned()`. Neither touches `ctx` under
/// the guard, so it is never held across a re-entry into the VM.
fn type_id_event_names() -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<i64, String>>
{
    static NAMES: OnceLock<cratonvm_types::lock_order::OrderedMutex<HashMap<i64, String>>> =
        OnceLock::new();
    NAMES.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

fn type_id_from_class_mirror(ctx: &mut dyn NativeContext, args: &[Value]) -> i64 {
    let Some(Value::Object(Some(mirror))) = args.first().copied() else {
        return 0;
    };
    let Some(name) = class_name_for_mirror(ctx, mirror) else {
        return 0;
    };
    let id = type_id(&name);
    // Remember the JFR name this id stands for, so a settings key that names the
    // id can be resolved back. `jfr_event_name` is cached per class.
    if let Some(class_id) = ctx.class_id_from_mirror(mirror) {
        let event_name = jfr_event_name(ctx, class_id);
        type_id_event_names()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, event_name);
    }
    id
}

fn known_jfr_type_for_class_mirror(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<Value> {
    let name = match args.first() {
        Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
        _ => None,
    }?;
    let name = name
        .strip_prefix('[')
        .map(|component| canonical_jfr_type_name(component.trim_matches(&['L', ';'][..])))
        .unwrap_or(name);
    let field = match name.as_str() {
        "boolean" => "BOOLEAN",
        "char" => "CHAR",
        "float" => "FLOAT",
        "double" => "DOUBLE",
        "byte" => "BYTE",
        "short" => "SHORT",
        "int" => "INT",
        "long" => "LONG",
        "java.lang.Class" => "CLASS",
        "java.lang.String" => "STRING",
        "java.lang.Thread" => "THREAD",
        _ => return None,
    };
    let type_class = ctx.ensure_class_initialized("jdk/jfr/internal/Type").ok()?;
    let field_index = ctx.static_field_index_by_name(type_class, field)?;
    match ctx.get_static_field(type_class, field_index) {
        Value::Object(Some(value)) => Some(Value::Object(Some(value))),
        _ => None,
    }
}

/// Threads the caller has excluded from recording via `JVM.exclude(Thread)`.
///
/// Keyed by identity hash, never by `ObjectRef`: the key is stable across a
/// moving GC and the table holds no heap reference that would have to be
/// scanned or remapped.
fn excluded_threads() -> &'static Mutex<HashSet<i32>> {
    static EXCLUDED: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    EXCLUDED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn jfr_thread_key(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<i32> {
    match args.first() {
        Some(Value::Object(Some(thread))) => Some(ctx.identity_hash_code(*thread)),
        _ => None,
    }
}

/// `EventConfiguration` objects handed to `JVM.setConfiguration(Class, cfg)`,
/// keyed by the canonical event-class name.
///
/// The value is a GLOBAL ROOT handle, not a raw `ObjectRef`: the configuration
/// outlives the native call and must stay both reachable and correctly
/// forwarded across a moving GC (`add_global_root` / `resolve_global_root`).
fn event_configurations() -> &'static Mutex<HashMap<String, usize>> {
    static CONFIGS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CONFIGS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn dump_path() -> &'static Mutex<Option<String>> {
    static DUMP_PATH: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    DUMP_PATH.get_or_init(|| Mutex::new(None))
}

fn counter_time() -> i64 {
    CLOCK_ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

fn epoch_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}

/// Host RAM and swap totals, in bytes — the two numbers `JVM.hostTotalMemory()`
/// and `JVM.hostTotalSwapMemory()` are specified to return ("the total amount
/// of memory / swap memory of the host system whether or not this JVM runs in a
/// container", `jdk/jfr/internal/JVM.java`).
///
/// The host CAN answer both, so neither is a constant. Probed the same way the
/// rest of the tree already does it, with no new dependency:
///   * Windows — `GlobalMemoryStatusEx` via raw FFI (identical `MEMORYSTATUSEX`
///     layout to `jfr/src/builtin.rs`, `vm/src/runtime/crash_handler.rs` and
///     `vm-cli/src/main.rs`; `clashing_extern_declarations` is `deny` in the
///     workspace lints, so the layout must stay identical). `ullTotalPageFile`
///     is the system commit limit (physical + page file), so the page-file
///     portion — the thing that corresponds to swap — is the difference.
///   * Linux — `MemTotal:` / `SwapTotal:` (kB) from `/proc/meminfo`.
///   * Anything else / probe failure — `None`, and the callers fall back to
///     the JDK's `0` "unknown" sentinel.
fn host_memory_totals() -> Option<(i64, i64)> {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            dw_length: u32,
            dw_memory_load: u32,
            ull_total_phys: u64,
            ull_avail_phys: u64,
            ull_total_page_file: u64,
            ull_avail_page_file: u64,
            ull_total_virtual: u64,
            ull_avail_virtual: u64,
            ull_avail_extended_virtual: u64,
        }
        extern "system" {
            fn GlobalMemoryStatusEx(lp_buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
            dw_memory_load: 0,
            ull_total_phys: 0,
            ull_avail_phys: 0,
            ull_total_page_file: 0,
            ull_avail_page_file: 0,
            ull_total_virtual: 0,
            ull_avail_virtual: 0,
            ull_avail_extended_virtual: 0,
        };
        if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
            return None;
        }
        let total = status.ull_total_phys.min(i64::MAX as u64) as i64;
        let commit_limit = status.ull_total_page_file.min(i64::MAX as u64) as i64;
        return Some((total, commit_limit.saturating_sub(total).max(0)));
    }
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
        let mut total_kb: Option<u64> = None;
        let mut swap_kb: Option<u64> = None;
        for line in contents.lines() {
            // Lines look like: "MemTotal:       16331640 kB".
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kb = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
                swap_kb = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            }
            if total_kb.is_some() && swap_kb.is_some() {
                break;
            }
        }
        let total = total_kb?.saturating_mul(1024).min(i64::MAX as u64) as i64;
        // A kernel built without swap support omits SwapTotal entirely; that
        // really is zero swap, not an unknown.
        let swap = swap_kb
            .unwrap_or(0)
            .saturating_mul(1024)
            .min(i64::MAX as u64) as i64;
        return Some((total, swap));
    }
    #[allow(unreachable_code)]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// jdk.jfr.Event payload capture
// ---------------------------------------------------------------------------

/// The JVM field descriptors this bridge can carry into a JFR event.
///
/// Everything else — any reference type other than `String`, and every array —
/// is skipped, because CratonVM's chunk writer models exactly these and a
/// field it cannot describe in metadata must not appear in a payload either.
/// The most common casualty is a `Class`-typed field (netty's
/// `AbstractAllocatorEvent.allocatorType`); on HotSpot that is a constant-pool
/// reference, which needs a checkpoint event this writer does not emit.
const JFR_FIELD_DESCRIPTORS: [&str; 9] =
    ["Z", "B", "C", "S", "I", "J", "F", "D", "Ljava/lang/String;"];

/// Per-event-class JFR facts — `(@Name value, @Enabled default)` — keyed by the
/// event class's internal name.
///
/// Both are immutable class-file data, so one read per class is enough — and
/// `commit()` is hot enough that re-deriving them per event would be visible.
/// They share one row (and therefore one lock acquisition) because every caller
/// wants both: the name to record under, and whether the type is recorded at
/// all.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of both acquisition
/// sites in `event_type_facts`: the lookup's `if let` body is a bare `return`,
/// and the store is a temporary-guard `.insert(..)`. `ctx.class_annotations`
/// runs between them, after the read guard has been dropped.
fn event_types(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<String, (String, bool)>> {
    static TYPES: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<String, (String, bool)>>,
    > = OnceLock::new();
    TYPES.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The JFR name of an event class: its `@Name` value when it declares one,
/// else its binary class name.
///
/// This is what `EventType.getName()` answers on HotSpot and what
/// `RecordingStream.onEvent(String, …)` matches against, so it is the name the
/// recorder must store. `@Name` is deliberately read from the exact class and
/// not inherited — `jdk.jfr.Name` is not `@Inherited`, and netty relies on
/// that: `FreeBufferEvent` and `AllocateBufferEvent` share a superclass and
/// carry different names.
fn jfr_event_name(ctx: &mut dyn NativeContext, class_id: ClassId) -> String {
    jfr_event_type_facts(ctx, class_id).0
}

/// The JFR name of an event class and whether its TYPE is recorded by default.
///
/// **Name** — `@Name` when the class declares one, else the binary class name;
/// read from the exact class and deliberately not inherited (see
/// [`jfr_event_name`]).
///
/// **Default** — `jdk.jfr.Enabled` defaults to `true`, so a user event class
/// that says nothing is on, which is why a bare `new Recording()` records
/// custom events (see [`java_event_enabled`]). A class annotated
/// `@Enabled(false)` is the other half of that rule and was missing: the
/// default was hard-coded to `true`, so an explicitly-disabled type fired
/// anyway. netty's `JfrEventSafeTest.enableDefaults` asserts exactly that it
/// does not. Unlike `jdk.jfr.Name`, `jdk.jfr.Enabled` **is** `@Inherited`, so
/// this half of the walk continues into superclasses until the annotation is
/// found or the JFR base class is reached.
fn jfr_event_type_facts(ctx: &mut dyn NativeContext, class_id: ClassId) -> (String, bool) {
    let internal = ctx.class_name_of_id(class_id).unwrap_or_default();
    if let Some(cached) = event_types()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&internal)
    {
        return cached.clone();
    }
    let mut name = internal.replace('/', ".");
    for annotation in ctx.class_annotations(class_id) {
        if annotation.type_descriptor != "Ljdk/jfr/Name;" {
            continue;
        }
        for (element, value) in &annotation.elements {
            if element == "value" {
                if let AnnotationElementValue::StringVal(text) = value {
                    if !text.is_empty() {
                        name = text.clone();
                    }
                }
            }
        }
    }
    let mut enabled = true;
    let mut current = Some(class_id);
    'walk: while let Some(cid) = current {
        let cname = ctx.class_name_of_id(cid).unwrap_or_default();
        if cname == "jdk/jfr/Event"
            || cname == "jdk/internal/event/Event"
            || cname == "java/lang/Object"
        {
            break;
        }
        for annotation in ctx.class_annotations(cid) {
            if annotation.type_descriptor != "Ljdk/jfr/Enabled;" {
                continue;
            }
            // A boolean element arrives as `Int`; `@Enabled` with no element
            // at all is the annotation's own default, which is `true`.
            enabled = annotation
                .elements
                .iter()
                .find(|(element, _)| element == "value")
                .map_or(true, |(_, value)| match value {
                    AnnotationElementValue::Int(flag) => *flag != 0,
                    _ => true,
                });
            break 'walk;
        }
        current = ctx.superclass_of(cid);
    }
    let facts = (name, enabled);
    event_types()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .insert(internal, facts.clone());
    facts
}

/// The event's field values as `(name, descriptor, value)`, superclass fields
/// first — the order JFR itself declares inherited fields in.
///
/// Static fields are skipped: netty's event classes each hold a `private static
/// final INSTANCE` of their own type purely so `isEventEnabled()` has a
/// receiver, and treating that as a payload field would put the event class
/// inside its own event.
///
/// A field name declared twice in the chain (Java shadowing) is kept only at
/// its most-derived declaration, because the recorder rejects an event type
/// with a duplicate field name outright — which would silently drop the whole
/// event rather than one field.
fn capture_event_fields(
    ctx: &mut dyn NativeContext,
    event: ObjectRef,
) -> Vec<(String, String, Value)> {
    let mut current = Some(ctx.class_id_of_object(event));
    // `startTime` and `duration` are JFR's own implicit fields: every event type
    // declares them first and positionally, and consumers reach them by those
    // names. A payload field with either name would be shadowed by the implicit
    // one at every consumer, so it is dropped here rather than written into a
    // record where it can never be read back.
    let mut seen: HashSet<String> = ["startTime", "duration"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    // Collected most-derived first so shadowing resolves the Java way, then
    // reversed for JFR's superclass-first declaration order.
    let mut derived_first: Vec<(String, String, Value)> = Vec::new();
    while let Some(class_id) = current {
        let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
        // `jdk.jfr.Event` declares no fields of its own; `jdk.internal.event
        // .Event` is the base the JDK's own `Utils.isEventBaseClass` stops at.
        if class_name == "jdk/jfr/Event"
            || class_name == "jdk/internal/event/Event"
            || class_name == "java/lang/Object"
        {
            break;
        }
        for field in ctx.declared_fields(class_id) {
            if field.is_static || !JFR_FIELD_DESCRIPTORS.contains(&field.descriptor.as_str()) {
                continue;
            }
            if !seen.insert(field.name.clone()) {
                continue;
            }
            let value = ctx.get_field(event, field.slot_index);
            derived_first.push((field.name, field.descriptor, value));
        }
        current = ctx.superclass_of(class_id);
    }
    derived_first.reverse();
    derived_first
}

/// `begin()`/`end()` timestamps for the event being built on each thread.
///
/// HotSpot keeps `startTime`/`duration` in fields that `EventInstrumentation`
/// weaves into the event class. CratonVM does not instrument event classes
/// (`retransformClasses` above is a no-op and `isInstrumented` reports `false`
/// to match), so there is no field to put them in. JFR's contract is that
/// `begin()`, `end()` and `commit()` for one event run on one thread, which is
/// what makes a single slot per thread sufficient — and why nesting two
/// in-flight events on one thread would attribute the inner one's timing to
/// whichever committed first.
///
/// Keyed by `(vm_identity, thread id)`: Rust tests build several independent
/// `Vm`s in one process and thread ids restart per VM.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of all three
/// acquisition sites (`Event.begin`, `Event.end`, `commit`): each computes its
/// `(vm, thread)` key BEFORE taking the guard, then does one `insert`,
/// `get_mut` or `remove` under it. No `ctx` call happens under the guard.
fn event_timing(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<(usize, u64), (u64, Option<u64>)>> {
    static TIMING: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<(usize, u64), (u64, Option<u64>)>>,
    > = OnceLock::new();
    TIMING.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

// ---------------------------------------------------------------------------
// jdk.jfr.Recording settings
// ---------------------------------------------------------------------------

/// One open `jdk.jfr.Recording` the Java boundary started, and the settings read
/// off it.
///
/// The `Recording` is held as a GLOBAL ROOT so its settings map can be re-read
/// after `start()` — a bare `ObjectRef` would not survive the GCs in between.
struct JavaRecording {
    /// `NativeContext::vm_identity` of the owning VM; the table is
    /// process-global while the root is per-VM.
    vm: usize,
    recording: usize,
    /// JFR event names this recording set `#enabled=true` for.
    enabled: HashSet<String>,
    /// JFR event names this recording set `#enabled=false` for. Kept separately
    /// from "absent from `enabled`", because absent and explicitly-off mean
    /// different things — see [`java_event_enabled`].
    disabled: HashSet<String>,
    /// `#threshold` per event name, in nanoseconds.
    thresholds: HashMap<String, u64>,
}

/// ARCH-2026-08-04 A6 — deliberately NOT given a `LockLevel`.
///
/// `refresh_java_recording_settings` and `forget_java_recording` both hold this
/// guard across `ctx.resolve_global_root(..)` inside an `iter().position(..)`
/// predicate — a re-entry into the VM under the guard, which is exactly the
/// shape a level is supposed to forbid. The `ctx` call is per-element, so it
/// cannot simply be hoisted the way the sites in this file's other tables were;
/// closing it needs the lookup restructured. Left in the §A6 backlog rather
/// than stamped with a level nobody can honour.
fn java_recordings() -> &'static Mutex<Vec<JavaRecording>> {
    static RECORDINGS: OnceLock<Mutex<Vec<JavaRecording>>> = OnceLock::new();
    RECORDINGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Per-VM set of event names a settings re-read has already failed to admit.
///
/// Without this memo, every commit of an explicitly-disabled event would trigger
/// a full Java-side `getSettings()` re-read. Cleared by
/// [`publish_java_recording_settings`], i.e. by every start, stop and re-read,
/// so a later `enable(...)` is still picked up.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of all three
/// acquisition sites: `publish_java_recording_settings` removes, the fast
/// refusal check's `if let` body is a bare `return false`, and the memo store
/// is `entry(..).or_default().insert(..)`. All temporary guards, no `ctx`.
///
/// It IS acquired while `java_recordings`' guard is held, which is legal only
/// because `java_recordings` is deliberately NOT ordered — see its own comment.
fn java_events_known_disabled(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HashSet<String>>> {
    static DENIED: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HashSet<String>>>,
    > = OnceLock::new();
    DENIED.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Per-VM set of Java event names this bridge has decided to record.
///
/// This is what makes the VM-side allow-list work without the recorder needing
/// to know which of its event types came from Java. The allow-list it publishes
/// is `explicitly enabled ∪ admitted`, so:
///
///   * a Java event class is recorded because `java_event_enabled` admitted it
///     and added it here, and
///   * CratonVM's own built-in `jdk.*` events are NOT, unless a recording names
///     one explicitly.
///
/// That asymmetry is HotSpot's, measured on JDK 25 rather than assumed: a
/// `new Recording()` with no `enable(...)` call records both of a probe's two
/// custom events and none of the JDK's own, because `jdk.jfr.Enabled` defaults
/// to `true` for a user event class while the JDK's metadata ships most `jdk.*`
/// types with `enabled=false`.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), on a reading of all three
/// acquisition sites: a `remove`, a `for name in ..get(..).into_iter().flatten()`
/// whose body only pushes into a local `Vec`, and an `entry(..).or_default()
/// .insert(..)` whose `publish_java_recording_settings(ctx)` follows AFTER the
/// statement — and therefore after the guard — has ended.
///
/// Same nesting note as [`java_events_known_disabled`]: taken under
/// `java_recordings`' unordered guard, which is why that one stays unordered.
fn java_events_admitted(
) -> &'static cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HashSet<String>>> {
    static ADMITTED: OnceLock<
        cratonvm_types::lock_order::OrderedMutex<HashMap<usize, HashSet<String>>>,
    > = OnceLock::new();
    ADMITTED.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Parse a JFR timespan setting value into nanoseconds.
///
/// The spelling is `<number> <unit>` with unit in `ns`/`us`/`ms`/`s`/`m`/`h`/`d`
/// (`jdk.jfr.internal.util.ValueParser`); `withoutThreshold()` writes `0 ns`. A
/// bare number is nanoseconds, which is also what the JDK's parser accepts.
/// Anything unparseable answers `None` and the setting is ignored rather than
/// guessed at — a wrong threshold silently drops events.
fn parse_jfr_timespan_nanos(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("infinity") {
        return Some(u64::MAX);
    }
    let split = value
        .find(|c: char| !c.is_ascii_digit() && c != '-' && c != '+')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let amount: u64 = number.trim().parse().ok()?;
    let multiplier = match unit.trim() {
        "" | "ns" => 1u64,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 3_600 * 1_000_000_000,
        "d" => 86_400 * 1_000_000_000,
        _ => return None,
    };
    Some(amount.saturating_mul(multiplier))
}

/// What one `jdk.jfr.Recording`'s settings map says.
type ReadSettings = (HashSet<String>, HashSet<String>, HashMap<String, u64>);

/// Read `recording.getSettings()` and translate it into `(enabled, disabled,
/// thresholds)`.
///
/// Settings keys are `<identifier>#<setting>`, where the identifier is either an
/// event NAME (from `enable(String)`) or the decimal type id this bridge handed
/// out (from `enable(Class)` — see [`type_id_event_names`]).
///
/// Returns `None` when the map cannot be read at all, which the caller must
/// treat as "no filter": guessing an empty set there would silently turn every
/// recording into a recording of nothing.
fn read_java_recording_settings(
    ctx: &mut dyn NativeContext,
    recording: ObjectRef,
) -> Option<ReadSettings> {
    let mut scope = NativeHandleScope::new(ctx);
    let recording = scope.root(recording);
    let settings = {
        let receiver = scope.get(&recording);
        match scope.invoke_virtual(receiver, "getSettings", "()Ljava/util/Map;", &[]) {
            Ok(Some(Value::Object(Some(map)))) => map,
            _ => return None,
        }
    };
    let settings = scope.root(settings);
    // `keySet().toArray()` rather than an entry-set iterator: two virtual calls
    // and then pure array reads, instead of one call per entry per step.
    let keys = {
        let map = scope.get(&settings);
        match scope.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[]) {
            Ok(Some(Value::Object(Some(set)))) => {
                let set = scope.root(set);
                let receiver = scope.get(&set);
                match scope.invoke_virtual(receiver, "toArray", "()[Ljava/lang/Object;", &[]) {
                    Ok(Some(Value::Object(Some(array)))) => array,
                    _ => return None,
                }
            }
            _ => return None,
        }
    };
    let keys = scope.root(keys);

    let mut enabled: HashSet<String> = HashSet::new();
    let mut disabled: HashSet<String> = HashSet::new();
    let mut thresholds: HashMap<String, u64> = HashMap::new();
    let length = {
        let array = scope.get(&keys);
        scope.array_length(array)
    };
    for index in 0..length {
        let key = {
            let array = scope.get(&keys);
            match scope.get_array_element(array, index) {
                Value::Object(Some(key)) => key,
                _ => continue,
            }
        };
        let key = scope.root(key);
        let Some(text) = ({
            let key = scope.get(&key);
            scope.read_string(key)
        }) else {
            continue;
        };
        // `#` separates the identifier from the setting name, and an event name
        // cannot contain one, so splitting at the LAST `#` is unambiguous.
        let Some((identifier, setting)) = text.rsplit_once('#') else {
            continue;
        };
        if setting != "enabled" && setting != "threshold" {
            // `stackTrace`, `period`, `cutoff`, `throttle`, `level` and the
            // per-event control classes have no counterpart in the Rust
            // recorder. Skipping them is visible in the code rather than
            // implied by a match arm that silently accepts everything.
            continue;
        }
        let value = {
            let map = scope.get(&settings);
            let key = scope.get(&key);
            match scope.invoke_virtual(
                map,
                "get",
                "(Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(key))],
            ) {
                Ok(Some(Value::Object(Some(value)))) => scope.read_string(value),
                _ => None,
            }
        };
        let Some(value) = value else {
            continue;
        };
        let name = match identifier.parse::<i64>() {
            Ok(id) => match type_id_event_names()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&id)
                .cloned()
            {
                Some(name) => name,
                // An id this bridge never handed out cannot be resolved to a
                // name; there is nothing honest to do with it.
                None => continue,
            },
            Err(_) => identifier.to_owned(),
        };
        match setting {
            "enabled" => {
                // A map holds one value per key, so `enable(X)` followed by
                // `disable(X)` leaves only the last one — no ordering to track.
                if value.trim().eq_ignore_ascii_case("true") {
                    disabled.remove(&name);
                    enabled.insert(name);
                } else {
                    enabled.remove(&name);
                    disabled.insert(name);
                }
            }
            "threshold" => {
                if let Some(nanos) = parse_jfr_timespan_nanos(&value) {
                    if nanos == 0 {
                        thresholds.remove(&name);
                    } else {
                        thresholds.insert(name, nanos);
                    }
                }
            }
            _ => {}
        }
    }
    Some((enabled, disabled, thresholds))
}

/// Re-read `recording`'s settings and push the union across every open Java
/// recording into the VM recorder.
///
/// The union is the right answer and matches HotSpot: an event type is recorded
/// if *any* running recording enables it, and CratonVM's Java boundary drives
/// one VM-side recording shared by every open `Recording`/`RecordingStream`.
fn refresh_java_recording_settings(ctx: &mut dyn NativeContext, recording: ObjectRef) {
    let vm = ctx.vm_identity();
    let read = read_java_recording_settings(ctx, recording);
    {
        let mut table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let slot = table.iter().position(|entry| {
            entry.vm == vm && ctx.resolve_global_root(entry.recording) == Some(recording)
        });
        match (slot, read) {
            (Some(slot), Some((enabled, disabled, thresholds))) => {
                table[slot].enabled = enabled;
                table[slot].disabled = disabled;
                table[slot].thresholds = thresholds;
            }
            (Some(slot), None) => {
                // The map could not be read. Drop the entry rather than leave a
                // stale filter in place; with no entry the recording falls back
                // to "record everything".
                let entry = table.remove(slot);
                ctx.remove_global_root(entry.recording);
            }
            (None, Some((enabled, disabled, thresholds))) => {
                let root = ctx.add_global_root(recording);
                table.push(JavaRecording {
                    vm,
                    recording: root,
                    enabled,
                    disabled,
                    thresholds,
                });
            }
            (None, None) => {}
        }
    }
    publish_java_recording_settings(ctx);
}

/// Forget `recording`'s settings — it stopped or closed.
fn forget_java_recording_settings(ctx: &mut dyn NativeContext, recording: ObjectRef) {
    let vm = ctx.vm_identity();
    let removed = {
        let mut table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        table
            .iter()
            .position(|entry| {
                entry.vm == vm && ctx.resolve_global_root(entry.recording) == Some(recording)
            })
            .map(|slot| table.remove(slot))
    };
    if let Some(entry) = removed {
        ctx.remove_global_root(entry.recording);
    }
    publish_java_recording_settings(ctx);
}

/// Hand this VM's effective allow-list and thresholds to the VM recorder.
///
/// The allow-list is `explicitly enabled ∪ admitted` (see
/// [`java_events_admitted`]) — which is what keeps CratonVM's own `jdk.*`
/// built-ins out of a Java recording that never asked for them, while letting
/// every Java event class in by default.
fn publish_java_recording_settings(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    // Any change to the settings invalidates the "already refused" memo.
    java_events_known_disabled()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&vm);
    let (enabled, thresholds) = {
        let table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mine: Vec<&JavaRecording> = table.iter().filter(|entry| entry.vm == vm).collect();
        if mine.is_empty() {
            // No Java recording is open: no name filter at all, which is what
            // every CratonVM-internal recording needs. Nothing can be admitted
            // against a recording that does not exist, so drop that set too.
            java_events_admitted()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&vm);
            (None, Vec::new())
        } else {
            let mut enabled: Vec<String> = Vec::new();
            let mut thresholds: HashMap<String, u64> = HashMap::new();
            for entry in mine {
                for name in &entry.enabled {
                    if !enabled.iter().any(|existing| existing == name) {
                        enabled.push(name.clone());
                    }
                }
                for (name, nanos) in &entry.thresholds {
                    // The most permissive threshold wins, so one recording's
                    // narrow filter cannot suppress another's events.
                    thresholds
                        .entry(name.clone())
                        .and_modify(|existing| *existing = (*existing).min(*nanos))
                        .or_insert(*nanos);
                }
            }
            for name in java_events_admitted()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&vm)
                .into_iter()
                .flatten()
            {
                if !enabled.iter().any(|existing| existing == name) {
                    enabled.push(name.clone());
                }
            }
            let thresholds: Vec<(String, u64)> = thresholds.into_iter().collect();
            (Some(enabled), thresholds)
        }
    };
    ctx.jfr_configure_java_recording(enabled.as_deref(), &thresholds);
}

/// Whether a Java event class named `event_name` should be recorded, given the
/// settings of every open Java recording in this VM.
///
/// The rule, measured against HotSpot JDK 25 rather than assumed — a probe with
/// two custom events and a bare `new Recording()` records BOTH there:
///
///   * explicitly `enable`d by any recording → yes;
///   * otherwise explicitly `disable`d → no;
///   * otherwise the TYPE's own default, `type_default_enabled` — which is
///     `true` for a user event class that says nothing, because
///     `jdk.jfr.Enabled` defaults to `true`, and `false` for one annotated
///     `@Enabled(false)`. An earlier version of this function defaulted to
///     *no*, which made a `new Recording()` record nothing and disagreed with
///     HotSpot on three of the probe's rows; the version after that hard-coded
///     *yes*, which fired `@Enabled(false)` events. See
///     [`jfr_event_type_facts`].
///
/// A `true` answer also ADMITS the name (see [`java_events_admitted`]), which is
/// what lets the VM-side allow-list keep CratonVM's built-in events out while
/// letting Java's in.
///
/// With no Java recording open at all the answer is the type's own default and
/// nothing is admitted: the recorder then has no name filter, which is what
/// CratonVM's own recordings need.
fn java_event_enabled(
    ctx: &mut dyn NativeContext,
    event_name: &str,
    type_default_enabled: bool,
) -> bool {
    let vm = ctx.vm_identity();

    // `admits` is the per-recording answer OR-ed together, which is HotSpot's
    // rule: a type is recorded if any running recording would record it. A
    // recording that does not mention the name applies the TYPE's default —
    // `true` — so it admits; only a recording that explicitly disables the name
    // refuses. Therefore "disabled" requires *every* open recording to have said
    // so, which is why this cannot be a "first opinion wins" scan.
    let (any_recording, admits) = {
        let table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut any_recording = false;
        let mut admits = false;
        for entry in table.iter().filter(|entry| entry.vm == vm) {
            any_recording = true;
            if entry.enabled.contains(event_name)
                || (type_default_enabled && !entry.disabled.contains(event_name))
            {
                admits = true;
                break;
            }
        }
        (any_recording, admits)
    };
    if !any_recording {
        return type_default_enabled;
    }
    if admits {
        admit_java_event(ctx, vm, event_name);
        return true;
    }

    if java_events_known_disabled()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&vm)
        .is_some_and(|names| names.contains(event_name))
    {
        return false;
    }
    // First refusal of this name. A `Recording.enable(...)` AFTER `start()` is
    // legal, and its only funnel is a private `Recording.setSetting` this bridge
    // does not intercept, so re-read every open recording once before saying no.
    let roots: Vec<usize> = {
        let table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        table
            .iter()
            .filter(|entry| entry.vm == vm)
            .map(|entry| entry.recording)
            .collect()
    };
    for root in roots {
        if let Some(recording) = ctx.resolve_global_root(root) {
            refresh_java_recording_settings(ctx, recording);
        }
    }
    let still_disabled = {
        let table = java_recordings()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut any_recording = false;
        let mut admits = false;
        for entry in table.iter().filter(|entry| entry.vm == vm) {
            any_recording = true;
            if entry.enabled.contains(event_name)
                || (type_default_enabled && !entry.disabled.contains(event_name))
            {
                admits = true;
                break;
            }
        }
        any_recording && !admits
    };
    if still_disabled {
        java_events_known_disabled()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(vm)
            .or_default()
            .insert(event_name.to_owned());
        return false;
    }
    admit_java_event(ctx, vm, event_name);
    true
}

/// Record that `event_name` is being recorded, republishing the allow-list the
/// first time a name is added.
fn admit_java_event(ctx: &mut dyn NativeContext, vm: usize, event_name: &str) {
    let newly_admitted = java_events_admitted()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(vm)
        .or_default()
        .insert(event_name.to_owned());
    if newly_admitted {
        publish_java_recording_settings(ctx);
    }
}

// ---------------------------------------------------------------------------
// RecordingStream event delivery
// ---------------------------------------------------------------------------

/// One live `jdk.jfr.consumer.RecordingStream` and the consumers it registered.
///
/// Both the stream and every `Consumer` are held as GLOBAL ROOTS
/// (`add_global_root`), never as bare `ObjectRef`s. A consumer is parked here
/// from `onEvent(...)` until `close()` — arbitrarily many GCs later — and a
/// moving collector relocates it in between; an address-keyed side table is the
/// failure mode that has bitten several overlay tables in this VM.
///
/// The stream itself is rooted for identity rather than for use: `close()` has
/// to find the entry that belongs to the closing stream, and comparing
/// freshly-resolved roots is exact where an identity hash would only be
/// probably-unique among live objects.
struct JavaEventStream {
    /// `NativeContext::vm_identity` of the VM that owns these roots. The table
    /// is process-global while the roots are per-VM, so every read filters on
    /// this — a global-root handle from another `Vm` in the same process would
    /// otherwise resolve to that VM's heap.
    vm: usize,
    stream: usize,
    started: bool,
    /// `(event-name filter, Consumer global root)`. A `None` filter is
    /// `onEvent(Consumer)`, which subscribes to every event.
    subscriptions: Vec<(Option<String>, usize)>,
}

/// ARCH-2026-08-04 A6 — deliberately NOT given a `LockLevel`, for the same
/// reason as [`java_recordings`]: `java_stream_slot_or_insert` and the
/// `close()` bridge both pass `ctx` into `java_stream_slot` while holding this
/// guard, and that helper calls `ctx.resolve_global_root` once per entry.
fn java_event_streams() -> &'static Mutex<Vec<JavaEventStream>> {
    static STREAMS: OnceLock<Mutex<Vec<JavaEventStream>>> = OnceLock::new();
    STREAMS.get_or_init(|| Mutex::new(Vec::new()))
}

thread_local! {
    /// Set while this thread is inside a consumer callback.
    ///
    /// Delivery is synchronous on the committing thread (see
    /// `dispatch_to_java_streams`), so a consumer that itself commits an event
    /// would recurse without bound. The inner commit is still recorded into the
    /// repository; only its delivery is suppressed.
    static DISPATCHING: Cell<bool> = const { Cell::new(false) };
}

/// Index of `stream`'s entry in the table, resolving each stored root so the
/// comparison is between two current addresses.
fn java_stream_slot(
    ctx: &dyn NativeContext,
    table: &[JavaEventStream],
    stream: ObjectRef,
) -> Option<usize> {
    let vm = ctx.vm_identity();
    table
        .iter()
        .position(|entry| entry.vm == vm && ctx.resolve_global_root(entry.stream) == Some(stream))
}

/// Find `stream`'s entry, creating it if this is the first call for it.
fn java_stream_slot_or_insert(ctx: &mut dyn NativeContext, stream: ObjectRef) -> usize {
    let existing = {
        let table = java_event_streams()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        java_stream_slot(ctx, &table, stream)
    };
    if let Some(index) = existing {
        return index;
    }
    let vm = ctx.vm_identity();
    let root = ctx.add_global_root(stream);
    let mut table = java_event_streams()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    table.push(JavaEventStream {
        vm,
        stream: root,
        started: false,
        subscriptions: Vec::new(),
    });
    table.len() - 1
}

/// Build the `jdk.jfr.ValueDescriptor` for one field.
///
/// `ValueDescriptor(Class, String)` is public API and routes through
/// `Utils.getValidType`, which this file also implements — so the accepted set
/// here is exactly [`JFR_FIELD_DESCRIPTORS`] by construction.
fn value_descriptor(
    scope: &mut NativeHandleScope<'_>,
    descriptor: &str,
    name: &str,
) -> Option<ObjectRef> {
    let mirror = match descriptor {
        "Z" => scope.primitive_class_mirror("boolean"),
        "B" => scope.primitive_class_mirror("byte"),
        "C" => scope.primitive_class_mirror("char"),
        "S" => scope.primitive_class_mirror("short"),
        "I" => scope.primitive_class_mirror("int"),
        "J" => scope.primitive_class_mirror("long"),
        "F" => scope.primitive_class_mirror("float"),
        "D" => scope.primitive_class_mirror("double"),
        "Ljava/lang/String;" => {
            let class_id = scope.ensure_class_initialized("java/lang/String").ok()?;
            scope.get_class_mirror(class_id)
        }
        _ => return None,
    };
    let mirror = scope.root(mirror);
    let text = scope.create_string(name);
    let text = scope.root(text);
    let (mirror, text) = (scope.get(&mirror), scope.get(&text));
    match scope.new_object_initialized(
        "jdk/jfr/ValueDescriptor",
        "(Ljava/lang/Class;Ljava/lang/String;)V",
        &[Value::Object(Some(mirror)), Value::Object(Some(text))],
    ) {
        Ok(Some(Value::Object(Some(vd)))) => Some(vd),
        _ => None,
    }
}

/// Build the `TimeConverter` a `RecordedEvent` needs to answer
/// `getStartTime()`/`getEndTime()`/`getDuration()`.
///
/// Its only constructor takes a `ChunkHeader`, which only exists while parsing
/// a file, so the object is allocated without running `<init>` and its four
/// fields are set directly. With `startTicks = 0`, `startNanos = 0` and
/// `divisor = 1.0`, `convertTimestamp(t)` is the identity — which is what makes
/// it correct to hand this converter absolute epoch nanoseconds as "ticks".
///
/// Returns `None` when the field layout is not what we expect, rather than
/// guessing: a missing `divisor` would leave it 0.0 and turn every timestamp
/// into infinity. The caller then builds a converter-less event whose field
/// accessors still work.
fn identity_time_converter(scope: &mut NativeHandleScope<'_>) -> Option<ObjectRef> {
    let utc = {
        let class_id = scope
            .ensure_class_initialized("java/time/ZoneOffset")
            .ok()?;
        let index = scope.static_field_index_by_name(class_id, "UTC")?;
        match scope.get_static_field(class_id, index) {
            Value::Object(Some(utc)) => utc,
            _ => return None,
        }
    };
    let utc = scope.root(utc);
    let converter = match scope.new_object("jdk/jfr/internal/consumer/TimeConverter") {
        Ok(Some(Value::Object(Some(converter)))) => converter,
        _ => return None,
    };
    let converter = scope.root(converter);
    let (converter, utc) = (scope.get(&converter), scope.get(&utc));
    scope.set_field_by_name(converter, "startTicks", Value::Long(0));
    scope.set_field_by_name(converter, "startNanos", Value::Long(0));
    scope.set_field_by_name(converter, "divisor", Value::Double(1.0));
    scope.set_field_by_name(converter, "zoneOffset", Value::Object(Some(utc)));
    // Read the one field whose absence would be silent and catastrophic.
    match scope.get_field_by_name(converter, "divisor") {
        Value::Double(1.0) => Some(converter),
        other => {
            tracing::warn!(
                ?other,
                "jdk.jfr.internal.consumer.TimeConverter has an unexpected field layout; \
                 RecordedEvent timestamps will be unavailable while field access still works"
            );
            None
        }
    }
}

/// Materialise a `jdk.jfr.consumer.RecordedEvent` for a committed event.
///
/// Every object here is built through a real constructor — `ValueDescriptor`,
/// `PlatformEventType`, `EventType`, `ObjectContext` and `RecordedEvent` all
/// have one this boundary can reach — so the only field-level poking is
/// [`identity_time_converter`]'s.
///
/// The descriptor list is `[startTime, duration] ++ fields` and the value array
/// is `fields` alone. That is not a mismatch: `RecordedEvent.objectAt` returns
/// the tick fields for indices 0 and 1 and shifts the rest, and it decides
/// whether to shift by 1 or 2 from `objects.length + 2 == fields.size()` — so
/// declaring both implicit fields is what makes `getInt("capacity")` land on
/// the right slot.
fn recorded_event_for(
    ctx: &mut dyn NativeContext,
    event_name: &str,
    fields: &[(String, String, Value)],
    start_ns: u64,
    end_ns: u64,
) -> Option<ObjectRef> {
    let mut scope = NativeHandleScope::new(ctx);

    // Root every reference-valued field FIRST. Everything below allocates, and
    // a `String` field's `ObjectRef` was copied out of the heap before this
    // function ran — a moving collector would leave that copy dangling.
    let mut roots: Vec<Option<NativeHandle>> = Vec::with_capacity(fields.len());
    for (_, _, value) in fields {
        roots.push(match value {
            Value::Object(Some(reference)) => Some(scope.root(*reference)),
            _ => None,
        });
    }

    let mut boxed: Vec<Option<NativeHandle>> = Vec::with_capacity(fields.len());
    for (index, (_, descriptor, value)) in fields.iter().enumerate() {
        let object = match &roots[index] {
            // A reference field is already an object; only primitives need a
            // wrapper, and `RecordedObject.getInt`/`getBoolean` require the
            // boxed form.
            Some(handle) => Some(scope.get(handle)),
            None => match crate::lang_class::box_value(&mut *scope, *value, descriptor) {
                Value::Object(Some(wrapper)) => Some(wrapper),
                _ => None,
            },
        };
        boxed.push(object.map(|object| scope.root(object)));
    }

    let values = scope.new_array(ArrayElementType::Reference, fields.len());
    let values = scope.root(values);
    for (index, handle) in boxed.iter().enumerate() {
        let element = match handle {
            Some(handle) => Value::Object(Some(scope.get(handle))),
            None => Value::Object(None),
        };
        let array = scope.get(&values);
        scope.set_array_element(array, index, element);
    }

    let descriptors = match scope.new_object_initialized("java/util/ArrayList", "()V", &[]) {
        Ok(Some(Value::Object(Some(list)))) => list,
        _ => return None,
    };
    let descriptors = scope.root(descriptors);
    let implicit = [("startTime", "J"), ("duration", "J")];
    for (name, descriptor) in implicit
        .iter()
        .map(|(name, descriptor)| ((*name).to_owned(), (*descriptor).to_owned()))
        .chain(
            fields
                .iter()
                .map(|(name, descriptor, _)| (name.clone(), descriptor.clone())),
        )
    {
        let value_descriptor = value_descriptor(&mut scope, &descriptor, &name)?;
        let value_descriptor = scope.root(value_descriptor);
        let (list, value_descriptor) = (scope.get(&descriptors), scope.get(&value_descriptor));
        scope
            .invoke_virtual(
                list,
                "add",
                "(Ljava/lang/Object;)Z",
                &[Value::Object(Some(value_descriptor))],
            )
            .ok()?;
    }

    let converter = identity_time_converter(&mut scope).map(|converter| scope.root(converter));

    let name = scope.create_string(event_name);
    let name = scope.root(name);
    let name = scope.get(&name);
    let platform_type = match scope.new_object_initialized(
        "jdk/jfr/internal/PlatformEventType",
        "(Ljava/lang/String;JZZ)V",
        &[
            Value::Object(Some(name)),
            Value::Long(type_id(event_name)),
            // (isJDK, dynamicSettings) — neither is true of an application
            // event class, and `MetadataReader` passes the same pair when it
            // rebuilds an event type out of a parsed chunk.
            Value::Int(0),
            Value::Int(0),
        ],
    ) {
        Ok(Some(Value::Object(Some(platform_type)))) => platform_type,
        // A `@Name` the JDK rejects as a class name reaches `Type`'s
        // constructor as an `InternalError`. Keep delivering the event with no
        // event type rather than dropping it.
        _ => {
            return recorded_event_without_type(&mut scope, &descriptors, &values, start_ns, end_ns)
        }
    };
    let platform_type = scope.root(platform_type);
    let platform_type = scope.get(&platform_type);
    let event_type = match scope.new_object_initialized(
        "jdk/jfr/EventType",
        "(Ljdk/jfr/internal/PlatformEventType;)V",
        &[Value::Object(Some(platform_type))],
    ) {
        Ok(Some(Value::Object(Some(event_type)))) => event_type,
        _ => {
            return recorded_event_without_type(&mut scope, &descriptors, &values, start_ns, end_ns)
        }
    };
    let event_type = scope.root(event_type);

    let (event_type, list) = (scope.get(&event_type), scope.get(&descriptors));
    let converter_value = match &converter {
        Some(handle) => Value::Object(Some(scope.get(handle))),
        None => Value::Object(None),
    };
    let context = match scope.new_object_initialized(
        "jdk/jfr/internal/consumer/ObjectContext",
        "(Ljdk/jfr/EventType;Ljava/util/List;Ljdk/jfr/internal/consumer/TimeConverter;)V",
        &[
            Value::Object(Some(event_type)),
            Value::Object(Some(list)),
            converter_value,
        ],
    ) {
        Ok(Some(Value::Object(Some(context)))) => context,
        _ => return None,
    };
    let context = scope.root(context);
    let (context, array) = (scope.get(&context), scope.get(&values));
    match scope.new_object_initialized(
        "jdk/jfr/consumer/RecordedEvent",
        "(Ljdk/jfr/internal/consumer/ObjectContext;[Ljava/lang/Object;JJ)V",
        &[
            Value::Object(Some(context)),
            Value::Object(Some(array)),
            Value::Long(start_ns as i64),
            Value::Long(end_ns as i64),
        ],
    ) {
        Ok(Some(Value::Object(Some(event)))) => Some(event),
        _ => None,
    }
}

/// Last-resort shape of [`recorded_event_for`]: the field values and their
/// descriptors, with a null `EventType`. `getInt`/`getBoolean`/`hasField` all
/// still answer; only `getEventType()` comes back null.
fn recorded_event_without_type(
    scope: &mut NativeHandleScope<'_>,
    descriptors: &NativeHandle,
    values: &NativeHandle,
    start_ns: u64,
    end_ns: u64,
) -> Option<ObjectRef> {
    let list = scope.get(descriptors);
    let context = match scope.new_object_initialized(
        "jdk/jfr/internal/consumer/ObjectContext",
        "(Ljdk/jfr/EventType;Ljava/util/List;Ljdk/jfr/internal/consumer/TimeConverter;)V",
        &[
            Value::Object(None),
            Value::Object(Some(list)),
            Value::Object(None),
        ],
    ) {
        Ok(Some(Value::Object(Some(context)))) => context,
        _ => return None,
    };
    let context = scope.root(context);
    let (context, array) = (scope.get(&context), scope.get(values));
    match scope.new_object_initialized(
        "jdk/jfr/consumer/RecordedEvent",
        "(Ljdk/jfr/internal/consumer/ObjectContext;[Ljava/lang/Object;JJ)V",
        &[
            Value::Object(Some(context)),
            Value::Object(Some(array)),
            Value::Long(start_ns as i64),
            Value::Long(end_ns as i64),
        ],
    ) {
        Ok(Some(Value::Object(Some(event)))) => Some(event),
        _ => None,
    }
}

/// Hand a committed event to every consumer a started `RecordingStream`
/// registered for it.
///
/// **Delivery is synchronous, on the committing thread.** HotSpot delivers from
/// the stream's own thread, which polls an on-disk chunk repository CratonVM
/// does not produce; there is no door on this boundary to start a Java
/// dispatcher thread from a native. The observable differences are that a
/// consumer runs inside the emitting call (so it must not block on that call's
/// completion) and that events arrive in commit order with no flush batching.
/// `DISPATCHING` keeps a consumer that commits its own event from recursing.
fn dispatch_to_java_streams(
    ctx: &mut dyn NativeContext,
    event_name: &str,
    fields: &[(String, String, Value)],
    start_ns: u64,
    end_ns: u64,
) {
    if DISPATCHING.with(Cell::get) {
        return;
    }
    let vm = ctx.vm_identity();
    let targets: Vec<usize> = {
        let table = java_event_streams()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        table
            .iter()
            .filter(|entry| entry.started && entry.vm == vm)
            .flat_map(|entry| entry.subscriptions.iter())
            .filter(|(filter, _)| match filter {
                Some(name) => name == event_name,
                // `onEvent(Consumer)` subscribes to every event.
                None => true,
            })
            .map(|(_, consumer)| *consumer)
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let Some(event) = recorded_event_for(ctx, event_name, fields, start_ns, end_ns) else {
        tracing::warn!(
            event = %event_name,
            "could not materialise a jdk.jfr.consumer.RecordedEvent; the event was recorded but \
             not delivered to RecordingStream consumers"
        );
        return;
    };
    let mut scope = NativeHandleScope::new(ctx);
    let event = scope.root(event);
    DISPATCHING.with(|flag| flag.set(true));
    for consumer in targets {
        // Resolve per iteration: `accept` runs arbitrary Java, so the previous
        // resolution can have been relocated by then.
        let Some(consumer) = scope.resolve_global_root(consumer) else {
            continue;
        };
        let delivered = scope.get(&event);
        if let Err(error) = scope.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(delivered))],
        ) {
            // HotSpot's `Dispatcher` also isolates a consumer's failure from
            // the emitting thread; report it rather than letting `commit()`
            // throw into application code that never opted into the stream.
            tracing::warn!(
                event = %event_name,
                ?error,
                "a RecordingStream consumer threw; the exception is not propagated to commit()"
            );
        }
    }
    DISPATCHING.with(|flag| flag.set(false));
}

fn saved_dump_path(ctx: &mut dyn NativeContext) -> Value {
    let path = dump_path()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .to_string_lossy()
                .into_owned()
        });
    Value::Object(Some(ctx.create_string(&path)))
}

/// Register every native declared by JDK 25's `jdk.jfr.internal.JVM`.
///
/// Logging calls, and the tuning knobs for a chunk writer this bridge does not
/// own, are deliberately no-ops. They still must succeed: OpenJDK's Java
/// implementation uses them while constructing a recording and before an
/// `EventDirectoryStream` can be started.
///
/// "Configuration" is no longer in that list. A recording's
/// `enable`/`disable`/`threshold` settings ARE consumed — read off
/// `Recording.getSettings()` at `start()` and applied both here (per-type
/// `Event.isEnabled()`) and in the recorder (the name filter on
/// `RecordingSettings`). What is still ignored is named at its `continue` in
/// `read_java_recording_settings`.
pub fn register_jfr_natives(registry: &mut NativeMethodRegistry) {
    const JVM: &str = "jdk/jfr/internal/JVM";

    // Every entry below is `void` and has NO paired reader anywhere on this
    // native surface, so a no-op cannot make any observable answer disagree
    // with it — that is the test each one had to pass to stay here.
    //
    // What none of them can do is the thing they exist for on HotSpot: hand
    // data to the recorder. This crate does not depend on `cratonvm-jfr`, and
    // the whole `NativeContext` JFR surface is the single hard-coded
    // `emit_virtual_thread_pinned_jfr(&'static str)` (`native-api/src/registry.rs`),
    // so there is no door from here into the `FlightRecorder` the VM owns.
    // That, not any individual entry, is the reason this file tops out at the
    // lifecycle/clock contract; see the report escalation.
    //
    // `registerNatives()V` is a genuine no-op on HotSpot too (the JNI
    // registration it performs has no Java-visible effect); the `set*` tuning
    // knobs address a chunk writer this bridge does not own; `log`/`logEvent`/
    // `subscribeLogLevel` are JFR's own internal trace channel, not the
    // application's logging; `flush`/`markChunkFinal`/`emitOldObjectSamples`/
    // `emitDataLoss` operate on chunks that do not exist. The two that would
    // otherwise belong here — `exclude`/`include` — were pulled OUT because
    // `isExcluded` reads them back; see their registrations further down.
    for (name, descriptor) in [
        ("registerNatives", "()V"),
        ("markChunkFinal", "()V"),
        ("log", "(IILjava/lang/String;)V"),
        ("logEvent", "(I[Ljava/lang/String;Z)V"),
        ("subscribeLogLevel", "(Ljdk/jfr/internal/LogTag;I)V"),
        ("retransformClasses", "([Ljava/lang/Class;)V"),
        ("setEnabled", "(JZ)V"),
        ("setFileNotification", "(J)V"),
        ("setGlobalBufferCount", "(J)V"),
        ("setGlobalBufferSize", "(J)V"),
        ("setMemorySize", "(J)V"),
        ("setMethodSamplingPeriod", "(JJ)V"),
        ("setCPURate", "(D)V"),
        ("setCPUPeriod", "(J)V"),
        ("setRepositoryLocation", "(Ljava/lang/String;)V"),
        ("setDumpPath", "(Ljava/lang/String;)V"),
        ("setForceInstrumentation", "(Z)V"),
        ("setCompressedIntegers", "(Z)V"),
        ("setStackDepth", "(I)V"),
        ("setStackTraceEnabled", "(JZ)V"),
        ("setThreadBufferSize", "(J)V"),
        ("storeMetadataDescriptor", "([B)V"),
        ("flush", "(Ljdk/jfr/internal/event/EventWriter;II)V"),
        ("flush", "()V"),
        ("abort", "(Ljava/lang/String;)V"),
        (
            "uncaughtException",
            "(Ljava/lang/Thread;Ljava/lang/Throwable;)V",
        ),
        ("emitOldObjectSamples", "(JZZ)V"),
        ("emitDataLoss", "(J)V"),
        ("unregisterStackFilter", "(J)V"),
        ("setMiscellaneous", "(JJ)V"),
    ] {
        registry.register_with_kind(
            JVM,
            name,
            descriptor,
            |_ctx, _args| Ok(None),
            NativeKind::Bridge,
        );
    }

    registry.register_with_kind(
        JVM,
        "beginRecording",
        "()V",
        |ctx, _args| {
            ctx.jfr_begin_java_recording();
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "endRecording",
        "()V",
        |ctx, _args| {
            ctx.jfr_end_java_recording();
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "isRecording",
        "()Z",
        |ctx, _args| Ok(Some(Value::Int(ctx.jfr_java_recording_active() as i32))),
        NativeKind::Bridge,
    );
    // `begin()`/`end()` are no longer no-ops: they are the only place the
    // event's own start time and duration exist. See `event_timing`.
    registry.register("jdk/jfr/Event", "begin", "()V", |ctx, _args| {
        let key = (ctx.vm_identity(), ctx.thread_id());
        event_timing()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(key, (epoch_nanos().max(0) as u64, None));
        Ok(None)
    });
    registry.register("jdk/jfr/Event", "end", "()V", |ctx, _args| {
        let key = (ctx.vm_identity(), ctx.thread_id());
        let now = epoch_nanos().max(0) as u64;
        if let Some(slot) = event_timing()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(&key)
        {
            slot.1 = Some(now);
        }
        Ok(None)
    });
    // `isEnabled()`/`shouldCommit()` answer for THIS event type, not just "is a
    // recording running".
    //
    // The recording-active check stays first and is a single relaxed atomic
    // load, so a process that never touches `jdk.jfr` pays exactly what it paid
    // before. Only inside a recording does the per-name lookup run — and that is
    // the point: netty guards every buffer allocation with
    // `AllocateBufferEvent.isEventEnabled()`, and a test that enabled only the
    // chunk event should not be paying to fill and commit buffer events.
    for name in ["isEnabled", "shouldCommit"] {
        registry.register("jdk/jfr/Event", name, "()Z", |ctx, args| {
            if !ctx.jfr_java_recording_active() {
                return Ok(Some(Value::Int(0)));
            }
            let Some(Value::Object(Some(event))) = args.first().copied() else {
                return Ok(Some(Value::Int(1)));
            };
            let event_class = ctx.class_id_of_object(event);
            let (event_name, type_default) = jfr_event_type_facts(ctx, event_class);
            Ok(Some(Value::Int(i32::from(java_event_enabled(
                ctx,
                &event_name,
                type_default,
            )))))
        });
    }
    registry.register("jdk/jfr/Event", "commit", "()V", |ctx, args| {
        if !ctx.jfr_java_recording_active() {
            return Ok(None);
        }
        let Some(Value::Object(Some(event))) = args.first().copied() else {
            return Ok(None);
        };
        // Consume this thread's begin/end pair. An event committed without
        // `begin()` is an instant event at commit time, which is what HotSpot
        // records for one too.
        let (start_ns, end_ns) = {
            let key = (ctx.vm_identity(), ctx.thread_id());
            let taken = event_timing()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&key);
            match taken {
                Some((start, end)) => (start, end.unwrap_or(start)),
                None => {
                    let now = epoch_nanos().max(0) as u64;
                    (now, now)
                }
            }
        };
        // The event is rooted for the whole body: name resolution can load the
        // `@Name` annotation's class and field capture reads through the object,
        // so neither may hold a pre-GC `ObjectRef` copy.
        let mut scope = NativeHandleScope::new(ctx);
        let event = scope.root(event);
        let receiver = scope.get(&event);
        let event_class = scope.class_id_of_object(receiver);
        let (event_name, type_default) = jfr_event_type_facts(&mut *scope, event_class);
        // An application may call `commit()` without asking `shouldCommit()`
        // first; a disabled event type must not be recorded either way.
        if !java_event_enabled(&mut *scope, &event_name, type_default) {
            return Ok(None);
        }
        let receiver = scope.get(&event);
        let fields = capture_event_fields(&mut *scope, receiver);
        // Record first, deliver second: a consumer that throws must not cost
        // the recording its event.
        //
        // `fields` carries a raw `ObjectRef` for each `String` field, so nothing
        // between here and `recorded_event_for` (which roots them first thing)
        // may allocate. `jfr_emit_java_event`'s VM side only reads those strings'
        // characters into Rust and touches the Rust recorder — no Java
        // allocation, so no GC point.
        scope.jfr_emit_java_event(
            &event_name,
            &fields,
            start_ns,
            end_ns.saturating_sub(start_ns),
        );
        dispatch_to_java_streams(&mut *scope, &event_name, &fields, start_ns, end_ns);
        Ok(None)
    });
    // `createJFR(boolean simulateFailure)` is NOT a constant: HotSpot's
    // `jfr_create_jfr` returns TRUE immediately if the recorder already exists,
    // otherwise `JfrRecorder::create(simulate_failure)` — which fails on
    // purpose when the flag is set. The flag has a live Java caller:
    // `JVMSupport.createFailedNativeJFR()` is exactly `JVM.createJFR(true)` and
    // is specified to come back `false` (`jdk/jfr/internal/JVMSupport.java`).
    // Ignoring the argument made that call claim success. Track the same
    // created/not-created state HotSpot does and honour the flag.
    registry.register_with_kind(
        JVM,
        "setOutput",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(path))) = args.first() {
                if let Some(path) = ctx.read_string(*path) {
                    ctx.jfr_set_java_output(&path);
                }
            }
            Ok(None)
        },
        NativeKind::Bridge,
    );

    registry.register("jdk/jfr/Recording", "start", "()V", |ctx, args| {
        ctx.jfr_begin_java_recording();
        // Read the recording's `enable`/`disable`/`threshold` settings now. Every
        // documented usage sets them before `start()`, and this is the one place
        // the `Recording` object is in hand with the recording about to go live.
        if let Some(Value::Object(Some(recording))) = args.first().copied() {
            refresh_java_recording_settings(ctx, recording);
        }
        Ok(None)
    });
    registry.register("jdk/jfr/Recording", "stop", "()Z", |ctx, args| {
        ctx.jfr_end_java_recording();
        if let Some(Value::Object(Some(recording))) = args.first().copied() {
            forget_java_recording_settings(ctx, recording);
        }
        Ok(Some(Value::Int(1)))
    });
    registry.register(
        "jdk/jfr/Recording",
        "dump",
        "(Ljava/nio/file/Path;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(path))) = args.get(1) {
                if let Ok(Some(Value::Object(Some(text)))) =
                    ctx.invoke_virtual(*path, "toString", "()Ljava/lang/String;", &[])
                {
                    if let Some(text) = ctx.read_string(text) {
                        ctx.jfr_dump_java_recording(&text);
                    }
                }
            }
            Ok(None)
        },
    );

    registry.register_with_kind(
        JVM,
        "createJFR",
        "(Z)Z",
        |_ctx, args| {
            if CREATED.load(Ordering::Acquire) {
                return Ok(Some(Value::Int(1)));
            }
            let simulate_failure = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
            if simulate_failure {
                return Ok(Some(Value::Int(0)));
            }
            CREATED.store(true, Ordering::Release);
            Ok(Some(Value::Int(1)))
        },
        NativeKind::Bridge,
    );
    // `destroyJFR` is documented as returning "if an instance was actually
    // destroyed" and as ignoring the call when nothing was created
    // (`jdk/jfr/internal/JVM.java`), so a bare `true` was wrong on the
    // never-created path. `JVMSupport.destroyJFR` feeds the answer straight
    // into `nativeOK = !result`, i.e. into `hasJFR()`.
    registry.register_with_kind(
        JVM,
        "destroyJFR",
        "()Z",
        |_ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                CREATED.swap(false, Ordering::AcqRel),
            ))))
        },
        NativeKind::Bridge,
    );
    // KEEP, and the real JDK behaviour it matches is now cited rather than
    // assumed. HotSpot's `jfr_is_available` is `!Jfr::is_disabled()`, i.e. a
    // read of the `-XX:-FlightRecorder` kill switch and nothing else; CratonVM
    // has no such switch, so the constant IS that read's only possible answer.
    //
    // The wave-3 justification that used to sit here was factually wrong on
    // both halves and is corrected for the record: `JVMSupport.checkAvailability()`
    // (JDK 25 source) calls `JVM.isAvailable()` inside a `try` and DISCARDS the
    // result — only an `UnsatisfiedLinkError`/`Throwable` marks JFR
    // unavailable — so answering `false` would not have disabled anything; and
    // `JVMSupport` never throws `UnsupportedOperationException` at all (its
    // three `ensureWith*` helpers throw `InternalError`, `IOException` and
    // `IllegalStateException`). The only consumer of the VALUE is the public
    // `FlightRecorder.isAvailable()`, and `true` is right there: a `Recording`
    // and a `RecordingStream` really can be constructed and driven on this
    // boundary. What is absent is event PAYLOAD collection, and the entry
    // points that would expose it say so without inventing data —
    // `emitEvent`/`isInstrumented`/`getAllowedToDoEventRetransforms` answer
    // `false`, `getEventWriter` answers `null` (the JDK's own "not recording
    // on this thread" reply, see below) and `newEventWriter` throws by name.
    registry.register_with_kind(
        JVM,
        "isAvailable",
        "()Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
        NativeKind::Bridge,
    );

    registry.register_with_kind(
        JVM,
        "counterTime",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(counter_time()))),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "nanosNow",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(epoch_nanos()))),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getChunkStartNanos",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(epoch_nanos()))),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getTicksFrequency",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(TICKS_PER_SECOND))),
        NativeKind::Bridge,
    );
    // Derived, not a literal. OpenJDK uses this factor as
    // `nanosToTicks(nanos) = (long)(nanos * factor)`
    // (`JVMSupport.nanosToTicks`), so it is exactly ticks-per-nanosecond —
    // `TICKS_PER_SECOND / NANOS_PER_SECOND`. Computing it from the same
    // constant `getTicksFrequency()` and `counterTime()` use means changing the
    // tick rate can no longer leave a stale `1.0` behind rescaling every
    // recorded timestamp.
    registry.register_with_kind(
        JVM,
        "getTimeConversionFactor",
        "()D",
        |_ctx, _args| {
            Ok(Some(Value::Double(
                TICKS_PER_SECOND as f64 / NANOS_PER_SECOND as f64,
            )))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getPid",
        "()Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.create_string(&std::process::id().to_string()),
            ))))
        },
        NativeKind::Bridge,
    );

    // `emitEvent(long eventTypeId, long timestamp, long when)` — IMPLEMENTED
    // (was a flat `false`, i.e. "nothing was emitted"). It goes through the SAME
    // `NativeContext` JFR route `beginRecording`/`isRecording` use, so there is
    // one door into the recorder rather than two: the answer is now "yes"
    // exactly when a recording is running.
    //
    // The three longs the JDK passes ARE this entry point's whole payload — it
    // is the periodic-event door, not the EventWriter one, which still has no
    // chunk writer behind it (see `newEventWriter`).
    registry.register_with_kind(
        JVM,
        "emitEvent",
        "(JJJ)Z",
        |ctx, args| {
            let longs: Vec<i64> = args
                .iter()
                .filter_map(|v| match v {
                    Value::Long(l) => Some(*l),
                    _ => None,
                })
                .collect();
            if !ctx.jfr_java_recording_active() {
                return Ok(Some(Value::Int(0)));
            }
            let timestamp = longs.get(1).copied().unwrap_or(0).max(0) as u64;
            ctx.jfr_emit_java_event("jdk.PeriodicEvent", &[], timestamp, 0);
            Ok(Some(Value::Int(1)))
        },
        NativeKind::Bridge,
    );

    // Constant `false` answers that are STATEMENTS OF FACT about this VM, not
    // placeholders — each names the CratonVM property that makes it true:
    //   * `emitEvent`/`setThreshold`  — the Java-side per-event recorder is not
    //   * `setThreshold` — tunes a per-event threshold this bridge does not
    //     consume, and there is no getter that could disagree;
    //   * `getAllowedToDoEventRetransforms`/`isInstrumented` — `retransform
    //     Classes` above is a no-op, so no event class is ever instrumented;
    //     answering `true` to either would contradict that no-op;
    //   * `shouldRotateDisk` — there is no on-disk chunk repository to rotate;
    //   * `isContainerized` — CratonVM reports host, not cgroup, limits.
    // `isExcluded(Thread)` is deliberately NOT in this list: it is a guard whose
    // answer must follow `exclude`/`include` (see below).
    for (name, descriptor) in [
        ("setThreshold", "(JJ)Z"),
        ("getAllowedToDoEventRetransforms", "()Z"),
        ("isExcluded", "(Ljava/lang/Class;)Z"),
        ("isInstrumented", "(Ljava/lang/Class;)Z"),
        ("shouldRotateDisk", "()Z"),
        ("isContainerized", "()Z"),
    ] {
        registry.register_with_kind(
            JVM,
            name,
            descriptor,
            |_ctx, _args| Ok(Some(Value::Int(0))),
            NativeKind::Bridge,
        );
    }
    // Constant `true` = "the request was accepted". `addStringConstant`,
    // `setCutoff` and `setThrottle` tune a recorder whose settings this bridge
    // does not consume, and no JDK caller can observe the stored value through
    // any other entry point (there is no `getCutoff`/`getThrottle`), so nothing
    // is made to disagree by accepting them. `isProduct` is simply true: this
    // is not a fastdebug build.
    for (name, descriptor) in [
        ("addStringConstant", "(JLjava/lang/String;)Z"),
        ("setCutoff", "(JJ)Z"),
        ("setThrottle", "(JJJ)Z"),
        ("isProduct", "()Z"),
    ] {
        registry.register_with_kind(
            JVM,
            name,
            descriptor,
            |_ctx, _args| Ok(Some(Value::Int(1))),
            NativeKind::Bridge,
        );
    }

    // Zero here means "no such id / nothing recorded", which is what a Java
    // caller gets on a JVM with no chunk repository: no stack trace has been
    // interned (`getStackTraceId`, `registerStackFilter`), no event has been
    // committed (`commit`), no event class has been unloaded
    // (`getUnloadedEventClassCount`). `getThreadId` and the two `hostTotal*`
    // probes are deliberately NOT in this list — see below.
    // `getStackTraceId(int skipFrames[, long hash])` — IMPLEMENTED (was a flat
    // `0`, "no trace interned"). The id has to be STABLE: events reference
    // traces by id and a chunk carries each trace once, so the same call site
    // asked twice must get the same number back. Interning by the rendered
    // frame list gives that without a VM-side table — `capture_stack_trace` is
    // already on the native ABI.
    //
    // `0` remains the answer when there is no walkable stack, which is the
    // JDK's own "no such trace".
    fn jfr_stack_trace_id(ctx: &mut dyn NativeContext, skip: i32) -> i64 {
        static IDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
            std::sync::OnceLock::new();
        let frames = ctx.capture_stack_trace(0);
        let skip = skip.max(0) as usize;
        if frames.len() <= skip {
            return 0;
        }
        let mut key = String::new();
        for frame in frames.iter().skip(skip) {
            key.push_str(&frame.class_name);
            key.push('.');
            key.push_str(&frame.method_name);
            key.push(':');
            key.push_str(&frame.line_number.to_string());
            key.push('\n');
        }
        let table = IDS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
        let next = table.len() as i64 + 1;
        *table.entry(key).or_insert(next)
    }
    // JDK 25 declares only the `(IJ)J` arity (both images); `(I)J` is the
    // older spelling and is declared nowhere, so it stays ambient.
    for descriptor in ["(IJ)J", "(I)J"] {
        let cb: cratonvm_native_api::NativeCallback = |ctx, args| {
            let skip = args.iter().find_map(|v| v.as_int()).unwrap_or(0);
            Ok(Some(Value::Long(jfr_stack_trace_id(ctx, skip))))
        };
        if descriptor == "(IJ)J" {
            registry.register_with_kind(
                JVM,
                "getStackTraceId",
                descriptor,
                cb,
                cratonvm_native_api::NativeKind::Bridge,
            );
        } else {
            registry.register(JVM, "getStackTraceId", descriptor, cb);
        }
    }

    for (name, descriptor) in [
        ("getUnloadedEventClassCount", "()J"),
        ("commit", "(J)J"),
        (
            "registerStackFilter",
            "([Ljava/lang/String;[Ljava/lang/String;)J",
        ),
    ] {
        registry.register_with_kind(
            JVM,
            name,
            descriptor,
            |_ctx, _args| Ok(Some(Value::Long(0))),
            NativeKind::Bridge,
        );
    }

    // These two used to sit in the `0` group above on the grounds that CratonVM
    // "has no portable host-RAM probe". It does — three of them, already in the
    // tree (`jfr/src/builtin.rs`, `vm/src/runtime/crash_handler.rs`,
    // `vm-cli/src/main.rs`). Neither number is CratonVM's to invent: both are
    // properties of the HOST, which is why `JVM.java` documents them as
    // reported "whether or not this JVM runs in a container". Probe the host;
    // fall back to the JDK's `0` sentinel only where the platform has no probe.
    registry.register_with_kind(
        JVM,
        "hostTotalMemory",
        "()J",
        |_ctx, _args| {
            Ok(Some(Value::Long(
                host_memory_totals().map(|(total, _)| total).unwrap_or(0),
            )))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "hostTotalSwapMemory",
        "()J",
        |_ctx, _args| {
            Ok(Some(Value::Long(
                host_memory_totals().map(|(_, swap)| swap).unwrap_or(0),
            )))
        },
        NativeKind::Bridge,
    );

    // `exclude`/`include` are the JFR thread filter and `isExcluded` is the
    // guard that reads it. As a no-op/no-op/constant-false trio the guard
    // contradicted the mutators outright: a caller could exclude a thread and
    // then be told by the JVM that the same thread was still being recorded.
    // Track the exclusions and answer the guard from the same table.
    registry.register_with_kind(
        JVM,
        "exclude",
        "(Ljava/lang/Thread;)V",
        |ctx, args| {
            if let Some(key) = jfr_thread_key(ctx, args) {
                excluded_threads()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(key);
            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "include",
        "(Ljava/lang/Thread;)V",
        |ctx, args| {
            if let Some(key) = jfr_thread_key(ctx, args) {
                excluded_threads()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&key);
            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "isExcluded",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            let excluded = match jfr_thread_key(ctx, args) {
                Some(key) => excluded_threads()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .contains(&key),
                None => false,
            };
            Ok(Some(Value::Int(i32::from(excluded))))
        },
        NativeKind::Bridge,
    );

    // Every thread reported the same id (0), so any JFR consumer that groups
    // or joins records by thread collapsed the whole process onto one thread.
    // Report the receiver's own id, exactly as `java/lang/Thread.getId` does
    // (`tid` field, falling back to the executing VM thread).
    registry.register_with_kind(
        JVM,
        "getThreadId",
        "(Ljava/lang/Thread;)J",
        |ctx, args| {
            let receiver_tid = match args.first() {
                Some(Value::Object(Some(thread))) => match ctx.get_field_by_name(*thread, "tid") {
                    Value::Long(tid) if tid > 0 => Some(tid),
                    Value::Int(tid) if tid > 0 => Some(tid as i64),
                    _ => None,
                },
                _ => None,
            };
            Ok(Some(Value::Long(
                receiver_tid.unwrap_or_else(|| ctx.thread_id().max(1) as i64),
            )))
        },
        NativeKind::Bridge,
    );

    // OpenJDK's Type table uses these IDs as map-key identity.  Returning the
    // same placeholder for every type silently collapses the table and makes
    // standard values such as java.lang.String appear unsupported.
    registry.register_with_kind(
        JVM,
        "getTypeId",
        "(Ljava/lang/Class;)J",
        |ctx, args| Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args)))),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getTypeId",
        "(Ljava/lang/String;)J",
        |ctx, args| {
            let id = match args.first() {
                Some(Value::Object(Some(name))) => {
                    ctx.read_string(*name).map(|name| type_id(&name))
                }
                _ => None,
            }
            .unwrap_or_default();
            Ok(Some(Value::Long(id)))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getClassId",
        "(Ljava/lang/Class;)J",
        |ctx, args| Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args)))),
        NativeKind::Bridge,
    );

    registry.register_with_kind(
        JVM,
        "getAllEventClasses",
        "()Ljava/util/List;",
        |ctx, _args| ctx.new_object_initialized("java/util/ArrayList", "()V", &[]),
        NativeKind::Bridge,
    );
    // KEEP for `getEventWriter`, and the JDK caller that makes `null` the right
    // answer is now named. `jdk.jfr.tracing.MethodTracer` (JDK 25) guards every
    // emit with `... && JVM.getEventWriter() != null` — i.e. `null` is the
    // JDK's own encoding of "this thread has no chunk buffer, do not record",
    // a value it tests for, not a value it trips over. `EventWriter
    // .getEventWriter()` likewise reads `JVM.getEventWriter()` and only calls
    // `newEventWriter()` when it is null.
    //
    // `newEventWriter` is the opposite end of that same `if`: its result is
    // used UNCHECKED, so a null there is an NPE at a distance inside woven
    // event bytecode, and a hand-built `EventWriter` would be worse still —
    // every `put*` on it writes through raw `startPosition`/`currentPosition`/
    // `maxPosition` addresses into a chunk buffer that must then be decoded by
    // `JVM.flush(EventWriter,II)`. Throwing names the gap at the gap.
    //
    // Reachability, so the pair is not mistaken for a live lie: the ONLY caller
    // of either is the `commit()` body that `EventInstrumentation` weaves into
    // an event class, and weaving goes through `retransformClasses`, a no-op
    // here — which `isInstrumented`/`getAllowedToDoEventRetransforms` above
    // already report as `false`. So nothing on this surface contradicts
    // anything else: no class is instrumented, therefore no writer is ever
    // requested, therefore `isAvailable() == true` promises only the lifecycle
    // and clock surface that IS implemented.
    registry.register_with_kind(
        JVM,
        "getEventWriter",
        "()Ljdk/jfr/internal/event/EventWriter;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "newEventWriter",
        "()Ljdk/jfr/internal/event/EventWriter;",
        |_ctx, _args| {
            Err(MethodCallFailed::from(
                RuntimeError::UnsupportedOperationException {
                    message: "jdk.jfr.internal.JVM.newEventWriter: CratonVM has no JFR chunk \
                              buffer; event payloads are recorded by the Rust jfr backend, not \
                              through this boundary"
                        .to_owned(),
                },
            ))
        },
        NativeKind::Bridge,
    );
    // `setConfiguration` used to report success from the blanket "return 1"
    // group while storing nothing, and `getConfiguration` answered a constant
    // null — so OpenJDK's `EventConfiguration` round-trip
    // (`JVM.setConfiguration(cls, cfg)` then `JVM.getConfiguration(cls)`, which
    // is how `EventWriterFactory`/`EventHandlerCreator` find an event class's
    // settings) always came back empty despite the setter claiming it worked.
    // Keep the configuration in a side table keyed by the canonical event class
    // name, holding a GLOBAL ROOT so the object survives and is forwarded
    // across a moving GC.
    registry.register_with_kind(
        JVM,
        "setConfiguration",
        "(Ljava/lang/Class;Ljdk/jfr/internal/event/EventConfiguration;)Z",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
                _ => None,
            };
            let Some(name) = name else {
                return Ok(Some(Value::Int(0)));
            };
            let handle = match args.get(1) {
                Some(Value::Object(Some(config))) => Some(ctx.add_global_root(*config)),
                _ => None,
            };
            let previous = {
                let mut table = event_configurations()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match handle {
                    Some(handle) => table.insert(name, handle),
                    None => table.remove(&name),
                }
            };
            if let Some(previous) = previous {
                ctx.remove_global_root(previous);
            }
            Ok(Some(Value::Int(1)))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getConfiguration",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
                _ => None,
            };
            let handle = name.and_then(|name| {
                event_configurations()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .get(&name)
                    .copied()
            });
            let config = handle.and_then(|handle| ctx.resolve_global_root(handle));
            Ok(Some(Value::Object(config)))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "setDumpPath",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let value = match args.first() {
                Some(Value::Object(Some(path))) => ctx.read_string(*path),
                _ => None,
            };
            *dump_path()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = value;
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "getDumpPath",
        "()Ljava/lang/String;",
        |ctx, _args| Ok(Some(saved_dump_path(ctx))),
        NativeKind::Bridge,
    );
    // OpenJDK's JFR bootstrap uses Class.equals() to look up the small set of
    // built-in value types.  A class mirror may have been materialised through
    // a different bootstrap path by then; resolve by the VM's canonical class
    // identity instead.  Delegate to the sibling String overload so the
    // authoritative JDK type table, including its exact ids, remains in Java.
    registry.register(
        "jdk/jfr/internal/Type",
        "getKnownType",
        "(Ljava/lang/Class;)Ljdk/jfr/internal/Type;",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
                _ => None,
            };
            let Some(name) = name else {
                return Ok(Some(Value::Object(None)));
            };
            if let Some(known) = known_jfr_type_for_class_mirror(ctx, args) {
                return Ok(Some(known));
            }
            // During TypeLibrary.<clinit> the Java known-types map has not yet
            // been populated, although a ValueDescriptor still needs the
            // canonical primitive/String/Thread/Class type object. Construct
            // the same public Type shape directly from the stable native id;
            // later Java-side metadata lookups use its name and id, not map
            // identity. Unknown classes deliberately stay null, matching the
            // Java `Type.getKnownType` lookup that this replaces.
            if matches!(
                name.as_str(),
                "boolean"
                    | "byte"
                    | "char"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "java.lang.Class"
                    | "java.lang.String"
                    | "java.lang.Thread"
            ) {
                let name_object = ctx.create_string(&name);
                let pin = ctx.pin_native_root(name_object);
                let name_object = ctx.read_native_pin(pin, name_object);
                let result = ctx.new_object_initialized(
                    "jdk/jfr/internal/Type",
                    "(Ljava/lang/String;Ljava/lang/String;JLjava/lang/Boolean;)V",
                    &[
                        Value::Object(Some(name_object)),
                        Value::Object(None),
                        Value::Long(type_id(&name)),
                        Value::Object(None),
                    ],
                );
                ctx.unpin_native_roots(pin);
                return result;
            }
            Ok(Some(Value::Object(None)))
        },
    );
    // MetadataLoader asks Utils to validate the value class.  On a real JVM
    // Type.getKnownType(Class) is safe because bootstrap mirrors are unique;
    // route it through the authoritative Type singletons here so a separately
    // materialised primitive mirror cannot make a valid metadata field fail.
    registry.register(
        "jdk/jfr/internal/util/Utils",
        "getValidType",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljdk/jfr/internal/Type;",
        |ctx, args| {
            let raw_name = match args.first() {
                Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
                _ => None,
            };
            let Some(mut name) = raw_name else {
                return Ok(Some(Value::Object(None)));
            };
            if let Some(known) = known_jfr_type_for_class_mirror(ctx, args) {
                return Ok(Some(known));
            }
            // `ValueDescriptor` permits arrays and asks us about the component
            // type. The real `Utils.getValidType` performs that unwrapping
            // before consulting Type's known-type table. Class.getName()
            // exposes arrays in descriptor form, so decode precisely that
            // representation without allocating another Class mirror.
            while let Some(component) = name.strip_prefix('[') {
                name = component.to_owned();
            }
            if let Some(reference) = name
                .strip_prefix('L')
                .and_then(|reference| reference.strip_suffix(';'))
            {
                name = reference.replace('/', ".");
            } else if name.len() == 1 {
                name = match name.as_str() {
                    "Z" => "boolean",
                    "B" => "byte",
                    "C" => "char",
                    "S" => "short",
                    "I" => "int",
                    "J" => "long",
                    "F" => "float",
                    "D" => "double",
                    _ => return Ok(Some(Value::Object(None))),
                }
                .to_owned();
            }
            if !matches!(
                name.as_str(),
                "boolean"
                    | "byte"
                    | "char"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "java.lang.Class"
                    | "java.lang.String"
                    | "java.lang.Thread"
            ) {
                return Ok(Some(Value::Object(None)));
            }
            let name_object = ctx.create_string(&name);
            let pin = ctx.pin_native_root(name_object);
            let name_object = ctx.read_native_pin(pin, name_object);
            let result = ctx.new_object_initialized(
                "jdk/jfr/internal/Type",
                "(Ljava/lang/String;Ljava/lang/String;JLjava/lang/Boolean;)V",
                &[
                    Value::Object(Some(name_object)),
                    Value::Object(None),
                    Value::Long(type_id(&name)),
                    Value::Object(None),
                ],
            );
            ctx.unpin_native_roots(pin);
            result
        },
    );
    registry.register(
        "jdk/jfr/AnnotationElement",
        "checkType",
        "(Ljava/lang/Class;)V",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror),
                _ => None,
            }
            .unwrap_or_else(|| "<unknown>".to_owned());
            let mut component = name.as_str();
            while let Some(rest) = component.strip_prefix('[') {
                component = rest;
            }
            let component = component
                .strip_prefix('L')
                .and_then(|value| value.strip_suffix(';'))
                .map(|value| value.replace('/', "."))
                .unwrap_or_else(|| component.to_owned());
            let allowed = matches!(
                component.as_str(),
                "boolean"
                    | "byte"
                    | "char"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "java.lang.String"
            );
            if allowed {
                Ok(None)
            } else {
                Err(MethodCallFailed::from(
                    RuntimeError::IllegalArgumentException {
                        message: format!(
                            "Only primitive, String, or arrays thereof are allowed (got {name})"
                        ),
                    },
                ))
            }
        },
    );
    // NOT a constant-valued stub — a deliberate behavioural OVERRIDE of real
    // JDK bytecode, and the one entry in this file whose justification could
    // not be re-derived from source alone. Recorded precisely so it can be
    // retested rather than re-argued:
    //
    // The real `JDKEvents.initialize()` (JDK 25) registers ~30 mirror event
    // classes through `MetadataRepository.register`, adds five periodic
    // container events, and calls `JFRTracing.enable()`. Its whole body is
    // already wrapped in `catch (Exception e) { Logger.log(WARN) }`, so the
    // wave-3 claim that letting it run "rejects a perfectly usable
    // RecordingStream" can only hold for an *Error* escaping that catch — the
    // candidate being the `InternalError` that `JVMSupport.setConfiguration`
    // throws when `JVM.setConfiguration` returns false. That native answered a
    // blanket `true` while storing nothing when this override was written and
    // now round-trips through a real side table (see above), so the failure
    // this override was added for may well be gone. Removing it needs a run,
    // which this pass cannot do; flagged for the next JFR bootstrap run.
    registry.register(
        "jdk/jfr/internal/JDKEvents",
        "initialize",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "jdk/jfr/internal/instrument/JDKEvents",
        "initialize",
        "()V",
        |_ctx, _args| Ok(None),
    );
    // `RecordingStream` — an OVERRIDE of real bytecode, not a constant stub.
    //
    // The real `startAsync()` (JDK 25) is four statements:
    //     PlatformRecording pr = ...getPlatformRecording(recording);
    //     long startNanos = pr.start();
    //     updateOnCompleteHandler();
    //     directoryStream.startAsync(startNanos);
    // The last one stays skipped, and the reason is unchanged and still worth
    // keeping: `EventDirectoryStream` polls an on-disk chunk repository that
    // nothing in CratonVM produces, so its loop has no blocking source and
    // spins a core forever.
    //
    // What used to go with it was the RECORDING STATE TRANSITION, and that was
    // the whole bug: `Event.isEnabled()` and `shouldCommit()` both answer
    // `jfr_java_recording_active()`, so with no transition they answered
    // `false` while a stream was running, `commit()` returned early, and
    // nothing was ever recorded — let alone delivered. netty's
    // `JfrEventsTest` fails all 10 of its tests on exactly that: every test
    // opens a stream, allocates a buffer and blocks until the event arrives.
    // So the transition happens here, through the same
    // `jfr_begin_java_recording` door the non-streaming `Recording.start()`
    // uses, and delivery is `Event.commit()`'s synchronous dispatch (see
    // `dispatch_to_java_streams`) rather than a chunk-repository poll.
    //
    // `start()` gets the same treatment, and its divergence is deliberate: on
    // HotSpot the blocking sibling processes events until the stream closes,
    // which requires the dispatcher thread this boundary has no way to start.
    // It therefore performs the transition and RETURNS instead of blocking.
    // That is still a strict improvement on what it did before, which was to
    // fall through to `directoryStream.start` and spin a core forever.
    for name in ["startAsync", "start"] {
        registry.register(
            "jdk/jfr/consumer/RecordingStream",
            name,
            "()V",
            |ctx, args| {
                let Some(Value::Object(Some(stream))) = args.first().copied() else {
                    return Ok(None);
                };
                let slot = java_stream_slot_or_insert(ctx, stream);
                java_event_streams()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())[slot]
                    .started = true;
                ctx.jfr_begin_java_recording();
                // A stream's `enable(...)` calls land on the `Recording` it owns
                // (`RecordingStream.enable` delegates straight to it), so the
                // settings live on that private field, not on the stream.
                if let Value::Object(Some(recording)) = ctx.get_field_by_name(stream, "recording") {
                    refresh_java_recording_settings(ctx, recording);
                }
                Ok(None)
            },
        );
    }
    // `onEvent(String, Consumer)` / `onEvent(Consumer)` — captured here instead
    // of in the JDK's `Dispatcher`, because the dispatcher is only ever read by
    // the `EventDirectoryStream` loop that is not running. The `Consumer` is
    // held as a global root: it is parked from this call until `close()`, which
    // is arbitrarily many GCs later, so a bare `ObjectRef` would dangle.
    registry.register(
        "jdk/jfr/consumer/RecordingStream",
        "onEvent",
        "(Ljava/lang/String;Ljava/util/function/Consumer;)V",
        |ctx, args| {
            let Some(Value::Object(Some(stream))) = args.first().copied() else {
                return Ok(None);
            };
            let filter = match args.get(1) {
                Some(Value::Object(Some(name))) => ctx.read_string(*name),
                _ => None,
            };
            let Some(Value::Object(Some(consumer))) = args.get(2).copied() else {
                return Ok(None);
            };
            let slot = java_stream_slot_or_insert(ctx, stream);
            let root = ctx.add_global_root(consumer);
            java_event_streams()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())[slot]
                .subscriptions
                .push((filter, root));
            Ok(None)
        },
    );
    registry.register(
        "jdk/jfr/consumer/RecordingStream",
        "onEvent",
        "(Ljava/util/function/Consumer;)V",
        |ctx, args| {
            let Some(Value::Object(Some(stream))) = args.first().copied() else {
                return Ok(None);
            };
            let Some(Value::Object(Some(consumer))) = args.get(1).copied() else {
                return Ok(None);
            };
            let slot = java_stream_slot_or_insert(ctx, stream);
            let root = ctx.add_global_root(consumer);
            java_event_streams()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())[slot]
                .subscriptions
                .push((None, root));
            Ok(None)
        },
    );
    // `close()` must undo `startAsync` symmetrically, or `Event.isEnabled()`
    // stays true after the try-with-resources block and the next unrelated
    // event class starts committing into a recording nobody is reading.
    //
    // The JDK body is `directoryStream.setChunkCompleteHandler(null);
    // recording.close(); directoryStream.close();`. It is replayed below rather
    // than skipped: it already runs cleanly on this VM today, and it is what
    // makes `onClose(Runnable)` actions fire. Our own teardown happens FIRST so
    // a failure in the replay cannot leave the recording latched on.
    registry.register(
        "jdk/jfr/consumer/RecordingStream",
        "close",
        "()V",
        |ctx, args| {
            let Some(Value::Object(Some(stream))) = args.first().copied() else {
                return Ok(None);
            };
            let vm = ctx.vm_identity();
            let (removed, remaining_started) = {
                let mut table = java_event_streams()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                let removed = java_stream_slot(ctx, &table, stream).map(|slot| table.remove(slot));
                // Only this VM's streams keep this VM's recording alive.
                let remaining = table
                    .iter()
                    .filter(|entry| entry.started && entry.vm == vm)
                    .count();
                (removed, remaining)
            };
            if let Some(entry) = removed {
                ctx.remove_global_root(entry.stream);
                for (_, consumer) in entry.subscriptions {
                    ctx.remove_global_root(consumer);
                }
            }
            // Only the last started stream ends the recording — two concurrent
            // streams share one, and ending it under the first to close would
            // silently disable the other.
            if remaining_started == 0 {
                ctx.jfr_end_java_recording();
            }
            // This stream's settings stop contributing to the enable union
            // whether or not it was the last one, or the next test's events
            // would still be admitted by a closed stream's `enable(...)`.
            if let Value::Object(Some(recording)) = ctx.get_field_by_name(stream, "recording") {
                forget_java_recording_settings(ctx, recording);
            }
            // Replay the JDK's own close. `recording` and `directoryStream` are
            // private fields of `RecordingStream`; a missing one means the JDK
            // shape changed, in which case skipping the replay is the safe
            // answer — our teardown above has already run.
            //
            // BOTH fields are read and rooted before EITHER `close()` runs:
            // `Recording.close()` is Java and can move `stream`, so reading the
            // second field off the pre-call `ObjectRef` afterwards would be a
            // use-after-move.
            let mut scope = NativeHandleScope::new(ctx);
            let recording = match scope.get_field_by_name(stream, "recording") {
                Value::Object(Some(recording)) => Some(scope.root(recording)),
                _ => None,
            };
            let directory_stream = match scope.get_field_by_name(stream, "directoryStream") {
                Value::Object(Some(directory_stream)) => Some(scope.root(directory_stream)),
                _ => None,
            };
            if let Some(handle) = &recording {
                let recording = scope.get(handle);
                let _ = scope.invoke_virtual(recording, "close", "()V", &[]);
            }
            if let Some(handle) = &directory_stream {
                let directory_stream = scope.get(handle);
                let _ = scope.invoke_virtual(directory_stream, "close", "()V", &[]);
            }
            Ok(None)
        },
    );
    registry.register_with_kind(
        JVM,
        "setMethodTraceFilters",
        "([Ljava/lang/String;[Ljava/lang/String;[Ljava/lang/String;[I)[J",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.new_array(ArrayElementType::Long, 0),
            ))))
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        JVM,
        "drainStaleMethodTracerIds",
        "()[J",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.new_array(ArrayElementType::Long, 0),
            ))))
        },
        NativeKind::Bridge,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn registers_the_recordingstream_bootstrap_boundary() {
        let mut registry = NativeMethodRegistry::new();
        register_jfr_natives(&mut registry);
        assert!(registry
            .find("jdk/jfr/internal/JVM", "isAvailable", "()Z")
            .is_some());
        assert!(registry
            .find("jdk/jfr/internal/JVM", "createJFR", "(Z)Z")
            .is_some());
        assert!(registry
            .find("jdk/jfr/internal/JVM", "beginRecording", "()V")
            .is_some());
        assert!(registry
            .find("jdk/jfr/internal/JDKEvents", "initialize", "()V")
            .is_some());
        // The streaming surface is a SET: a stream that starts a recording but
        // never ends it leaves `Event.isEnabled()` true for the rest of the
        // process, and a stream whose consumers are never captured delivers
        // nothing however well the recording state behaves. Each of these had
        // to be added together for `JfrEventsTest` to pass, so each is pinned.
        for (name, descriptor) in [
            ("startAsync", "()V"),
            ("start", "()V"),
            ("close", "()V"),
            (
                "onEvent",
                "(Ljava/lang/String;Ljava/util/function/Consumer;)V",
            ),
            ("onEvent", "(Ljava/util/function/Consumer;)V"),
        ] {
            assert!(
                registry
                    .find("jdk/jfr/consumer/RecordingStream", name, descriptor)
                    .is_some(),
                "jdk/jfr/consumer/RecordingStream.{name}{descriptor} must stay registered"
            );
        }
        // `begin`/`end` carry the event's start time and duration, which no
        // field on the object can hold because CratonVM does not weave event
        // classes. As no-ops every event was an instant event at commit time.
        for (name, descriptor) in [("begin", "()V"), ("end", "()V"), ("commit", "()V")] {
            assert!(
                registry.find("jdk/jfr/Event", name, descriptor).is_some(),
                "jdk/jfr/Event.{name}{descriptor} must stay registered"
            );
        }
        // The thread filter and the event-configuration round trip are
        // stateful: the guard/reader must exist alongside its mutator, or the
        // pair silently disagrees again (wave-2 stub removal).
        for (name, descriptor) in [
            ("exclude", "(Ljava/lang/Thread;)V"),
            ("include", "(Ljava/lang/Thread;)V"),
            ("isExcluded", "(Ljava/lang/Thread;)Z"),
            (
                "setConfiguration",
                "(Ljava/lang/Class;Ljdk/jfr/internal/event/EventConfiguration;)Z",
            ),
            ("getConfiguration", "(Ljava/lang/Class;)Ljava/lang/Object;"),
            ("getThreadId", "(Ljava/lang/Thread;)J"),
        ] {
            assert!(
                registry
                    .find("jdk/jfr/internal/JVM", name, descriptor)
                    .is_some(),
                "jdk/jfr/internal/JVM.{name}{descriptor} must stay registered"
            );
        }
    }

    #[test]
    fn jfr_type_ids_are_stable_and_distinct() {
        let string = type_id("java.lang.String");
        assert_eq!(string, type_id("java.lang.String"));
        assert_ne!(string, type_id("java.lang.Thread"));
        assert_ne!(string, 0);
    }

    /// Every id this bridge hands out must clear
    /// `JVM.RESERVED_CLASS_ID_LIMIT` (500). Below it the JDK's
    /// `TypeLibrary.types` map — keyed by id — already holds the ~200 built-in
    /// types whose ids are baked into `metadata.bin`, so a low id makes
    /// `EventType.getEventType(myEventClass)` answer with somebody else's event
    /// type instead of failing.
    #[test]
    fn jfr_type_ids_clear_the_jdks_reserved_class_id_range() {
        assert_eq!(FIRST_JFR_TYPE_ID, 500);
        for name in [
            "com.example.First",
            "com.example.Second",
            "com.example.Third",
        ] {
            assert!(
                type_id(name) >= FIRST_JFR_TYPE_ID,
                "{name} was assigned an id inside the JDK's reserved range"
            );
        }
    }

    /// The descriptor set the payload capture admits has to be exactly the set
    /// the chunk writer can declare in metadata — a field written into a
    /// payload but absent from the metadata mis-frames every field after it.
    #[test]
    fn capturable_field_descriptors_match_the_writable_jfr_types() {
        for descriptor in ["Z", "B", "C", "S", "I", "J", "F", "D", "Ljava/lang/String;"] {
            assert!(
                JFR_FIELD_DESCRIPTORS.contains(&descriptor),
                "{descriptor} must be capturable"
            );
        }
        for descriptor in [
            "Ljava/lang/Class;",
            "Ljava/lang/Object;",
            "[I",
            "[Ljava/lang/String;",
        ] {
            assert!(
                !JFR_FIELD_DESCRIPTORS.contains(&descriptor),
                "{descriptor} has no JFR field type this writer can declare and must be skipped"
            );
        }
    }

    /// `JVMSupport.nanosToTicks` is `(long)(nanos * getTimeConversionFactor())`
    /// and `counterTime()` counts nanoseconds, so the factor and the advertised
    /// tick frequency have to stay in step with `counter_time`'s unit.
    #[test]
    fn tick_frequency_and_conversion_factor_agree() {
        assert_eq!(TICKS_PER_SECOND, NANOS_PER_SECOND);
        assert_eq!(TICKS_PER_SECOND as f64 / NANOS_PER_SECOND as f64, 1.0);
    }

    /// The host really can answer `hostTotalMemory`; on the two platforms with
    /// a probe it must not fall back to the `0` "unknown" sentinel.
    #[test]
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    fn host_memory_totals_are_probed_not_zero() {
        let (total, swap) = host_memory_totals().expect("host RAM probe");
        assert!(total > 0, "hostTotalMemory must be a real total");
        assert!(swap >= 0, "hostTotalSwapMemory must not be negative");
    }

    /// `withoutThreshold()` writes `0 ns`, `withThreshold(Duration)` writes the
    /// unit-suffixed form. A value this parser cannot read must answer `None`
    /// rather than a guess: a wrong threshold silently drops events.
    #[test]
    fn jfr_timespan_settings_parse_to_nanoseconds() {
        assert_eq!(parse_jfr_timespan_nanos("0 ns"), Some(0));
        assert_eq!(parse_jfr_timespan_nanos("20 ms"), Some(20_000_000));
        assert_eq!(parse_jfr_timespan_nanos("20ms"), Some(20_000_000));
        assert_eq!(parse_jfr_timespan_nanos("1 s"), Some(1_000_000_000));
        assert_eq!(parse_jfr_timespan_nanos("500 us"), Some(500_000));
        assert_eq!(parse_jfr_timespan_nanos("2 m"), Some(120_000_000_000));
        assert_eq!(parse_jfr_timespan_nanos("1 h"), Some(3_600_000_000_000));
        assert_eq!(parse_jfr_timespan_nanos("1 d"), Some(86_400_000_000_000));
        // A bare number is nanoseconds, which is what the JDK's own parser does.
        assert_eq!(parse_jfr_timespan_nanos("750"), Some(750));
        assert_eq!(parse_jfr_timespan_nanos("infinity"), Some(u64::MAX));
        assert_eq!(parse_jfr_timespan_nanos("everything"), None);
        assert_eq!(parse_jfr_timespan_nanos("20 fortnights"), None);
        assert_eq!(parse_jfr_timespan_nanos(""), None);
    }

    #[test]
    fn primitive_class_descriptors_match_openjdk_jfr_type_names() {
        assert_eq!(canonical_jfr_type_name("Z"), "boolean");
        assert_eq!(canonical_jfr_type_name("I"), "int");
        assert_eq!(
            canonical_jfr_type_name("java/lang/String"),
            "java.lang.String"
        );
    }
}
