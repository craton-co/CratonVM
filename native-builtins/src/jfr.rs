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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, RuntimeError};
use cratonvm_types::{ArrayElementType, Value};

static RECORDING: AtomicBool = AtomicBool::new(false);
static CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();
static NEXT_TYPE_ID: AtomicI64 = AtomicI64::new(1);

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
        ("setOutput", "(Ljava/lang/String;)V"),
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
        registry.register(JVM, name, descriptor, |_ctx, _args| Ok(None));
    }

    registry.register(JVM, "beginRecording", "()V", |_ctx, _args| {
        RECORDING.store(true, Ordering::Release);
        Ok(None)
    });
    registry.register(JVM, "endRecording", "()V", |_ctx, _args| {
        RECORDING.store(false, Ordering::Release);
        Ok(None)
    });
    registry.register(JVM, "isRecording", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(RECORDING.load(Ordering::Acquire) as i32)))
    });
    // `true` here is a claim about THIS boundary, and it is one the rest of the
    // file keeps: `JVMSupport.ensureAvailable()` only gates the lifecycle and
    // clock surface, all of which is really implemented above. What is missing
    // is event payload collection, and every entry point that would expose the
    // gap already says so honestly — `emitEvent`/`isInstrumented`/
    // `getAllowedToDoEventRetransforms` answer `false`, `newEventWriter` throws.
    // So no caller is told "available" and then handed a fake event: it gets a
    // working `Recording`/`RecordingStream` that reports zero events.
    // (Answering `false` instead is not free — `JVMSupport` then makes every
    // `jdk.jfr` entry point throw `UnsupportedOperationException`, which takes
    // out otherwise-fine users such as Micrometer's virtual-thread binder.)
    registry.register(JVM, "createJFR", "(Z)Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    registry.register(JVM, "destroyJFR", "()Z", |_ctx, _args| {
        RECORDING.store(false, Ordering::Release);
        Ok(Some(Value::Int(1)))
    });
    registry.register(JVM, "isAvailable", "()Z", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });

    registry.register(JVM, "counterTime", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(counter_time())))
    });
    registry.register(JVM, "nanosNow", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_nanos())))
    });
    registry.register(JVM, "getChunkStartNanos", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(epoch_nanos())))
    });
    registry.register(JVM, "getTicksFrequency", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(1_000_000_000)))
    });
    // Spec-correct, not a placeholder: the factor converts `counterTime()`
    // ticks to nanoseconds, and `counterTime()` above already returns
    // nanoseconds (`getTicksFrequency()` == 1e9 says the same thing). Any other
    // value would contradict those two.
    registry.register(JVM, "getTimeConversionFactor", "()D", |_ctx, _args| {
        Ok(Some(Value::Double(1.0)))
    });
    registry.register(JVM, "getPid", "()Ljava/lang/String;", |ctx, _args| {
        Ok(Some(Value::Object(Some(
            ctx.create_string(&std::process::id().to_string()),
        ))))
    });

    // Constant `false` answers that are STATEMENTS OF FACT about this VM, not
    // placeholders — each names the CratonVM property that makes it true:
    //   * `emitEvent`/`setThreshold`  — the Java-side per-event recorder is not
    //     wired to a chunk writer here (payload collection is owned by the Rust
    //     `jfr` crate), so no event is emitted through this boundary and the
    //     honest answer to "did you emit it?" is no;
    //   * `getAllowedToDoEventRetransforms`/`isInstrumented` — `retransform
    //     Classes` above is a no-op, so no event class is ever instrumented;
    //     answering `true` to either would contradict that no-op;
    //   * `shouldRotateDisk` — there is no on-disk chunk repository to rotate;
    //   * `isContainerized` — CratonVM reports host, not cgroup, limits.
    // `isExcluded(Thread)` is deliberately NOT in this list: it is a guard whose
    // answer must follow `exclude`/`include` (see below).
    for (name, descriptor) in [
        ("emitEvent", "(JJJ)Z"),
        ("setThreshold", "(JJ)Z"),
        ("getAllowedToDoEventRetransforms", "()Z"),
        ("isExcluded", "(Ljava/lang/Class;)Z"),
        ("isInstrumented", "(Ljava/lang/Class;)Z"),
        ("shouldRotateDisk", "()Z"),
        ("isContainerized", "()Z"),
    ] {
        registry.register(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(0))));
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
        registry.register(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(1))));
    }

    // Zero here means "no such id / nothing recorded", which is what a Java
    // caller gets on a JVM with no chunk repository: no stack trace has been
    // interned (`getStackTraceId`, `registerStackFilter`), no event has been
    // committed (`commit`), no event class has been unloaded
    // (`getUnloadedEventClassCount`). `hostTotalMemory`/`hostTotalSwapMemory`
    // are the one honest gap in this group: CratonVM has no portable host-RAM
    // probe, and 0 is the JDK's own "unknown" sentinel for them.
    // `getThreadId` is deliberately NOT in this list — see below.
    for (name, descriptor) in [
        ("getUnloadedEventClassCount", "()J"),
        ("getStackTraceId", "(IJ)J"),
        ("commit", "(J)J"),
        ("hostTotalMemory", "()J"),
        ("hostTotalSwapMemory", "()J"),
        (
            "registerStackFilter",
            "([Ljava/lang/String;[Ljava/lang/String;)J",
        ),
    ] {
        registry.register(JVM, name, descriptor, |_ctx, _args| {
            Ok(Some(Value::Long(0)))
        });
    }

    // `exclude`/`include` are the JFR thread filter and `isExcluded` is the
    // guard that reads it. As a no-op/no-op/constant-false trio the guard
    // contradicted the mutators outright: a caller could exclude a thread and
    // then be told by the JVM that the same thread was still being recorded.
    // Track the exclusions and answer the guard from the same table.
    registry.register(JVM, "exclude", "(Ljava/lang/Thread;)V", |ctx, args| {
        if let Some(key) = jfr_thread_key(ctx, args) {
            excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(key);
        }
        Ok(None)
    });
    registry.register(JVM, "include", "(Ljava/lang/Thread;)V", |ctx, args| {
        if let Some(key) = jfr_thread_key(ctx, args) {
            excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&key);
        }
        Ok(None)
    });
    registry.register(JVM, "isExcluded", "(Ljava/lang/Thread;)Z", |ctx, args| {
        let excluded = match jfr_thread_key(ctx, args) {
            Some(key) => excluded_threads()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .contains(&key),
            None => false,
        };
        Ok(Some(Value::Int(i32::from(excluded))))
    });

    // Every thread reported the same id (0), so any JFR consumer that groups
    // or joins records by thread collapsed the whole process onto one thread.
    // Report the receiver's own id, exactly as `java/lang/Thread.getId` does
    // (`tid` field, falling back to the executing VM thread).
    registry.register(
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
    );

    // OpenJDK's Type table uses these IDs as map-key identity.  Returning the
    // same placeholder for every type silently collapses the table and makes
    // standard values such as java.lang.String appear unsupported.
    registry.register(JVM, "getTypeId", "(Ljava/lang/Class;)J", |ctx, args| {
        Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args))))
    });
    registry.register(JVM, "getTypeId", "(Ljava/lang/String;)J", |ctx, args| {
        let id = match args.first() {
            Some(Value::Object(Some(name))) => ctx.read_string(*name).map(|name| type_id(&name)),
            _ => None,
        }
        .unwrap_or_default();
        Ok(Some(Value::Long(id)))
    });
    registry.register(JVM, "getClassId", "(Ljava/lang/Class;)J", |ctx, args| {
        Ok(Some(Value::Long(type_id_from_class_mirror(ctx, args))))
    });

    registry.register(
        JVM,
        "getAllEventClasses",
        "()Ljava/util/List;",
        |ctx, _args| ctx.new_object_initialized("java/util/ArrayList", "()V", &[]),
    );
    // A null `EventWriter` is the JDK's own "this thread has no chunk buffer
    // yet" answer and `EventWriterFactory` null-checks it, so returning it is
    // spec-correct rather than a stub. `newEventWriter` is the opposite: it is
    // the JDK's "make me one" call and its result is used unchecked, so a null
    // there is an NPE at a distance inside woven event bytecode. Both are in
    // practice unreachable — their only caller is the `commit` body that
    // `retransformClasses` weaves into an event class, and `retransform
    // Classes`/`isInstrumented` above say no class is ever instrumented — but
    // if that ever changes, fail by name at the actual gap instead of handing
    // back a null the caller cannot survive.
    registry.register(
        JVM,
        "getEventWriter",
        "()Ljdk/jfr/internal/event/EventWriter;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
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
    registry.register(
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
    );
    registry.register(
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
    );
    registry.register(JVM, "setDumpPath", "(Ljava/lang/String;)V", |ctx, args| {
        let value = match args.first() {
            Some(Value::Object(Some(path))) => ctx.read_string(*path),
            _ => None,
        };
        *dump_path()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = value;
        Ok(None)
    });
    registry.register(JVM, "getDumpPath", "()Ljava/lang/String;", |ctx, _args| {
        Ok(Some(saved_dump_path(ctx)))
    });
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
            let name = Value::Object(Some(ctx.create_string(&name)));
            ctx.invoke(
                "jdk/jfr/internal/Type",
                "getKnownType",
                "(Ljava/lang/String;)Ljdk/jfr/internal/Type;",
                &[name],
            )
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
        |ctx, args| Ok(known_jfr_type_for_class_mirror(ctx, args)),
    );
    // HotSpot's JDKEvents bootstrap registers native mirror events such as
    // jdk.MethodTrace. CratonVM records through its Rust JFR backend instead;
    // letting the Java bootstrap validate HotSpot-only mirror fields rejects a
    // perfectly usable RecordingStream before it can be configured.
    registry.register(
        "jdk/jfr/internal/JDKEvents",
        "initialize",
        "()V",
        |_ctx, _args| Ok(None),
    );
    // Without a HotSpot repository producer, EventDirectoryStream's Java
    // polling loop has no blocking source and spins forever. A stream with no
    // producer remains valid: configuration, handlers, and close work, while
    // startAsync exposes an empty stream instead of consuming a CPU.
    registry.register(
        "jdk/jfr/consumer/RecordingStream",
        "startAsync",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        JVM,
        "setMethodTraceFilters",
        "([Ljava/lang/String;[Ljava/lang/String;[Ljava/lang/String;[I)[J",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                ctx.new_array(ArrayElementType::Long, 0),
            ))))
        },
    );
    registry.register(JVM, "drainStaleMethodTracerIds", "()[J", |ctx, _args| {
        Ok(Some(Value::Object(Some(
            ctx.new_array(ArrayElementType::Long, 0),
        ))))
    });
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

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
                registry.find("jdk/jfr/internal/JVM", name, descriptor).is_some(),
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
