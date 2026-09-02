// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use smallvec::smallvec;

use crate::event::{
    EventField, EventInstance, EventPeriod, EventType, EventTypeId, EventTypeRegistry, EventValue,
    FieldKind,
};
use crate::recording::FlightRecorder;

// ---------------------------------------------------------------------------
// Per-emit-site event-type ID cache
// ---------------------------------------------------------------------------
//
// Each `emit_*` function needs to resolve a constant event-type name (e.g.
// "jdk.GarbageCollection") to an `EventTypeId`. Previously this was done via
// `recorder.type_registry.find_by_name(NAME)` — a hashmap probe of a literal
// string — on every call, even when no recording was active.
//
// We cache one resolved ID per call-site. A cache hit is accepted only for the
// registry instance and length that populated it, so the enabled path never
// reuses an ID assigned by another EventTypeRegistry.
//
// The cache is populated ONLY on a successful lookup. An unsuccessful lookup
// (registry not yet initialised, or type genuinely absent) must NOT be cached:
// a subsequent call may use a fully initialised registry and still succeed.

#[derive(Clone, Copy)]
struct CachedEventTypeId {
    registry_key: usize,
    registry_len: usize,
    id: EventTypeId,
}

type EventTypeIdCache = OnceLock<CachedEventTypeId>;

/// Look up an event-type ID by name, caching the result in the supplied
/// per-call-site cache. Returns `None` if the registry has no such type.
#[inline]
fn cached_event_id(
    cache: &'static EventTypeIdCache,
    recorder: &FlightRecorder,
    name: &str,
) -> Option<EventTypeId> {
    let registry = &recorder.type_registry;
    let registry_key = registry as *const EventTypeRegistry as usize;
    let registry_len = registry.len();

    if let Some(cached) = cache.get() {
        if cached.registry_key == registry_key && cached.registry_len == registry_len {
            if registry
                .get(cached.id)
                .is_some_and(|event_type| event_type.name.as_str() == name)
            {
                return Some(cached.id);
            }
        }
    }

    let id = registry.find_by_name(name)?;
    // Cache only successful lookups; misses may resolve later.
    if cache.get().is_none() {
        let _ = cache.set(CachedEventTypeId {
            registry_key,
            registry_len,
            id,
        });
    }
    Some(id)
}

/// Common emit path for every built-in `emit_*` function: validate the event's
/// runtime field shape against its registry-declared field types, then push it
/// onto the calling thread's ring shard.
///
/// Finding 4(b) hardening: the ~55 built-ins construct their `EventInstance`
/// fields by hand, so a future edit that desyncs a built-in's field shape from
/// its `register_builtin_events` declaration (wrong field count, or a value
/// variant that does not match the declared `type_name`) would silently
/// corrupt the on-disk chunk — the writer dispatches on the runtime variant
/// but the reader decodes off the declared type. Routing every built-in emit
/// through here runs the same declared-type validation that
/// [`emit_custom_event`] applies, **behind `debug_assert!`** so it fires loudly
/// in debug/test builds while costing nothing in release. On a release-build
/// mismatch we still push the event (built-ins have no `Result` to return and
/// historically never dropped events), matching prior behaviour.
#[inline]
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
fn push_builtin_event(recorder: &FlightRecorder, event: EventInstance) {
    #[cfg(debug_assertions)]
    if let Err(e) = validate_builtin_field_shape(recorder, event.type_id, &event.fields) {
        debug_assert!(
            false,
            "JFR built-in emit field shape does not match the registry \
             declaration for type_id {:?}: {}",
            event.type_id, e,
        );
    }
    crate::repository::push_to_thread_ring(event);
}

/// Validate `field_values` against the registry-declared field types for
/// `type_id` (the same declared-type check used by `emit_custom_event`'s
/// [`validate_and_lock_shape`], minus the per-event-type shape lock which only
/// the dynamic custom-event path needs). Returns `Ok(())` when the shape
/// matches; `Err(EmitError)` otherwise. Only ever called from a `debug_assert!`.
#[cfg(debug_assertions)]
fn validate_builtin_field_shape(
    recorder: &FlightRecorder,
    type_id: EventTypeId,
    field_values: &[EventValue],
) -> Result<(), EmitError> {
    let event_type = recorder
        .type_registry
        .get(type_id)
        .ok_or(EmitError::UnknownEventType)?;
    if event_type.fields.len() != field_values.len() {
        return Err(EmitError::FieldCountMismatch {
            expected: event_type.fields.len(),
            actual: field_values.len(),
        });
    }
    for (idx, (decl, value)) in event_type
        .fields
        .iter()
        .zip(field_values.iter())
        .enumerate()
    {
        let declared = FieldKind::from_declared(&decl.type_name).ok_or_else(|| {
            EmitError::UnknownDeclaredType {
                field_index: idx,
                declared: decl.type_name.clone(),
            }
        })?;
        let actual = FieldKind::of_value(value);
        if !actual.matches_declared(declared) {
            return Err(EmitError::DeclaredTypeMismatch {
                field_index: idx,
                declared,
                actual,
            });
        }
    }
    Ok(())
}

