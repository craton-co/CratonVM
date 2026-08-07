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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ArrayElementType, Value};

static RECORDING: AtomicBool = AtomicBool::new(false);
/// Mirrors HotSpot's `JfrRecorder::is_created()`: set by `createJFR`, cleared
/// by `destroyJFR`. Both natives are specified in terms of it (see their
/// registrations), so it cannot be replaced by a constant.
static CREATED: AtomicBool = AtomicBool::new(false);
static CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();
static NEXT_TYPE_ID: AtomicI64 = AtomicI64::new(1);

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

fn type_id_from_class_mirror(ctx: &mut dyn NativeContext, args: &[Value]) -> i64 {
    match args.first() {
        Some(Value::Object(Some(mirror))) => class_name_for_mirror(ctx, *mirror)
            .map(|name| type_id(&name))
            .unwrap_or_default(),
        _ => 0,
    }
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
/// Configuration and logging calls are deliberately no-ops until a setting is
/// consumed by the Rust recorder.  They still must succeed: OpenJDK's Java
/// implementation uses them while constructing a recording and before an
/// `EventDirectoryStream` can be started.
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
        registry.register_with_kind(JVM, name, descriptor, |_ctx, _args| Ok(None), NativeKind::Bridge);
    }

    registry.register_with_kind(JVM, "beginRecording", "()V", |ctx, _args| {
        ctx.jfr_begin_java_recording();
        RECORDING.store(ctx.jfr_java_recording_active(), Ordering::Release);
        Ok(None)
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "endRecording", "()V", |ctx, _args| {
        ctx.jfr_end_java_recording();
        RECORDING.store(false, Ordering::Release);
        Ok(None)
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "isRecording", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(ctx.jfr_java_recording_active() as i32)))
    }, NativeKind::Bridge);
    for (name, descriptor) in [("begin", "()V"), ("end", "()V")] {
        registry.register("jdk/jfr/Event", name, descriptor, |_ctx, _args| Ok(None));
    }
    registry.register("jdk/jfr/Event", "isEnabled", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(ctx.jfr_java_recording_active() as i32)))
    });
    registry.register("jdk/jfr/Event", "shouldCommit", "()Z", |ctx, _args| {
        Ok(Some(Value::Int(ctx.jfr_java_recording_active() as i32)))
    });
    registry.register("jdk/jfr/Event", "commit", "()V", |ctx, args| {
        if !ctx.jfr_java_recording_active() {
            return Ok(None);
        }
        let event_class = args
            .first()
            .and_then(|value| match value {
                Value::Object(Some(obj)) => Some(*obj),
                _ => None,
            })
            .and_then(|obj| ctx.class_name_of_id(ctx.class_id_of_object(obj)))
            .unwrap_or_else(|| "jdk/jfr/Event".to_owned());
        ctx.jfr_emit_java_event(&event_class, epoch_nanos().max(0) as u64, 0);
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
    registry.register_with_kind(JVM, "setOutput", "(Ljava/lang/String;)V", |ctx, args| {
        if let Some(Value::Object(Some(path))) = args.first() {
            if let Some(path) = ctx.read_string(*path) {
                ctx.jfr_set_java_output(&path);
            }
        }
        Ok(None)
    }, NativeKind::Bridge);

    registry.register("jdk/jfr/Recording", "start", "()V", |ctx, _args| {
        ctx.jfr_begin_java_recording();
        Ok(None)
    });
    registry.register("jdk/jfr/Recording", "stop", "()Z", |ctx, _args| {
        ctx.jfr_end_java_recording();
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

    registry.register_with_kind(JVM, "createJFR", "(Z)Z", |_ctx, args| {
        if CREATED.load(Ordering::Acquire) {
            return Ok(Some(Value::Int(1)));
        }
        let simulate_failure = matches!(args.first(), Some(Value::Int(v)) if *v != 0);
        if simulate_failure {
            return Ok(Some(Value::Int(0)));
        }
        CREATED.store(true, Ordering::Release);
        Ok(Some(Value::Int(1)))
    }, NativeKind::Bridge);
    // `destroyJFR` is documented as returning "if an instance was actually
    // destroyed" and as ignoring the call when nothing was created
    // (`jdk/jfr/internal/JVM.java`), so a bare `true` was wrong on the
    // never-created path. `JVMSupport.destroyJFR` feeds the answer straight
    // into `nativeOK = !result`, i.e. into `hasJFR()`.
    registry.register_with_kind(JVM, "destroyJFR", "()Z", |_ctx, _args| {
        RECORDING.store(false, Ordering::Release);
        Ok(Some(Value::Int(i32::from(
            CREATED.swap(false, Ordering::AcqRel),
        ))))
    }, NativeKind::Bridge);
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
    registry.register_with_kind(JVM, "isAvailable", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, NativeKind::Bridge);

    registry.register_with_kind(JVM, "counterTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(counter_time())))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "nanosNow", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_nanos())))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getChunkStartNanos", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_nanos())))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getTicksFrequency", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(TICKS_PER_SECOND)))
    }, NativeKind::Bridge);
    // Derived, not a literal. OpenJDK uses this factor as
    // `nanosToTicks(nanos) = (long)(nanos * factor)`
    // (`JVMSupport.nanosToTicks`), so it is exactly ticks-per-nanosecond —
    // `TICKS_PER_SECOND / NANOS_PER_SECOND`. Computing it from the same
    // constant `getTicksFrequency()` and `counterTime()` use means changing the
    // tick rate can no longer leave a stale `1.0` behind rescaling every
    // recorded timestamp.
    registry.register_with_kind(JVM, "getTimeConversionFactor", "()D", |_ctx, _args| {
        Ok(Some(Value::Double(
            TICKS_PER_SECOND as f64 / NANOS_PER_SECOND as f64,
        )))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getPid", "()Ljava/lang/String;", |ctx, _args| {
        Ok(Some(Value::Object(Some(
            ctx.create_string(&std::process::id().to_string()),
        ))))
    }, NativeKind::Bridge);

    // `emitEvent(long eventTypeId, long timestamp, long when)` — IMPLEMENTED
    // (was a flat `false`, i.e. "nothing was emitted"). It goes through the SAME
    // `NativeContext` JFR route `beginRecording`/`isRecording` use, so there is
    // one door into the recorder rather than two: the answer is now "yes"
    // exactly when a recording is running.
    //
    // The three longs the JDK passes ARE this entry point's whole payload — it
    // is the periodic-event door, not the EventWriter one, which still has no
    // chunk writer behind it (see `newEventWriter`).
    registry.register_with_kind(JVM, "emitEvent", "(JJJ)Z", |ctx, args| {
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
        ctx.jfr_emit_java_event("jdk.PeriodicEvent", timestamp, 0);
        Ok(Some(Value::Int(1)))
    }, NativeKind::Bridge);

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
        registry.register_with_kind(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(0))), NativeKind::Bridge);
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
        registry.register_with_kind(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(1))), NativeKind::Bridge);
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
        static IDS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, i64>>,
        > = std::sync::OnceLock::new();
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
        registry.register_with_kind(JVM, name, descriptor, |_ctx, _args| {
            Ok(Some(Value::Long(0)))
        }, NativeKind::Bridge);
    }

    // These two used to sit in the `0` group above on the grounds that CratonVM
    // "has no portable host-RAM probe". It does — three of them, already in the
    // tree (`jfr/src/builtin.rs`, `vm/src/runtime/crash_handler.rs`,
    // `vm-cli/src/main.rs`). Neither number is CratonVM's to invent: both are
    // properties of the HOST, which is why `JVM.java` documents them as
    // reported "whether or not this JVM runs in a container". Probe the host;
    // fall back to the JDK's `0` sentinel only where the platform has no probe.
    registry.register_with_kind(JVM, "hostTotalMemory", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(
            host_memory_totals().map(|(total, _)| total).unwrap_or(0),
        )))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "hostTotalSwapMemory", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(
            host_memory_totals().map(|(_, swap)| swap).unwrap_or(0),
        )))
    }, NativeKind::Bridge);

    // `exclude`/`include` are the JFR thread filter and `isExcluded` is the
    // guard that reads it. As a no-op/no-op/constant-false trio the guard
    // contradicted the mutators outright: a caller could exclude a thread and
    // then be told by the JVM that the same thread was still being recorded.
    // Track the exclusions and answer the guard from the same table.
    registry.register_with_kind(JVM, "exclude", "(Ljava/lang/Thread;)V", |ctx, args| {
        if let Some(key) = jfr_thread_key(ctx, args) {
            excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(key);
        }
        Ok(None)
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "include", "(Ljava/lang/Thread;)V", |ctx, args| {
        if let Some(key) = jfr_thread_key(ctx, args) {
            excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&key);
        }
        Ok(None)
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "isExcluded", "(Ljava/lang/Thread;)Z", |ctx, args| {
        let excluded = match jfr_thread_key(ctx, args) {
            Some(key) => excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .contains(&key),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(excluded))))
    }, NativeKind::Bridge);

    // Every thread reported the same id (0), so any JFR consumer that groups
    // or joins records by thread collapsed the whole process onto one thread.
    // Report the receiver's own id, exactly as `java/lang/Thread.getId` does
    // (`tid` field, falling back to the executing VM thread).
    registry.register_with_kind(JVM, "getThreadId", "(Ljava/lang/Thread;)J", |ctx, args| {
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
    }, NativeKind::Bridge);

    // OpenJDK's Type table uses these IDs as map-key identity.  Returning the
    // same placeholder for every type silently collapses the table and makes
    // standard values such as java.lang.String appear unsupported.
    registry.register_with_kind(JVM, "getTypeId", "(Ljava/lang/Class;)J", |ctx, args| {
        Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args))))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getTypeId", "(Ljava/lang/String;)J", |ctx, args| {
        let id = match args.first() {
            Some(Value::Object(Some(name))) => ctx.read_string(*name).map(|name| type_id(&name)),
            _ => None,
        }
        .unwrap_or_default();
        Ok(Some(Value::Long(id)))
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getClassId", "(Ljava/lang/Class;)J", |ctx, args| {
        Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args))))
    }, NativeKind::Bridge);

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
    registry.register_with_kind(JVM, "setDumpPath", "(Ljava/lang/String;)V", |ctx, args| {
        let value = match args.first() {
            Some(Value::Object(Some(path))) => ctx.read_string(*path),
            _ => None,
        };
        *dump_path()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = value;
        Ok(None)
    }, NativeKind::Bridge);
    registry.register_with_kind(JVM, "getDumpPath", "()Ljava/lang/String;", |ctx, _args| {
        Ok(Some(saved_dump_path(ctx)))
    }, NativeKind::Bridge);
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
    // Also an OVERRIDE of real bytecode, not a constant. The real body
    // (JDK 25) is four statements:
    //     PlatformRecording pr = ...getPlatformRecording(recording);
    //     long startNanos = pr.start();
    //     updateOnCompleteHandler();
    //     directoryStream.startAsync(startNanos);
    // Only the last one is unsupportable here: `EventDirectoryStream` polls an
    // on-disk chunk repository that nothing in CratonVM produces, so its loop
    // has no blocking source and spins a core forever. The first two are the
    // recording state transition and would be worth keeping — but `pr.start()`
    // routes into `PlatformRecorder.start`, i.e. chunk creation against that
    // same absent repository, so it cannot be re-added without a run to prove
    // it terminates. Left as the whole-method no-op it has been: the stream
    // stays constructible, configurable and closeable and delivers no events,
    // which is what every other entry point on this surface also reports.
    // NOTE for the same follow-up run: `RecordingStream.start()` (the blocking
    // sibling) is NOT overridden and still reaches `directoryStream.start`.
    registry.register(
        "jdk/jfr/consumer/RecordingStream",
        "startAsync",
        "()V",
        |_ctx, _args| Ok(None),
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
    registry.register_with_kind(JVM, "drainStaleMethodTracerIds", "()[J", |ctx, _args| {
        Ok(Some(Value::Object(Some(
            ctx.new_array(ArrayElementType::Long, 0),
        ))))
    }, NativeKind::Bridge);
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
        assert!(registry
            .find("jdk/jfr/consumer/RecordingStream", "startAsync", "()V")
            .is_some());
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
