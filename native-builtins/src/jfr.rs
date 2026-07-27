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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
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
        ("exclude", "(Ljava/lang/Thread;)V"),
        ("include", "(Ljava/lang/Thread;)V"),
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
    registry.register(JVM, "getTimeConversionFactor", "()D", |_ctx, _args| {
        Ok(Some(Value::Double(1.0)))
    });
    registry.register(JVM, "getPid", "()Ljava/lang/String;", |ctx, _args| {
        Ok(Some(Value::Object(Some(
            ctx.create_string(&std::process::id().to_string()),
        ))))
    });

    for (name, descriptor) in [
        ("emitEvent", "(JJJ)Z"),
        ("setThreshold", "(JJ)Z"),
        ("getAllowedToDoEventRetransforms", "()Z"),
        ("isExcluded", "(Ljava/lang/Thread;)Z"),
        ("isExcluded", "(Ljava/lang/Class;)Z"),
        ("isInstrumented", "(Ljava/lang/Class;)Z"),
        ("shouldRotateDisk", "()Z"),
        ("isContainerized", "()Z"),
    ] {
        registry.register(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(0))));
    }
    for (name, descriptor) in [
        ("addStringConstant", "(JLjava/lang/String;)Z"),
        ("setCutoff", "(JJ)Z"),
        ("setThrottle", "(JJJ)Z"),
        (
            "setConfiguration",
            "(Ljava/lang/Class;Ljdk/jfr/internal/event/EventConfiguration;)Z",
        ),
        ("isProduct", "()Z"),
    ] {
        registry.register(JVM, name, descriptor, |_ctx, _args| Ok(Some(Value::Int(1))));
    }

    for (name, descriptor) in [
        ("getUnloadedEventClassCount", "()J"),
        ("getStackTraceId", "(IJ)J"),
        ("getThreadId", "(Ljava/lang/Thread;)J"),
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
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    registry.register(
        JVM,
        "getConfiguration",
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
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