/// Register all built-in JVM event types into the given registry.
pub fn register_builtin_events(registry: &mut EventTypeRegistry) {
    // Helper to build an EventType shell (id is overwritten by register).
    let stub_id = EventTypeId(0);

    // 1. jdk.GarbageCollection
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GarbageCollection".into(),
        category: vec![
            "Java Virtual Machine".into(),
            "GC".into(),
            "Collector".into(),
        ],
        description: "Garbage collection".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("name", "string", "Name"),
            EventField::new("cause", "string", "Cause"),
            EventField::new("sumOfPauses", "long", "Sum of Pauses (ns)"),
            EventField::new("longestPause", "long", "Longest Pause (ns)"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 2. jdk.GCPhasePause
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GCPhasePause".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Phases".into()],
        description: "GC phase pause".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("name", "string", "Phase Name"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 3. jdk.YoungGarbageCollection
    registry.register(EventType {
        id: stub_id,
        name: "jdk.YoungGarbageCollection".into(),
        category: vec![
            "Java Virtual Machine".into(),
            "GC".into(),
            "Collector".into(),
        ],
        description: "Young generation garbage collection".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("tenuringThreshold", "int", "Tenuring Threshold"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 4. jdk.OldGarbageCollection
    registry.register(EventType {
        id: stub_id,
        name: "jdk.OldGarbageCollection".into(),
        category: vec![
            "Java Virtual Machine".into(),
            "GC".into(),
            "Collector".into(),
        ],
        description: "Old generation garbage collection".into(),
        fields: vec![EventField::new("gcId", "int", "GC Identifier")],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 5. jdk.ThreadStart
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ThreadStart".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Thread start".into(),
        fields: vec![
            EventField::new("thread", "string", "Java Thread"),
            EventField::new("parentThread", "string", "Parent Java Thread"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 6. jdk.ThreadEnd
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ThreadEnd".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Thread end".into(),
        fields: vec![EventField::new("thread", "string", "Java Thread")],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });

    // 7. jdk.ThreadSleep
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ThreadSleep".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Thread sleep".into(),
        fields: vec![EventField::new("time", "long", "Sleep Time (ns)")],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 8. jdk.ThreadPark
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ThreadPark".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Thread park".into(),
        fields: vec![
            EventField::new("parkedClass", "string", "Class Parked On"),
            EventField::new("timeout", "long", "Park Timeout (ns)"),
            EventField::new("address", "long", "Address of Object Parked On"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 8b. jdk.VirtualThreadPinned (JEP 491)
    registry.register(EventType {
        id: stub_id,
        name: "jdk.VirtualThreadPinned".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Virtual thread pinned to carrier".into(),
        fields: vec![
            EventField::new("threadName", "string", "Virtual Thread Name"),
            EventField::new("pinReason", "string", "Reason for Pinning"),
            EventField::new("virtualThreadId", "long", "Virtual Thread ID"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 9. jdk.JavaMonitorWait
    registry.register(EventType {
        id: stub_id,
        name: "jdk.JavaMonitorWait".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Waiting on a Java monitor".into(),
        fields: vec![
            EventField::new("monitorClass", "string", "Monitor Class"),
            EventField::new("notifier", "string", "Notifier Thread"),
            EventField::new("timeout", "long", "Timeout (ns)"),
            EventField::new("timedOut", "boolean", "Timed Out"),
            EventField::new("address", "long", "Monitor Address"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 10. jdk.JavaMonitorEnter
    registry.register(EventType {
        id: stub_id,
        name: "jdk.JavaMonitorEnter".into(),
        category: vec!["Java Application".into(), "Threading".into()],
        description: "Contended monitor enter".into(),
        fields: vec![
            EventField::new("monitorClass", "string", "Monitor Class"),
            EventField::new("previousOwner", "string", "Previous Monitor Owner"),
            EventField::new("address", "long", "Monitor Address"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 11. jdk.ClassLoad
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ClassLoad".into(),
        category: vec!["Java Virtual Machine".into(), "Class Loading".into()],
        description: "Class load".into(),
        fields: vec![
            EventField::new("loadedClass", "string", "Loaded Class"),
            EventField::new("definingClassLoader", "string", "Defining Class Loader"),
            EventField::new("initiatingClassLoader", "string", "Initiating Class Loader"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 12. jdk.ClassUnload
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ClassUnload".into(),
        category: vec!["Java Virtual Machine".into(), "Class Loading".into()],
        description: "Class unload".into(),
        fields: vec![
            EventField::new("unloadedClass", "string", "Unloaded Class"),
            EventField::new("definingClassLoader", "string", "Defining Class Loader"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });

    // 13. jdk.Compilation
    registry.register(EventType {
        id: stub_id,
        name: "jdk.Compilation".into(),
        category: vec!["Java Virtual Machine".into(), "Compiler".into()],
        description: "JIT compilation".into(),
        fields: vec![
            EventField::new("method", "string", "Java Method"),
            EventField::new("compileId", "int", "Compilation Identifier"),
            EventField::new("compileLevel", "int", "Compilation Level"),
            EventField::new("succeded", "boolean", "Succeeded"),
            EventField::new("isOsr", "boolean", "On Stack Replacement"),
            EventField::new("codeSize", "int", "Compiled Code Size"),
            EventField::new("inlinedBytes", "int", "Inlined Code Size"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(100)),
    });

    // 14. jdk.ObjectAllocationInNewTLAB
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ObjectAllocationInNewTLAB".into(),
        category: vec!["Java Application".into(), "Allocation".into()],
        description: "Object allocation in new TLAB".into(),
        fields: vec![
            EventField::new("objectClass", "string", "Object Class"),
            EventField::new("allocationSize", "long", "Allocation Size"),
            EventField::new("tlabSize", "long", "TLAB Size"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 15. jdk.ObjectAllocationOutsideTLAB
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ObjectAllocationOutsideTLAB".into(),
        category: vec!["Java Application".into(), "Allocation".into()],
        description: "Object allocation outside TLAB".into(),
        fields: vec![
            EventField::new("objectClass", "string", "Object Class"),
            EventField::new("allocationSize", "long", "Allocation Size"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 16. jdk.ExecutionSample
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ExecutionSample".into(),
        category: vec!["Java Virtual Machine".into(), "Profiling".into()],
        description: "Execution sample".into(),
        fields: vec![
            EventField::new("sampledThread", "string", "Sampled Thread"),
            EventField::new("stackTrace", "string", "Stack Trace"),
            EventField::new("state", "string", "Thread State"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 17. jdk.CPULoad
    registry.register(EventType {
        id: stub_id,
        name: "jdk.CPULoad".into(),
        category: vec!["Operating System".into(), "Processor".into()],
        description: "CPU load".into(),
        fields: vec![
            EventField::new("jvmUser", "float", "JVM User"),
            EventField::new("jvmSystem", "float", "JVM System"),
            EventField::new("machineTotal", "float", "Machine Total"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 18. jdk.JavaThreadStatistics
    registry.register(EventType {
        id: stub_id,
        name: "jdk.JavaThreadStatistics".into(),
        category: vec!["Java Virtual Machine".into(), "Threading".into()],
        description: "Java thread statistics".into(),
        fields: vec![
            EventField::new("activeCount", "long", "Active Thread Count"),
            EventField::new("daemonCount", "long", "Daemon Thread Count"),
            EventField::new("accumulatedCount", "long", "Accumulated Thread Count"),
            EventField::new("peakCount", "long", "Peak Thread Count"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 19. jdk.GCHeapSummary
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GCHeapSummary".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Heap".into()],
        description: "GC heap summary".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("when", "string", "When"),
            EventField::new("heapSpace", "string", "Heap Space"),
            EventField::new("heapUsed", "long", "Heap Used"),
            EventField::new("heapCommitted", "long", "Heap Committed Size"),
            EventField::new("heapMax", "long", "Heap Max Size"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 20. jdk.MetaspaceSummary
    registry.register(EventType {
        id: stub_id,
        name: "jdk.MetaspaceSummary".into(),
        category: vec![
            "Java Virtual Machine".into(),
            "GC".into(),
            "Metaspace".into(),
        ],
        description: "Metaspace summary".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("when", "string", "When"),
            EventField::new("metaspaceUsed", "long", "Metaspace Used"),
            EventField::new("metaspaceCommitted", "long", "Metaspace Committed"),
            EventField::new("metaspaceReserved", "long", "Metaspace Reserved"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 21. jdk.ActiveRecording
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ActiveRecording".into(),
        category: vec!["Flight Recorder".into()],
        description: "Active flight recording".into(),
        fields: vec![
            EventField::new("id", "long", "Recording Id"),
            EventField::new("name", "string", "Recording Name"),
            EventField::new("destination", "string", "Destination"),
            EventField::new("maxAge", "long", "Max Age (ns)"),
            EventField::new("maxSize", "long", "Max Size"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 22. jdk.ActiveSetting
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ActiveSetting".into(),
        category: vec!["Flight Recorder".into()],
        description: "Active recording setting".into(),
        fields: vec![
            EventField::new("id", "long", "Recording Id"),
            EventField::new("name", "string", "Setting Name"),
            EventField::new("value", "string", "Setting Value"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 24. jdk.Deoptimization
    registry.register(EventType {
        id: stub_id,
        name: "jdk.Deoptimization".into(),
        category: vec!["Java Virtual Machine".into(), "Compiler".into()],
        description: "Method deoptimization".into(),
        fields: vec![
            EventField::new("method", "string", "Java Method"),
            EventField::new("compileId", "int", "Compilation Id"),
            EventField::new("reason", "string", "Deoptimization Reason"),
            EventField::new("action", "string", "Deoptimization Action"),
            EventField::new("bci", "int", "Bytecode Index"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 25. jdk.FileRead
    registry.register(EventType {
        id: stub_id,
        name: "jdk.FileRead".into(),
        category: vec!["Java Application".into(), "File I/O".into()],
        description: "Reading data from a file".into(),
        fields: vec![
            EventField::new("path", "string", "File Path or Descriptor"),
            EventField::new("bytesRead", "long", "Bytes Read"),
            EventField::new("endOfFile", "boolean", "Reached End of File"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 26. jdk.FileWrite
    registry.register(EventType {
        id: stub_id,
        name: "jdk.FileWrite".into(),
        category: vec!["Java Application".into(), "File I/O".into()],
        description: "Writing data to a file".into(),
        fields: vec![
            EventField::new("path", "string", "File Path or Descriptor"),
            EventField::new("bytesWritten", "long", "Bytes Written"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 27. jdk.SocketRead
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SocketRead".into(),
        category: vec!["Java Application".into(), "Network".into()],
        description: "Reading data from a socket".into(),
        fields: vec![
            EventField::new("host", "string", "Remote Host"),
            EventField::new("port", "int", "Remote Port"),
            EventField::new("bytesRead", "long", "Bytes Read"),
            EventField::new("endOfStream", "boolean", "Reached End of Stream"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 28. jdk.SocketWrite
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SocketWrite".into(),
        category: vec!["Java Application".into(), "Network".into()],
        description: "Writing data to a socket".into(),
        fields: vec![
            EventField::new("host", "string", "Remote Host"),
            EventField::new("port", "int", "Remote Port"),
            EventField::new("bytesWritten", "long", "Bytes Written"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(10)),
    });

    // 29. jdk.SafepointBegin
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SafepointBegin".into(),
        category: vec!["Java Virtual Machine".into(), "Runtime".into()],
        description: "Safepoint begin".into(),
        fields: vec![
            EventField::new("safepointId", "long", "Safepoint Identifier"),
            EventField::new("totalThreadCount", "int", "Total Thread Count"),
            EventField::new("jniCriticalThreadCount", "int", "JNI Critical Thread Count"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 30. jdk.SafepointEnd
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SafepointEnd".into(),
        category: vec!["Java Virtual Machine".into(), "Runtime".into()],
        description: "Safepoint end".into(),
        fields: vec![EventField::new(
            "safepointId",
            "long",
            "Safepoint Identifier",
        )],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: Some(Duration::from_millis(0)),
    });

    // 31. jdk.ObjectAllocationSample (replaces ObjectAllocationInNewTLAB in newer JDKs)
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ObjectAllocationSample".into(),
        category: vec!["Java Application".into(), "Memory".into()],
        description: "Object allocation sample".into(),
        fields: vec![
            EventField::new("objectClass", "string", "Object Class"),
            EventField::new("weight", "long", "Sample Weight"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 32. jdk.NativeMethodSample
    registry.register(EventType {
        id: stub_id,
        name: "jdk.NativeMethodSample".into(),
        category: vec!["Java Virtual Machine".into(), "Profiling".into()],
        description: "Native method execution sample".into(),
        fields: vec![
            EventField::new("method", "string", "Method"),
            EventField::new("thread", "string", "Thread Name"),
            EventField::new("state", "string", "Thread State"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 33. jdk.AllocationRequiringGC
    registry.register(EventType {
        id: stub_id,
        name: "jdk.AllocationRequiringGC".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into()],
        description: "Allocation that triggered garbage collection".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("size", "long", "Allocation Size (bytes)"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 34. jdk.SystemProcess
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SystemProcess".into(),
        category: vec!["Operating System".into()],
        description: "System process info".into(),
        fields: vec![
            EventField::new("pid", "long", "Process ID"),
            EventField::new("commandLine", "string", "Command Line"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 35. jdk.InitialEnvironmentVariable
    registry.register(EventType {
        id: stub_id,
        name: "jdk.InitialEnvironmentVariable".into(),
        category: vec!["Operating System".into()],
        description: "Initial environment variable".into(),
        fields: vec![
            EventField::new("key", "string", "Key"),
            EventField::new("value", "string", "Value"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 36. jdk.SystemGC
    registry.register(EventType {
        id: stub_id,
        name: "jdk.SystemGC".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into()],
        description: "System.gc() invocation".into(),
        fields: vec![EventField::new(
            "invokedConcurrent",
            "boolean",
            "Invoked Concurrent",
        )],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 37. jdk.GCReferenceStatistics
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GCReferenceStatistics".into(),
        category: vec![
            "Java Virtual Machine".into(),
            "GC".into(),
            "Reference".into(),
        ],
        description: "GC reference processing statistics".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
            EventField::new("type", "string", "Reference Type"),
            EventField::new("count", "long", "Count"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::None,
        threshold: None,
    });

    // 38. jdk.ThreadCPULoad
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ThreadCPULoad".into(),
        category: vec!["Java Virtual Machine".into(), "Profiling".into()],
        description: "Per-thread CPU load".into(),
        fields: vec![
            EventField::new("user", "float", "User CPU Time"),
            EventField::new("system", "float", "System CPU Time"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 39. jdk.NetworkUtilization
    registry.register(EventType {
        id: stub_id,
        name: "jdk.NetworkUtilization".into(),
        category: vec!["Operating System".into(), "Network".into()],
        description: "Network interface utilization".into(),
        fields: vec![
            EventField::new("networkInterface", "string", "Network Interface"),
            EventField::new("readRate", "long", "Read Rate (bytes/s)"),
            EventField::new("writeRate", "long", "Write Rate (bytes/s)"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 40. jdk.PhysicalMemory
    registry.register(EventType {
        id: stub_id,
        name: "jdk.PhysicalMemory".into(),
        category: vec!["Operating System".into(), "Memory".into()],
        description: "Physical memory info".into(),
        fields: vec![
            EventField::new("totalSize", "long", "Total Size (bytes)"),
            EventField::new("usedSize", "long", "Used Size (bytes)"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 41. jdk.ContainerCPUUsage
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ContainerCPUUsage".into(),
        category: vec!["Operating System".into(), "Container".into()],
        description: "Container CPU usage".into(),
        fields: vec![
            EventField::new("cpuTime", "long", "CPU Time (ns)"),
            EventField::new("cpuUserTime", "long", "CPU User Time (ns)"),
            EventField::new("cpuSystemTime", "long", "CPU System Time (ns)"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 42. jdk.ContainerMemoryUsage
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ContainerMemoryUsage".into(),
        category: vec!["Operating System".into(), "Container".into()],
        description: "Container memory usage".into(),
        fields: vec![
            EventField::new("memoryUsage", "long", "Memory Usage (bytes)"),
            EventField::new("memoryLimit", "long", "Memory Limit (bytes)"),
            EventField::new("memoryFailCount", "long", "Fail Count"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EverySecond,
        threshold: None,
    });

    // 43. jdk.ExceptionStatistics
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ExceptionStatistics".into(),
        category: vec!["Java Application".into()],
        description: "Exception statistics".into(),
        fields: vec![EventField::new(
            "throwables",
            "long",
            "Total Throwables Created",
        )],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 44. jdk.JavaExceptionThrow
    registry.register(EventType {
        id: stub_id,
        name: "jdk.JavaExceptionThrow".into(),
        category: vec!["Java Application".into()],
        description: "Java exception thrown".into(),
        fields: vec![
            EventField::new("message", "string", "Exception Message"),
            EventField::new("thrownClass", "string", "Exception Class"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 45. jdk.JavaErrorThrow
    registry.register(EventType {
        id: stub_id,
        name: "jdk.JavaErrorThrow".into(),
        category: vec!["Java Application".into()],
        description: "Java error thrown".into(),
        fields: vec![
            EventField::new("message", "string", "Error Message"),
            EventField::new("thrownClass", "string", "Error Class"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 46. jdk.ModuleRequire
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ModuleRequire".into(),
        category: vec!["Java Virtual Machine".into(), "Modules".into()],
        description: "Module dependency".into(),
        fields: vec![
            EventField::new("source", "string", "Source Module"),
            EventField::new("requiredModule", "string", "Required Module"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 47. jdk.ModuleExport
    registry.register(EventType {
        id: stub_id,
        name: "jdk.ModuleExport".into(),
        category: vec!["Java Virtual Machine".into(), "Modules".into()],
        description: "Module export".into(),
        fields: vec![
            EventField::new("exportedPackage", "string", "Exported Package"),
            EventField::new("targetModule", "string", "Target Module"),
        ],
        has_thread: false,
        has_stacktrace: false,
        period: EventPeriod::EveryChunk,
        threshold: None,
    });

    // 48. cratonvm.JitCompileDecision
    //
    // The first `cratonvm.`-namespaced event in this registry, because it has
    // no `jdk.*` counterpart: HotSpot's `jdk.Compilation` reports what the
    // compiler DID, and this reports what it DECIDED and why. It is registered
    // here rather than in a private registry of its own (the way
    // `phase::PHASE_*_EVENT` is) precisely so `jfr print` and `jfr summary`
    // find it in the metadata section of an ordinary recording — an event that
    // exists but is not in the metadata is worse than no event at all.
    //
    // Seven fields, one under the `EVENT_FIELD_INLINE` = 8 inline capacity, so
    // this event still emits without spilling `EventInstance.fields` to the
    // heap. Class, method and descriptor are joined into one `method` string
    // rather than being three fields, matching `jdk.Compilation` and leaving
    // that headroom intact.
    //
    // `EventPeriod::BeginEnd` with `threshold: None`: a decision taken before
    // the compile runs has zero duration, and a threshold would then silently
    // drop exactly the refusals this event exists to report. The duration is
    // real once the call site moves to the end of a successful compile, and is
    // carried in `end_time` the way every other duration event here carries it.
    registry.register(EventType {
        id: stub_id,
        name: crate::jit_decision::JIT_COMPILE_DECISION_EVENT.into(),
        category: vec!["Java Virtual Machine".into(), "Compiler".into()],
        description: "JIT compile admission decision, and the reason for it".into(),
        fields: vec![
            EventField::new("method", "string", "Java Method"),
            EventField::new("door", "string", "Compile Door"),
            EventField::new("outcome", "string", "Compile Outcome"),
            EventField::new("reason", "string", "Decision Reason"),
            EventField::new("bailBci", "int", "Bail Bytecode Index (-1 if none)"),
            EventField::new("bailOpcode", "int", "Bail Opcode (-1 if none)"),
            EventField::new("bytecodeSize", "int", "Method Bytecode Size"),
        ],
        has_thread: true,
        has_stacktrace: false,
        period: EventPeriod::BeginEnd,
        threshold: None,
    });
}

// ---------------------------------------------------------------------------
// Per-thread JFR thread-id helper
// ---------------------------------------------------------------------------
//
// Round-4 (2026-05-17) fix: emit_* helpers that previously hardcoded
// `thread_id: 0` (16 sites — GC bookkeeping, periodic samples, recording
// metadata, etc.) now plumb a stable per-OS-thread id. The id is generated
// on first use per thread by an atomic counter and cached in a thread-local
// `Cell<u64>` so subsequent reads are a single TLS load.
//
// JFR semantics: `0` is conventionally "no thread" / "VM-internal".
// `JFR_THREAD_INTERNAL` (== 0) is exported for emit sites that intentionally
// represent VM-internal threads (e.g. GC worker, JIT compiler thread) whose
// real OS thread id is not yet plumbed through. The new id helper preserves
// 0 as the unused sentinel by starting the counter at 1.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

/// Sentinel value meaning "no thread / VM internal".
pub const JFR_THREAD_INTERNAL: u64 = 0;

static NEXT_JFR_THREAD_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// 0 = uninitialised (the first call lazily assigns from
    /// `NEXT_JFR_THREAD_ID`). Subsequent calls return the cached value.
    static JFR_THREAD_ID: Cell<u64> = const { Cell::new(0) };
}

/// Return a stable JFR thread id for the calling OS thread.
///
/// The id is allocated lazily on first call and is unique across the process
/// lifetime. It is guaranteed non-zero so the `JFR_THREAD_INTERNAL == 0`
/// sentinel remains distinct.
///
/// Round-4 hot-path cost (steady state): one TLS read + branch (no atomics).
/// First-call cost: one Relaxed `fetch_add` on the global counter.
#[inline]
pub fn current_jfr_thread_id() -> u64 {
    JFR_THREAD_ID.with(|cell| {
        let id = cell.get();
        if id != 0 {
            return id;
        }
        let new_id = NEXT_JFR_THREAD_ID.fetch_add(1, AtomicOrdering::Relaxed);
        cell.set(new_id);
        new_id
    })
}

// ---------------------------------------------------------------------------
// Emission helper functions
// ---------------------------------------------------------------------------

/// Emit a garbage collection event.
///
/// Called at the end of a GC cycle with the cause string and total pause duration.
pub fn emit_gc_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    name: &'static str,
    cause: &'static str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GarbageCollection") {
        // Round-4: name/cause are now `&'static str` (every caller passes
        // a string literal — GC collector name and pause cause are part of
        // a fixed taxonomy). `EventValue::Str` avoids the per-event
        // `Arc::from(&str)` heap allocation.
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::Int(gc_id));
        fields.push(EventValue::Str(name));
        fields.push(EventValue::Str(cause));
        let pause_ns = duration_ns.min(i64::MAX as u64) as i64;
        fields.push(EventValue::Long(pause_ns)); // sumOfPauses
        fields.push(EventValue::Long(pause_ns)); // longestPause
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(), // GC manager thread
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a class load event.
///
/// Called when a class is successfully loaded and linked.
///
/// Round-5: `defining_loader` and `initiating_loader` are now `&'static str`
/// because every in-tree caller already passes a literal from the fixed
/// taxonomy ("bootstrap", "app", "platform"). This eliminates the two
/// per-event `Arc::from(&str)` heap allocations on the class-load hot path.
/// `class_name` remains dynamic — use `emit_class_load_event_arc` to plumb
/// a pre-interned `Arc<str>` for it.
pub fn emit_class_load_event(
    recorder: &mut FlightRecorder,
    class_name: &str,
    defining_loader: &'static str,
    initiating_loader: &'static str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassLoad") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(class_name)));
        fields.push(EventValue::Str(defining_loader));
        fields.push(EventValue::Str(initiating_loader));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_class_load_event` variant accepting a pre-interned `Arc<str>` for
/// the class name. Use this from sites where the constant-pool string is
/// already in hand (skips the per-event `Arc::from(&str)` reallocation).
pub fn emit_class_load_event_arc(
    recorder: &mut FlightRecorder,
    class_name: Arc<str>,
    defining_loader: &'static str,
    initiating_loader: &'static str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassLoad") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(class_name));
        fields.push(EventValue::Str(defining_loader));
        fields.push(EventValue::Str(initiating_loader));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a thread start event.
///
/// Called when a new Java thread is started.
///
/// Round-5: `parent_thread_name` is now `&'static str`. In-tree callers pass
/// the fixed-taxonomy literals "virtual" / "platform"; switching to `Str`
/// saves the per-event `Arc::from(&str)` allocation.
pub fn emit_thread_start_event(
    recorder: &mut FlightRecorder,
    thread_name: &str,
    parent_thread_name: &'static str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadStart") {
        // `thread_name` is the user-supplied Java thread name — still Arc.
        // Round-9 HIGH-5: callers that already hold an `Arc<str>` should
        // prefer `emit_thread_start_event_arc` below to avoid this realloc.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(thread_name)));
        fields.push(EventValue::Str(parent_thread_name));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-9 HIGH-5: `emit_thread_start_event` variant accepting a pre-interned
/// `Arc<str>` for the thread name. Use this from sites where the Thread
/// object's interned name is already in hand (skips the per-event
/// `Arc::from(&str)` reallocation).
pub fn emit_thread_start_event_arc(
    recorder: &mut FlightRecorder,
    thread_name: Arc<str>,
    parent_thread_name: &'static str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadStart") {
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(thread_name));
        fields.push(EventValue::Str(parent_thread_name));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a JIT compilation event.
///
/// Called after a method is compiled by the JIT compiler.
pub fn emit_compilation_event(
    recorder: &mut FlightRecorder,
    method: &str,
    compile_id: i32,
    compile_level: i32,
    succeeded: bool,
    is_osr: bool,
    code_size: i32,
    inlined_bytes: i32,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Compilation") {
        // `method` is a fully-qualified Java method descriptor, dynamic — Arc.
        // Prefer `emit_compilation_event_arc` when the method descriptor is
        // already interned as `Arc<str>` by the JIT layer.
        let mut fields = crate::event::EventFields::with_capacity(7);
        fields.push(EventValue::String(Arc::from(method)));
        fields.push(EventValue::Int(compile_id));
        fields.push(EventValue::Int(compile_level));
        fields.push(EventValue::Boolean(succeeded));
        fields.push(EventValue::Boolean(is_osr));
        fields.push(EventValue::Int(code_size));
        fields.push(EventValue::Int(inlined_bytes));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_compilation_event` variant accepting a pre-interned `Arc<str>` for
/// the method descriptor. Skips the per-event `Arc::from(&str)`.
pub fn emit_compilation_event_arc(
    recorder: &mut FlightRecorder,
    method: Arc<str>,
    compile_id: i32,
    compile_level: i32,
    succeeded: bool,
    is_osr: bool,
    code_size: i32,
    inlined_bytes: i32,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Compilation") {
        let mut fields = crate::event::EventFields::with_capacity(7);
        fields.push(EventValue::String(method));
        fields.push(EventValue::Int(compile_id));
        fields.push(EventValue::Int(compile_level));
        fields.push(EventValue::Boolean(succeeded));
        fields.push(EventValue::Boolean(is_osr));
        fields.push(EventValue::Int(code_size));
        fields.push(EventValue::Int(inlined_bytes));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a `cratonvm.JitCompileDecision` — the compiler's admission verdict for
/// one method, at one door.
///
/// This is the delivery end of [`crate::jit_decision`]; see that module for why
/// the event exists, how it is armed, and why the producer reaches it through
/// an installed sink rather than calling this directly. **The producer must
/// have checked [`crate::jit_decision::jit_decision_enabled`] before building
/// the `decision`** — the `is_enabled()` guard below is the ordinary built-in
/// fast path and cannot refund a `String` that has already been formatted.
///
/// Field order here must match the `cratonvm.JitCompileDecision` declaration in
/// `register_builtin_events` exactly; `push_builtin_event` debug-asserts that it
/// does, because the reader decodes off the declared `type_name` and a desync
/// would corrupt every later event in the chunk.
pub fn emit_jit_compile_decision_event(
    recorder: &mut FlightRecorder,
    decision: &crate::jit_decision::JitCompileDecision<'_>,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    let name = crate::jit_decision::JIT_COMPILE_DECISION_EVENT;
    if let Some(type_id) = cached_event_id(&ID, recorder, name) {
        // The same `class.name+descriptor` spelling the `[ir] admission` stderr
        // line uses, so a JFR dump and a `CRATONVM_DBG_JITC` log name the same
        // method identically and can be joined without a translation step.
        let method = format!(
            "{}.{}{}",
            decision.class_name, decision.method_name, decision.method_descriptor
        );
        let mut fields = crate::event::EventFields::with_capacity(7);
        fields.push(EventValue::String(Arc::from(method.as_str())));
        fields.push(EventValue::Str(decision.door.name()));
        fields.push(EventValue::Str(decision.outcome.name()));
        fields.push(decision.reason.to_event_value());
        fields.push(EventValue::Int(decision.bail_bci));
        fields.push(EventValue::Int(decision.bail_opcode));
        fields.push(EventValue::Int(decision.bytecode_size));
        let event = EventInstance {
            type_id,
            start_time: decision.start_time_ns,
            end_time: decision
                .start_time_ns
                .saturating_add(decision.duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a thread end event.
///
/// Called when a Java thread terminates.
pub fn emit_thread_end_event(
    recorder: &mut FlightRecorder,
    thread_name: &str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadEnd") {
        // Round-9 HIGH-5: prefer `emit_thread_end_event_arc` below when the
        // Thread name is already interned as an `Arc<str>`.
        let mut fields = crate::event::EventFields::with_capacity(1);
        fields.push(EventValue::String(Arc::from(thread_name)));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-9 HIGH-5: `emit_thread_end_event` variant accepting a pre-interned
/// `Arc<str>` for the thread name.
pub fn emit_thread_end_event_arc(
    recorder: &mut FlightRecorder,
    thread_name: Arc<str>,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadEnd") {
        let mut fields = crate::event::EventFields::with_capacity(1);
        fields.push(EventValue::String(thread_name));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a thread sleep event.
///
/// Called when Thread.sleep() is invoked.
pub fn emit_thread_sleep_event(
    recorder: &mut FlightRecorder,
    sleep_time_ns: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadSleep") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![EventValue::Long(sleep_time_ns),],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a Java monitor wait event.
///
/// Called when Object.wait() is invoked on a contended monitor.
pub fn emit_monitor_wait_event(
    recorder: &mut FlightRecorder,
    monitor_class: &str,
    notifier_thread: &'static str,
    timeout_ns: i64,
    timed_out: bool,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorWait") {
        // Round-5: `notifier_thread` callers pass literals ("unknown" today;
        // future "GC", "main", etc. taxonomies) — `Str` skips one Arc alloc.
        // `monitor_class` remains dynamic; use `*_arc` variant to plumb a
        // pre-interned `Arc<str>` from the class metadata.
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::String(Arc::from(monitor_class)));
        fields.push(EventValue::Str(notifier_thread));
        fields.push(EventValue::Long(timeout_ns));
        fields.push(EventValue::Boolean(timed_out));
        fields.push(EventValue::Long(address));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_monitor_wait_event` variant accepting a pre-interned `Arc<str>` for
/// the monitor class — skips the per-event `Arc::from(&str)` reallocation
/// when the caller already holds the runtime class name as `Arc<str>`.
pub fn emit_monitor_wait_event_arc(
    recorder: &mut FlightRecorder,
    monitor_class: Arc<str>,
    notifier_thread: &'static str,
    timeout_ns: i64,
    timed_out: bool,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorWait") {
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::String(monitor_class));
        fields.push(EventValue::Str(notifier_thread));
        fields.push(EventValue::Long(timeout_ns));
        fields.push(EventValue::Boolean(timed_out));
        fields.push(EventValue::Long(address));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a Java monitor enter event.
///
/// Called when entering a contended monitor (synchronized block).
///
/// Round-5: `previous_owner` is now `&'static str` (callers in tree pass
/// "unknown" / thread-state literals). For `monitor_class`, use
/// `emit_monitor_enter_event_arc` when the runtime class is already
/// available as `Arc<str>` to skip the per-event allocation.
pub fn emit_monitor_enter_event(
    recorder: &mut FlightRecorder,
    monitor_class: &str,
    previous_owner: &'static str,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorEnter") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(monitor_class)));
        fields.push(EventValue::Str(previous_owner));
        fields.push(EventValue::Long(address));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_monitor_enter_event` variant accepting a pre-interned `Arc<str>`
/// for the monitor class. Use when the runtime class metadata already holds
/// an `Arc<str>` to skip the per-event allocation.
pub fn emit_monitor_enter_event_arc(
    recorder: &mut FlightRecorder,
    monitor_class: Arc<str>,
    previous_owner: &'static str,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorEnter") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(monitor_class));
        fields.push(EventValue::Str(previous_owner));
        fields.push(EventValue::Long(address));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a class unload event.
///
/// Called when a class is unloaded by the GC.
///
/// Round-5: `defining_loader` is now `&'static str` (callers pass loader
/// taxonomy literals — "bootstrap", "app", "platform").
pub fn emit_class_unload_event(
    recorder: &mut FlightRecorder,
    class_name: &str,
    defining_loader: &'static str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassUnload") {
        // class_name is dynamic; defining_loader is taxonomy literal.
        // Prefer `emit_class_unload_event_arc` when an interned `Arc<str>` is
        // already on hand from class metadata — avoids the per-event
        // `Arc::from(&str)` reallocation on the unload hot path.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(class_name)));
        fields.push(EventValue::Str(defining_loader));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_class_unload_event` variant accepting a pre-interned
/// `Arc<str>` for the class name. Callers in the class-loading layer already
/// hold the class name as an `Arc<str>` from `Class.name_arc` — pass it
/// through directly to skip the per-event `Arc::from(&str)` reallocation.
pub fn emit_class_unload_event_arc(
    recorder: &mut FlightRecorder,
    class_name: Arc<str>,
    defining_loader: &'static str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassUnload") {
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(class_name));
        fields.push(EventValue::Str(defining_loader));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a thread park event.
///
/// Called when LockSupport.park() is invoked.
pub fn emit_thread_park_event(
    recorder: &mut FlightRecorder,
    parked_class: &'static str,
    timeout_ns: i64,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadPark") {
        // Round-4: `parked_class` is always a literal class name (`LockSupport`).
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::Str(parked_class));
        fields.push(EventValue::Long(timeout_ns));
        fields.push(EventValue::Long(address));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a virtual thread pinned event (JEP 491 — JDK 24+).
///
/// Fired when a virtual thread attempts to park while holding a monitor
/// (synchronized block) or inside a native method.
pub fn emit_virtual_thread_pinned_event(
    recorder: &mut FlightRecorder,
    thread_name: &str,
    pin_reason: &'static str,
    carrier_thread_id: u64,
    virtual_thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.VirtualThreadPinned") {
        // Round-4: `pin_reason` is a JEP-491 enum literal (e.g. "Synchronized",
        // "Native"). Thread name is dynamic. Prefer
        // `emit_virtual_thread_pinned_event_arc` from sites holding the
        // virtual-thread name as `Arc<str>` already.
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(thread_name)));
        fields.push(EventValue::Str(pin_reason));
        fields.push(EventValue::Long(virtual_thread_id as i64));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: carrier_thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_virtual_thread_pinned_event` variant accepting a
/// pre-interned `Arc<str>` for the thread name. Virtual-thread machinery
/// already keeps the name as `Arc<str>` on the Thread object, so callers
/// can clone-and-pass without re-allocating.
pub fn emit_virtual_thread_pinned_event_arc(
    recorder: &mut FlightRecorder,
    thread_name: Arc<str>,
    pin_reason: &'static str,
    carrier_thread_id: u64,
    virtual_thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.VirtualThreadPinned") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(thread_name));
        fields.push(EventValue::Str(pin_reason));
        fields.push(EventValue::Long(virtual_thread_id as i64));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: carrier_thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a GC heap summary event.
pub fn emit_gc_heap_summary_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    when: &'static str,
    heap_space: &'static str,
    heap_used: i64,
    heap_committed: i64,
    heap_max: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GCHeapSummary") {
        // Round-4: `when` is "Before GC" / "After GC", `heap_space` is "Eden",
        // "Survivor", "Old", etc. — fixed enum-like literals.
        let mut fields = crate::event::EventFields::with_capacity(6);
        fields.push(EventValue::Int(gc_id));
        fields.push(EventValue::Str(when));
        fields.push(EventValue::Str(heap_space));
        fields.push(EventValue::Long(heap_used));
        fields.push(EventValue::Long(heap_committed));
        fields.push(EventValue::Long(heap_max));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an object allocation in new TLAB event.
///
/// Called when the allocator needs a new TLAB for an allocation.
pub fn emit_allocation_in_new_tlab_event(
    recorder: &mut FlightRecorder,
    object_class: &str,
    allocation_size: i64,
    tlab_size: i64,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationInNewTLAB") {
        // `object_class` is a Java class name — fully dynamic.
        // Prefer `emit_allocation_in_new_tlab_event_arc` from callers that
        // already hold an `Arc<str>` interned by the runtime constant pool
        // to skip the per-event `Arc::from(&str)`.
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(object_class)));
        fields.push(EventValue::Long(allocation_size));
        fields.push(EventValue::Long(tlab_size));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an object allocation outside TLAB event.
pub fn emit_allocation_outside_tlab_event(
    recorder: &mut FlightRecorder,
    object_class: &str,
    allocation_size: i64,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationOutsideTLAB") {
        // Prefer `emit_allocation_outside_tlab_event_arc` from callers that
        // already hold a constant-pool `Arc<str>`.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(object_class)));
        fields.push(EventValue::Long(allocation_size));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_allocation_in_new_tlab_event` variant accepting a pre-interned
/// `Arc<str>` for the object class. Skips the per-event `Arc::from(&str)`
/// reallocation on the allocation hot path.
pub fn emit_allocation_in_new_tlab_event_arc(
    recorder: &mut FlightRecorder,
    object_class: Arc<str>,
    allocation_size: i64,
    tlab_size: i64,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationInNewTLAB") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(object_class));
        fields.push(EventValue::Long(allocation_size));
        fields.push(EventValue::Long(tlab_size));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_allocation_outside_tlab_event` variant accepting a pre-interned
/// `Arc<str>` for the object class.
pub fn emit_allocation_outside_tlab_event_arc(
    recorder: &mut FlightRecorder,
    object_class: Arc<str>,
    allocation_size: i64,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationOutsideTLAB") {
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(object_class));
        fields.push(EventValue::Long(allocation_size));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a GC phase pause event.
pub fn emit_gc_phase_pause_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    phase_name: &'static str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GCPhasePause") {
        // Round-4: `phase_name` is from the fixed GC-phase taxonomy
        // ("Pause Init Mark", "Pause Remark", etc.) — always a literal.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::Int(gc_id));
        fields.push(EventValue::Str(phase_name));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a young generation GC event.
pub fn emit_young_gc_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    tenuring_threshold: i32,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.YoungGarbageCollection") {
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::Int(gc_id));
        fields.push(EventValue::Int(tenuring_threshold));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an old generation GC event.
pub fn emit_old_gc_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.OldGarbageCollection") {
        let mut fields = crate::event::EventFields::with_capacity(1);
        fields.push(EventValue::Int(gc_id));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a metaspace summary event.
pub fn emit_metaspace_summary_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    when: &'static str,
    metaspace_used: i64,
    metaspace_committed: i64,
    metaspace_reserved: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.MetaspaceSummary") {
        // Round-4: `when` is "Before GC" / "After GC" — literal.
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::Int(gc_id));
        fields.push(EventValue::Str(when));
        fields.push(EventValue::Long(metaspace_used));
        fields.push(EventValue::Long(metaspace_committed));
        fields.push(EventValue::Long(metaspace_reserved));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an execution sample event (profiling).
pub fn emit_execution_sample_event(
    recorder: &mut FlightRecorder,
    sampled_thread: &str,
    stack_trace: &str,
    state: &'static str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ExecutionSample") {
        // Round-4: `state` is a fixed JVM thread-state enum
        // ("RUNNABLE", "BLOCKED", ...) — literal. Thread name and stack
        // trace are dynamic — prefer `emit_execution_sample_event_arc`
        // when the profiler already holds pre-interned `Arc<str>` for them.
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(sampled_thread)));
        fields.push(EventValue::String(Arc::from(stack_trace)));
        fields.push(EventValue::Str(state));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_execution_sample_event` variant accepting pre-interned `Arc<str>`
/// for the thread name and stack trace. Skips two per-event allocations on
/// the profiler hot path.
pub fn emit_execution_sample_event_arc(
    recorder: &mut FlightRecorder,
    sampled_thread: Arc<str>,
    stack_trace: Arc<str>,
    state: &'static str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ExecutionSample") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(sampled_thread));
        fields.push(EventValue::String(stack_trace));
        fields.push(EventValue::Str(state));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a CPU load event.
pub fn emit_cpu_load_event(
    recorder: &mut FlightRecorder,
    jvm_user: f32,
    jvm_system: f32,
    machine_total: f32,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.CPULoad") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::Float(jvm_user));
        fields.push(EventValue::Float(jvm_system));
        fields.push(EventValue::Float(machine_total));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a Java thread statistics event.
pub fn emit_thread_statistics_event(
    recorder: &mut FlightRecorder,
    active_count: i64,
    daemon_count: i64,
    accumulated_count: i64,
    peak_count: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaThreadStatistics") {
        let mut fields = crate::event::EventFields::with_capacity(4);
        fields.push(EventValue::Long(active_count));
        fields.push(EventValue::Long(daemon_count));
        fields.push(EventValue::Long(accumulated_count));
        fields.push(EventValue::Long(peak_count));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an active recording metadata event.
pub fn emit_active_recording_event(
    recorder: &mut FlightRecorder,
    recording_id: i64,
    name: &str,
    destination: &str,
    max_age_ns: i64,
    max_size: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ActiveRecording") {
        // Recording metadata: `name` and `destination` are user-supplied —
        // keep `&str` + Arc. Round-4: thread_id from per-thread cache.
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::Long(recording_id));
        fields.push(EventValue::String(Arc::from(name)));
        fields.push(EventValue::String(Arc::from(destination)));
        fields.push(EventValue::Long(max_age_ns));
        fields.push(EventValue::Long(max_size));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an active setting metadata event.
///
/// Prefer `emit_active_setting_event_arc` from sites that read settings out
/// of a settings table keyed on `Arc<str>` already.
pub fn emit_active_setting_event(
    recorder: &mut FlightRecorder,
    recording_id: i64,
    name: &str,
    value: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ActiveSetting") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::Long(recording_id));
        fields.push(EventValue::String(Arc::from(name)));
        fields.push(EventValue::String(Arc::from(value)));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_active_setting_event` variant accepting pre-interned
/// `Arc<str>` for both name and value. The recording-settings layer reads
/// from a map keyed on `Arc<str>`; cloning the Arcs is an atomic increment.
pub fn emit_active_setting_event_arc(
    recorder: &mut FlightRecorder,
    recording_id: i64,
    name: Arc<str>,
    value: Arc<str>,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ActiveSetting") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::Long(recording_id));
        fields.push(EventValue::String(name));
        fields.push(EventValue::String(value));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a deoptimization event.
///
/// Called when the JIT deoptimizes a compiled method.
///
/// Prefer `emit_deoptimization_event_arc` from JIT call sites that hold an
/// interned `Arc<str>` for the method descriptor.
pub fn emit_deoptimization_event(
    recorder: &mut FlightRecorder,
    method: &str,
    compile_id: i32,
    reason: &'static str,
    action: &'static str,
    bci: i32,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Deoptimization") {
        // Round-4: `reason` and `action` are fixed JIT taxonomies
        // ("class_check", "reinterpret", etc.). Method is dynamic.
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::String(Arc::from(method)));
        fields.push(EventValue::Int(compile_id));
        fields.push(EventValue::Str(reason));
        fields.push(EventValue::Str(action));
        fields.push(EventValue::Int(bci));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_deoptimization_event` variant accepting a
/// pre-interned `Arc<str>` for the method descriptor. JIT compilation
/// records already key on `Arc<str>` method names, so the JIT layer can
/// pass them through without the per-event `Arc::from(&str)` reallocation.
pub fn emit_deoptimization_event_arc(
    recorder: &mut FlightRecorder,
    method: Arc<str>,
    compile_id: i32,
    reason: &'static str,
    action: &'static str,
    bci: i32,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Deoptimization") {
        let mut fields = crate::event::EventFields::with_capacity(5);
        fields.push(EventValue::String(method));
        fields.push(EventValue::Int(compile_id));
        fields.push(EventValue::Str(reason));
        fields.push(EventValue::Str(action));
        fields.push(EventValue::Int(bci));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a file read event.
///
/// Prefer `emit_file_read_event_arc` from sites holding the resolved
/// canonical path as `Arc<str>` (the FD layer caches this per descriptor).
pub fn emit_file_read_event(
    recorder: &mut FlightRecorder,
    path: &str,
    bytes_read: i64,
    end_of_file: bool,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(Arc::from(path)),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_file),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_file_read_event` variant accepting a pre-interned
/// `Arc<str>` for the path. File descriptors keep the resolved canonical
/// path as `Arc<str>` per descriptor; cloning the Arc costs an atomic
/// increment instead of a heap allocation + UTF-8 copy.
pub fn emit_file_read_event_arc(
    recorder: &mut FlightRecorder,
    path: Arc<str>,
    bytes_read: i64,
    end_of_file: bool,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(path),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_file),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a file write event.
///
/// Prefer `emit_file_write_event_arc` from FD-aware call sites.
pub fn emit_file_write_event(
    recorder: &mut FlightRecorder,
    path: &str,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(Arc::from(path)),
                EventValue::Long(bytes_written),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_file_write_event` variant accepting a pre-interned
/// `Arc<str>` for the path.
pub fn emit_file_write_event_arc(
    recorder: &mut FlightRecorder,
    path: Arc<str>,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![EventValue::String(path), EventValue::Long(bytes_written),],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a socket read event.
///
/// Prefer `emit_socket_read_event_arc` from socket channels that already
/// cache the resolved host as `Arc<str>`.
pub fn emit_socket_read_event(
    recorder: &mut FlightRecorder,
    host: &str,
    port: i32,
    bytes_read: i64,
    end_of_stream: bool,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(Arc::from(host)),
                EventValue::Int(port),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_stream),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_socket_read_event` variant accepting a pre-interned
/// `Arc<str>` for the host. Socket channels cache the resolved host once
/// per connect; cloning the Arc per event is an atomic increment.
pub fn emit_socket_read_event_arc(
    recorder: &mut FlightRecorder,
    host: Arc<str>,
    port: i32,
    bytes_read: i64,
    end_of_stream: bool,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(host),
                EventValue::Int(port),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_stream),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a socket write event.
///
/// Prefer `emit_socket_write_event_arc` from socket-aware call sites.
pub fn emit_socket_write_event(
    recorder: &mut FlightRecorder,
    host: &str,
    port: i32,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(Arc::from(host)),
                EventValue::Int(port),
                EventValue::Long(bytes_written),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_socket_write_event` variant accepting a pre-interned
/// `Arc<str>` for the host.
pub fn emit_socket_write_event_arc(
    recorder: &mut FlightRecorder,
    host: Arc<str>,
    port: i32,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: smallvec![
                EventValue::String(host),
                EventValue::Int(port),
                EventValue::Long(bytes_written),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a safepoint begin event.
pub fn emit_safepoint_begin_event(
    recorder: &mut FlightRecorder,
    safepoint_id: i64,
    total_threads: i32,
    jni_critical_threads: i32,
    start_time_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SafepointBegin") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::Long(safepoint_id));
        fields.push(EventValue::Int(total_threads));
        fields.push(EventValue::Int(jni_critical_threads));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns,
            // Round-4: VM thread that initiated the safepoint.
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a safepoint end event.
pub fn emit_safepoint_end_event(
    recorder: &mut FlightRecorder,
    safepoint_id: i64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SafepointEnd") {
        let mut fields = crate::event::EventFields::with_capacity(1);
        fields.push(EventValue::Long(safepoint_id));
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a System.gc() event.
pub fn emit_system_gc_event(
    recorder: &mut FlightRecorder,
    invoked_concurrent: bool,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SystemGC") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: smallvec![EventValue::Boolean(invoked_concurrent)],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an allocation-requiring-GC event.
pub fn emit_allocation_requiring_gc_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    size: i64,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.AllocationRequiringGC") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: smallvec![EventValue::Int(gc_id), EventValue::Long(size)],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a Java exception throw event.
pub fn emit_java_exception_throw_event(
    recorder: &mut FlightRecorder,
    message: &str,
    thrown_class: &str,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaExceptionThrow") {
        // message and thrown_class are dynamic; Arc. Prefer
        // `emit_java_exception_throw_event_arc` from sites holding the
        // throwable's class name as `Arc<str>` (always true after the
        // throwable layer has consulted Class metadata).
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(message)));
        fields.push(EventValue::String(Arc::from(thrown_class)));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_java_exception_throw_event` variant accepting
/// pre-interned `Arc<str>` for the throwable's class name (always available
/// from `Class.name_arc`) and message (which the throwable already keeps as
/// an `Arc<str>` after the detail-message field has been read). Halves the
/// per-throw heap allocations.
pub fn emit_java_exception_throw_event_arc(
    recorder: &mut FlightRecorder,
    message: Arc<str>,
    thrown_class: Arc<str>,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaExceptionThrow") {
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(message));
        fields.push(EventValue::String(thrown_class));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a network utilization event.
///
/// Prefer `emit_network_utilization_event_arc` for periodic samplers that
/// cache interface names as `Arc<str>` once per host probe.
pub fn emit_network_utilization_event(
    recorder: &mut FlightRecorder,
    interface: &str,
    read_rate: i64,
    write_rate: i64,
    time_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.NetworkUtilization") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(Arc::from(interface)));
        fields.push(EventValue::Long(read_rate));
        fields.push(EventValue::Long(write_rate));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Round-5 Fix 2: `emit_network_utilization_event` variant accepting a
/// pre-interned `Arc<str>` for the interface name. Per-chunk samplers
/// resolve the interface list once and keep `Arc<str>` per NIC; cloning
/// the Arc is an atomic increment instead of a heap allocation per emit.
pub fn emit_network_utilization_event_arc(
    recorder: &mut FlightRecorder,
    interface: Arc<str>,
    read_rate: i64,
    write_rate: i64,
    time_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.NetworkUtilization") {
        let mut fields = crate::event::EventFields::with_capacity(3);
        fields.push(EventValue::String(interface));
        fields.push(EventValue::Long(read_rate));
        fields.push(EventValue::Long(write_rate));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a thread CPU load event.
pub fn emit_thread_cpu_load_event(
    recorder: &mut FlightRecorder,
    user: f32,
    system: f32,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadCPULoad") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: smallvec![EventValue::Float(user), EventValue::Float(system),],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit an object allocation sample event.
///
/// Prefer `emit_allocation_sample_event_arc` from sites holding the class
/// name as a constant-pool `Arc<str>` already.
pub fn emit_allocation_sample_event(
    recorder: &mut FlightRecorder,
    object_class: &str,
    weight: i64,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationSample") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: smallvec![
                EventValue::String(Arc::from(object_class)),
                EventValue::Long(weight),
            ],
        };
        push_builtin_event(recorder, event);
    }
}

/// `emit_allocation_sample_event` variant accepting a pre-interned `Arc<str>`
/// for the object class.
pub fn emit_allocation_sample_event_arc(
    recorder: &mut FlightRecorder,
    object_class: Arc<str>,
    weight: i64,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationSample") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: smallvec![EventValue::String(object_class), EventValue::Long(weight),],
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a Java error throw event (round-7 #7).
///
/// `jdk.JavaErrorThrow` is the JFR counterpart to `JavaExceptionThrow`,
/// fired for `java.lang.Error` subclasses (OutOfMemoryError, StackOverflowError,
/// etc.). These are unrecoverable conditions that warrant always-on capture
/// even in the default profile, hence the separate event type.
///
/// Call from the throw bytecode handler when the runtime type of the thrown
/// object is `java.lang.Error` or a subclass.
///
/// `class_name` is `&'static str` because Error subclasses live in a small
/// fixed taxonomy ("java.lang.OutOfMemoryError", "java.lang.StackOverflowError",
/// "java.lang.AssertionError", ...) that the throw path can statically
/// resolve. `message` arrives as an interned `Arc<str>` from the Throwable's
/// detail-message field — no per-event reallocation.
pub fn emit_java_error_throw_event(
    recorder: &mut FlightRecorder,
    class_name: &'static str,
    message: Arc<str>,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaErrorThrow") {
        // Field order matches registration at builtin.rs line 790:
        //   0: message (string), 1: thrownClass (string)
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(message));
        fields.push(EventValue::Str(class_name));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns, // instant event
            thread_id,
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Read real host physical-memory totals cheaply, without pulling in a
/// heavyweight `sysinfo`-style dependency.
///
/// STUB fix (2026-06-17): `jfr` has no `sysinfo`/`windows`/`winapi` dependency
/// (see jfr/Cargo.toml), so this probes the OS directly:
///   * Windows: `GlobalMemoryStatusEx` via raw FFI (same binding the VM crash
///     handler already uses, `vm/src/runtime/crash_handler.rs`).
///   * Linux: parse `MemTotal:` / `MemAvailable:` (kB) from `/proc/meminfo`.
///   * Other targets / probe failure: returns `None`, so the caller's
///     supplied values are used unchanged.
///
/// Returns `Some((total_bytes, used_bytes))` where `used = total - available`,
/// clamped to `>= 0`, both as `i64` per the JFR `long` field type. `None` when
/// the host could not be queried.
fn host_physical_memory() -> Option<(i64, i64)> {
    #[cfg(target_os = "windows")]
    {
        // Raw FFI mirror of `vm/src/runtime/crash_handler.rs` —
        // `GlobalMemoryStatusEx`. Layout per the Win32 `MEMORYSTATUSEX` struct.
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
        // SAFETY: `status` is initialized with the required `dw_length`,
        // points to writable storage for the exact C layout declared above,
        // and remains alive for the duration of the synchronous Win32 call.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        if ok != 0 {
            let total = status.ull_total_phys.min(i64::MAX as u64) as i64;
            let avail = status.ull_avail_phys.min(i64::MAX as u64) as i64;
            let used = total.saturating_sub(avail).max(0);
            return Some((total, used));
        }
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        // Parse the two kB-valued lines we need from /proc/meminfo. Cheap:
        // one ~1-4 KiB read, no allocation beyond the file string.
        let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
        let mut total_kb: Option<u64> = None;
        let mut avail_kb: Option<u64> = None;
        for line in contents.lines() {
            // Lines look like: "MemTotal:       16331640 kB".
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total_kb = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                avail_kb = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            }
            if total_kb.is_some() && avail_kb.is_some() {
                break;
            }
        }
        let total_kb = total_kb?;
        // MemAvailable is absent on very old kernels; fall back to 0 available
        // (i.e. used == total) rather than failing the whole probe.
        let avail_kb = avail_kb.unwrap_or(0);
        let total = total_kb.saturating_mul(1024).min(i64::MAX as u64) as i64;
        let avail = avail_kb.saturating_mul(1024).min(i64::MAX as u64) as i64;
        let used = total.saturating_sub(avail).max(0);
        return Some((total, used));
    }
    #[allow(unreachable_code)]
    {
        None
    }
}

/// Emit a physical-memory sample (round-7 #7).
///
/// `jdk.PhysicalMemory` is a periodic event (EveryChunk) reporting host RAM
/// totals at chunk-rollover boundaries. The OpenJDK reference implementation
/// reads `/proc/meminfo` on Linux, `sysctl hw.memsize` on macOS, and
/// `GlobalMemoryStatusEx` on Windows.
///
/// STUB fix (2026-06-17): this no longer reports caller stand-in values as
/// host RAM by default. It first probes the real host via
/// [`host_physical_memory`] (`GlobalMemoryStatusEx` on Windows, `/proc/meminfo`
/// on Linux — no new dependency). When the probe succeeds those real totals
/// are used. When it fails (unsupported target, or the syscall/parse errored)
/// it falls back to the caller-supplied `total_size` / `used_size`, so the
/// explicit-contract path is preserved: a caller that already has authoritative
/// numbers from the host VM's memory subsystem (e.g. the GC manager's committed
/// bytes) can still pass them and they are honoured on platforms where the
/// host probe is unavailable. Both fields are `i64` per the JFR `long` type.
///
/// FOLLOW-UP (out of this file's scope): the lone wired caller,
/// `vm/src/vm/vm_init.rs`, currently passes `max_heap_size` for both `total`
/// and `used` as a stand-in. With this change the host probe overrides that on
/// Windows/Linux; the caller could be simplified to pass `0`/`0` (or its real
/// committed bytes) now that the fallback is no longer the primary source.
pub fn emit_physical_memory_event(
    recorder: &mut FlightRecorder,
    total_size: i64,
    used_size: i64,
    time_ns: u64,
) {
    // Prefer the real host totals; fall back to the caller-supplied values
    // when the host cannot be queried (unsupported OS / probe failure).
    let (total_size, used_size) = match host_physical_memory() {
        Some((total, used)) => (total, used),
        None => (total_size, used_size),
    };
    // Round-9 CRIT-2 fix (2026-05-17): PhysicalMemory is a one-shot startup
    // event emitted from vm_init for diagnostic/posterity purposes — it
    // records host RAM totals to the repository regardless of whether any
    // recording has been started. The previous `is_enabled()` gate caused
    // the wired emit to no-op in vm-cli (which never calls start_recording),
    // making `emit_physical_memory_event` dead code. The repository ingest
    // path itself is safe for un-recorded pushes; skip the global gate here.
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.PhysicalMemory") {
        // Field order matches registration at builtin.rs line 709:
        //   0: totalSize (long), 1: usedSize (long)
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::Long(total_size));
        fields.push(EventValue::Long(used_size));
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns, // instant event
            // PhysicalMemory has `has_thread: false` per registration; the
            // ring infrastructure still needs a thread_id, use the sampler's.
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

// ---------------------------------------------------------------------------
// Round-5 JFR Fix 4: wire previously-stub-only event types
// ---------------------------------------------------------------------------
//
// These three emit functions cover the highest-value unwired event types
// from the round-4 audit. Each one had a `register_*` definition but no
// `emit_*` counterpart, so the event type appeared in the JFR metadata
// catalog but no events of that type were ever produced.

/// Emit a `jdk.InitialEnvironmentVariable` event.
///
/// Per-process startup snapshot of one OS environment variable. Called from
/// `vm_init` for each environment variable the VM cares about (typically
/// `CRATONVM_*`, `JAVA_*`, `_JAVA_OPTIONS`, ...). The event is registered
/// `EveryChunk` so it appears in every chunk header; emitting it once at
/// startup is the OpenJDK reference behaviour.
///
/// Both `key` and `value` arrive as borrowed `&str` from the OS env table;
/// they are interned per-event into `Arc<str>` for the string pool.
pub fn emit_initial_environment_variable_event(
    recorder: &mut FlightRecorder,
    key: &str,
    value: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.InitialEnvironmentVariable") {
        // Field order matches the registration above: key, value.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(key)));
        fields.push(EventValue::String(Arc::from(value)));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            // has_thread=false on registration — startup VM thread is fine.
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a `jdk.ExceptionStatistics` event.
///
/// Periodic (EveryChunk) snapshot of the total number of throwables created
/// since process start (or since the last reset, depending on caller policy).
/// Designed to be called from the chunk-rollover sampler, not on each throw.
///
/// `total_throwables` is a monotonic counter; the JFR consumer computes the
/// per-chunk delta. Callers should pass the running total they have been
/// tracking — typically a single `AtomicU64` incremented by the throw
/// bytecode handler.
pub fn emit_exception_statistics_event(
    recorder: &mut FlightRecorder,
    total_throwables: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ExceptionStatistics") {
        // Field order matches the registration above: throwables.
        let mut fields = crate::event::EventFields::with_capacity(1);
        fields.push(EventValue::Long(total_throwables));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a `jdk.ModuleRequire` event.
///
/// Fired during class loading when a class with a `module-info` declares a
/// `requires` directive on another module. `source` is the requiring module
/// (e.g. `java.base`), `required_module` is the depended-on module.
///
/// Both arrive as `&str` from the class-loading layer; module names tend to
/// be short and stable per chunk, so the string pool deduplicates them.
pub fn emit_module_require_event(
    recorder: &mut FlightRecorder,
    source: &str,
    required_module: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ModuleRequire") {
        // Field order: source, requiredModule.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(source)));
        fields.push(EventValue::String(Arc::from(required_module)));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Emit a `jdk.ModuleExport` event.
///
/// Fired during class loading when a class with a `module-info` declares an
/// `exports` directive. `exported_package` is the package being exported
/// (e.g. `java.lang`), `target_module` is the receiving module (or
/// `"unqualified"` when the export has no qualifier).
pub fn emit_module_export_event(
    recorder: &mut FlightRecorder,
    exported_package: &str,
    target_module: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() {
        return;
    }
    static ID: EventTypeIdCache = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ModuleExport") {
        // Field order: exportedPackage, targetModule.
        let mut fields = crate::event::EventFields::with_capacity(2);
        fields.push(EventValue::String(Arc::from(exported_package)));
        fields.push(EventValue::String(Arc::from(target_module)));
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: current_jfr_thread_id(),
            fields,
        };
        push_builtin_event(recorder, event);
    }
}

/// Register a custom user-defined JFR event type.
///
/// This supports `jdk.jfr.Event` subclassing: Java code that extends Event
/// can register custom event types via annotation processing or explicit registration.
///
/// Returns the assigned `EventTypeId` for the custom event.
pub fn register_custom_event(
    registry: &mut EventTypeRegistry,
    name: &str,
    category: &[&str],
    description: &str,
    fields: &[(&str, &str, &str)], // (name, type, description)
    has_thread: bool,
    has_stacktrace: bool,
    threshold: Option<Duration>,
) -> EventTypeId {
    let event_type = EventType {
        id: EventTypeId(0), // overwritten by register()
        name: name.to_string(),
        category: category.iter().map(|s| s.to_string()).collect(),
        description: description.to_string(),
        fields: fields
            .iter()
            .map(|(n, t, d)| EventField::new(n, t, d))
            .collect(),
        has_thread,
        has_stacktrace,
        period: EventPeriod::None,
        threshold,
    };
    registry.register(event_type)
}

/// Errors returned by [`emit_custom_event`] when per-emit validation
/// (task #30) rejects an event before it touches the ring.
///
/// In release builds the writer prefers an early `Err` return over
/// silently corrupting the chunk: a single bad emit would otherwise
/// desynchronise every later event in the chunk because the reader
/// decodes off the declared `type_name`, not the runtime variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// `event_name` is not present in the recorder's `type_registry`.
    UnknownEventType,
    /// Number of supplied field values does not match the registry-declared
    /// field count for this event type.
    FieldCountMismatch { expected: usize, actual: usize },
    /// A registry-declared field has a `type_name` that does not map to a
    /// known [`FieldKind`] (e.g. typo in `register_custom_event`).
    UnknownDeclaredType {
        field_index: usize,
        declared: String,
    },
    /// The runtime [`EventValue`] variant does not match the declared field
    /// type for this position.
    DeclaredTypeMismatch {
        field_index: usize,
        declared: FieldKind,
        actual: FieldKind,
    },
    /// The runtime variant sequence for a subsequent emit does not match
    /// the shape locked in on this event type's first emit. See
    /// [`FlightRecorder::field_shape_lock`].
    ShapeLockMismatch {
        field_index: usize,
        locked: FieldKind,
        actual: FieldKind,
    },
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmitError::UnknownEventType => write!(f, "unknown event type"),
            EmitError::FieldCountMismatch { expected, actual } => {
                write!(
                    f,
                    "field count mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            EmitError::UnknownDeclaredType {
                field_index,
                declared,
            } => {
                write!(
                    f,
                    "field {}: unknown declared type '{}'",
                    field_index, declared
                )
            }
            EmitError::DeclaredTypeMismatch {
                field_index,
                declared,
                actual,
            } => {
                write!(
                    f,
                    "field {}: declared {:?} but supplied {:?} — would desync the chunk",
                    field_index, declared, actual
                )
            }
            EmitError::ShapeLockMismatch {
                field_index,
                locked,
                actual,
            } => {
                write!(
                    f,
                    "field {}: first emit locked variant {:?}, this emit supplies {:?}",
                    field_index, locked, actual
                )
            }
        }
    }
}

impl std::error::Error for EmitError {}

/// Validate `field_values` against the registry-declared field types for
/// `type_id`, AND against the per-event-type shape lock (task #30).
///
/// On the very first emit for `type_id`, this also POPULATES the shape lock
/// with the runtime variant sequence — subsequent emits must match it.
///
/// Returns `Ok(())` if the emit is safe; `Err(EmitError)` otherwise. The
/// caller is responsible for the `debug_assert!`-vs-early-return policy.
fn validate_and_lock_shape(
    recorder: &mut FlightRecorder,
    type_id: EventTypeId,
    field_values: &[EventValue],
) -> Result<(), EmitError> {
    // 1. Declared-type check against the registry. Snapshot the kinds into
    //    a small stack vector so we drop the immutable borrow before we
    //    mutate the shape-lock map.
    let declared_kinds: smallvec::SmallVec<[FieldKind; crate::event::EVENT_FIELD_INLINE]> = {
        let event_type = recorder
            .type_registry
            .get(type_id)
            .ok_or(EmitError::UnknownEventType)?;
        if event_type.fields.len() != field_values.len() {
            return Err(EmitError::FieldCountMismatch {
                expected: event_type.fields.len(),
                actual: field_values.len(),
            });
        }
        let mut kinds = smallvec::SmallVec::with_capacity(event_type.fields.len());
        for (idx, decl) in event_type.fields.iter().enumerate() {
            let declared = FieldKind::from_declared(&decl.type_name).ok_or_else(|| {
                EmitError::UnknownDeclaredType {
                    field_index: idx,
                    declared: decl.type_name.clone(),
                }
            })?;
            kinds.push(declared);
        }
        kinds
    };

    for (idx, (value, &declared)) in field_values.iter().zip(declared_kinds.iter()).enumerate() {
        let actual = FieldKind::of_value(value);
        if !actual.matches_declared(declared) {
            return Err(EmitError::DeclaredTypeMismatch {
                field_index: idx,
                declared,
                actual,
            });
        }
    }

    // 2. Shape-lock check. The lock is keyed by `EventTypeId` so a single
    //    bad emit can never silently change the per-chunk wire shape.
    //    `Null` is the wildcard variant — we do NOT overwrite a locked
    //    concrete kind with `Null`, and we accept `Null` against any
    //    locked kind. This avoids spuriously rejecting nullable strings.
    let actual_kinds: smallvec::SmallVec<[FieldKind; crate::event::EVENT_FIELD_INLINE]> =
        field_values.iter().map(FieldKind::of_value).collect();

    if let Some(locked) = recorder.field_shape_lock.get(&type_id) {
        // Length check is redundant (already verified above against the
        // registry, and the lock was populated under that same registry
        // count), but defensive: registry mutation between emits would
        // otherwise be undetected.
        if locked.len() != actual_kinds.len() {
            return Err(EmitError::FieldCountMismatch {
                expected: locked.len(),
                actual: actual_kinds.len(),
            });
        }
        for (idx, (&l, &a)) in locked.iter().zip(actual_kinds.iter()).enumerate() {
            if a == FieldKind::Null {
                continue;
            }
            if l == FieldKind::Null {
                continue;
            }
            if l != a {
                return Err(EmitError::ShapeLockMismatch {
                    field_index: idx,
                    locked: l,
                    actual: a,
                });
            }
        }
    } else {
        // First emit for this event type: lock the shape in. We store the
        // runtime variants (not the declared kinds) because the WRITER
        // dispatches on those — that is the wire-format truth.
        recorder
            .field_shape_lock
            .insert(type_id, actual_kinds.into_vec());
    }

    Ok(())
}

/// Emit a custom user event with the given fields.
///
/// Note: this cannot use a per-site `OnceLock` cache because `event_name` is
/// dynamic. The `is_enabled()` fast-path still skips the registry probe when
/// no recordings are active.
///
/// Task #30 (HIGH correctness): every emit is validated against (a) the
/// registry-declared field types AND (b) the per-event-type shape locked
/// in on the first successful emit. Mismatches are rejected before the
/// event reaches the ring: in debug builds via `debug_assert!` with a
/// useful diagnostic, in release builds via an `Err` return. Returns
/// `Ok(())` on success, `Ok(())` when JFR is disabled (the fast-path
/// drops the event), or `Err(EmitError)` on validation failure.
pub fn emit_custom_event(
    recorder: &mut FlightRecorder,
    event_name: &str,
    field_values: Vec<EventValue>,
    time_ns: u64,
    thread_id: u64,
) -> Result<(), EmitError> {
    if !crate::is_enabled() {
        return Ok(());
    }
    let type_id = match recorder.type_registry.find_by_name(event_name) {
        Some(id) => id,
        None => {
            // Unknown event names are not a wire-format hazard (the event
            // is simply not emitted), so we preserve the original
            // "silently skip" semantics for callers that pre-register
            // events lazily.
            return Ok(());
        }
    };
    if let Err(e) = validate_and_lock_shape(recorder, type_id, &field_values) {
        // Debug builds: surface the bug loudly with a useful diagnostic.
        // Release builds: prefer an early `Err` return over corrupting
        // the chunk — silently letting a mismatched emit through would
        // desync every later event in the same chunk because the reader
        // decodes off the declared `type_name`, not the runtime variant.
        debug_assert!(
            false,
            "JFR emit_custom_event validation failed for '{}': {}",
            event_name, e
        );
        return Err(e);
    }
    // Round-5 Fix 1: bridge Vec→SmallVec at the public boundary; the
    // common case (≤ 8 fields) keeps the storage inline despite the
    // caller-allocated `Vec`. Long fields still spill to the heap once,
    // matching the prior behaviour.
    let event = EventInstance {
        type_id,
        start_time: time_ns,
        end_time: time_ns,
        thread_id,
        fields: smallvec::SmallVec::from_vec(field_values),
    };
    crate::repository::push_to_thread_ring(event);
    Ok(())
}

/// JFR configuration profile — corresponds to .jfc files (default.jfc, profile.jfc).
///
/// Defines which events are enabled and their settings (threshold, stacktrace, period).
///
/// EXPERIMENTAL / NOT-YET-WIRED (S1, 2026-06-10): this profile model and its
/// constructors ([`default_profile`](JfrProfile::default_profile),
/// [`detailed_profile`](JfrProfile::detailed_profile)) are a **planned but
/// currently unwired** configuration surface. Nothing in [`FlightRecorder`]'s
/// recording-startup path consults a `JfrProfile`, and the only caller of
/// [`apply_to`](JfrProfile::apply_to) is this crate's own test suite — so the
/// `enabled` / `threshold` / `stacktrace` / `period` values **do not currently
/// gate any recording**. Real JFR drives recording configuration from these
/// `.jfc` profiles; this models that surface ahead of plumbing it through
/// `FlightRecorder`. Treat it as data-only until that wiring lands. The
/// `stacktrace` and `period` fields are additionally inert because stack-trace
/// capture and periodic-event sampling are not yet implemented (see
/// `dump::write_metadata_section` for the stack-trace gap).
#[derive(Debug, Clone)]
pub struct JfrProfile {
    pub name: String,
    pub description: String,
    pub settings: Vec<JfrEventSetting>,
}

/// Settings for a single event type in a JFR profile.
///
/// EXPERIMENTAL / NOT-YET-WIRED (S1, 2026-06-10): see [`JfrProfile`]. These
/// per-event settings are not yet applied to live recordings; `stacktrace` and
/// `period` are inert (no stack-trace capture, no periodic sampling).
#[derive(Debug, Clone)]
pub struct JfrEventSetting {
    pub event_name: String,
    pub enabled: bool,
    pub threshold: Option<Duration>,
    pub stacktrace: Option<bool>,
    pub period: Option<String>,
}

impl JfrProfile {
    /// Create the "default" JFR profile (low-overhead, suitable for production).
    pub fn default_profile() -> Self {
        Self {
            name: "default".to_string(),
            description: "Low overhead configuration for continuous use".to_string(),
            settings: vec![
                // GC events: enabled with 0ms threshold
                JfrEventSetting {
                    event_name: "jdk.GarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.GCPhasePause".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.YoungGarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.OldGarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.GCHeapSummary".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                // Thread events
                JfrEventSetting {
                    event_name: "jdk.ThreadStart".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ThreadEnd".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                // Class loading
                JfrEventSetting {
                    event_name: "jdk.ClassLoad".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(0)),
                    stacktrace: Some(true),
                    period: None,
                },
                // JIT
                JfrEventSetting {
                    event_name: "jdk.Compilation".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(100)),
                    stacktrace: None,
                    period: None,
                },
                // CPU/memory periodic
                JfrEventSetting {
                    event_name: "jdk.CPULoad".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everyChunk".into()),
                },
                JfrEventSetting {
                    event_name: "jdk.JavaThreadStatistics".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everyChunk".into()),
                },
                // File/socket I/O: higher threshold for low overhead
                JfrEventSetting {
                    event_name: "jdk.FileRead".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(20)),
                    stacktrace: Some(false),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.FileWrite".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(20)),
                    stacktrace: Some(false),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SocketRead".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(20)),
                    stacktrace: Some(false),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SocketWrite".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(20)),
                    stacktrace: Some(false),
                    period: None,
                },
                // Exceptions: disabled by default in production
                JfrEventSetting {
                    event_name: "jdk.JavaExceptionThrow".into(),
                    enabled: false,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.JavaErrorThrow".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                // Allocation: sampled
                JfrEventSetting {
                    event_name: "jdk.ObjectAllocationSample".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
            ],
        }
    }

    /// Create the "profile" JFR profile (higher overhead, for detailed analysis).
    pub fn detailed_profile() -> Self {
        Self {
            name: "profile".to_string(),
            description: "Detailed profiling configuration".to_string(),
            settings: vec![
                // GC events: all enabled
                JfrEventSetting {
                    event_name: "jdk.GarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.GCPhasePause".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.YoungGarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.OldGarbageCollection".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.GCHeapSummary".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.GCReferenceStatistics".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.AllocationRequiringGC".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SystemGC".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                // Thread events
                JfrEventSetting {
                    event_name: "jdk.ThreadStart".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ThreadEnd".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ThreadSleep".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(10)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ThreadPark".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(10)),
                    stacktrace: Some(true),
                    period: None,
                },
                // Monitor events
                JfrEventSetting {
                    event_name: "jdk.JavaMonitorEnter".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(10)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.JavaMonitorWait".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(10)),
                    stacktrace: Some(true),
                    period: None,
                },
                // Class loading
                JfrEventSetting {
                    event_name: "jdk.ClassLoad".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(0)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ClassUnload".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: None,
                },
                // JIT
                JfrEventSetting {
                    event_name: "jdk.Compilation".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(0)),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.Deoptimization".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                // CPU/memory periodic
                JfrEventSetting {
                    event_name: "jdk.CPULoad".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everySecond".into()),
                },
                JfrEventSetting {
                    event_name: "jdk.ThreadCPULoad".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everySecond".into()),
                },
                JfrEventSetting {
                    event_name: "jdk.JavaThreadStatistics".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everySecond".into()),
                },
                JfrEventSetting {
                    event_name: "jdk.MetaspaceSummary".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everyChunk".into()),
                },
                // File/socket I/O: lower threshold for more detail
                JfrEventSetting {
                    event_name: "jdk.FileRead".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(1)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.FileWrite".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(1)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SocketRead".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(1)),
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SocketWrite".into(),
                    enabled: true,
                    threshold: Some(Duration::from_millis(1)),
                    stacktrace: Some(true),
                    period: None,
                },
                // Exceptions: enabled for profiling
                JfrEventSetting {
                    event_name: "jdk.JavaExceptionThrow".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.JavaErrorThrow".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                // Allocation: full tracking
                JfrEventSetting {
                    event_name: "jdk.ObjectAllocationSample".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ObjectAllocationInNewTLAB".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.ObjectAllocationOutsideTLAB".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: None,
                },
                // Safepoints
                JfrEventSetting {
                    event_name: "jdk.SafepointBegin".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                JfrEventSetting {
                    event_name: "jdk.SafepointEnd".into(),
                    enabled: true,
                    threshold: Some(Duration::ZERO),
                    stacktrace: None,
                    period: None,
                },
                // Execution sample for profiling
                JfrEventSetting {
                    event_name: "jdk.ExecutionSample".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: Some("everySecond".into()),
                },
                JfrEventSetting {
                    event_name: "jdk.NativeMethodSample".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: Some(true),
                    period: Some("everySecond".into()),
                },
                // Network
                JfrEventSetting {
                    event_name: "jdk.NetworkUtilization".into(),
                    enabled: true,
                    threshold: None,
                    stacktrace: None,
                    period: Some("everySecond".into()),
                },
            ],
        }
    }

    /// Apply this profile's settings to a `RecordingSettings`.
    ///
    /// EXPERIMENTAL / NOT-YET-WIRED (S1, 2026-06-10): no production path calls
    /// this — only the crate's own `t6_profile_apply_to_settings` test does, so
    /// in practice profiles never reach any live recording. When it *is* called
    /// it maps only `enabled` (→ `enabled_events`) and `threshold` (→
    /// `event_thresholds`); the `stacktrace` and `period` fields are NOT applied
    /// (those features are unimplemented — see [`JfrProfile`]).
    pub fn apply_to(
        &self,
        settings: &mut crate::recording::RecordingSettings,
        registry: &EventTypeRegistry,
    ) {
        for es in &self.settings {
            if !es.enabled {
                // If there's an event ID, it stays excluded from enabled_events
                continue;
            }
            if let Some(type_id) = registry.find_by_name(&es.event_name) {
                settings.enabled_events.insert(type_id);
                if let Some(threshold) = es.threshold {
                    settings.event_thresholds.insert(type_id, threshold);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::RecordingSettings;

    #[test]
    fn test_builtin_event_count() {
        let mut registry = EventTypeRegistry::new();
        register_builtin_events(&mut registry);
        assert!(
            registry.len() >= 20,
            "Expected at least 20 built-in events, got {}",
            registry.len()
        );
    }

    #[test]
    fn test_builtin_exact_count_23() {
        let mut registry = EventTypeRegistry::new();
        register_builtin_events(&mut registry);
        // 48th: `cratonvm.JitCompileDecision` (2026-09-01). The test NAME is
        // historical and has been wrong since the count passed 23; the
        // assertion is what matters.
        assert_eq!(registry.len(), 48); // 28 original + 19 T6.2 + 1 cratonvm.*
    }

    // --- GC events ---

    #[test]
    fn test_gc_event_fields() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.GarbageCollection").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 5);
        assert_eq!(et.fields[0].name, "gcId");
        assert_eq!(et.fields[1].name, "name");
        assert_eq!(et.fields[2].name, "cause");
        assert!(et.has_thread);
        assert!(!et.has_stacktrace);
        assert!(et.threshold.is_some());
    }

    #[test]
    fn test_gc_phase_pause_exists() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        assert!(reg.find_by_name("jdk.GCPhasePause").is_some());
    }

    #[test]
    fn test_young_gc_exists() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.YoungGarbageCollection").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
    }

    #[test]
    fn test_old_gc_exists() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.OldGarbageCollection").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 1);
    }

    #[test]
    fn test_gc_heap_summary() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.GCHeapSummary").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 6);
        assert!(!et.has_thread);
    }

    #[test]
    fn test_metaspace_summary() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.MetaspaceSummary").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 5);
    }

    // --- Thread events ---

    #[test]
    fn test_thread_start_fields() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ThreadStart").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
        assert!(et.has_thread);
        assert!(et.has_stacktrace);
        assert!(et.threshold.is_none());
    }

    #[test]
    fn test_thread_end_fields() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ThreadEnd").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 1);
        assert!(!et.has_stacktrace);
    }

    #[test]
    fn test_thread_sleep() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ThreadSleep").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.threshold, Some(Duration::from_millis(10)));
    }

    #[test]
    fn test_thread_park() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ThreadPark").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
    }

    #[test]
    fn test_java_monitor_wait() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.JavaMonitorWait").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 5);
    }

    #[test]
    fn test_java_monitor_enter() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.JavaMonitorEnter").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
    }

    #[test]
    fn test_java_thread_statistics() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.JavaThreadStatistics").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 4);
        assert!(!et.has_thread);
    }

    // --- Class loading events ---

    #[test]
    fn test_class_load() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ClassLoad").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
        assert!(et.has_stacktrace);
    }

    #[test]
    fn test_class_unload() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ClassUnload").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
        assert!(!et.has_stacktrace);
        assert!(et.threshold.is_none());
    }

    // --- Compiler events ---

    #[test]
    fn test_compilation() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.Compilation").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 7);
        assert_eq!(et.threshold, Some(Duration::from_millis(100)));
    }

    // --- Allocation events ---

    #[test]
    fn test_allocation_in_new_tlab() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ObjectAllocationInNewTLAB").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
        assert!(et.has_stacktrace);
    }

    #[test]
    fn test_allocation_outside_tlab() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ObjectAllocationOutsideTLAB").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
    }

    // --- Profiling events ---

    #[test]
    fn test_execution_sample() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ExecutionSample").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
    }

    #[test]
    fn test_cpu_load() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.CPULoad").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
        assert!(!et.has_thread);
    }

    // --- Flight Recorder events ---

    #[test]
    fn test_active_recording() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ActiveRecording").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 5);
        assert_eq!(et.category, vec!["Flight Recorder"]);
    }

    #[test]
    fn test_active_setting() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let id = reg.find_by_name("jdk.ActiveSetting").unwrap();
        let et = reg.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
    }

    // --- Category checks ---

    #[test]
    fn test_gc_events_have_gc_category() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let gc_names = [
            "jdk.GarbageCollection",
            "jdk.GCPhasePause",
            "jdk.YoungGarbageCollection",
            "jdk.OldGarbageCollection",
        ];
        for name in &gc_names {
            let id = reg.find_by_name(name).unwrap();
            let et = reg.get(id).unwrap();
            assert!(
                et.category.contains(&"GC".to_string()),
                "{} should have GC category",
                name
            );
        }
    }

    #[test]
    fn test_thread_events_have_threading_category() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let thread_names = [
            "jdk.ThreadStart",
            "jdk.ThreadEnd",
            "jdk.ThreadSleep",
            "jdk.ThreadPark",
        ];
        for name in &thread_names {
            let id = reg.find_by_name(name).unwrap();
            let et = reg.get(id).unwrap();
            assert!(
                et.category.contains(&"Threading".to_string()),
                "{} should have Threading category",
                name
            );
        }
    }

    #[test]
    fn test_all_builtin_ids_unique() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let ids: Vec<EventTypeId> = reg.iter().map(|(id, _)| *id).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "All builtin event IDs should be unique"
        );
    }

    #[test]
    fn test_all_builtin_names_unique() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let names: Vec<&str> = reg.iter().map(|(_, et)| et.name.as_str()).collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "All builtin event names should be unique"
        );
    }

    // --- Emission function tests ---
    //
    // Round-4/5 per-thread-ring contract (C33): `emit_*` no longer writes
    // directly into the recording's `EventRepository`. Events land in the
    // calling thread's `SpscEventRing` shard and are not visible to
    // `Recording::event_count()` / `get_events()` until the test calls
    // `FlightRecorder::drain_per_thread_into_repository()`.
    //
    // The pattern below in every test is:
    //   1. Drain the global ring once to clear any stragglers pushed by
    //      other tests running in parallel on this thread or others.
    //   2. Emit the event under test.
    //   3. Drain into the recording's repository.
    //   4. Assert on `event_count()` / `get_events()`.

    fn make_recorder() -> FlightRecorder {
        let mut fr = FlightRecorder::new();
        register_builtin_events(&mut fr.type_registry);
        fr
    }

    /// Acquire the process-wide test serialization lock and discard any events
    /// left in the per-thread ring registry before the test under load runs.
    ///
    /// The returned [`MutexGuard`](parking_lot::MutexGuard) must be held for
    /// the whole test (`let _g = drain_ring_baseline();`): it serializes this
    /// test against every other test that emits into / drains the global ring,
    /// so a concurrent `drain_all()` cannot steal events and break exact
    /// `event_count() == N` assertions. The baseline drain then clears any
    /// stragglers left on this thread's shard by earlier tests.
    #[must_use]
    fn drain_ring_baseline() -> parking_lot::MutexGuard<'static, ()> {
        let guard = crate::repository::jfr_test_guard();
        let _ = crate::repository::global_ring_registry().drain_all();
        guard
    }

    #[test]
    fn test_emit_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1000, 500);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_builtin_event_id_cache_is_registry_keyed() {
        let _g = drain_ring_baseline();

        let mut first = make_recorder();
        let first_gc_id = first
            .type_registry
            .find_by_name("jdk.GarbageCollection")
            .unwrap();
        let first_rid = first.new_recording(RecordingSettings::new("first"));
        first.start_recording(first_rid);
        emit_gc_event(&mut first, 1, "G1 Young", "Allocation Failure", 1000, 500);
        first.drain_per_thread_into_repository();
        {
            let rec = first.get_recording_mut(first_rid).unwrap();
            assert_eq!(rec.event_count(), 1);
            assert_eq!(rec.get_events()[0].type_id, first_gc_id);
        }
        first.stop_recording(first_rid);

        let mut second = FlightRecorder::new();
        let custom_id = register_custom_event(
            &mut second.type_registry,
            "test.BeforeBuiltins",
            &["Test"],
            "pre-registered custom event",
            &[],
            true,
            false,
            None,
        );
        assert_eq!(custom_id, EventTypeId(1));
        register_builtin_events(&mut second.type_registry);
        let second_gc_id = second
            .type_registry
            .find_by_name("jdk.GarbageCollection")
            .unwrap();
        assert_ne!(first_gc_id, second_gc_id);

        let second_rid = second.new_recording(RecordingSettings::new("second"));
        second.start_recording(second_rid);
        emit_gc_event(&mut second, 2, "G1 Young", "Allocation Failure", 2000, 500);
        second.drain_per_thread_into_repository();
        let rec = second.get_recording_mut(second_rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        assert_eq!(rec.get_events()[0].type_id, second_gc_id);
    }

    #[test]
    fn test_emit_class_load_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_class_load_event(
            &mut fr,
            "java/lang/Object",
            "bootstrap",
            "bootstrap",
            2000,
            100,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_start_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_thread_start_event(&mut fr, "main", "", 1, 3000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_compilation_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_compilation_event(
            &mut fr,
            "java/lang/String.hashCode:()I",
            1,
            3,
            true,
            false,
            256,
            128,
            4000,
            200,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_allocation_in_new_tlab_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_allocation_in_new_tlab_event(&mut fr, "java/lang/Object", 64, 4096, 1, 5000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_event_not_recorded_when_stopped() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        // Do NOT start the recording. With no running recording in this
        // FlightRecorder, `drain_per_thread_into_repository` discards every
        // drained event (regardless of which other test pushed it onto the
        // global ring), so the assertion remains exact.
        let _g = drain_ring_baseline();
        emit_gc_event(&mut fr, 1, "G1", "Test", 1000, 500);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 0);
    }

    // --- New emission function tests (M26 fixes) ---

    #[test]
    fn test_emit_thread_end_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_thread_end_event(&mut fr, "worker-1", 42, 6000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_sleep_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_thread_sleep_event(&mut fr, 1_000_000, 1, 7000, 1_000_000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_monitor_wait_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_monitor_wait_event(
            &mut fr,
            "java/lang/Object",
            "main",
            0,
            false,
            0xDEAD,
            1,
            8000,
            500,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_monitor_enter_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_monitor_enter_event(&mut fr, "java/util/HashMap", "main", 0xBEEF, 1, 9000, 200);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_class_unload_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_class_unload_event(&mut fr, "com/example/OldClass", "app", 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_park_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_thread_park_event(
            &mut fr,
            "java/util/concurrent/locks/AQS",
            0,
            0xCAFE,
            1,
            11000,
            300,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_gc_heap_summary_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_gc_heap_summary_event(
            &mut fr,
            1,
            "Before GC",
            "G1 Eden",
            1024 * 1024,
            512 * 1024,
            2048 * 1024,
            12000,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_sleep_saturating_add() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        // Use u64::MAX to test saturating_add doesn't overflow
        emit_thread_sleep_event(&mut fr, i64::MAX, 1, u64::MAX - 10, u64::MAX);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    // S41 — JFR Event Completeness

    #[test]
    fn s41_builtin_event_count_47() {
        let mut registry = EventTypeRegistry::new();
        register_builtin_events(&mut registry);
        // 47 `jdk.*` + `cratonvm.JitCompileDecision`. The name is kept at 47
        // because `docs/observability/phase-accounting.md` cites these tests by
        // name.
        assert_eq!(registry.len(), 48);
    }

    #[test]
    fn s41_new_event_types_registered() {
        let mut registry = EventTypeRegistry::new();
        register_builtin_events(&mut registry);
        assert!(registry.find_by_name("jdk.Deoptimization").is_some());
        assert!(registry.find_by_name("jdk.FileRead").is_some());
        assert!(registry.find_by_name("jdk.FileWrite").is_some());
        assert!(registry.find_by_name("jdk.SocketRead").is_some());
        assert!(registry.find_by_name("jdk.SocketWrite").is_some());
    }

    #[test]
    fn s41_emit_deoptimization_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_deoptimization_event(
            &mut fr,
            "com/example/Foo.bar:()V",
            1,
            "NullCheck",
            "Reinterpret",
            42,
            1,
            10000,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        // With cross-test parallelism we may pick up stragglers; find our
        // event by its unique method-name marker rather than asserting an
        // exact count.
        let events = rec.get_events();
        let ev = events
            .iter()
            .find(|e| match e.fields.first() {
                Some(EventValue::String(s)) => s.contains("Foo"),
                _ => false,
            })
            .expect("expected deoptimization event with 'Foo' method to be drained");
        assert_eq!(ev.fields.len(), 5);
    }

    #[test]
    fn s41_emit_file_read_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_file_read_event(&mut fr, "fd:3", 1024, false, 1, 10000, 500);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 3);
    }

    #[test]
    fn s41_emit_file_write_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_file_write_event(&mut fr, "fd:1", 512, 1, 10000, 300);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_socket_read_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_socket_read_event(&mut fr, "localhost", 8080, 256, false, 1, 10000, 2000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 4);
    }

    #[test]
    fn s41_emit_socket_write_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_socket_write_event(&mut fr, "example.com", 443, 128, 1, 10000, 1000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_gc_phase_pause_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_gc_phase_pause_event(&mut fr, 1, "Mark", 10000, 5000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_young_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_young_gc_event(&mut fr, 1, 15, 10000, 3000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_old_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_old_gc_event(&mut fr, 2, 20000, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_metaspace_summary_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_metaspace_summary_event(&mut fr, 1, "After GC", 1024, 2048, 4096, 15000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_allocation_outside_tlab_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_allocation_outside_tlab_event(&mut fr, "java/lang/Object", 64, 1, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_execution_sample_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_execution_sample_event(&mut fr, "main", "Main.run:5", "RUNNABLE", 1, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_cpu_load_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_cpu_load_event(&mut fr, 0.25, 0.05, 0.60, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        // Find our event by the distinctive 0.25 jvmUser field; cross-test
        // emits of jdk.CPULoad with other values are possible.
        let events = rec.get_events();
        let found = events.iter().any(|e| match e.fields.first() {
            Some(EventValue::Float(v)) => (v - 0.25).abs() < 0.001,
            _ => false,
        });
        assert!(
            found,
            "expected cpu_load event with 0.25 in field[0] to be drained"
        );
    }

    #[test]
    fn s41_emit_thread_statistics_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_thread_statistics_event(&mut fr, 10, 3, 50, 12, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_active_recording_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_active_recording_event(&mut fr, 1, "default", "/tmp/rec.jfr", 0, 0, 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_active_setting_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_active_setting_event(&mut fr, 1, "threshold", "10ms", 10000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_deopt_event_has_correct_fields() {
        let fr = crate::create_flight_recorder();
        let et = fr.type_registry.find_by_name("jdk.Deoptimization").unwrap();
        let event_type = fr.type_registry.get(et).unwrap();
        assert_eq!(event_type.fields.len(), 5);
        assert_eq!(event_type.fields[0].name, "method");
        assert_eq!(event_type.fields[2].name, "reason");
        assert_eq!(event_type.fields[4].name, "bci");
    }

    #[test]
    fn s41_file_io_events_have_threshold() {
        let fr = crate::create_flight_recorder();
        let read_id = fr.type_registry.find_by_name("jdk.FileRead").unwrap();
        let read_type = fr.type_registry.get(read_id).unwrap();
        assert!(read_type.threshold.is_some());
        assert_eq!(read_type.threshold.unwrap(), Duration::from_millis(10));

        let write_id = fr.type_registry.find_by_name("jdk.FileWrite").unwrap();
        let write_type = fr.type_registry.get(write_id).unwrap();
        assert!(write_type.threshold.is_some());
    }

    #[test]
    fn s41_socket_events_have_correct_fields() {
        let fr = crate::create_flight_recorder();
        let read_id = fr.type_registry.find_by_name("jdk.SocketRead").unwrap();
        let read_type = fr.type_registry.get(read_id).unwrap();
        assert_eq!(read_type.fields.len(), 4);
        assert_eq!(read_type.fields[0].name, "host");
        assert_eq!(read_type.fields[1].name, "port");
        assert_eq!(read_type.fields[2].name, "bytesRead");
        assert_eq!(read_type.fields[3].name, "endOfStream");
    }

    // --- T6.2 new events ---

    #[test]
    fn t6_safepoint_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr
            .type_registry
            .find_by_name("jdk.SafepointBegin")
            .is_some());
        assert!(fr.type_registry.find_by_name("jdk.SafepointEnd").is_some());
    }

    #[test]
    fn t6_allocation_sample_exists() {
        let fr = crate::create_flight_recorder();
        let id = fr
            .type_registry
            .find_by_name("jdk.ObjectAllocationSample")
            .unwrap();
        let et = fr.type_registry.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
        assert!(et.has_stacktrace);
    }

    #[test]
    fn t6_native_method_sample_exists() {
        let fr = crate::create_flight_recorder();
        assert!(fr
            .type_registry
            .find_by_name("jdk.NativeMethodSample")
            .is_some());
    }

    #[test]
    fn t6_allocation_requiring_gc_exists() {
        let fr = crate::create_flight_recorder();
        assert!(fr
            .type_registry
            .find_by_name("jdk.AllocationRequiringGC")
            .is_some());
    }

    #[test]
    fn t6_exception_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr
            .type_registry
            .find_by_name("jdk.JavaExceptionThrow")
            .is_some());
        assert!(fr
            .type_registry
            .find_by_name("jdk.JavaErrorThrow")
            .is_some());
        assert!(fr
            .type_registry
            .find_by_name("jdk.ExceptionStatistics")
            .is_some());
    }

    #[test]
    fn t6_network_utilization_exists() {
        let fr = crate::create_flight_recorder();
        let id = fr
            .type_registry
            .find_by_name("jdk.NetworkUtilization")
            .unwrap();
        let et = fr.type_registry.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
        assert_eq!(et.fields[0].name, "networkInterface");
    }

    #[test]
    fn t6_container_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr
            .type_registry
            .find_by_name("jdk.ContainerCPUUsage")
            .is_some());
        assert!(fr
            .type_registry
            .find_by_name("jdk.ContainerMemoryUsage")
            .is_some());
    }

    #[test]
    fn t6_module_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.ModuleRequire").is_some());
        assert!(fr.type_registry.find_by_name("jdk.ModuleExport").is_some());
    }

    #[test]
    fn t6_system_gc_event_exists() {
        let fr = crate::create_flight_recorder();
        let id = fr.type_registry.find_by_name("jdk.SystemGC").unwrap();
        let et = fr.type_registry.get(id).unwrap();
        assert!(et.has_stacktrace);
    }

    #[test]
    fn t6_total_event_count_47() {
        let fr = crate::create_flight_recorder();
        // 47 `jdk.*` + `cratonvm.JitCompileDecision`; see `s41_builtin_event_count_47`.
        assert_eq!(fr.type_registry.len(), 48);
    }

    // --- Custom event support ---

    #[test]
    fn t6_register_custom_event() {
        let mut reg = EventTypeRegistry::new();
        let id = register_custom_event(
            &mut reg,
            "com.example.MyEvent",
            &["Application", "Custom"],
            "A custom event",
            &[("message", "string", "Message"), ("count", "int", "Count")],
            true,
            true,
            None,
        );
        let et = reg.get(id).unwrap();
        assert_eq!(et.name, "com.example.MyEvent");
        assert_eq!(et.fields.len(), 2);
        assert!(et.has_thread);
        assert!(et.has_stacktrace);
    }

    #[test]
    fn t6_emit_custom_event() {
        let mut fr = crate::create_flight_recorder();
        let _id = register_custom_event(
            &mut fr.type_registry,
            "test.CustomEvent",
            &["Test"],
            "test",
            &[("data", "string", "Data")],
            false,
            false,
            None,
        );
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_custom_event(
            &mut fr,
            "test.CustomEvent",
            vec![EventValue::from_str("hello")],
            1000,
            1,
        )
        .expect("emit should succeed");
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Task #30 (HIGH correctness): emit_custom_event variant validation
    // -----------------------------------------------------------------------
    //
    // The JFR writer dispatches on the Rust `EventValue` variant; the reader
    // dispatches on the registry-declared `type_name`. A per-emit mismatch
    // would silently desync the entire chunk. These tests confirm that:
    //   1. float-vs-double mismatch is detected and rejected
    //   2. string-vs-int mismatch is detected and rejected
    //   3. correctly-shaped emits succeed across many calls
    // and that the per-event-type shape lock cannot be bypassed by a
    // subsequent caller swapping variants.

    fn t30_setup_recorder(
        field_type: &str,
    ) -> (
        FlightRecorder,
        EventTypeId,
        u64,
        parking_lot::MutexGuard<'static, ()>,
    ) {
        // Hold the global test lock for the test's duration: these tests depend
        // on `is_enabled()` staying true (set by our own `start_recording`); a
        // concurrent test's `stop_recording` would otherwise flip the global
        // `JFR_ENABLED` flag off, making `emit_custom_event` early-return
        // `Ok(())` before validation runs and breaking the rejection asserts.
        let guard = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let id = register_custom_event(
            &mut fr.type_registry,
            "test.task30.Validate",
            &["Test"],
            "task #30 variant validation",
            &[("value", field_type, "A typed value")],
            false,
            false,
            None,
        );
        let rid = fr.new_recording(RecordingSettings::new("task30"));
        fr.start_recording(rid);
        (fr, id, rid, guard)
    }

    // Mismatch tests use `#[cfg_attr(debug_assertions, should_panic)]` because
    // in debug builds the validator fires a `debug_assert!` with a diagnostic
    // (acceptance criterion #2). In release builds the same call returns
    // `Err(...)` (criterion #3) — which we check explicitly below.
    //
    // Each pair (debug / release) is split into two named tests so the failure
    // modes can be inspected independently.

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
    fn t30_float_vs_double_mismatch_rejected() {
        // Declared "double" + supplied `EventValue::Float(_)`:
        //   writer would emit 4 bytes, reader expects 8 → chunk desync.
        let (mut fr, _id, _rid, _g) = t30_setup_recorder("double");
        let err = emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::Float(1.5)],
            1000,
            1,
        );
        // Release path: the call above returned an Err; assert its shape.
        // (Debug path: the `debug_assert!` inside the emit panicked, so
        // execution never reaches this point — `#[should_panic]` covers it.)
        assert!(
            matches!(
                err,
                Err(EmitError::DeclaredTypeMismatch {
                    field_index: 0,
                    declared: FieldKind::Double,
                    actual: FieldKind::Float,
                })
            ),
            "expected DeclaredTypeMismatch(Double, Float), got {:?}",
            err
        );
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
    fn t30_double_vs_float_mismatch_rejected() {
        // Symmetric case: declared "float" + supplied `EventValue::Double(_)`.
        // Writer would emit 8 bytes, reader would read 4 — same desync hazard.
        let (mut fr, _id, _rid, _g) = t30_setup_recorder("float");
        let err = emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::Double(1.5)],
            1000,
            1,
        );
        assert!(
            matches!(
                err,
                Err(EmitError::DeclaredTypeMismatch {
                    field_index: 0,
                    declared: FieldKind::Float,
                    actual: FieldKind::Double,
                })
            ),
            "expected DeclaredTypeMismatch(Float, Double), got {:?}",
            err
        );
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
    fn t30_string_vs_int_mismatch_rejected() {
        // Declared "int" + supplied `EventValue::String(_)`.
        // The reader would try to varint-decode the string's tag/length
        // prefix as an i32 and then misread every following field.
        let (mut fr, _id, _rid, _g) = t30_setup_recorder("int");
        let err = emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::from_str("not an int")],
            1000,
            1,
        );
        assert!(
            matches!(
                err,
                Err(EmitError::DeclaredTypeMismatch {
                    field_index: 0,
                    declared: FieldKind::Int,
                    actual: FieldKind::String,
                })
            ),
            "expected DeclaredTypeMismatch(Int, String), got {:?}",
            err
        );
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
    fn t30_int_vs_string_mismatch_rejected() {
        // Symmetric case: declared "string" + supplied `EventValue::Int(_)`.
        let (mut fr, _id, _rid, _g) = t30_setup_recorder("string");
        let err = emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::Int(42)],
            1000,
            1,
        );
        assert!(
            matches!(
                err,
                Err(EmitError::DeclaredTypeMismatch {
                    field_index: 0,
                    declared: FieldKind::String,
                    actual: FieldKind::Int,
                })
            ),
            "expected DeclaredTypeMismatch(String, Int), got {:?}",
            err
        );
    }

    #[test]
    fn t30_correct_shape_succeeds_across_many_calls() {
        // Declared "double" + always-`Double` emits: every call must succeed,
        // and the per-event-type shape lock must accept all of them.
        let (mut fr, _id, _rid, _g) = t30_setup_recorder("double");
        for i in 0..32u64 {
            let r = emit_custom_event(
                &mut fr,
                "test.task30.Validate",
                vec![EventValue::Double(i as f64 + 0.5)],
                1000 + i,
                1,
            );
            assert!(r.is_ok(), "emit #{} should succeed, got {:?}", i, r);
        }
    }

    #[test]
    fn t30_shape_lock_populated_on_first_emit() {
        // The first emit must populate `field_shape_lock` with the runtime
        // variant sequence; same-shape subsequent emits then keep working.
        let (mut fr, type_id, _rid, _g) = t30_setup_recorder("string");
        emit_custom_event(
            &mut fr,
            "test.task30.Validate",
            vec![EventValue::from_str("v1")],
            1000,
            1,
        )
        .expect("first emit succeeds");
        assert_eq!(
            fr.field_shape_lock.get(&type_id).cloned(),
            Some(vec![FieldKind::String])
        );
        for i in 0..8 {
            emit_custom_event(
                &mut fr,
                "test.task30.Validate",
                vec![EventValue::from_str(&format!("v{}", i + 2))],
                1000 + i as u64,
                1,
            )
            .expect("same-shape emit succeeds");
        }
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "validation failed"))]
    fn t30_field_count_mismatch_rejected() {
        // Declares 2 fields but emit supplies 1 — guards against partial
        // event payloads silently truncating the chunk.
        // Hold the global test lock so a concurrent `stop_recording` cannot
        // flip `JFR_ENABLED` off and make the emit early-return before
        // validation runs.
        let _g = crate::repository::jfr_test_guard();
        let mut fr = crate::create_flight_recorder();
        let _id = register_custom_event(
            &mut fr.type_registry,
            "test.task30.Count",
            &["Test"],
            "count",
            &[("a", "long", "a"), ("b", "long", "b")],
            false,
            false,
            None,
        );
        let rid = fr.new_recording(RecordingSettings::new("count"));
        fr.start_recording(rid);
        let err = emit_custom_event(
            &mut fr,
            "test.task30.Count",
            vec![EventValue::Long(1)],
            1000,
            1,
        );
        assert!(
            matches!(
                err,
                Err(EmitError::FieldCountMismatch {
                    expected: 2,
                    actual: 1
                })
            ),
            "expected FieldCountMismatch(2,1), got {:?}",
            err
        );
    }

    // --- JFR profiles ---

    #[test]
    fn t6_default_profile() {
        let profile = JfrProfile::default_profile();
        assert_eq!(profile.name, "default");
        assert!(!profile.settings.is_empty());
        // GC events should be enabled
        assert!(profile
            .settings
            .iter()
            .any(|s| s.event_name == "jdk.GarbageCollection" && s.enabled));
        // Exception throw should be disabled in default profile
        assert!(profile
            .settings
            .iter()
            .any(|s| s.event_name == "jdk.JavaExceptionThrow" && !s.enabled));
    }

    #[test]
    fn t6_detailed_profile() {
        let profile = JfrProfile::detailed_profile();
        assert_eq!(profile.name, "profile");
        assert!(!profile.settings.is_empty());
        // Exception throw should be enabled in detailed profile
        assert!(profile
            .settings
            .iter()
            .any(|s| s.event_name == "jdk.JavaExceptionThrow" && s.enabled));
        // Safepoints should be enabled
        assert!(profile
            .settings
            .iter()
            .any(|s| s.event_name == "jdk.SafepointBegin" && s.enabled));
    }

    #[test]
    fn t6_profile_apply_to_settings() {
        let fr = crate::create_flight_recorder();
        let profile = JfrProfile::default_profile();
        let mut settings = RecordingSettings::new("profiled");
        profile.apply_to(&mut settings, &fr.type_registry);
        // At least some events should be enabled
        assert!(!settings.enabled_events.is_empty());
    }

    // --- New emit helpers ---

    #[test]
    fn t6_emit_safepoint_events() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_safepoint_begin_event(&mut fr, 1, 10, 0, 1000);
        emit_safepoint_end_event(&mut fr, 1, 1000, 500);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 2);
    }

    #[test]
    fn t6_emit_exception_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_java_exception_throw_event(
            &mut fr,
            "test error",
            "java.lang.RuntimeException",
            1000,
            1,
        );
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn t6_emit_network_utilization() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        let _g = drain_ring_baseline();
        fr.start_recording(rid);
        emit_network_utilization_event(&mut fr, "eth0", 1024, 512, 1000);
        fr.drain_per_thread_into_repository();
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }
}
