use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use crate::event::{EventField, EventInstance, EventPeriod, EventType, EventTypeId, EventTypeRegistry, EventValue};
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
// We now cache the resolved ID per call-site in a `OnceLock<EventTypeId>`.
// Combined with the `crate::is_enabled()` fast-path guard at the top of every
// emit_* function, the disabled-path cost drops from ~30-100 ns (HashMap probe
// + Vec alloc) to ~2-3 ns (one relaxed atomic load + branch).
//
// The cache uses `EventTypeId::INVALID` as a sentinel when the named type is
// not registered (which shouldn't happen for built-in events but is handled
// defensively). Subsequent calls hit the cached sentinel without re-probing.

/// Look up an event-type ID by name, caching the result in the supplied
/// `OnceLock`. Returns `None` if the registry has no such type.
#[inline]
fn cached_event_id(
    cache: &'static OnceLock<EventTypeId>,
    recorder: &FlightRecorder,
    name: &str,
) -> Option<EventTypeId> {
    let id = *cache.get_or_init(|| {
        recorder
            .type_registry
            .find_by_name(name)
            .unwrap_or(EventTypeId::INVALID)
    });
    if id.is_invalid() { None } else { Some(id) }
}

/// Register all built-in JVM event types into the given registry.
pub fn register_builtin_events(registry: &mut EventTypeRegistry) {
    // Helper to build an EventType shell (id is overwritten by register).
    let stub_id = EventTypeId(0);

    // 1. jdk.GarbageCollection
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GarbageCollection".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Collector".into()],
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
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Collector".into()],
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
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Collector".into()],
        description: "Old generation garbage collection".into(),
        fields: vec![
            EventField::new("gcId", "int", "GC Identifier"),
        ],
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
        fields: vec![
            EventField::new("thread", "string", "Java Thread"),
        ],
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
        fields: vec![
            EventField::new("time", "long", "Sleep Time (ns)"),
        ],
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
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Metaspace".into()],
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
        fields: vec![
            EventField::new("safepointId", "long", "Safepoint Identifier"),
        ],
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
        fields: vec![
            EventField::new("invokedConcurrent", "boolean", "Invoked Concurrent"),
        ],
        has_thread: true,
        has_stacktrace: true,
        period: EventPeriod::None,
        threshold: None,
    });

    // 37. jdk.GCReferenceStatistics
    registry.register(EventType {
        id: stub_id,
        name: "jdk.GCReferenceStatistics".into(),
        category: vec!["Java Virtual Machine".into(), "GC".into(), "Reference".into()],
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
        fields: vec![
            EventField::new("throwables", "long", "Total Throwables Created"),
        ],
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
    name: &str,
    cause: &str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GarbageCollection") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0, // GC thread
            fields: vec![
                EventValue::Int(gc_id),
                EventValue::String(Arc::from(name)),
                EventValue::String(Arc::from(cause)),
                EventValue::Long(duration_ns.min(i64::MAX as u64) as i64), // sumOfPauses
                EventValue::Long(duration_ns.min(i64::MAX as u64) as i64), // longestPause
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a class load event.
///
/// Called when a class is successfully loaded and linked.
pub fn emit_class_load_event(
    recorder: &mut FlightRecorder,
    class_name: &str,
    defining_loader: &str,
    initiating_loader: &str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassLoad") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![
                EventValue::String(Arc::from(class_name)),
                EventValue::String(Arc::from(defining_loader)),
                EventValue::String(Arc::from(initiating_loader)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a thread start event.
///
/// Called when a new Java thread is started.
pub fn emit_thread_start_event(
    recorder: &mut FlightRecorder,
    thread_name: &str,
    parent_thread_name: &str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadStart") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(thread_name)),
                EventValue::String(Arc::from(parent_thread_name)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Compilation") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![
                EventValue::String(Arc::from(method)),
                EventValue::Int(compile_id),
                EventValue::Int(compile_level),
                EventValue::Boolean(succeeded),
                EventValue::Boolean(is_osr),
                EventValue::Int(code_size),
                EventValue::Int(inlined_bytes),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadEnd") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(thread_name)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadSleep") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::Long(sleep_time_ns),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a Java monitor wait event.
///
/// Called when Object.wait() is invoked on a contended monitor.
pub fn emit_monitor_wait_event(
    recorder: &mut FlightRecorder,
    monitor_class: &str,
    notifier_thread: &str,
    timeout_ns: i64,
    timed_out: bool,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorWait") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(monitor_class)),
                EventValue::String(Arc::from(notifier_thread)),
                EventValue::Long(timeout_ns),
                EventValue::Boolean(timed_out),
                EventValue::Long(address),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a Java monitor enter event.
///
/// Called when entering a contended monitor (synchronized block).
pub fn emit_monitor_enter_event(
    recorder: &mut FlightRecorder,
    monitor_class: &str,
    previous_owner: &str,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaMonitorEnter") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(monitor_class)),
                EventValue::String(Arc::from(previous_owner)),
                EventValue::Long(address),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a class unload event.
///
/// Called when a class is unloaded by the GC.
pub fn emit_class_unload_event(
    recorder: &mut FlightRecorder,
    class_name: &str,
    defining_loader: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ClassUnload") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id: 0,
            fields: vec![
                EventValue::String(Arc::from(class_name)),
                EventValue::String(Arc::from(defining_loader)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a thread park event.
///
/// Called when LockSupport.park() is invoked.
pub fn emit_thread_park_event(
    recorder: &mut FlightRecorder,
    parked_class: &str,
    timeout_ns: i64,
    address: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadPark") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(parked_class)),
                EventValue::Long(timeout_ns),
                EventValue::Long(address),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a virtual thread pinned event (JEP 491 — JDK 24+).
///
/// Fired when a virtual thread attempts to park while holding a monitor
/// (synchronized block) or inside a native method.
pub fn emit_virtual_thread_pinned_event(
    recorder: &mut FlightRecorder,
    thread_name: &str,
    pin_reason: &str,
    carrier_thread_id: u64,
    virtual_thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.VirtualThreadPinned") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: carrier_thread_id,
            fields: vec![
                EventValue::String(Arc::from(thread_name)),
                EventValue::String(Arc::from(pin_reason)),
                EventValue::Long(virtual_thread_id as i64),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a GC heap summary event.
pub fn emit_gc_heap_summary_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    when: &str,
    heap_space: &str,
    heap_used: i64,
    heap_committed: i64,
    heap_max: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GCHeapSummary") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Int(gc_id),
                EventValue::String(Arc::from(when)),
                EventValue::String(Arc::from(heap_space)),
                EventValue::Long(heap_used),
                EventValue::Long(heap_committed),
                EventValue::Long(heap_max),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationInNewTLAB") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns, // instant event
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(object_class)),
                EventValue::Long(allocation_size),
                EventValue::Long(tlab_size),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationOutsideTLAB") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(object_class)),
                EventValue::Long(allocation_size),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a GC phase pause event.
pub fn emit_gc_phase_pause_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    phase_name: &str,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.GCPhasePause") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![
                EventValue::Int(gc_id),
                EventValue::String(Arc::from(phase_name)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.YoungGarbageCollection") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![
                EventValue::Int(gc_id),
                EventValue::Int(tenuring_threshold),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit an old generation GC event.
pub fn emit_old_gc_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.OldGarbageCollection") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![
                EventValue::Int(gc_id),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a metaspace summary event.
pub fn emit_metaspace_summary_event(
    recorder: &mut FlightRecorder,
    gc_id: i32,
    when: &str,
    metaspace_used: i64,
    metaspace_committed: i64,
    metaspace_reserved: i64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.MetaspaceSummary") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Int(gc_id),
                EventValue::String(Arc::from(when)),
                EventValue::Long(metaspace_used),
                EventValue::Long(metaspace_committed),
                EventValue::Long(metaspace_reserved),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit an execution sample event (profiling).
pub fn emit_execution_sample_event(
    recorder: &mut FlightRecorder,
    sampled_thread: &str,
    stack_trace: &str,
    state: &str,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ExecutionSample") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(sampled_thread)),
                EventValue::String(Arc::from(stack_trace)),
                EventValue::String(Arc::from(state)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.CPULoad") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Float(jvm_user),
                EventValue::Float(jvm_system),
                EventValue::Float(machine_total),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaThreadStatistics") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Long(active_count),
                EventValue::Long(daemon_count),
                EventValue::Long(accumulated_count),
                EventValue::Long(peak_count),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ActiveRecording") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Long(recording_id),
                EventValue::String(Arc::from(name)),
                EventValue::String(Arc::from(destination)),
                EventValue::Long(max_age_ns),
                EventValue::Long(max_size),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit an active setting metadata event.
pub fn emit_active_setting_event(
    recorder: &mut FlightRecorder,
    recording_id: i64,
    name: &str,
    value: &str,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ActiveSetting") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Long(recording_id),
                EventValue::String(Arc::from(name)),
                EventValue::String(Arc::from(value)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a deoptimization event.
///
/// Called when the JIT deoptimizes a compiled method.
pub fn emit_deoptimization_event(
    recorder: &mut FlightRecorder,
    method: &str,
    compile_id: i32,
    reason: &str,
    action: &str,
    bci: i32,
    thread_id: u64,
    timestamp_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.Deoptimization") {
        let event = EventInstance {
            type_id,
            start_time: timestamp_ns,
            end_time: timestamp_ns,
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(method)),
                EventValue::Int(compile_id),
                EventValue::String(Arc::from(reason)),
                EventValue::String(Arc::from(action)),
                EventValue::Int(bci),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a file read event.
pub fn emit_file_read_event(
    recorder: &mut FlightRecorder,
    path: &str,
    bytes_read: i64,
    end_of_file: bool,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(path)),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_file),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a file write event.
pub fn emit_file_write_event(
    recorder: &mut FlightRecorder,
    path: &str,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.FileWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(path)),
                EventValue::Long(bytes_written),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a socket read event.
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketRead") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(host)),
                EventValue::Int(port),
                EventValue::Long(bytes_read),
                EventValue::Boolean(end_of_stream),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a socket write event.
pub fn emit_socket_write_event(
    recorder: &mut FlightRecorder,
    host: &str,
    port: i32,
    bytes_written: i64,
    thread_id: u64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SocketWrite") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(host)),
                EventValue::Int(port),
                EventValue::Long(bytes_written),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SafepointBegin") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns,
            thread_id: 0,
            fields: vec![
                EventValue::Long(safepoint_id),
                EventValue::Int(total_threads),
                EventValue::Int(jni_critical_threads),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a safepoint end event.
pub fn emit_safepoint_end_event(
    recorder: &mut FlightRecorder,
    safepoint_id: i64,
    start_time_ns: u64,
    duration_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SafepointEnd") {
        let event = EventInstance {
            type_id,
            start_time: start_time_ns,
            end_time: start_time_ns.saturating_add(duration_ns),
            thread_id: 0,
            fields: vec![EventValue::Long(safepoint_id)],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a System.gc() event.
pub fn emit_system_gc_event(
    recorder: &mut FlightRecorder,
    invoked_concurrent: bool,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.SystemGC") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: vec![EventValue::Boolean(invoked_concurrent)],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.AllocationRequiringGC") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: vec![EventValue::Int(gc_id), EventValue::Long(size)],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.JavaExceptionThrow") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(message)),
                EventValue::String(Arc::from(thrown_class)),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit a network utilization event.
pub fn emit_network_utilization_event(
    recorder: &mut FlightRecorder,
    interface: &str,
    read_rate: i64,
    write_rate: i64,
    time_ns: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.NetworkUtilization") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id: 0,
            fields: vec![
                EventValue::String(Arc::from(interface)),
                EventValue::Long(read_rate),
                EventValue::Long(write_rate),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ThreadCPULoad") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: vec![
                EventValue::Float(user),
                EventValue::Float(system),
            ],
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// Emit an object allocation sample event.
pub fn emit_allocation_sample_event(
    recorder: &mut FlightRecorder,
    object_class: &str,
    weight: i64,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() { return; }
    static ID: OnceLock<EventTypeId> = OnceLock::new();
    if let Some(type_id) = cached_event_id(&ID, recorder, "jdk.ObjectAllocationSample") {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: vec![
                EventValue::String(Arc::from(object_class)),
                EventValue::Long(weight),
            ],
        };
        crate::repository::push_to_thread_ring(event);
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
        fields: fields.iter().map(|(n, t, d)| EventField::new(n, t, d)).collect(),
        has_thread,
        has_stacktrace,
        period: EventPeriod::None,
        threshold,
    };
    registry.register(event_type)
}

/// Emit a custom user event with the given fields.
///
/// Note: this cannot use a per-site `OnceLock` cache because `event_name` is
/// dynamic. The `is_enabled()` fast-path still skips the registry probe when
/// no recordings are active.
pub fn emit_custom_event(
    recorder: &mut FlightRecorder,
    event_name: &str,
    field_values: Vec<EventValue>,
    time_ns: u64,
    thread_id: u64,
) {
    if !crate::is_enabled() { return; }
    if let Some(type_id) = recorder.type_registry.find_by_name(event_name) {
        let event = EventInstance {
            type_id,
            start_time: time_ns,
            end_time: time_ns,
            thread_id,
            fields: field_values,
        };
        crate::repository::push_to_thread_ring(event);
    }
}

/// JFR configuration profile — corresponds to .jfc files (default.jfc, profile.jfc).
///
/// Defines which events are enabled and their settings (threshold, stacktrace, period).
#[derive(Debug, Clone)]
pub struct JfrProfile {
    pub name: String,
    pub description: String,
    pub settings: Vec<JfrEventSetting>,
}

/// Settings for a single event type in a JFR profile.
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
                JfrEventSetting { event_name: "jdk.GarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.GCPhasePause".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.YoungGarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.OldGarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.GCHeapSummary".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                // Thread events
                JfrEventSetting { event_name: "jdk.ThreadStart".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.ThreadEnd".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                // Class loading
                JfrEventSetting { event_name: "jdk.ClassLoad".into(), enabled: true, threshold: Some(Duration::from_millis(0)), stacktrace: Some(true), period: None },
                // JIT
                JfrEventSetting { event_name: "jdk.Compilation".into(), enabled: true, threshold: Some(Duration::from_millis(100)), stacktrace: None, period: None },
                // CPU/memory periodic
                JfrEventSetting { event_name: "jdk.CPULoad".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everyChunk".into()) },
                JfrEventSetting { event_name: "jdk.JavaThreadStatistics".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everyChunk".into()) },
                // File/socket I/O: higher threshold for low overhead
                JfrEventSetting { event_name: "jdk.FileRead".into(), enabled: true, threshold: Some(Duration::from_millis(20)), stacktrace: Some(false), period: None },
                JfrEventSetting { event_name: "jdk.FileWrite".into(), enabled: true, threshold: Some(Duration::from_millis(20)), stacktrace: Some(false), period: None },
                JfrEventSetting { event_name: "jdk.SocketRead".into(), enabled: true, threshold: Some(Duration::from_millis(20)), stacktrace: Some(false), period: None },
                JfrEventSetting { event_name: "jdk.SocketWrite".into(), enabled: true, threshold: Some(Duration::from_millis(20)), stacktrace: Some(false), period: None },
                // Exceptions: disabled by default in production
                JfrEventSetting { event_name: "jdk.JavaExceptionThrow".into(), enabled: false, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.JavaErrorThrow".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                // Allocation: sampled
                JfrEventSetting { event_name: "jdk.ObjectAllocationSample".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
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
                JfrEventSetting { event_name: "jdk.GarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.GCPhasePause".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.YoungGarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.OldGarbageCollection".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.GCHeapSummary".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.GCReferenceStatistics".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.AllocationRequiringGC".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.SystemGC".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                // Thread events
                JfrEventSetting { event_name: "jdk.ThreadStart".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.ThreadEnd".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.ThreadSleep".into(), enabled: true, threshold: Some(Duration::from_millis(10)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.ThreadPark".into(), enabled: true, threshold: Some(Duration::from_millis(10)), stacktrace: Some(true), period: None },
                // Monitor events
                JfrEventSetting { event_name: "jdk.JavaMonitorEnter".into(), enabled: true, threshold: Some(Duration::from_millis(10)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.JavaMonitorWait".into(), enabled: true, threshold: Some(Duration::from_millis(10)), stacktrace: Some(true), period: None },
                // Class loading
                JfrEventSetting { event_name: "jdk.ClassLoad".into(), enabled: true, threshold: Some(Duration::from_millis(0)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.ClassUnload".into(), enabled: true, threshold: None, stacktrace: None, period: None },
                // JIT
                JfrEventSetting { event_name: "jdk.Compilation".into(), enabled: true, threshold: Some(Duration::from_millis(0)), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.Deoptimization".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                // CPU/memory periodic
                JfrEventSetting { event_name: "jdk.CPULoad".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everySecond".into()) },
                JfrEventSetting { event_name: "jdk.ThreadCPULoad".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everySecond".into()) },
                JfrEventSetting { event_name: "jdk.JavaThreadStatistics".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everySecond".into()) },
                JfrEventSetting { event_name: "jdk.MetaspaceSummary".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everyChunk".into()) },
                // File/socket I/O: lower threshold for more detail
                JfrEventSetting { event_name: "jdk.FileRead".into(), enabled: true, threshold: Some(Duration::from_millis(1)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.FileWrite".into(), enabled: true, threshold: Some(Duration::from_millis(1)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.SocketRead".into(), enabled: true, threshold: Some(Duration::from_millis(1)), stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.SocketWrite".into(), enabled: true, threshold: Some(Duration::from_millis(1)), stacktrace: Some(true), period: None },
                // Exceptions: enabled for profiling
                JfrEventSetting { event_name: "jdk.JavaExceptionThrow".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.JavaErrorThrow".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                // Allocation: full tracking
                JfrEventSetting { event_name: "jdk.ObjectAllocationSample".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.ObjectAllocationInNewTLAB".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                JfrEventSetting { event_name: "jdk.ObjectAllocationOutsideTLAB".into(), enabled: true, threshold: None, stacktrace: Some(true), period: None },
                // Safepoints
                JfrEventSetting { event_name: "jdk.SafepointBegin".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                JfrEventSetting { event_name: "jdk.SafepointEnd".into(), enabled: true, threshold: Some(Duration::ZERO), stacktrace: None, period: None },
                // Execution sample for profiling
                JfrEventSetting { event_name: "jdk.ExecutionSample".into(), enabled: true, threshold: None, stacktrace: Some(true), period: Some("everySecond".into()) },
                JfrEventSetting { event_name: "jdk.NativeMethodSample".into(), enabled: true, threshold: None, stacktrace: Some(true), period: Some("everySecond".into()) },
                // Network
                JfrEventSetting { event_name: "jdk.NetworkUtilization".into(), enabled: true, threshold: None, stacktrace: None, period: Some("everySecond".into()) },
            ],
        }
    }

    /// Apply this profile's settings to a `RecordingSettings`.
    pub fn apply_to(&self, settings: &mut crate::recording::RecordingSettings, registry: &EventTypeRegistry) {
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
        assert_eq!(registry.len(), 47); // 28 original + 19 new T6.2 events
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
            "jdk.GarbageCollection", "jdk.GCPhasePause",
            "jdk.YoungGarbageCollection", "jdk.OldGarbageCollection",
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
            "jdk.ThreadStart", "jdk.ThreadEnd",
            "jdk.ThreadSleep", "jdk.ThreadPark",
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
        assert_eq!(ids.len(), unique.len(), "All builtin event IDs should be unique");
    }

    #[test]
    fn test_all_builtin_names_unique() {
        let mut reg = EventTypeRegistry::new();
        register_builtin_events(&mut reg);
        let names: Vec<&str> = reg.iter().map(|(_, et)| et.name.as_str()).collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(names.len(), unique.len(), "All builtin event names should be unique");
    }

    // --- Emission function tests ---

    fn make_recorder() -> FlightRecorder {
        let mut fr = FlightRecorder::new();
        register_builtin_events(&mut fr.type_registry);
        fr
    }

    #[test]
    fn test_emit_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1000, 500);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_class_load_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_class_load_event(&mut fr, "java/lang/Object", "bootstrap", "bootstrap", 2000, 100);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_start_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_thread_start_event(&mut fr, "main", "", 1, 3000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_compilation_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_compilation_event(&mut fr, "java/lang/String.hashCode:()I", 1, 3, true, false, 256, 128, 4000, 200);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_allocation_in_new_tlab_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_allocation_in_new_tlab_event(&mut fr, "java/lang/Object", 64, 4096, 1, 5000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_event_not_recorded_when_stopped() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        // Do NOT start the recording
        emit_gc_event(&mut fr, 1, "G1", "Test", 1000, 500);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 0);
    }

    // --- New emission function tests (M26 fixes) ---

    #[test]
    fn test_emit_thread_end_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_thread_end_event(&mut fr, "worker-1", 42, 6000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_sleep_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_thread_sleep_event(&mut fr, 1_000_000, 1, 7000, 1_000_000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_monitor_wait_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_monitor_wait_event(&mut fr, "java/lang/Object", "main", 0, false, 0xDEAD, 1, 8000, 500);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_monitor_enter_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_monitor_enter_event(&mut fr, "java/util/HashMap", "main", 0xBEEF, 1, 9000, 200);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_class_unload_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_class_unload_event(&mut fr, "com/example/OldClass", "app", 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_park_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_thread_park_event(&mut fr, "java/util/concurrent/locks/AQS", 0, 0xCAFE, 1, 11000, 300);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_gc_heap_summary_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_gc_heap_summary_event(&mut fr, 1, "Before GC", "G1 Eden", 1024 * 1024, 512 * 1024, 2048 * 1024, 12000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn test_emit_thread_sleep_saturating_add() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        // Use u64::MAX to test saturating_add doesn't overflow
        emit_thread_sleep_event(&mut fr, i64::MAX, 1, u64::MAX - 10, u64::MAX);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    // S41 — JFR Event Completeness

    #[test]
    fn s41_builtin_event_count_47() {
        let mut registry = EventTypeRegistry::new();
        register_builtin_events(&mut registry);
        assert_eq!(registry.len(), 47);
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
        fr.start_recording(rid);
        emit_deoptimization_event(
            &mut fr, "com/example/Foo.bar:()V", 1, "NullCheck", "Reinterpret", 42, 1, 10000,
        );
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 5);
        assert!(matches!(&events[0].fields[0], EventValue::String(s) if s.contains("Foo")));
    }

    #[test]
    fn s41_emit_file_read_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_file_read_event(&mut fr, "fd:3", 1024, false, 1, 10000, 500);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 3);
    }

    #[test]
    fn s41_emit_file_write_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_file_write_event(&mut fr, "fd:1", 512, 1, 10000, 300);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_socket_read_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_socket_read_event(&mut fr, "localhost", 8080, 256, false, 1, 10000, 2000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert_eq!(events[0].fields.len(), 4);
    }

    #[test]
    fn s41_emit_socket_write_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_socket_write_event(&mut fr, "example.com", 443, 128, 1, 10000, 1000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_gc_phase_pause_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_gc_phase_pause_event(&mut fr, 1, "Mark", 10000, 5000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_young_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_young_gc_event(&mut fr, 1, 15, 10000, 3000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_old_gc_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_old_gc_event(&mut fr, 2, 20000, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_metaspace_summary_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_metaspace_summary_event(&mut fr, 1, "After GC", 1024, 2048, 4096, 15000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_allocation_outside_tlab_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_allocation_outside_tlab_event(&mut fr, "java/lang/Object", 64, 1, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_execution_sample_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_execution_sample_event(&mut fr, "main", "Main.run:5", "RUNNABLE", 1, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_cpu_load_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_cpu_load_event(&mut fr, 0.25, 0.05, 0.60, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
        let events = rec.get_events();
        assert!(matches!(&events[0].fields[0], EventValue::Float(v) if (*v - 0.25).abs() < 0.001));
    }

    #[test]
    fn s41_emit_thread_statistics_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_thread_statistics_event(&mut fr, 10, 3, 50, 12, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_active_recording_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_active_recording_event(&mut fr, 1, "default", "/tmp/rec.jfr", 0, 0, 10000);
        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn s41_emit_active_setting_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_active_setting_event(&mut fr, 1, "threshold", "10ms", 10000);
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
        assert!(fr.type_registry.find_by_name("jdk.SafepointBegin").is_some());
        assert!(fr.type_registry.find_by_name("jdk.SafepointEnd").is_some());
    }

    #[test]
    fn t6_allocation_sample_exists() {
        let fr = crate::create_flight_recorder();
        let id = fr.type_registry.find_by_name("jdk.ObjectAllocationSample").unwrap();
        let et = fr.type_registry.get(id).unwrap();
        assert_eq!(et.fields.len(), 2);
        assert!(et.has_stacktrace);
    }

    #[test]
    fn t6_native_method_sample_exists() {
        let fr = crate::create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.NativeMethodSample").is_some());
    }

    #[test]
    fn t6_allocation_requiring_gc_exists() {
        let fr = crate::create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.AllocationRequiringGC").is_some());
    }

    #[test]
    fn t6_exception_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.JavaExceptionThrow").is_some());
        assert!(fr.type_registry.find_by_name("jdk.JavaErrorThrow").is_some());
        assert!(fr.type_registry.find_by_name("jdk.ExceptionStatistics").is_some());
    }

    #[test]
    fn t6_network_utilization_exists() {
        let fr = crate::create_flight_recorder();
        let id = fr.type_registry.find_by_name("jdk.NetworkUtilization").unwrap();
        let et = fr.type_registry.get(id).unwrap();
        assert_eq!(et.fields.len(), 3);
        assert_eq!(et.fields[0].name, "networkInterface");
    }

    #[test]
    fn t6_container_events_exist() {
        let fr = crate::create_flight_recorder();
        assert!(fr.type_registry.find_by_name("jdk.ContainerCPUUsage").is_some());
        assert!(fr.type_registry.find_by_name("jdk.ContainerMemoryUsage").is_some());
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
        assert_eq!(fr.type_registry.len(), 47);
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
        fr.start_recording(rid);
        emit_custom_event(
            &mut fr,
            "test.CustomEvent",
            vec![EventValue::from_str("hello")],
            1000,
            1,
        );
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    // --- JFR profiles ---

    #[test]
    fn t6_default_profile() {
        let profile = JfrProfile::default_profile();
        assert_eq!(profile.name, "default");
        assert!(!profile.settings.is_empty());
        // GC events should be enabled
        assert!(profile.settings.iter().any(|s| s.event_name == "jdk.GarbageCollection" && s.enabled));
        // Exception throw should be disabled in default profile
        assert!(profile.settings.iter().any(|s| s.event_name == "jdk.JavaExceptionThrow" && !s.enabled));
    }

    #[test]
    fn t6_detailed_profile() {
        let profile = JfrProfile::detailed_profile();
        assert_eq!(profile.name, "profile");
        assert!(!profile.settings.is_empty());
        // Exception throw should be enabled in detailed profile
        assert!(profile.settings.iter().any(|s| s.event_name == "jdk.JavaExceptionThrow" && s.enabled));
        // Safepoints should be enabled
        assert!(profile.settings.iter().any(|s| s.event_name == "jdk.SafepointBegin" && s.enabled));
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
        fr.start_recording(rid);
        emit_safepoint_begin_event(&mut fr, 1, 10, 0, 1000);
        emit_safepoint_end_event(&mut fr, 1, 1000, 500);
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 2);
    }

    #[test]
    fn t6_emit_exception_event() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_java_exception_throw_event(&mut fr, "test error", "java.lang.RuntimeException", 1000, 1);
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }

    #[test]
    fn t6_emit_network_utilization() {
        let mut fr = make_recorder();
        let rid = fr.new_recording(RecordingSettings::new("test"));
        fr.start_recording(rid);
        emit_network_utilization_event(&mut fr, "eth0", 1024, 512, 1000);
        let rec = fr.get_recording(rid).unwrap();
        assert_eq!(rec.event_count(), 1);
    }
}
