// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot's jvmstat perf-data file, `/tmp/hsperfdata_<user>/<pid>`
//! (`VmConfig::perf_data_file`, `CRATONVM_PERF_DATA_FILE`, default on;
//! interpreter round i3 wave 81, lane L1;
//! `docs/feature-designs/real-jdk-mode.md`).
//!
//! Other processes find a JVM through this file: `jps` and
//! `VirtualMachine.list()` list the numeric file names of every
//! `hsperfdata_*` directory (`LocalVmManager`), and the attach provider's
//! `testAttachable` reads the `sun.rt.jvmCapabilities` string counter, whose
//! first character is the attach bit (`MonitoredVmUtil.isAttachable`): `0`
//! makes an external `VirtualMachine.attach` throw
//! `AttachNotSupportedException("The VM does not support the attach
//! mechanism")` at once, as HotSpot run with `-XX:+DisableAttachMechanism`
//! answers. Without the file a client assumes any pid attachable.
//!
//! The layout is jvmstat v2.0 (`sun.jvmstat.perfdata.monitor.v2_0`
//! `PerfDataBufferPrologue` / `PerfDataBuffer`, HotSpot's `perfMemory.hpp`
//! and `perfData.cpp`), MEASURED on the Linux host (HotSpot 25.0.4,
//! `m81`/`hsperf.py`): a 32768-byte file, mode 0600 in a 0755 directory, a
//! 32-byte prologue (magic `cafec0c0` as bytes, byte order 1 = little
//! endian, version 2.0, accessible 1, `used`, overflow 0, a modification
//! time stamp, entry offset 32, entry count), then the entries, each a
//! 20-byte header (entry length, name offset 20, vector length, type code
//! `J` / `B`, flags (bit 0: supported), units, variability, data offset),
//! the NUL-terminated name, padding to the data's alignment, the data, and
//! padding to 8 bytes. A zero entry length ends the walk.
//!
//! The identity counters a client reads to find and name the VM (`jps -l`,
//! `VirtualMachine.list()`, `testAttachable`) are written once. Since
//! interpreter round i4 wave 86 (lane L1) the file also carries the live
//! counters `jstat -gc` and `jstat -class` read ([`LIVE_COUNTERS`],
//! `sun.os.hrt.frequency`), which a sampler thread ([`start_sampler`])
//! writes in place through a shared writable mapping every 100 ms from the
//! collector's and class manager's own counters; the interpreter is not
//! touched.

/// The file's size: HotSpot's default `PerfDataMemorySize`, a multiple of
/// the page size its reader's `mmap` requires (MEASURED: 32768).
pub(crate) const PERF_DATA_SIZE: usize = 32768;

/// `sizeof(PerfDataPrologue)`.
const PROLOGUE_SIZE: usize = 32;

/// `sizeof(PerfDataEntry)`, the fixed part of an entry.
const ENTRY_HEADER_SIZE: usize = 20;

/// `sun.jvmstat.monitor.Units` codes.
pub(crate) const UNITS_NONE: u8 = 1;
pub(crate) const UNITS_STRING: u8 = 5;

/// `sun.jvmstat.monitor.Variability` codes.
pub(crate) const VARIABILITY_CONSTANT: u8 = 1;
pub(crate) const VARIABILITY_VARIABLE: u8 = 3;

/// One counter's value: a `long` (type `J`), or a string of `capacity`
/// bytes including its NUL (type `B`, units `String`), as HotSpot's
/// `PerfStringConstant` sizes it (`strlen + 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PerfValue {
    Long(i64),
    Str { text: String, capacity: usize },
}

/// One perf-data counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerfEntry {
    pub(crate) name: String,
    pub(crate) value: PerfValue,
    /// The `supported` flag (bit 0): HotSpot sets it for the `java.*`
    /// namespaces and clears it for `sun.*`.
    pub(crate) supported: bool,
    pub(crate) units: u8,
    pub(crate) variability: u8,
}

/// HotSpot's default `-XX:PerfMaxStringConstLength`: a `PerfStringConstant`
/// holds at most this many bytes of its text (plus the NUL).
pub(crate) const PERF_MAX_STRING_CONST_LENGTH: usize = 1024;

impl PerfEntry {
    /// A string constant sized to its text (`PerfStringConstant`:
    /// `MIN2(strlen + 1, PerfMaxStringConstLength + 1)`, so a longer text,
    /// a long `java.rt.vmArgs` for one, is cut at 1024 bytes).
    pub(crate) fn string_constant(name: &str, text: &str, supported: bool) -> Self {
        PerfEntry {
            name: name.to_string(),
            value: PerfValue::Str {
                text: text.to_string(),
                capacity: (text.len() + 1).min(PERF_MAX_STRING_CONST_LENGTH + 1),
            },
            supported,
            units: UNITS_STRING,
            variability: VARIABILITY_CONSTANT,
        }
    }
}

fn put_i32(buf: &mut [u8], at: usize, value: i32) {
    buf[at..at + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_i64(buf: &mut [u8], at: usize, value: i64) {
    buf[at..at + 8].copy_from_slice(&value.to_ne_bytes());
}

/// The prologue, in this machine's byte order (HotSpot writes native
/// order and records which).
pub(crate) fn encode_prologue(
    used: i32,
    mod_time_stamp: i64,
    num_entries: i32,
) -> [u8; PROLOGUE_SIZE] {
    let mut p = [0u8; PROLOGUE_SIZE];
    // `PERFDATA_MAGIC` 0xcafec0c0, always these bytes in this order.
    p[0..4].copy_from_slice(&[0xca, 0xfe, 0xc0, 0xc0]);
    // `PERFDATA_BIG_ENDIAN` 0 / `PERFDATA_LITTLE_ENDIAN` 1.
    p[4] = if cfg!(target_endian = "big") { 0 } else { 1 };
    p[5] = 2; // major version
    p[6] = 0; // minor version
    p[7] = 1; // accessible
    put_i32(&mut p, 8, used);
    put_i32(&mut p, 12, 0); // overflow
    put_i64(&mut p, 16, mod_time_stamp);
    put_i32(&mut p, 24, PROLOGUE_SIZE as i32); // entry offset
    put_i32(&mut p, 28, num_entries);
    p
}

/// One entry as HotSpot's `PerfData::create_entry` lays it out.
pub(crate) fn encode_entry(entry: &PerfEntry) -> Vec<u8> {
    let (type_code, element_size, elements, vector_length) = match &entry.value {
        PerfValue::Long(_) => (b'J', 8usize, 1usize, 0usize),
        PerfValue::Str { capacity, .. } => (b'B', 1, *capacity, *capacity),
    };
    let name_len = entry.name.len() + 1;
    let mut size = ENTRY_HEADER_SIZE + name_len;
    if size % element_size != 0 {
        size += element_size - size % element_size;
    }
    let data_offset = size;
    size += element_size * elements;
    size = (size + 7) & !7;

    let mut buf = vec![0u8; size];
    put_i32(&mut buf, 0, size as i32);
    put_i32(&mut buf, 4, ENTRY_HEADER_SIZE as i32);
    put_i32(&mut buf, 8, vector_length as i32);
    buf[12] = type_code;
    buf[13] = u8::from(entry.supported);
    buf[14] = entry.units;
    buf[15] = entry.variability;
    put_i32(&mut buf, 16, data_offset as i32);
    buf[ENTRY_HEADER_SIZE..ENTRY_HEADER_SIZE + entry.name.len()]
        .copy_from_slice(entry.name.as_bytes());
    match &entry.value {
        PerfValue::Long(v) => put_i64(&mut buf, data_offset, *v),
        PerfValue::Str { text, capacity } => {
            // At most `capacity - 1` bytes, so a NUL always ends the text.
            let n = text.len().min(capacity.saturating_sub(1));
            buf[data_offset..data_offset + n].copy_from_slice(&text.as_bytes()[..n]);
        }
    }
    buf
}

/// The whole file: the prologue, `entries` in order (those that would not
/// fit are left out), zeros to [`PERF_DATA_SIZE`]. Test-only since wave 86
/// moved the writer to [`encode_perf_data_with_offsets`] (interpreter round
/// i4 wave 96, lane L7: the test-only public API gate).
#[cfg(test)]
pub(crate) fn encode_perf_data(entries: &[PerfEntry], mod_time_stamp: i64) -> Vec<u8> {
    encode_perf_data_with_offsets(entries, mod_time_stamp).0
}

/// `encode_perf_data`, with each entry's data offset in the file
/// (`usize::MAX` for one left out), for the counters updated in place
/// (interpreter round i4 wave 86, lane L1).
pub(crate) fn encode_perf_data_with_offsets(
    entries: &[PerfEntry],
    mod_time_stamp: i64,
) -> (Vec<u8>, Vec<usize>) {
    let mut buf = vec![0u8; PERF_DATA_SIZE];
    let mut offsets = Vec::with_capacity(entries.len());
    let mut at = PROLOGUE_SIZE;
    let mut count = 0i32;
    for entry in entries {
        let bytes = encode_entry(entry);
        if at + bytes.len() > PERF_DATA_SIZE {
            offsets.push(usize::MAX);
            continue;
        }
        let data_offset = i32::from_ne_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        offsets.push(at + usize::try_from(data_offset).unwrap_or(0));
        buf[at..at + bytes.len()].copy_from_slice(&bytes);
        at += bytes.len();
        count += 1;
    }
    let prologue = encode_prologue(at as i32, mod_time_stamp, count);
    buf[..PROLOGUE_SIZE].copy_from_slice(&prologue);
    (buf, offsets)
}

/// `sun.jvmstat.monitor.Units` codes of the live counters.
const UNITS_BYTES: u8 = 2;
const UNITS_TICKS: u8 = 3;
const UNITS_EVENTS: u8 = 4;
const UNITS_HERTZ: u8 = 6;
/// `sun.jvmstat.monitor.Variability.MONOTONIC`.
const VARIABILITY_MONOTONIC: u8 = 2;

// The `sun.os.hrt.frequency` this VM's tick counters use, and their scaling
// from the nanoseconds and milliseconds the VM keeps
// (`cratonvm_types::perf_ticks`). zt t21/pl10
// (`CRATONVM_PERF_HRT_QPC_FREQUENCY`, default ON, `=0` kill): HotSpot's own
// unit, the performance counter's frequency on Windows (10000000; it was
// 1000000000 here, so a program reading the counter or
// `Perf.highResFrequency` saw another value; `jstat` only divides by it),
// nanoseconds elsewhere. Every tick written below goes through
// `ticks_from_nanos` / `ticks_from_millis`, so all of them agree with it.
use cratonvm_types::perf_ticks::{hrt_frequency, ticks_from_nanos};

/// The counters `jstat -gc` and `jstat -class` read (`jstat_options`), kept
/// live in the file (interpreter round i4 wave 86, lane L1): name, units,
/// variability, the `supported` flag. Indices are the `LIVE_*` constants;
/// [`sample_live_counters`] fills them in this order.
///
/// zt t9/pl: the last seven are the `sun.rt.*` safepoint counters HotSpot's
/// `RuntimeService` keeps and the `sun.threads.*` ones its VM thread and
/// `CPUTimeCounters` keep, read in-process through `Perf.attach(0)`
/// (`sun.management.HotspotRuntime` / `HotspotThread`'s internal counters;
/// [`PerfDataFile::attach_view`]).
pub(crate) const LIVE_COUNTERS: [(&str, u8, u8, bool); LIVE_COUNT] = [
    ("sun.os.hrt.ticks", UNITS_TICKS, VARIABILITY_VARIABLE, false),
    ("sun.gc.collector.0.invocations", UNITS_EVENTS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.collector.0.time", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.collector.1.invocations", UNITS_EVENTS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.collector.1.time", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.collector.2.invocations", UNITS_EVENTS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.collector.2.time", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.gc.generation.0.space.0.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.0.space.0.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.0.space.1.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.0.space.1.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.0.space.2.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.0.space.2.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.1.space.0.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.generation.1.space.0.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.metaspace.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.metaspace.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.compressedclassspace.capacity", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("sun.gc.compressedclassspace.used", UNITS_BYTES, VARIABILITY_VARIABLE, false),
    ("java.cls.loadedClasses", UNITS_EVENTS, VARIABILITY_MONOTONIC, true),
    ("java.cls.unloadedClasses", UNITS_EVENTS, VARIABILITY_MONOTONIC, true),
    ("java.cls.sharedLoadedClasses", UNITS_EVENTS, VARIABILITY_MONOTONIC, true),
    ("java.cls.sharedUnloadedClasses", UNITS_EVENTS, VARIABILITY_MONOTONIC, true),
    ("sun.cls.loadedBytes", UNITS_BYTES, VARIABILITY_MONOTONIC, false),
    ("sun.cls.unloadedBytes", UNITS_BYTES, VARIABILITY_MONOTONIC, false),
    ("sun.cls.sharedLoadedBytes", UNITS_BYTES, VARIABILITY_MONOTONIC, false),
    ("sun.cls.sharedUnloadedBytes", UNITS_BYTES, VARIABILITY_MONOTONIC, false),
    ("sun.cls.time", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.rt.safepoints", UNITS_EVENTS, VARIABILITY_MONOTONIC, false),
    ("sun.rt.safepointTime", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.rt.safepointSyncTime", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.rt.applicationTime", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.threads.vmOperationTime", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.threads.total_gc_cpu_time", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
    ("sun.threads.cpu_time.vm", UNITS_TICKS, VARIABILITY_MONOTONIC, false),
];
/// How many [`LIVE_COUNTERS`] there are.
pub(crate) const LIVE_COUNT: usize = 35;
const LIVE_TICKS: usize = 0;
const LIVE_COLLECTOR: usize = 1; // + 2 * n (invocations), + 2 * n + 1 (time)
const LIVE_EDEN: usize = 7; // capacity, used
const LIVE_S0: usize = 9;
const LIVE_S1: usize = 11;
const LIVE_OLD: usize = 13;
const LIVE_METASPACE: usize = 15;
const LIVE_CCS: usize = 17;
const LIVE_LOADED: usize = 19;
const LIVE_UNLOADED: usize = 20;
const LIVE_LOADED_BYTES: usize = 23;
const LIVE_UNLOADED_BYTES: usize = 24;
const LIVE_CLS_TIME: usize = 27;
const LIVE_SAFEPOINTS: usize = 28; // count, time, sync time, application time
const LIVE_VM_OPERATION_TIME: usize = 32;
const LIVE_VM_THREAD_CPU: usize = 34;
/// zt pl4 item 3: `sun.threads.total_gc_cpu_time` in [`LIVE_COUNTERS`].
const LIVE_GC_TOTAL_CPU: usize = 33;

/// `sun.os.hrt.frequency` and the [`LIVE_COUNTERS`] at zero, in that order.
pub(crate) fn live_entries() -> Vec<PerfEntry> {
    let mut entries = vec![PerfEntry {
        name: "sun.os.hrt.frequency".to_string(),
        value: PerfValue::Long(hrt_frequency()),
        supported: false,
        units: UNITS_HERTZ,
        variability: VARIABILITY_CONSTANT,
    }];
    entries.extend(LIVE_COUNTERS.iter().map(|&(name, units, variability, supported)| {
        PerfEntry {
            name: name.to_string(),
            value: PerfValue::Long(0),
            supported,
            units,
            variability,
        }
    }));
    entries
}

/// The heap half of a live sample, read inside
/// `GcBarrier::run_if_no_stw_requested`: per collector bean (at most three)
/// `(count, time ms)`, per space `(capacity, used)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HeapCounters {
    pub(crate) collectors: [(u64, u64); 3],
    pub(crate) eden: (u64, u64),
    pub(crate) s0: (u64, u64),
    pub(crate) s1: (u64, u64),
    pub(crate) old: (u64, u64),
}

/// The class half of a live sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ClassCounters {
    pub(crate) loaded_total: u64,
    pub(crate) unloaded: u64,
    pub(crate) metadata_bytes: u64,
    /// gci i33/ms: the committed metaspace (`sun.gc.metaspace.capacity`,
    /// HotSpot's `MetaspaceUtils::committed_bytes()`; jstat `MC`).
    pub(crate) metadata_committed: u64,
    /// gci i33/ms: the compressed class space's `(committed, used)`
    /// (`sun.gc.compressedclassspace.capacity` / `.used`; jstat `CCSC` /
    /// `CCSU`); `(0, 0)` without one.
    pub(crate) class_space: (u64, u64),
    /// zt t16/pl7: `sun.cls.time`, the class-trace events' self times in
    /// nanoseconds (`ClassTraceCounters::accumulated`; [`live_values`]
    /// writes them in ticks, zt t21/pl10); 0 under
    /// `CRATONVM_PERF_CLASS_TIMERS=0` (default ON since zt t26/rt2).
    pub(crate) time_ticks: u64,
    /// zt t29/cb: the model bytes of every class unloaded so far
    /// (`ClassManager::unloaded_class_bytes`, read with the walk that gave
    /// `metadata_bytes`): `sun.cls.unloadedBytes`, and added to
    /// `sun.cls.loadedBytes`; 0 under
    /// `CRATONVM_PERF_CLASS_BYTES_CUMULATIVE=0`.
    pub(crate) unloaded_bytes: u64,
}

/// zt t9/pl: the runtime half of a live sample, the GC barrier's safepoint
/// accounting (`GcBarrier::safepoint_stats`, nanoseconds, written in ticks of
/// `hrt_frequency`); all 0 under `CRATONVM_JMX_SAFEPOINT_COUNTERS=0`, as
/// the `VMManagementImpl` natives answer then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RuntimeCounters {
    pub(crate) safepoints: u64,
    pub(crate) stopped_ns: u64,
    pub(crate) sync_ns: u64,
    pub(crate) application_ns: u64,
    /// zt t30/cv (`zgc-zt12pl4-*` item 3): the CPU time, nanoseconds, the
    /// threads that ran the VM's stop-the-world operations spent in them
    /// (`GcBarrier::vm_thread_cpu_ns`): `sun.threads.cpu_time.vm`. Its own
    /// switch, `CRATONVM_PERF_VM_THREAD_CPU`; 0 off Linux and Windows.
    pub(crate) vm_thread_cpu_ns: u64,
    /// zt pl4 item 3 (G1): the GC worker threads' CPU, nanoseconds
    /// (`VmHeap::g1_gc_thread_cpu_ns`, parallel workers plus concurrent
    /// mark): `sun.threads.total_gc_cpu_time`. 0 on the other collectors, as
    /// HotSpot's ZGC and Serial read; `CRATONVM_PERF_GC_THREAD_CPU=0` 0.
    pub(crate) gc_thread_cpu_ns: u64,
}

/// The [`LIVE_COUNTERS`] values for `ticks` nanoseconds since the VM began
/// and the halves of a sample: the collectors' times in ticks
/// (`hrt_frequency`), the young generation as eden plus the two survivor
/// spaces, the old generation, metaspace as the class metadata (gci i33/ms:
/// capacity the committed figure and the compressed class space its own,
/// `vm_init::metaspace_stats`; `capacity == used` and the class space 0
/// under `CRATONVM_METASPACE_HOTSPOT_SPACES=0`), the
/// classes loaded as the cumulative total (`java.cls.loadedClasses` counts
/// every class ever loaded) and their bytes as the class metadata, the
/// shared (CDS) counters 0, and `sun.cls.time` the class-trace self times
/// (zt t16/pl7; 0 under `CRATONVM_PERF_CLASS_TIMERS=0`).
///
/// zt t9/pl: `sun.rt.safepoints` / `safepointTime` / `safepointSyncTime` /
/// `applicationTime` are the barrier's four figures, as HotSpot's
/// `RuntimeService` keeps them; `sun.threads.vmOperationTime` is the time
/// stopped less the time to reach the stop (HotSpot's VM thread adds only
/// the operation's own evaluation, after the safepoint is reached).
/// `sun.threads.total_gc_cpu_time` is HotSpot's sum of its GC worker
/// groups' CPU (`CPUTimeCounters`): its ZGC and Serial keep no group and read
/// 0 (measured on 25.0.3 Windows and 25.0.4 Linux), its G1 reads its
/// parallel and concurrent workers; here `runtime.gc_thread_cpu_ns`, G1's
/// pool helpers plus concurrent-mark workers (zt pl4 item 3), 0 on the other
/// collectors. zt t30/cv (`zgc-zt12pl4-*` item 3): `sun.threads.cpu_time.vm`
/// is the VM thread's CPU (`rt2_pc_J`: `zero=false`); it is
/// `runtime.vm_thread_cpu_ns`. Both in nanoseconds as HotSpot's
/// `CPUTimeCounters` write `os::thread_cpu_time` (not scaled to ticks); on
/// Windows with that clock's quantum resolution, as HotSpot's.
pub(crate) fn live_values(
    ticks: i64,
    heap: &HeapCounters,
    classes: &ClassCounters,
    runtime: &RuntimeCounters,
) -> [i64; LIVE_COUNT] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let mut v = [0i64; LIVE_COUNT];
    v[LIVE_TICKS] = ticks;
    // zt t27/lg1: the collectors' time is nanoseconds
    // ([`collector_time_ns`]), in ticks as the other timers.
    for (i, &(count, time_ns)) in heap.collectors.iter().enumerate() {
        v[LIVE_COLLECTOR + 2 * i] = n(count);
        v[LIVE_COLLECTOR + 2 * i + 1] = ticks_from_nanos(time_ns);
    }
    for (at, (capacity, used)) in [
        (LIVE_EDEN, heap.eden),
        (LIVE_S0, heap.s0),
        (LIVE_S1, heap.s1),
        (LIVE_OLD, heap.old),
        (LIVE_METASPACE, (classes.metadata_committed, classes.metadata_bytes)),
        (LIVE_CCS, classes.class_space),
    ] {
        v[at] = n(capacity);
        v[at + 1] = n(used);
    }
    v[LIVE_LOADED] = n(classes.loaded_total);
    v[LIVE_UNLOADED] = n(classes.unloaded);
    // zt t29/cb (perf-counter page item 1): HotSpot counts both
    // cumulatively (`ClassLoadingService` `_classbytes_loaded` /
    // `_classbytes_unloaded`), so `loaded - unloaded = live` and an unload
    // never lowers `loadedBytes`. `unloaded_bytes` 0: the live figure, as
    // before.
    v[LIVE_LOADED_BYTES] = n(classes
        .metadata_bytes
        .saturating_add(classes.unloaded_bytes));
    v[LIVE_UNLOADED_BYTES] = n(classes.unloaded_bytes);
    // zt t21/pl10: nanoseconds in ticks (`hrt_frequency`).
    v[LIVE_CLS_TIME] = ticks_from_nanos(classes.time_ticks);
    v[LIVE_SAFEPOINTS] = n(runtime.safepoints);
    v[LIVE_SAFEPOINTS + 1] = ticks_from_nanos(runtime.stopped_ns);
    v[LIVE_SAFEPOINTS + 2] = ticks_from_nanos(runtime.sync_ns);
    v[LIVE_SAFEPOINTS + 3] = ticks_from_nanos(runtime.application_ns);
    v[LIVE_VM_OPERATION_TIME] =
        ticks_from_nanos(runtime.stopped_ns.saturating_sub(runtime.sync_ns));
    v[LIVE_VM_THREAD_CPU] = n(runtime.vm_thread_cpu_ns);
    // zt pl4 item 3: nanoseconds, as HotSpot's `CPUTimeCounters` write
    // `os::thread_cpu_time` (not scaled to ticks).
    v[LIVE_GC_TOTAL_CPU] = n(runtime.gc_thread_cpu_ns);
    v
}

/// zt t9/pl: the runtime half of a sample from the GC barrier's safepoint
/// accounting; zeros under `CRATONVM_JMX_SAFEPOINT_COUNTERS=0` (`enabled`
/// false).
fn runtime_counters(shared: &crate::vm::SharedVm, enabled: bool) -> RuntimeCounters {
    // zt t30/cv: the VM-thread CPU has its own switch
    // (`CRATONVM_PERF_VM_THREAD_CPU`, read where it is booked), not this one.
    let vm_thread_cpu_ns = shared.mem.gc_barrier.vm_thread_cpu_ns();
    // zt pl4 item 3: G1's worker threads (its own switch,
    // `CRATONVM_PERF_GC_THREAD_CPU`, read where the CPU is booked).
    let gc_thread_cpu_ns = shared
        .mem
        .heap
        .g1_gc_thread_cpu_ns()
        .map_or(0, |(parallel, mark)| parallel.saturating_add(mark));
    if !enabled {
        return RuntimeCounters {
            vm_thread_cpu_ns,
            gc_thread_cpu_ns,
            ..RuntimeCounters::default()
        };
    }
    let s = shared.mem.gc_barrier.safepoint_stats();
    RuntimeCounters {
        safepoints: s.count,
        stopped_ns: s.stopped_ns,
        sync_ns: s.sync_ns,
        application_ns: s.application_ns,
        vm_thread_cpu_ns,
        gc_thread_cpu_ns,
    }
}

/// How often the sampler thread refreshes the live counters: HotSpot's
/// `PerfDataSamplingInterval` default (50 ms) is for its tick counter alone;
/// the GC and class counters it updates at the events themselves. One
/// refresh every 100 ms keeps `jstat <pid> <interval>` (whose interval is at
/// least 1 ms, usually 1000) current to a tenth of a second.
const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// zt t14/pl5: the sampler's period, [`SAMPLE_INTERVAL`] unless
/// `CRATONVM_DBG_PERF_SAMPLE_INTERVAL_MS=<n>` (a positive number of
/// milliseconds; diagnostic) sets another. A row that must tell an
/// event-time write (`CRATONVM_PERF_EVENT_REFRESH`) from a sampler pass sets
/// it far beyond the run (`60000`), so no pass can land between a collection
/// and the read (the t12 `pc-ER0` kill printed `true` when one did). Read
/// once per sampler.
fn sample_interval() -> std::time::Duration {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map_or(SAMPLE_INTERVAL, std::time::Duration::from_millis)
}

/// zt t20/pl9 (t16 defect D9): park the sampler until `interval` has passed,
/// `stop` is set ([`stop_sampler`]) or `wake` asks for a pass
/// ([`record_vm_init_done`]); `wake` is cleared on the way out. Both flags are
/// set before their unpark, so the unpark's token makes a park that has not
/// begun yet return at once.
///
/// `std::thread::park_timeout` may return before its timeout with no unpark
/// (std documents spurious wake-ups), and the loop took every return as a full
/// interval: an unasked pass then wrote the image while
/// `CRATONVM_DBG_PERF_SAMPLE_INTERVAL_MS=60000` parked the sampler, so the
/// event-refresh kill rows read figures no event wrote (t16 `pl7_values_SIR0`:
/// the TLAB counters of a mid-run collection with both refreshes off and the
/// define counters never written; `pl5_pc_SIER0-3`: `sun.rt.safepoints`
/// grew with `CRATONVM_PERF_EVENT_REFRESH=0`, which also turns the ZGC pause
/// refresh off). In a default run this only removed an extra pass.
fn park_for_interval(
    interval: std::time::Duration,
    stop: &std::sync::atomic::AtomicBool,
    wake: &std::sync::atomic::AtomicBool,
) {
    let acquire = std::sync::atomic::Ordering::Acquire;
    let deadline = std::time::Instant::now().checked_add(interval);
    loop {
        if stop.load(acquire) || wake.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let left = match deadline {
            Some(deadline) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return;
                }
                deadline - now
            }
            // An interval past `Instant`'s range: park in steps of it.
            None => interval,
        };
        std::thread::park_timeout(left);
    }
}

/// How often the class-metadata walk runs (it holds the class-manager read
/// lock for one pass over the class store): every 5 s, or after a
/// collection, as HotSpot updates its metaspace counters at each GC.
const METADATA_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The heap half of a sample, from the collector's own bean state (the same
/// sources as `GarbageCollectorMXBean` / `MemoryPoolMXBean`): Serial's
/// `Copy` / `MarkSweepCompact` and `Eden Space` / `Survivor Space` /
/// `Tenured Gen` on the generational backend (both survivor spaces the
/// survivor pool's capacity, the from-space holding its use); elsewhere the
/// backend's beans in order and the young / old generation totals.
fn heap_counters(shared: &crate::vm::SharedVm) -> HeapCounters {
    let mut s = HeapCounters::default();
    let as_u64 = |v: usize| v as u64; // Widening: usize -> u64
    match shared.mem.heap.gen_jmx_collectors().zip(shared.mem.heap.gen_jmx_memory_pools()) {
        Some((collectors, pools)) => {
            for (slot, c) in s.collectors.iter_mut().zip(collectors.iter()) {
                *slot = (c.count, collector_time_ns(c));
            }
            for p in pools.iter() {
                if p.name.contains("Eden") {
                    s.eden = (p.committed, p.used);
                } else if p.name.contains("Survivor") {
                    s.s0 = (p.committed, p.used);
                    s.s1 = (p.committed, 0);
                } else {
                    s.old = (p.committed, p.used);
                }
            }
        }
        None => {
            let heap = &shared.mem.heap;
            if let Some(beans) = shared.mem.backend_gc_beans() {
                s.collectors = backend_collector_slots(heap, &beans.collectors());
            }
            let (young_used, young_capacity) = heap.young_gen_stats();
            let (old_used, old_capacity) = heap.old_gen_stats();
            s.eden = (as_u64(young_capacity), as_u64(young_used));
            s.old = (as_u64(old_capacity), as_u64(old_used));
        }
    }
    s
}

/// Start the thread that keeps the live counters current, for the
/// perf-data file `shared` published (interpreter round i4 wave 86, lane
/// L1). Nothing when the VM wrote no file or could not map it. The thread
/// holds only a weak reference between samples and ends when the VM is gone
/// or the file was removed. The interpreter and the collectors are not
/// touched: everything is read from the counters they already keep.
pub(crate) fn start_sampler(shared: &std::sync::Arc<crate::vm::SharedVm>) {
    let live = shared
        .debug
        .perf_data_file
        .lock()
        .as_ref()
        .is_some_and(|file| !file.live_offsets.is_empty());
    if !live {
        return;
    }
    let weak = std::sync::Arc::downgrade(shared);
    // `sun.os.hrt.ticks`' origin. zt t22/x18: this VM's start under
    // `CRATONVM_PERF_HRT_VM_ORIGIN` (see [`hrt_ticks_origin`]).
    let started = hrt_ticks_origin(
        shared.debug.diagnostic_counters.start_instant(),
        std::time::Instant::now(),
        true,
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = std::sync::Arc::clone(&stop);
    let stopped = {
        let stopping = std::sync::Arc::clone(&stopping);
        move || stopping.load(std::sync::atomic::Ordering::Acquire)
    };
    // zt t20/pl9: a pass asked for before the interval ends.
    let wake = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waking = std::sync::Arc::clone(&wake);
    // zt t9/pl: read once per VM, as the JMX safepoint natives' switch is.
    let safepoint_counters =
        true;
    // zt t11/pl3: the ZGC-only figures (class counts, the opt-in tally).
    let zgc = shared.mem.heap.is_zgc();
    let interval = sample_interval();
    // zt t18/pl8: read once per VM, at the sampler's start.
    let sample_at_boot = true;
    // zt t20/pl9: read once per VM, at the sampler's start.
    let metaspace_max_before_gc = true;
    let app_class_path_defines_counted = define_counts_class_path_enabled();
    // zt t21/pl10: read once per VM, at the sampler's start (ZGC only).
    let metaspace_at_gc = zgc;
    // zt t29/cb: read once per VM, at the sampler's start.
    let class_bytes_cumulative = class_bytes_cumulative_enabled();
    let spawned = std::thread::Builder::new()
        .name("cratonvm-perf-data".to_string())
        .spawn(move || {
            let mut heap = HeapCounters::default();
            let mut metadata = crate::vm::vm_init::MetaspaceStats::default();
            // zt t29/cb: the unloaded classes' bytes at that walk.
            let mut metadata_unloaded_bytes = 0u64;
            // `sun.cls.methodBytes`, taken with the metaspace walk.
            let mut method_bytes = 0u64;
            let mut metadata_at: Option<(std::time::Instant, u64)> = None;
            // zt t21/pl10: the metaspace figures written at the last collection.
            let mut metaspace_held: Option<(u64, MetaspaceFigures)> = None;
            let mut class_count_memo: Option<(usize, u64, u64)> = None;
            // zt t15/pl6: the `sun.ci.lastMethod` written last.
            let mut last_method_written: Option<(std::sync::Arc<str>, std::sync::Arc<str>)> = None;
            // zt t18/pl8: the `sun.ci.lastFailedMethod` written last.
            let mut last_failed_written: Option<(std::sync::Arc<str>, std::sync::Arc<str>)> = None;
            // zt t18/pl8: the first pass runs at once, not one interval in:
            // HotSpot's sampled counters hold their figures from creation,
            // and these read 0 until a pass. The boot's end unparks the
            // sampler for one more pass (`record_vm_init_done`).
            // `CRATONVM_PERF_SAMPLE_AT_BOOT=0`: the first pass one interval in.
            let mut first_pass = sample_at_boot;
            loop {
                // Parked, not slept: `stop_sampler` unparks it (wave 87).
                // zt t20/pl9: for the whole interval, unless stopped or a
                // pass is asked for (`park_for_interval`).
                if !std::mem::take(&mut first_pass) {
                    park_for_interval(interval, &stopping, &waking);
                }
                // Never take a strong reference once the VM's teardown asked
                // the sampler to stop (wave 87): the owner's release must be
                // the last one.
                if stopped() {
                    return;
                }
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                // The collector's state, only while no pause is requested
                // (the JDWP server's rule for reading the heap off a
                // mutator); a sample a pause pre-empts keeps the last one.
                let mut fresh = None;
                shared
                    .mem
                    .gc_barrier
                    .run_if_no_stw_requested(|| fresh = Some(heap_counters(&shared)));
                if let Some(fresh) = fresh {
                    heap = fresh;
                }
                let collections: u64 = heap.collectors.iter().map(|&(count, _)| count).sum();
                let due = metadata_at.is_none_or(|(at, seen)| {
                    seen != collections || at.elapsed() >= METADATA_INTERVAL
                });
                // The class-store walk (the long part) never while a stop is
                // requested.
                if stopped() {
                    return;
                }
                if due {
                    // gci i33/ms: HotSpot's statistics (committed, the class
                    // space), the walk `heap_summary` reports from. zt
                    // t11/pl3: on ZGC under
                    // `CRATONVM_ZGC_METASPACE_EDGE_TALLY` (default ON), from the running
                    // arena tally instead (the classes defined since).
                    let cm = shared.classes.class_manager.read();
                    metadata = if zgc {
                        crate::vm::vm_init::metaspace_stats_for_zgc_sample(&cm)
                    } else {
                        crate::vm::vm_init::metaspace_stats(&cm)
                    };
                    // zt t29/cb: under the same read lock as the walk (the
                    // unload transaction adds under the write lock), so the
                    // two figures are of one moment and their sum never
                    // goes back.
                    metadata_unloaded_bytes = cm.unloaded_class_bytes();
                    method_bytes = crate::vm::vm_init::hs_live_method_bytes(&cm);
                    metadata_at = Some((std::time::Instant::now(), collections));
                }
                let unloaded = shared
                    .debug
                    .diagnostic_counters
                    .classes_unloaded
                    .load(std::sync::atomic::Ordering::Relaxed);
                // zt t11/pl3: the bean's instance-class count
                // (`CRATONVM_ZGC_PERF_CLASS_COUNTS`; every collector since
                // g1gen/c8).
                let (live_classes, class_path_defines, sys_class_bytes) = {
                    let cm = shared.classes.class_manager.read();
                    let live = sampled_live_classes(&cm, unloaded, due, &mut class_count_memo);
                    // zt t20/pl9: the application loader's class-path defines.
                    // zt t29/cb: the boot loader's class-file bytes.
                    (live, cm.app_class_path_defines(), cm.sys_class_path_bytes())
                };
                // zt t16/pl7: the class-trace timers (opt-in).
                let class_times = class_trace(&shared).map(|c| c.snapshot());
                // zt pl4 item 1: `CLASS_LOAD` and `sysClassLoadTime`.
                let class_loads = class_trace(&shared).map(|c| c.load_snapshot());
                let classes = ClassCounters {
                    loaded_total: live_classes + unloaded,
                    unloaded,
                    metadata_bytes: metadata.used,
                    metadata_committed: metadata.committed,
                    class_space: metadata
                        .class_space
                        .map_or((0, 0), |(used, committed, _)| (committed, used)),
                    time_ticks: class_times.map_or(0, |(_, total)| total),
                    unloaded_bytes: if class_bytes_cumulative {
                        metadata_unloaded_bytes
                    } else {
                        0
                    },
                };
                let runtime = runtime_counters(&shared, safepoint_counters);
                // zt t21/pl10: in ticks of `hrt_frequency`.
                let ticks = ticks_from_nanos(
                    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                );
                let mut values = live_values(ticks, &heap, &classes, &runtime);
                // zt t21/pl10: the metaspace counters as HotSpot holds them.
                let metaspace = metaspace_counter_values(
                    &metadata,
                    collections,
                    metaspace_max_before_gc,
                    metaspace_at_gc,
                    &mut metaspace_held,
                );
                put_metaspace_figures(&mut values, &metaspace);
                // zt t14/pl5: the appended variables (`add_hotspot_counters`).
                let (collector_times, last_cause) = zgc_extra_values(&shared, zgc, &[]);
                let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
                // zt t15/pl6: the thread, class and JIT counters, read from
                // the counters the registry, the class linker and the JIT
                // keep (atomics, and one short lock for the last compile).
                let threads = shared.threads.thread_registry.jmm_perf_thread_counts();
                let counters = &shared.debug.diagnostic_counters;
                let load = |cell: &std::sync::atomic::AtomicU64| {
                    n(cell.load(std::sync::atomic::Ordering::Relaxed))
                };
                let linked = load(&counters.classes_linked);
                let stats = shared.jit.tiered_manager.stats();
                let relaxed = std::sync::atomic::Ordering::Relaxed;
                let last_compiled = stats.last_compiled.lock().clone();
                let compiles = compile_values(
                    stats.total_compile_time_ns.load(relaxed),
                    stats.c1_compilations.load(relaxed),
                    stats.c2_compilations.load(relaxed),
                    stats.osr_compilations.load(relaxed),
                    last_compiled.as_ref().map(|last| last.osr),
                );
                let new_last_method = last_compiled.and_then(|last| {
                    let same = last_method_written.as_ref().is_some_and(|(class, method)| {
                        *class == last.class_name && *method == last.method_name
                    });
                    (!same).then_some((last.class_name, last.method_name))
                });
                // zt t18/pl8: the per-kind times and the bailouts.
                let last_failed = stats.last_failed.lock().clone();
                let compile_extras = compile_extra_values(
                    stats.standard_compile_time_ns.load(relaxed),
                    stats.osr_compile_time_ns.load(relaxed),
                    stats.total_bailouts.load(relaxed),
                    last_failed.as_ref().map(|last| last.osr),
                );
                let new_last_failed = last_failed.and_then(|last| {
                    let same = last_failed_written.as_ref().is_some_and(|(class, method)| {
                        *class == last.class_name && *method == last.method_name
                    });
                    (!same).then_some((last.class_name, last.method_name))
                });
                let extras = ExtraValues {
                    generation_capacity: Some([n(heap.eden.0), n(heap.old.0)]),
                    metaspace_max: Some([metaspace[2], metaspace[5]]),
                    collector_times,
                    last_cause,
                    init_times: None,
                    threads: Some([
                        threads.started,
                        i64::from(threads.live),
                        i64::from(threads.peak),
                        i64::from(threads.daemon),
                    ]),
                    compiles: Some(compiles),
                    compile_extras: Some(compile_extras),
                    class_events: Some([load(&counters.classes_initialized), linked, linked]),
                    tlab: published_tlab_values(&shared),
                    define_counts: Some(define_count_values(
                        [
                            load(&counters.app_classes_defined),
                            load(&counters.app_class_bytes),
                            load(&counters.unsafe_define_class_calls),
                        ],
                        class_path_defines,
                        app_class_path_defines_counted,
                    )),
                    // zt t21/pl10: nanoseconds in ticks.
                    class_times: class_times.map(|(times, _)| times.map(ticks_from_nanos)),
                    // zt t29/cb: added only under the switch.
                    sys_class_bytes: Some(n(sys_class_bytes)),
                    // zt pl4 item 1: the published compiles' sizes.
                    compile_sizes: Some(compile_size_values(&stats)),
                    // zt pl4 item 1: the sampler leaves `sun.gc.cause` to the
                    // pause events.
                    gc_cause_now: None,
                    // zt pl4 item 3: G1's worker groups (no refinement or
                    // service thread here: those read 0).
                    g1_thread_cpu: shared
                        .mem
                        .heap
                        .g1_gc_thread_cpu_ns()
                        .map(|(parallel, mark)| [n(mark), 0, n(parallel), 0]),
                    dependency: Some(dependency_counter_values(&shared, method_bytes)),
                    // zt pl4 item 1: times in ticks, the count as is.
                    class_loads: class_loads.map(|[time, self_time, count, sys]| {
                        [
                            ticks_from_nanos(time),
                            ticks_from_nanos(self_time),
                            n(count),
                            ticks_from_nanos(sys),
                        ]
                    }),
                };
                let guard = shared.debug.perf_data_file.lock();
                let Some(file) = guard.as_ref() else {
                    return;
                };
                file.write_live(&values);
                file.write_extras(&extras);
                if let Some((class, method)) = new_last_method {
                    file.write_last_method(&last_method_text(&class, &method));
                    last_method_written = Some((class, method));
                }
                if let Some((class, method)) = new_last_failed {
                    file.write_last_failed_method(&last_method_text(&class, &method));
                    last_failed_written = Some((class, method));
                }
            }
        });
    let Ok(thread) = spawned else {
        return;
    };
    if let Some(file) = shared.debug.perf_data_file.lock().as_ref() {
        *file.sampler.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(SamplerHandle { stop, wake, thread });
    } else {
        stop.store(true, std::sync::atomic::Ordering::Release);
        thread.thread().unpark();
    }
}

/// Stop this VM's live-counter sampler and wait for it to end (interpreter
/// round i4 wave 87, lane L1): the stop flag first, then an unpark, then the
/// join, so it takes no strong reference to the VM afterwards and holds none
/// when this returns -- the VM's owner then releases the last one, on its
/// own thread. Called from the VM's teardown (`Vm`'s `Drop`, which
/// `DestroyJavaVM` and an embedder's drop reach) and from the launcher's
/// exit (`remove_perf_data_file_at_exit`). Idempotent; nothing to do without
/// a sampler. The perf-data lock is not held across the join (the sampler
/// takes it to write); called on the sampler thread itself (its sample held
/// the last reference) it only sets the flag.
pub(crate) fn stop_sampler(shared: &crate::vm::SharedVm) {
    let handle = shared
        .debug
        .perf_data_file
        .lock()
        .as_ref()
        .and_then(|file| file.sampler.lock().unwrap_or_else(|e| e.into_inner()).take());
    let Some(SamplerHandle { stop, thread, .. }) = handle else {
        return;
    };
    stop.store(true, std::sync::atomic::Ordering::Release);
    if thread.thread().id() == std::thread::current().id() {
        return;
    }
    thread.thread().unpark();
    // Bounded: the caller is not in a blocking region, so a collection that
    // needs it could wait on it while the sampler waits for that pause
    // (`run_if_no_stw_requested`); after 2 s the thread is left to end on
    // its own (it takes no new reference once the flag is set).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    // Yield for the first 300 us (an idle sampler ends within that: a 1 ms
    // sleep here was most of a restored Hello's exit), then sleep.
    let spin_until = std::time::Instant::now() + std::time::Duration::from_micros(300);
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        if std::time::Instant::now() < spin_until {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    if thread.is_finished() {
        let _ = thread.join();
    }
}

/// zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 2): write
/// the safepoint counters (`sun.rt.safepoints` / `safepointTime` /
/// `safepointSyncTime` / `applicationTime`, `sun.threads.vmOperationTime`)
/// and the collectors' invocations / time now, on the thread whose explicit
/// collection (`System.gc`, `Runtime.gc`, `MemoryMXBean.gc`) just returned,
/// instead of at the sampler's next pass (up to 100 ms later): HotSpot
/// updates them at the pause, so a Java reader of the in-process counters
/// right after `System.gc()` sees the collection. The collector figures are
/// the beans' own (`collector_counters`, what `GarbageCollectorMXBean` reads
/// on a mutator; no heap figures, so no barrier lock is taken). The rest of
/// the live set stays the sampler's. `CRATONVM_PERF_EVENT_REFRESH=0`: the
/// sampler alone, as before. Nothing without a live perf-data image.
pub(crate) fn refresh_after_collection(shared: &crate::vm::SharedVm) {
    refresh_after_collection_with(shared, &[]);
}

/// zt t14/pl5: [`refresh_after_collection`], with `pending` the records of a
/// collection not yet booked as each bean's last
/// ([`zgc_last_collections`]); empty for every caller but the refresh before
/// the notifications are queued.
fn refresh_after_collection_with(
    shared: &crate::vm::SharedVm,
    pending: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) {
    let live = shared
        .debug
        .perf_data_file
        .lock()
        .as_ref()
        .is_some_and(|file| !file.live_offsets.is_empty());
    if !live {
        return;
    }
    let safepoint_counters =
        true;
    let runtime = runtime_counters(shared, safepoint_counters);
    let heap = HeapCounters {
        collectors: collector_counters(shared),
        ..HeapCounters::default()
    };
    let values = live_values(0, &heap, &ClassCounters::default(), &runtime);
    // zt t14/pl5: on ZGC, the pause collectors' last pause times and the last
    // cause too (`add_hotspot_counters`), read before the perf-data lock.
    let zgc = shared.mem.heap.is_zgc();
    let (collector_times, last_cause) = zgc_extra_values(shared, zgc, pending);
    let guard = shared.debug.perf_data_file.lock();
    if let Some(file) = guard.as_ref() {
        file.write_live_where(&values, event_refreshed);
        file.write_extras(&ExtraValues {
            collector_times,
            last_cause,
            // zt t16/pl7: this collection's TLAB statistics, published at its
            // start (`publish_tlab_stats`).
            tlab: published_tlab_values(shared),
            // zt pl4 item 1: a collection ends the cycle a concurrent mark
            // opened: no cycle is in progress after its pause.
            gc_cause_now: gc_cause_concurrent_enabled().then_some(NO_GC_CAUSE),
            ..ExtraValues::default()
        });
    }
}

/// zt t10/pl2: the [`LIVE_COUNTERS`] indices [`refresh_after_collection`]
/// writes: the safepoint block, `vmOperationTime`, and the collectors'
/// invocations and times.
fn event_refreshed(index: usize) -> bool {
    (LIVE_SAFEPOINTS..=LIVE_VM_OPERATION_TIME).contains(&index)
        || (LIVE_COLLECTOR..LIVE_EDEN).contains(&index)
}

/// zt t11/pl3 (t11/x8 P1): does [`PerfDataFile::write_live_where`] keep the
/// larger of the stored and the new value at `index`? Only for a monotonic
/// counter the event refresh writes ([`event_refreshed`]: the collector and
/// safepoint counters, the only ones with a second writer), and only while
/// that refresh is on. Pure.
fn keeps_max(index: usize, event_refresh: bool) -> bool {
    event_refresh
        && event_refreshed(index)
        && LIVE_COUNTERS
            .get(index)
            .is_some_and(|c| c.2 == VARIABILITY_MONOTONIC)
}

/// zt t10/pl2: the collector half of [`heap_counters`] alone, per bean
/// `(count, time ns)` ([`collector_time_ns`]), read as the
/// `GarbageCollectorMXBean` natives read it.
fn collector_counters(shared: &crate::vm::SharedVm) -> [(u64, u64); 3] {
    let mut out = [(0, 0); 3];
    match shared.mem.heap.gen_jmx_collectors() {
        Some(collectors) => {
            for (slot, c) in out.iter_mut().zip(collectors.iter()) {
                *slot = (c.count, collector_time_ns(c));
            }
        }
        None => {
            let heap = &shared.mem.heap;
            if let Some(beans) = shared.mem.backend_gc_beans() {
                out = backend_collector_slots(heap, &beans.collectors());
            }
        }
    }
    out
}

/// The three `sun.gc.collector.<n>` slots `(count, time ms)` of a backend
/// whose beans are `beans` (every heap but the generational one): the beans
/// in order, except on ZGC (zt t11/pl3, [`zgc_collector_slots`]).
fn backend_collector_slots(
    heap: &crate::memory::vm_heap::VmHeap,
    beans: &[cratonvm_gc::gc_metrics::JmxCollector],
) -> [(u64, u64); 3] {
    if heap.is_zgc()
    {
        let named: Vec<(&str, u64, u64)> = beans
            .iter()
            .map(|c| (c.name, c.count, collector_time_ns(c)))
            .collect();
        return zgc_collector_slots(&named);
    }
    let mut out = [(0, 0); 3];
    for (slot, c) in out.iter_mut().zip(beans.iter()) {
        *slot = (c.count, collector_time_ns(c));
    }
    out
}

/// `CRATONVM_PERF_COLLECTOR_TIME_NANOS` (zt t27/lg1, page
/// `zgc-zt13x10-review-residuals-*` item 9; default ON, `=0` kill; every
/// collector, both modes): `sun.gc.collector.<n>.time` (`jstat`'s `YGCT` /
/// `FGCT` / `CGCT`) carries the bean's accumulated pause time at nanosecond
/// precision (`JmxCollector::time_ns`), as HotSpot's tick timer does. `=0`:
/// its whole milliseconds, as before (sub-millisecond ZGC pauses left the
/// counter behind HotSpot's).
fn collector_time_nanos_enabled() -> bool {
    true
}

/// zt t27/lg1: a collector bean's time for the perf image, in nanoseconds
/// ([`collector_time_nanos_enabled`]).
fn collector_time_ns(c: &cratonvm_gc::gc_metrics::JmxCollector) -> u64 {
    if collector_time_nanos_enabled() {
        c.time_ns
    } else {
        c.time_ms.saturating_mul(1_000_000)
    }
}

/// zt t11/pl3 (`CRATONVM_ZGC_PERF_COLLECTOR_SLOTS`, default ON, `=0` kill):
/// HotSpot 25's ZGC perf collectors are its pause counters, at ordinals 0
/// and 2 (measured, Windows 25.0.3 `jcmd PerfCounter.print`:
/// `sun.gc.collector.0.name="ZGC minor collection pauses"`,
/// `sun.gc.collector.2.name="ZGC major collection pauses"`), so `jstat -gc`'s
/// `YGC` / `CGC` columns count pauses. The beans in order put `ZGC Minor
/// Cycles`, `ZGC Minor Pauses` and `ZGC Major Cycles` there instead. Slot 0
/// is the `ZGC Minor Pauses` bean, slot 2 the `ZGC Major Pauses` bean, slot 1
/// stays 0. zt t12/pl4: HotSpot's ZGC image has no collector 1 (t11 rows
/// `pl3_pc_*`: `perf1=null`), so a ZGC-shaped image leaves slot 1 out
/// ([`zgc_image_shape`]); the 0 is written only under its `=0` kill.
/// `named` is `(bean name, count, time ms)`; a missing bean reads 0. Pure.
fn zgc_collector_slots(named: &[(&str, u64, u64)]) -> [(u64, u64); 3] {
    let of = |name: &str| {
        named
            .iter()
            .find(|c| c.0 == name)
            .map_or((0, 0), |c| (c.1, c.2))
    };
    [
        of(cratonvm_gc::gc_metrics::JMX_ZGC_MINOR_PAUSES_COLLECTOR),
        (0, 0),
        of(cratonvm_gc::gc_metrics::JMX_ZGC_MAJOR_PAUSES_COLLECTOR),
    ]
}

/// zt t11/pl3 (`zgc-zt10pl2-perf-counter-residuals-*` item 2): on ZGC, the
/// [`refresh_after_collection`] write at the end of EVERY collection pause
/// (`run_collection_pause`, after the bean update and the world's release),
/// not only after an explicit collection: HotSpot updates its safepoint and
/// collector counters at the pause, so a `GarbageCollectorMXBean`
/// notification listener that reads the in-process counters sees the
/// collection that notified it -- when the delivery follows this pause's own
/// wake-up; a delivery thread an earlier hint woke can run the listener
/// first (zt t12/x9, the residuals page). `CRATONVM_ZGC_PERF_PAUSE_REFRESH=0`: the
/// sampler's next pass, as before (and `CRATONVM_PERF_EVENT_REFRESH=0` turns
/// every event write off). g1gen/c8: on every collector (`zgc-zt11pl3` item
/// 2, carried to G1 / Generational): the same parity holds there, and the
/// zt round left them out by its scope rule alone.
pub(crate) fn refresh_after_pause(shared: &crate::vm::SharedVm) {
    refresh_after_pause_with(shared, &[]);
}

/// zt t14/pl5: [`refresh_after_pause`] with `pending`, this collection's
/// records before they are booked as each bean's last (zt t13/x10's refresh
/// before the notifications are queued), which give the pause times and the
/// last cause ([`zgc_last_collections`]).
pub(crate) fn refresh_after_pause_with(
    shared: &crate::vm::SharedVm,
    pending: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) {
    if pause_refresh_applies(true) {
        refresh_after_collection_with(shared, pending);
    }
}

/// zt t11/pl3: [`refresh_after_pause`]'s gate (every collector since
/// g1gen/c8). Pure.
fn pause_refresh_applies(enabled: bool) -> bool {
    enabled
}

/// zt t11/pl3 (`CRATONVM_ZGC_PERF_CLASS_COUNTS`, default ON, `=0` kill): the
/// live-class figure behind `java.cls.loadedClasses`. HotSpot's counter is the
/// one `ClassLoadingMXBean.getTotalLoadedClassCount` reads (with the shared
/// one, 0 here): InstanceKlasses only. The sampler added every live class of
/// the store (array classes and compatibility stubs included) to the
/// instance-only unloaded count, so `jstat -class` and the in-process
/// `java.cls.*` counters disagreed with the bean. On ZGC: the bean's count
/// (`ClassManager::loaded_instance_class_count`, one store walk), taken
/// again only when the store's size or the unloaded count moved, or when
/// `stale` (the metadata walk's 5 s period, which also catches a stub
/// upgraded in place); else the memo. `=0`: the store's size, as before.
/// g1gen/c8: on every collector (zt t11 row `pl3_G1` printed
/// `total_matches_bean=false` on G1).
fn sampled_live_classes(
    cm: &crate::classloading::ClassManager,
    unloaded: u64,
    stale: bool,
    memo: &mut Option<(usize, u64, u64)>,
) -> u64 {
    let len = cm.loaded_count();
    if !true {
        return len as u64; // Widening: usize -> u64    
}
    if let Some((l, u, count)) = *memo {
        if !stale && l == len && u == unloaded {
            return count;
        }
    }
    let count = cm.loaded_instance_class_count() as u64; // Widening: usize -> u64
    *memo = Some((len, unloaded, count));
    count
}

/// `sun.rt.jvmCapabilities`' 64 characters, as HotSpot's `Management::init`
/// fills them (MEASURED: `11` then 62 `0`s by default, `01...` under
/// `-XX:+DisableAttachMechanism`): [0] attach supported, [1] `1`.
pub(crate) fn jvm_capabilities(attachable: bool) -> String {
    let mut text = String::with_capacity(64);
    text.push(if attachable { '1' } else { '0' });
    text.push('1');
    text.extend(std::iter::repeat('0').take(62));
    text
}

/// What this VM writes: what a client reads to find, name and attach to it.
/// `java_command` is `sun.java.command` (the main class or JAR and its
/// arguments: `jps -l`'s name and `VirtualMachine.list()`'s display name),
/// `vm_properties` the `java.vm.*` properties, `vm_args` the JVM options.
pub(crate) fn vm_entries(
    begin_millis: i64,
    attachable: bool,
    java_command: &str,
    vm_properties: &[(&str, &str)],
    vm_args: &str,
) -> Vec<PerfEntry> {
    let mut entries = vec![
        PerfEntry {
            name: "sun.rt.createVmBeginTime".to_string(),
            value: PerfValue::Long(begin_millis),
            supported: false,
            units: UNITS_NONE,
            variability: VARIABILITY_VARIABLE,
        },
        PerfEntry {
            name: "sun.rt.jvmCapabilities".to_string(),
            value: PerfValue::Str {
                text: jvm_capabilities(attachable),
                capacity: 65,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_CONSTANT,
        },
        PerfEntry::string_constant("sun.rt.javaCommand", java_command, false),
    ];
    for (key, value) in vm_properties {
        entries.push(PerfEntry::string_constant(
            &format!("java.property.{key}"),
            value,
            true,
        ));
    }
    entries.push(PerfEntry::string_constant("java.rt.vmArgs", vm_args, true));
    entries.push(PerfEntry::string_constant("java.rt.vmFlags", "", true));
    entries
}

/// The written file, removed when this is dropped (or by
/// [`PerfDataFile::remove`] on the launcher's exit paths).
#[derive(Debug)]
pub(crate) struct PerfDataFile {
    path: std::path::PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
    /// The published file's descriptor; `None` for an image kept in memory
    /// only ([`create_in_memory_for_vm`], zt t10/pl2).
    #[cfg(unix)]
    _lease: Option<std::fs::File>,
    /// The file mapped shared and writable, through which the live counters
    /// are updated in place (interpreter round i4 wave 86, lane L1); `None`
    /// when it could not be mapped, or once removed.
    #[cfg(unix)]
    view: std::sync::Mutex<Option<UnixView>>,
    #[cfg(windows)]
    mapping: std::sync::Mutex<Option<windows::Mapping>>,
    /// zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 4):
    /// the image when it lives in this process's memory only
    /// (`-XX:+PerfDisableSharedMem`, HotSpot's `create_standard_memory`): no
    /// file, no name, but the live counters and `Perf.attach(0)` work on it.
    /// `None` for a published file.
    memory: std::sync::Mutex<Option<MemoryImage>>,
    /// The absolute offsets of the [`LIVE_COUNTERS`]' `long` data in the file,
    /// in that order (`usize::MAX` for one left out); empty for a file
    /// written without them.
    live_offsets: Vec<usize>,
    /// zt t14/pl5: the data offsets of the variables [`add_hotspot_counters`]
    /// appended ([`ExtraOffsets::ABSENT`] until then, and for every one it
    /// did not add).
    extra: ExtraOffsets,
    /// The live-counter sampler of this VM's file ([`start_sampler`]), for
    /// [`stop_sampler`] to stop and join at the VM's teardown (interpreter
    /// round i4 wave 87, lane L1).
    sampler: std::sync::Mutex<Option<SamplerHandle>>,
    /// zt t9/pl: Java holds a direct buffer over the writable view
    /// ([`PerfDataFile::attach_view`], `Perf.attach(0)`), so [`Self::remove`]
    /// must leave the view mapped. Set under the view's lock.
    attached: std::sync::atomic::AtomicBool,
}

/// A running sampler: its stop flag (per VM, shared with the thread only)
/// and its thread.
#[derive(Debug)]
struct SamplerHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// zt t20/pl9: a pass asked for before the interval ends
    /// ([`record_vm_init_done`]); set before the unpark ([`park_for_interval`]).
    wake: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// zt t10/pl2: the perf-data image in this process's own memory
/// ([`PERF_DATA_SIZE`] bytes, 8-byte aligned), freed when dropped.
#[derive(Debug)]
struct MemoryImage {
    base: usize,
}

/// The image's length in 8-byte words.
const MEMORY_IMAGE_WORDS: usize = PERF_DATA_SIZE / 8;

impl MemoryImage {
    /// A zeroed image with `bytes` (at most [`PERF_DATA_SIZE`] of them)
    /// copied to its start.
    fn new(bytes: &[u8]) -> Self {
        let mut words = vec![0u64; MEMORY_IMAGE_WORDS].into_boxed_slice();
        let len = bytes.len().min(PERF_DATA_SIZE);
        // SAFETY: `words` owns PERF_DATA_SIZE bytes and `len` is at most
        // that; source and destination are distinct allocations.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast::<u8>(), len);
        }
        let base = Box::into_raw(words) as *mut u64 as usize; // Cast: the image's address
        MemoryImage { base }
    }
}

impl Drop for MemoryImage {
    fn drop(&mut self) {
        // SAFETY: `base` came from `Box::into_raw` of a boxed slice of
        // MEMORY_IMAGE_WORDS words in `new` and is freed once, here.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                self.base as *mut u64,
                MEMORY_IMAGE_WORDS,
            )));
        }
    }
}

/// A shared, writable mapping of the perf-data file's [`PERF_DATA_SIZE`]
/// bytes (Unix), unmapped when dropped.
#[cfg(unix)]
#[derive(Debug)]
struct UnixView {
    base: usize,
}

#[cfg(unix)]
impl UnixView {
    /// Map `file` (opened for reading and writing) shared and writable.
    fn map(file: &std::fs::File) -> Option<Self> {
        // SAFETY: a fresh mapping of an open descriptor; the result is
        // checked against MAP_FAILED and owned by the returned value.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PERF_DATA_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(file),
                0,
            )
        };
        if base == libc::MAP_FAILED || base.is_null() {
            return None;
        }
        Some(UnixView {
            base: base as usize, // Cast: the mapping's address
        })
    }
}

#[cfg(unix)]
impl Drop for UnixView {
    fn drop(&mut self) {
        // SAFETY: `base` is this value's own live mapping of PERF_DATA_SIZE
        // bytes, unmapped once.
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, PERF_DATA_SIZE);
        }
    }
}

impl PerfDataFile {
    /// Write the live counters' `values` (in [`LIVE_COUNTERS`] order) into
    /// the mapped file, where a reader's `mmap` sees them (interpreter round
    /// i4 wave 86, lane L1). Nothing once the file is removed or when it could
    /// not be mapped. Each value is one aligned 8-byte store.
    pub(crate) fn write_live(&self, values: &[i64]) {
        self.write_live_where(values, |_| true);
    }

    /// [`Self::write_live`] for the [`LIVE_COUNTERS`] indices `pick` accepts
    /// only (zt t10/pl2: the at-the-event refresh writes the counters it
    /// read and leaves the sampler's). A counter the event refresh also
    /// writes never goes back (zt t10/pl2): its value is the larger of the
    /// stored one and the new one, so a sampler pass that a pause pre-empted
    /// (and that writes its previous collector figures) cannot undo a fresher
    /// event-time write.
    ///
    /// zt t11/pl3 (t11/x8 C1 / P1): only those counters, and only while the
    /// event refresh is on ([`keeps_max`]): every other counter is stored as
    /// written, so `sun.cls.loadedBytes` (the class metadata, which an
    /// unload lowers) follows the sampler again instead of freezing at its
    /// high-water mark, and `CRATONVM_PERF_EVENT_REFRESH=0` is the single
    /// writer of before t10/pl2.
    pub(crate) fn write_live_where(&self, values: &[i64], pick: impl Fn(usize) -> bool) {
        let event_refresh =
            true;
        self.with_base(|base| {
            let Some(base) = base else {
                return;
            };
            for (i, (&offset, &value)) in self.live_offsets.iter().zip(values).enumerate() {
                if !pick(i)
                    || offset == usize::MAX
                    || offset % 8 != 0
                    || offset + 8 > PERF_DATA_SIZE
                {
                    continue;
                }
                let monotonic = keeps_max(i, event_refresh);
                // SAFETY: `base` maps (or owns) PERF_DATA_SIZE bytes for as
                // long as `with_base` holds its guards; `offset + 8` lies
                // inside and is 8-byte aligned (the mapping is page-aligned,
                // the memory image 8-byte aligned), so this is one aligned
                // 64-bit access, as HotSpot's `PerfLongVariant` writes its
                // value.
                let cell = unsafe { &*((base + offset) as *const std::sync::atomic::AtomicI64) };
                if monotonic {
                    cell.fetch_max(value, std::sync::atomic::Ordering::Relaxed);
                } else {
                    cell.store(value, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }

    /// zt t14/pl5: write the appended variables' `values` ([`ExtraValues`])
    /// at their offsets ([`Self::extra`]); a counter not added (offset
    /// `usize::MAX`) or a `None` value is skipped. Each long is one aligned
    /// 8-byte store; a string is rewritten in place, NUL-terminated and
    /// NUL-padded to its capacity, as HotSpot's `PerfString::set_string`.
    pub(crate) fn write_extras(&self, values: &ExtraValues) {
        let extra = self.extra;
        let mut longs: Vec<(usize, i64)> = Vec::with_capacity(48);
        if let Some(v) = values.generation_capacity {
            longs.extend(extra.generation_capacity.into_iter().zip(v));
        }
        if let Some(v) = values.metaspace_max {
            longs.extend(extra.metaspace_max.into_iter().zip(v));
        }
        if let Some(v) = values.collector_times {
            longs.extend(extra.collector_times.into_iter().zip(v));
        }
        if let Some(v) = values.init_times {
            longs.extend(extra.init_times.into_iter().zip(v));
        }
        let mut started = None;
        if let Some([first, rest @ ..]) = values.threads {
            started = Some(first);
            longs.extend(extra.threads[1..].iter().copied().zip(rest));
        }
        if let Some(v) = values.compiles {
            longs.extend(extra.compiles.into_iter().zip(v));
        }
        if let Some(v) = values.compile_extras {
            longs.extend(extra.compile_extras.into_iter().zip(v));
        }
        if let Some(v) = values.class_events {
            longs.extend(extra.class_events.into_iter().zip(v));
        }
        if let Some(v) = values.tlab {
            longs.extend(extra.tlab.into_iter().zip(v));
        }
        if let Some(v) = values.define_counts {
            longs.extend(extra.define_counts.into_iter().zip(v));
        }
        if let Some(v) = values.class_times {
            longs.extend(extra.class_times.into_iter().zip(v));
        }
        if let Some(v) = values.sys_class_bytes {
            longs.push((extra.sys_class_bytes, v));
        }
        if let Some(v) = values.compile_sizes {
            longs.extend(extra.compile_sizes.into_iter().zip(v));
        }
        if let Some(v) = values.class_loads {
            longs.extend(extra.class_loads.into_iter().zip(v));
        }
        if let Some(v) = values.g1_thread_cpu {
            longs.extend(extra.g1_thread_cpu.into_iter().zip(v));
        }
        if let Some(v) = values.dependency {
            longs.extend(extra.dependency.into_iter().zip(v));
        }
        self.with_base(|base| {
            let Some(base) = base else {
                return;
            };
            for (offset, value) in longs {
                store_long(base, offset, value);
            }
            if let Some(value) = started {
                store_long_max(base, extra.threads[0], value);
            }
            if let Some(cause) = values.last_cause {
                store_string(base, extra.gc_cause[1], GC_CAUSE_CAPACITY, cause);
            }
            if let Some(cause) = values.gc_cause_now {
                store_string(base, extra.gc_cause[0], GC_CAUSE_CAPACITY, cause);
            }
        });
    }

    /// zt t15/pl6: rewrite `sun.ci.lastMethod` with `text` (cut to
    /// [`CI_LAST_METHOD_CAPACITY`] less the NUL, as HotSpot's `jio_snprintf`
    /// into its buffer cuts it); nothing when the counter was not added.
    pub(crate) fn write_last_method(&self, text: &str) {
        self.write_ci_method(self.extra.last_method, text);
    }

    /// zt t18/pl8: rewrite `sun.ci.lastFailedMethod`, as
    /// [`Self::write_last_method`].
    pub(crate) fn write_last_failed_method(&self, text: &str) {
        self.write_ci_method(self.extra.last_failed_method, text);
    }

    /// The common half of the two above.
    fn write_ci_method(&self, offset: usize, text: &str) {
        if offset == usize::MAX {
            return;
        }
        self.with_base(|base| {
            if let Some(base) = base {
                store_string(base, offset, CI_LAST_METHOD_CAPACITY, text);
            }
        });
    }

    /// zt t14/pl5: append `entries` after the image's last entry, as HotSpot's
    /// `PerfDataManager` adds a counter to a perf memory readers already map:
    /// the entries' bytes first, then the prologue's `used`, its modification
    /// stamp and, last, its entry count (what a jvmstat reader polls for new
    /// entries; the in-process `PerfInstrumentation` walks up to `used`).
    /// Each entry's data offset, in order; `usize::MAX` for one that does not
    /// fit in [`PERF_DATA_SIZE`] (left out, as `encode_perf_data_with_offsets`
    /// leaves it out; HotSpot puts it in the C heap, out of the file). All
    /// `usize::MAX` without an image. The caller serialises appends (it holds
    /// the VM's perf-data lock).
    fn append_entries(&self, entries: &[PerfEntry]) -> Vec<usize> {
        self.with_base(|base| {
            let mut offsets = vec![usize::MAX; entries.len()];
            let Some(base) = base else {
                return offsets;
            };
            // SAFETY: the prologue's `used` (offset 8) and entry count
            // (offset 28) are 4-byte aligned `i32`s and its modification stamp
            // (offset 16) an 8-byte aligned `i64`, inside the image `with_base`
            // keeps valid (the view is page aligned, the memory image 8-byte
            // aligned).
            let (used_cell, stamp_cell, count_cell) = unsafe {
                (
                    &*((base + 8) as *const std::sync::atomic::AtomicI32),
                    &*((base + 16) as *const std::sync::atomic::AtomicI64),
                    &*((base + 28) as *const std::sync::atomic::AtomicI32),
                )
            };
            let ordering = std::sync::atomic::Ordering::Acquire;
            let Ok(mut at) = usize::try_from(used_cell.load(ordering)) else {
                return offsets;
            };
            if !(PROLOGUE_SIZE..=PERF_DATA_SIZE).contains(&at) {
                return offsets;
            }
            let mut count = count_cell.load(ordering);
            for (slot, entry) in offsets.iter_mut().zip(entries) {
                let bytes = encode_entry(entry);
                if at + bytes.len() > PERF_DATA_SIZE {
                    continue;
                }
                let data = i32::from_ne_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
                // SAFETY: `[at, at + bytes.len())` lies inside the image and
                // past `used`, which no reader parses until the stores below.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        (base + at) as *mut u8,
                        bytes.len(),
                    );
                }
                *slot = at + usize::try_from(data).unwrap_or(0);
                at += bytes.len();
                count += 1;
            }
            let release = std::sync::atomic::Ordering::Release;
            used_cell.store(i32::try_from(at).unwrap_or(i32::MAX), release);
            stamp_cell.fetch_add(1, release);
            count_cell.store(count, release);
            offsets
        })
    }

    /// Run `f` with the image's base address (the file's writable view, or
    /// the memory image; `None` when neither exists any more), holding the
    /// locks that keep it valid.
    fn with_base<R>(&self, f: impl FnOnce(Option<usize>) -> R) -> R {
        let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        let guard = self.view.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        let base = guard.as_ref().map(|view| view.base);
        #[cfg(windows)]
        let guard = self.mapping.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(windows)]
        let base = guard.as_ref().and_then(windows::Mapping::view_base);
        #[cfg(not(any(unix, windows)))]
        let base: Option<usize> = None;
        let base = base.or_else(|| memory.as_ref().map(|image| image.base));
        let result = f(base);
        #[cfg(any(unix, windows))]
        drop(guard);
        drop(memory);
        result
    }

    /// zt t9/pl (`zgc-zt7cc-in-process-perf-counters-are-empty-FIXED-20261019.md`):
    /// the writable view's address and length for `Perf.attach(0)`, which
    /// HotSpot answers with a direct buffer over its own perf-data memory
    /// (`PerfMemory::attach` for this VM's id); `None` when the file could
    /// not be mapped or is already removed. Once handed out the view is
    /// never unmapped ([`Self::remove`] leaves it, as HotSpot's perf memory
    /// stays mapped until the process ends), since a Java thread may still
    /// read through the buffer after the VM's exit path removed the file.
    ///
    /// zt t10/pl2: an image kept in memory only (`-XX:+PerfDisableSharedMem`)
    /// is handed out the same way, and is then never freed.
    pub(crate) fn attach_view(&self) -> Option<(usize, usize)> {
        let base = self.with_base(|base| {
            if base.is_some() {
                self.attached.store(true, std::sync::atomic::Ordering::Release);
            }
            base
        });
        base.map(|base| (base, PERF_DATA_SIZE))
    }

    /// zt t10/pl2: the base of this VM's own image (view or memory), if
    /// any; `Perf.detach` must never release it.
    pub(crate) fn own_base(&self) -> Option<usize> {
        self.with_base(|base| base)
    }

    /// Unlink the file now (idempotent). A view Java attached to
    /// ([`Self::attach_view`]) stays mapped (zt t9/pl); its file is still
    /// unlinked (Unix) and its handles closed (Windows). zt t10/pl2: a memory
    /// image Java attached to is likewise never freed.
    pub(crate) fn remove(&self) {
        {
            let taken = self.memory.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(image) = taken {
                if self.attached.load(std::sync::atomic::Ordering::Acquire) {
                    // Never freed: a Java direct buffer reads through it.
                    std::mem::forget(image);
                } else {
                    drop(image);
                }
            }
        }
        #[cfg(windows)]
        {
            let taken = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut mapping) = taken {
                if self.attached.load(std::sync::atomic::Ordering::Acquire) {
                    mapping.leak_view();
                }
                drop(mapping);
            }
        }
        #[cfg(unix)]
        {
            let taken = self.view.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(view) = taken {
                if self.attached.load(std::sync::atomic::Ordering::Acquire) {
                    // Never unmapped: a Java direct buffer reads through it.
                    std::mem::forget(view);
                } else {
                    drop(view);
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // A second embedded VM or a replacement file is not this lease.
            if std::fs::symlink_metadata(&self.path)
                .is_ok_and(|m| (m.dev(), m.ino()) == self.identity)
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Write this process's perf-data file for a VM configured by `config`
/// (`VmConfig::perf_data_file`): the attach bit is `0` under
/// `-XX:+DisableAttachMechanism`; `sun.rt.javaCommand` is the launcher's
/// `sun.java.command`; the `java.vm.*` properties are the VM's own
/// (`vm_identity_properties`, a `-D` override winning); `java.rt.vmArgs`
/// the `-D` options given (HotSpot lists every JVM option; this VM keeps no
/// other record of them). `None` when it could not be written.
pub(crate) fn create_for_vm(config: &crate::config::VmConfig, zgc: bool) -> Option<PerfDataFile> {
    let (bytes, live_offsets) = image_for_vm(config, zgc);
    let mut file = write_perf_data_file(std::process::id(), &bytes)?;
    file.live_offsets = live_offsets;
    Some(file)
}

/// zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 4): the
/// same image as [`create_for_vm`], kept in this process's memory only: no
/// file and no named mapping (so `jps` and `VirtualMachine.list()` do not see
/// the VM), but the sampler keeps its live counters current and
/// `Perf.attach(0)` hands it out, as HotSpot's `-XX:+PerfDisableSharedMem`
/// keeps its perf memory in the C heap (`create_standard_memory`).
pub(crate) fn create_in_memory_for_vm(
    config: &crate::config::VmConfig,
    zgc: bool,
) -> Option<PerfDataFile> {
    let (bytes, live_offsets) = image_for_vm(config, zgc);
    Some(PerfDataFile {
        path: std::path::PathBuf::new(),
        #[cfg(unix)]
        identity: (0, 0),
        #[cfg(unix)]
        _lease: None,
        #[cfg(unix)]
        view: std::sync::Mutex::new(None),
        #[cfg(windows)]
        mapping: std::sync::Mutex::new(None),
        memory: std::sync::Mutex::new(Some(MemoryImage::new(&bytes))),
        live_offsets,
        extra: ExtraOffsets::ABSENT,
        sampler: std::sync::Mutex::new(None),
        attached: std::sync::atomic::AtomicBool::new(false),
    })
}

/// zt t11/pl3 (`zgc-zt10pl2-perf-counter-residuals-*` item 4;
/// `CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK`, default ON since zt pl4 item 4,
/// `=0` kill): the image a VM keeps when [`create_for_vm`] could not publish
/// one (an unsafe or unwritable `hsperfdata` directory), so `Perf.attach(0)`
/// and the sampler still work. HotSpot 25's `PerfMemory::create_memory_region`
/// (POSIX and Windows alike) reverts to non-shared memory
/// (`create_standard_memory`, `PerfDisableSharedMem` set ergonomically)
/// whenever the shared region cannot be created. One case stays without an
/// image unless `=1` asks for it: the name is this process's own
/// ([`own_process_publication_exists`]: another VM of this process published
/// it), which HotSpot, one VM per process, never meets; processes that host
/// many VMs (the test suites, an embedder) would otherwise each keep an image
/// and a sampler thread. `None` when off.
pub(crate) fn in_memory_fallback_for_vm(
    config: &crate::config::VmConfig,
    zgc: bool,
) -> Option<PerfDataFile> {
    const KEY: &str = "CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK";
    if !cratonvm_types::flags::runtime_flag_default_on(KEY) {
        return None;
    }
    if !cratonvm_types::flags::runtime_flag_on(KEY) && own_process_publication_exists() {
        return None;
    }
    create_in_memory_for_vm(config, zgc)
}

/// zt pl4 item 4: has a VM of this process already published this process
/// id's perf data (the reason a later VM's publication fails without any
/// fault of the directory)?
fn own_process_publication_exists() -> bool {
    foreign_data_present_pid(std::process::id())
}

/// zt t10/pl2: whether this VM keeps its perf data in memory only: the
/// launcher's options (`VmConfig::jvm_args`, the last of each pair winning,
/// as its own fold does) leave `-XX:+UsePerfData` on and turn
/// `-XX:+PerfDisableSharedMem` on. `CRATONVM_PERF_DATA_IN_MEMORY=0`: never
/// (no perf data at all under that flag, as before).
///
/// zt t11/pl3 (t11/x8 C3 / P3): never under `CRATONVM_PERF_DATA_FILE=0`
/// either. That switch turns all perf data off (the `VmConfig` default reads
/// it, and the launcher folds `-XX:+PerfDisableSharedMem` into the same
/// field), so the flag must not bring an image back.
pub(crate) fn wants_memory_only_image(config: &crate::config::VmConfig) -> bool {
    memory_only_image_allowed(
        true,
        true,
    ) && perf_memory_without_file(&config.jvm_args)}

/// zt t11/pl3 (P3): [`wants_memory_only_image`]'s switch half. Pure.
fn memory_only_image_allowed(in_memory_switch: bool, perf_data_switch: bool) -> bool {
    in_memory_switch && perf_data_switch
}

/// zt t10/pl2: `UsePerfData && PerfDisableSharedMem` over `jvm_args`.
fn perf_memory_without_file(jvm_args: &[String]) -> bool {
    let mut use_perf_data = true;
    let mut disable_shared_mem = false;
    for arg in jvm_args {
        match arg.as_str() {
            "-XX:+UsePerfData" => use_perf_data = true,
            "-XX:-UsePerfData" => use_perf_data = false,
            "-XX:+PerfDisableSharedMem" => disable_shared_mem = true,
            "-XX:-PerfDisableSharedMem" => disable_shared_mem = false,
            _ => {}
        }
    }
    use_perf_data && disable_shared_mem
}

/// The image's bytes for a VM configured by `config` and the offsets of its
/// live counters' data, in [`LIVE_COUNTERS`] order (`usize::MAX` for one the
/// image leaves out). zt t12/pl4: `zgc` says the VM's heap is ZGC
/// ([`zgc_image_shape`]).
fn image_for_vm(config: &crate::config::VmConfig, zgc: bool) -> (Vec<u8>, Vec<usize>) {
    let given = |key: &str| {
        config
            .system_properties
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let identity = crate::vm::vm_init::vm_identity_properties(config);
    let vm_properties: Vec<(&str, String)> = identity
        .iter()
        .filter(|(key, _)| key.starts_with("java.vm."))
        .map(|(key, value)| (*key, given(*key).unwrap_or_else(|| value.clone())))
        .collect();
    let vm_properties: Vec<(&str, &str)> = vm_properties
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    let java_command = given("sun.java.command").unwrap_or_default();
    let vm_args = vm_args_text(config);
    let begin_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut entries = vm_entries(
        begin_millis,
        !config.disable_attach_mechanism,
        &java_command,
        &vm_properties,
        &vm_args,
    );
    // Wave 86 (lane L1): `sun.os.hrt.frequency` and the live counters
    // `jstat -gc` / `-class` read, kept current by `start_sampler`. zt
    // t12/pl4: on ZGC, HotSpot's ZGC shape (`zgc_image_shape`).
    let zgc_shape = zgc_image_shape(zgc);
    // zt t14/pl5 (review of t12/pl4): under
    // `CRATONVM_ZGC_PERF_COLLECTOR_SLOTS=0` the collector slots are the
    // beans in order (slot 1 the `ZGC Minor Pauses` bean, slot 0 `ZGC Minor
    // Cycles`), so the shape keeps `collector.1` and does not name the
    // collectors with HotSpot's pause names, which would mislabel them.
    let pause_slots =
        true;
    let mut live = live_entries().into_iter();
    entries.extend(live.next());
    let first_live = entries.len();
    let mut present = Vec::with_capacity(LIVE_COUNT);
    for (i, entry) in live.enumerate() {
        let keep = !(zgc_shape && absent_on_zgc(i) && (pause_slots || !is_collector_1(i)));
        present.push(keep);
        if keep {
            entries.push(entry);
        }
    }
    if zgc_shape {
        entries.extend(
            zgc_constant_entries()
                .into_iter()
                .filter(|e| pause_slots || !e.name.starts_with("sun.gc.collector.")),
        );
    }
    let (bytes, offsets) = encode_perf_data_with_offsets(&entries, 1);
    let live_offsets = spread_live_offsets(offsets.get(first_live..).unwrap_or_default(), &present);
    (bytes, live_offsets)
}

/// zt t12/pl4: the [`LIVE_COUNTERS`] offsets in counter order, from the
/// encoded offsets of the counters `present` kept (in order, possibly
/// followed by other entries' offsets); `usize::MAX` for a counter left out,
/// which every writer skips. Pure.
fn spread_live_offsets(encoded: &[usize], present: &[bool]) -> Vec<usize> {
    let mut packed = encoded.iter().copied();
    present
        .iter()
        .map(|&kept| {
            if kept {
                packed.next().unwrap_or(usize::MAX)
            } else {
                usize::MAX
            }
        })
        .collect()
}

/// zt t12/pl4 (`zgc-zt11pl3-perf-counters-what-is-left-*` item 1;
/// `CRATONVM_ZGC_PERF_IMAGE_SHAPE`, default ON, `=0` kill): does this VM's
/// image take HotSpot 25's ZGC shape? `zgc` says the heap is ZGC. HotSpot's
/// ZGC image (measured, Windows 25.0.3, `jcmd <pid> PerfCounter.print` and
/// `jstat -J-Djstat.showUnsupported=true -snap`, before and after
/// `GC.run`) has no `sun.gc.collector.1.*`, one space per generation
/// (`sun.gc.generation.<g>.spaces=1`, no `space.1` / `space.2`) and no
/// `sun.os.hrt.ticks` (also absent on its G1 image; `jstat -t` reads
/// `sun.rt.createVmBeginTime`), and names its collectors and spaces
/// ([`zgc_constant_entries`]). `=0`: every counter of [`LIVE_COUNTERS`], as
/// before (a zero `collector.1`, zero survivor spaces). Other collectors:
/// unchanged.
fn zgc_image_shape(zgc: bool) -> bool {
    zgc
}

/// zt t12/pl4: the [`LIVE_COUNTERS`] a ZGC-shaped image leaves out
/// ([`zgc_image_shape`]): `sun.os.hrt.ticks`, `sun.gc.collector.1.*` and
/// `sun.gc.generation.0.space.1.*` / `space.2.*`. Pure.
fn absent_on_zgc(index: usize) -> bool {
    index == LIVE_TICKS
        || (LIVE_COLLECTOR + 2..LIVE_COLLECTOR + 4).contains(&index)
        || (LIVE_S0..LIVE_OLD).contains(&index)
}

/// zt t14/pl5: is the [`LIVE_COUNTERS`] index one of `sun.gc.collector.1.*`?
/// Pure.
fn is_collector_1(index: usize) -> bool {
    (LIVE_COLLECTOR + 2..LIVE_COLLECTOR + 4).contains(&index)
}

/// zt t12/pl4: the constants HotSpot 25's ZGC image names its collectors,
/// generations and spaces with (measured values, `jcmd PerfCounter.print`):
/// string constants sized to their text, as `PerfStringConstant`, and the
/// per-generation space count (`spaces`, a `long` constant). `sun.*`, so
/// not `supported`.
fn zgc_constant_entries() -> Vec<PerfEntry> {
    let name = |counter: &str, text: &str| PerfEntry::string_constant(counter, text, false);
    let spaces = |counter: &str| PerfEntry {
        name: counter.to_string(),
        value: PerfValue::Long(1),
        supported: false,
        units: UNITS_NONE,
        variability: VARIABILITY_CONSTANT,
    };
    vec![
        name("sun.gc.collector.0.name", "ZGC minor collection pauses"),
        name("sun.gc.collector.2.name", "ZGC major collection pauses"),
        name("sun.gc.generation.0.name", "young"),
        spaces("sun.gc.generation.0.spaces"),
        name("sun.gc.generation.0.space.0.name", "space"),
        name("sun.gc.generation.1.name", "old"),
        spaces("sun.gc.generation.1.spaces"),
        name("sun.gc.generation.1.space.0.name", "space"),
    ]
}

/// zt t14/pl5: the data offsets of the variables [`add_hotspot_counters`]
/// appends, which [`PerfDataFile::write_extras`] updates; `usize::MAX` for one
/// not added (the switch off, another collector, a full image).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExtraOffsets {
    /// `sun.gc.generation.0.capacity`, `sun.gc.generation.1.capacity` (ZGC).
    generation_capacity: [usize; 2],
    /// `sun.gc.metaspace.maxCapacity`, `sun.gc.compressedclassspace.maxCapacity`.
    metaspace_max: [usize; 2],
    /// `sun.gc.collector.0.lastEntryTime` / `.lastExitTime`, then collector
    /// 2's (ZGC).
    collector_times: [usize; 4],
    /// `sun.gc.cause`, `sun.gc.lastCause` ([`GC_CAUSE_CAPACITY`] bytes each;
    /// ZGC).
    gc_cause: [usize; 2],
    /// `sun.rt.createVmEndTime`, `sun.rt.vmInitDoneTime`.
    init_times: [usize; 2],
    /// zt t15/pl6: `java.threads.started`, `live`, `livePeak`, `daemon`.
    threads: [usize; 4],
    /// zt t15/pl6: `java.ci.totalTime`, `sun.ci.totalCompiles`,
    /// `osrCompiles`, `standardCompiles`, `lastType` (the JIT on).
    compiles: [usize; 5],
    /// zt t15/pl6: `sun.ci.lastMethod` ([`CI_LAST_METHOD_CAPACITY`] bytes).
    last_method: usize,
    /// zt t18/pl8: `sun.ci.standardTime`, `osrTime`, `totalBailouts`,
    /// `lastFailedType` (the JIT on).
    compile_extras: [usize; 4],
    /// zt t18/pl8: `sun.ci.lastFailedMethod` ([`CI_LAST_METHOD_CAPACITY`]
    /// bytes).
    last_failed_method: usize,
    /// zt t15/pl6: `sun.cls.initializedClasses`, `linkedClasses`,
    /// `verifiedClasses`.
    class_events: [usize; 3],
    /// zt t16/pl7: `sun.gc.tlab.*` in [`TLAB_COUNTERS`] order.
    tlab: [usize; 10],
    /// zt t16/pl7: `sun.cls.defineAppClasses`, `appClassBytes`,
    /// `unsafeDefineClassCalls`.
    define_counts: [usize; 3],
    /// zt t16/pl7: the class-trace timers in [`CLASS_TIMER_COUNTERS`] order
    /// (absent under `CRATONVM_PERF_CLASS_TIMERS=0`).
    class_times: [usize; 8],
    /// zt t29/cb: `sun.cls.sysClassBytes` (absent under
    /// `CRATONVM_PERF_CLASS_BYTES_CUMULATIVE=0`).
    sys_class_bytes: usize,
    /// zt pl4 item 1: `sun.ci.standardBytes`, `osrBytes`, `lastSize`,
    /// `nmethodSize`, `nmethodCodeSize` (the JIT on).
    compile_sizes: [usize; 5],
    /// zt pl4 item 1: `sun.cls.appClassLoadTime`, `.self`,
    /// `appClassLoadCount`, `sysClassLoadTime` (absent under
    /// `CRATONVM_PERF_CLASS_TIMERS=0`).
    class_loads: [usize; 4],
    /// zt pl4 item 3: G1's `sun.threads.cpu_time.gc_conc_mark`,
    /// `gc_conc_refine`, `gc_parallel_workers`, `gc_service` (a G1 image).
    g1_thread_cpu: [usize; 4],
    /// The dependency, subtype-table and method-size counters, in
    /// [`DEPENDENCY_COUNTERS`] order.
    dependency: [usize; DEPENDENCY_COUNTERS.len()],
}

impl ExtraOffsets {
    /// Nothing added.
    pub(crate) const ABSENT: Self = Self {
        generation_capacity: [usize::MAX; 2],
        metaspace_max: [usize::MAX; 2],
        collector_times: [usize::MAX; 4],
        gc_cause: [usize::MAX; 2],
        init_times: [usize::MAX; 2],
        threads: [usize::MAX; 4],
        compiles: [usize::MAX; 5],
        last_method: usize::MAX,
        compile_extras: [usize::MAX; 4],
        last_failed_method: usize::MAX,
        class_events: [usize::MAX; 3],
        tlab: [usize::MAX; 10],
        define_counts: [usize::MAX; 3],
        class_times: [usize::MAX; 8],
        sys_class_bytes: usize::MAX,
        compile_sizes: [usize::MAX; 5],
        class_loads: [usize::MAX; 4],
        g1_thread_cpu: [usize::MAX; 4],
        dependency: [usize::MAX; DEPENDENCY_COUNTERS.len()],
    };
}

/// zt t14/pl5: one write of the appended variables ([`ExtraOffsets`]): the
/// sampler's figures always, the rest only where the writer has them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ExtraValues {
    /// The young and old generations' capacity: the same figures as their
    /// one space's (`space.0.capacity`), as HotSpot's
    /// `ZServiceabilityCounters::update_sizes` writes both; `None`: leave
    /// them.
    generation_capacity: Option<[i64; 2]>,
    /// The reserved metaspace and class space (`MetaspaceStats::reserved`,
    /// the class space's third figure), HotSpot's
    /// `MetaspacePerfCounters::update` `max_capacity`; `None`: leave them.
    metaspace_max: Option<[i64; 2]>,
    /// [`zgc_last_collections`]' pause times; `None`: leave them.
    collector_times: Option<[i64; 4]>,
    /// [`zgc_last_collections`]' cause; `None`: leave it.
    last_cause: Option<&'static str>,
    /// `createVmEndTime`, `vmInitDoneTime`; `None`: leave them.
    init_times: Option<[i64; 2]>,
    /// zt t15/pl6: `[started, live, livePeak, daemon]`
    /// (`ThreadRegistry::jmm_perf_thread_counts`);
    /// `started` is stored as the larger of the old and the new value, so it
    /// never goes back (HotSpot's `_total_threads_count` is a counter);
    /// `None`: leave them.
    threads: Option<[i64; 4]>,
    /// zt t15/pl6: [`compile_values`]; `None`: leave them.
    compiles: Option<[i64; 5]>,
    /// zt t18/pl8: [`compile_extra_values`]; `None`: leave them.
    compile_extras: Option<[i64; 4]>,
    /// zt t15/pl6: `[initialized, linked, verified]` classes; `None`: leave
    /// them.
    class_events: Option<[i64; 3]>,
    /// zt t16/pl7: the last published TLAB statistics
    /// ([`tlab_perf_values`], [`TLAB_COUNTERS`] order); `None`: leave them.
    tlab: Option<[i64; 10]>,
    /// zt t16/pl7: `[defines, their bytes, Unsafe.defineClass0 calls]`
    /// (`DiagnosticCounters::app_classes_defined` / `app_class_bytes` /
    /// `unsafe_define_class_calls`); `None`: leave them.
    define_counts: Option<[i64; 3]>,
    /// zt t16/pl7: `ClassTraceCounters::snapshot`'s eight timers, in ticks;
    /// `None`: leave them.
    class_times: Option<[i64; 8]>,
    /// zt t29/cb: the boot loader's class-file bytes
    /// (`ClassManager::sys_class_path_bytes`); `None`: leave it.
    sys_class_bytes: Option<i64>,
    /// zt pl4 item 1: [`compile_size_values`]; `None`: leave them.
    compile_sizes: Option<[i64; 5]>,
    /// zt pl4 item 1: `ClassTraceCounters::load_snapshot`, times in ticks;
    /// `None`: leave them.
    class_loads: Option<[i64; 4]>,
    /// zt pl4 item 1: `sun.gc.cause`, the cause of the cycle in progress or
    /// `No GC` ([`note_concurrent_cycle_opened`]); `None`: leave it.
    gc_cause_now: Option<&'static str>,
    /// zt pl4 item 3: G1's four worker-group CPU figures, nanoseconds, in
    /// [`ExtraOffsets::g1_thread_cpu`]'s order; `None`: leave them.
    g1_thread_cpu: Option<[i64; 4]>,
    /// [`dependency_counter_values`]; `None`: leave them.
    dependency: Option<[i64; DEPENDENCY_COUNTERS.len()]>,
}

/// HotSpot 25 counters this VM fills from its own analogues: the dependency
/// checker's witness searches (`Dependencies`), the nmethod dependency buckets
/// (`DependencyContext`), the secondary-supers table timer (`ClassLoader`) and
/// the live methods' size (`ClassLoadingService`). Name, units and
/// variability are HotSpot's; every one is created whether or not the JIT
/// runs, as HotSpot creates them at start-up under `-Xint` too.
/// [`dependency_counter_values`] gives the values, in this order.
pub(crate) const DEPENDENCY_COUNTERS: [(&str, u8, u8); 9] = [
    (
        "sun.ci.findWitnessAnywhere",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    (
        "sun.ci.findWitnessAnywhereSteps",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    ("sun.ci.findWitnessIn", UNITS_EVENTS, VARIABILITY_MONOTONIC),
    (
        "sun.ci.nmethodBucketsAllocated",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    (
        "sun.ci.nmethodBucketsDeallocated",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    // A counter HotSpot lowers again at each purge (`PerfLongCounter::dec`),
    // listed with a counter's variability.
    (
        "sun.ci.nmethodBucketsStale",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    (
        "sun.ci.nmethodBucketsStaleAccumulated",
        UNITS_EVENTS,
        VARIABILITY_MONOTONIC,
    ),
    (
        "sun.cls.secondarySuperHashTime",
        UNITS_TICKS,
        VARIABILITY_MONOTONIC,
    ),
    ("sun.cls.methodBytes", UNITS_BYTES, VARIABILITY_VARIABLE),
];

/// [`DEPENDENCY_COUNTERS`]' values: the class realm's witness searches
/// (`ClassRealm::cha_witness`), the JIT cache's dependency book
/// (`JitCache::dependency_counter_values`; zeros while nothing compiles), the class
/// store's closure-building nanoseconds in ticks
/// (`PublishedSupers::hash_nanos`), and `method_bytes`, the live methods'
/// `Method` bytes ([`crate::vm::vm_init::hs_live_method_bytes`], taken with
/// the sampler's class-store walk). Pure but for the loads.
fn dependency_counter_values(
    shared: &crate::vm::SharedVm,
    method_bytes: u64,
) -> [i64; DEPENDENCY_COUNTERS.len()] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let load =
        |cell: &std::sync::atomic::AtomicU64| n(cell.load(std::sync::atomic::Ordering::Relaxed));
    let witness = &shared.classes.cha_witness;
    let deps = shared.jit.jit_cache.dependency_counter_values();
    [
        load(&witness.anywhere_calls),
        load(&witness.anywhere_steps),
        n(deps.find_witness_in),
        n(deps.buckets_allocated),
        n(deps.buckets_deallocated),
        n(deps.buckets_stale),
        n(deps.buckets_stale_accumulated),
        ticks_from_nanos(shared.classes.published_supers.hash_nanos()),
        n(method_bytes),
    ]
}

/// zt t15/pl6: `sun.ci.lastMethod`'s byte length, HotSpot's
/// `CompilerCounters::cmname_buffer_length` (`compileBroker.hpp`).
const CI_LAST_METHOD_CAPACITY: usize = 160;

/// zt t15/pl6: `sun.ci.lastType`'s values, HotSpot's
/// `CompileBroker::compile_type` (`no_compile`, `normal_compile`,
/// `osr_compile`).
const CI_NO_COMPILE: i64 = 0;
const CI_NORMAL_COMPILE: i64 = 1;
const CI_OSR_COMPILE: i64 = 2;

/// zt t15/pl6: the JIT's figures for the `sun.ci.*` counters
/// ([`add_hotspot_counters`]), from its own statistics
/// (`cratonvm_jit::tiered::CompilationStats`):
/// `[java.ci.totalTime, sun.ci.totalCompiles, osrCompiles, standardCompiles,
/// lastType]`. `totalTime` is every attempt's wall time
/// (`total_compile_time_ns`; zt t18/pl8, whole milliseconds before) in ticks
/// (`hrt_frequency`), and `CompilationMXBean` answers its
/// whole milliseconds, as HotSpot's bean reads its `_perf_total_compilation`
/// (`Management::ticks_to_ms`). `standardCompiles` counts the published
/// method-entry bodies of both tiers (HotSpot's standard compiles are its
/// tier 1-4 compiles; CratonVM's single-pass and optimizing tiers stand for
/// them), `osrCompiles` the published OSR bodies, `totalCompiles` both.
/// `lastType` is [`CI_NORMAL_COMPILE`] / [`CI_OSR_COMPILE`] for the last of
/// those, [`CI_NO_COMPILE`] before one. Pure.
fn compile_values(total_ns: u64, c1: u64, c2: u64, osr: u64, last_osr: Option<bool>) -> [i64; 5] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let standard = c1.saturating_add(c2);
    [
        ticks_from_nanos(total_ns),
        n(standard.saturating_add(osr)),
        n(osr),
        n(standard),
        compile_type(last_osr),
    ]
}

/// zt t15/pl6: HotSpot's `compile_type` code for a compile (`Some(osr)`) or
/// none. Pure.
fn compile_type(osr: Option<bool>) -> i64 {
    match osr {
        None => CI_NO_COMPILE,
        Some(false) => CI_NORMAL_COMPILE,
        Some(true) => CI_OSR_COMPILE,
    }
}

/// zt t18/pl8: `[sun.ci.standardTime, osrTime, totalBailouts,
/// lastFailedType]` from the JIT's statistics: the published method-entry
/// and OSR compiles' wall time in ticks (`hrt_frequency`), as HotSpot's
/// `collect_statistics` adds a successful compile's time to its kind's
/// counter only; the compiles that ran and published nothing
/// (`CompilationStats::total_bailouts`); and the last of those's
/// [`compile_type`]. Pure.
fn compile_extra_values(
    standard_ns: u64,
    osr_ns: u64,
    bailouts: u64,
    last_failed_osr: Option<bool>,
) -> [i64; 4] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    [
        ticks_from_nanos(standard_ns),
        ticks_from_nanos(osr_ns),
        n(bailouts),
        compile_type(last_failed_osr),
    ]
}

/// zt t15/pl6: `sun.ci.lastMethod`'s text for a compiled method: HotSpot's
/// `"<class> <method>"`, the class in its internal form
/// (`Symbol::as_C_string` of the holder's name). Pure.
fn last_method_text(class_name: &str, method_name: &str) -> String {
    format!("{} {method_name}", class_name.replace('.', "/"))
}

/// zt t16/pl7: HotSpot 25's `sun.gc.tlab.*` counters, in the order
/// `ThreadLocalAllocStats::initialize` creates them, with its units (all
/// `create_variable`, so variable). The five `Bytes` ones hold heap WORDS, as
/// HotSpot's do: `_allocated_size` and the wastes are `HeapWord` counts, and
/// `publish` sets them unscaled (only its log line multiplies by
/// `HeapWordSize`; the t12 HotSpot snapshot's `gcWaste=191070` is no multiple
/// of 8).
const TLAB_COUNTERS: [(&str, u8); 10] = [
    ("sun.gc.tlab.allocThreads", UNITS_NONE),
    ("sun.gc.tlab.fills", UNITS_NONE),
    ("sun.gc.tlab.maxFills", UNITS_NONE),
    ("sun.gc.tlab.alloc", UNITS_BYTES),
    ("sun.gc.tlab.gcWaste", UNITS_BYTES),
    ("sun.gc.tlab.maxGcWaste", UNITS_BYTES),
    ("sun.gc.tlab.refillWaste", UNITS_BYTES),
    ("sun.gc.tlab.maxRefillWaste", UNITS_BYTES),
    ("sun.gc.tlab.slowAlloc", UNITS_NONE),
    ("sun.gc.tlab.maxSlowAlloc", UNITS_NONE),
];

/// zt t16/pl7: the [`TLAB_COUNTERS`] values HotSpot's
/// `ThreadLocalAllocStats::publish` sets for one collection: `totals` the
/// per-thread deltas folded as `accumulate_and_reset_statistics` folds them
/// (`TlabTotals::add_thread`: threads, refills and wastes only of a thread
/// that refilled, slow allocations of every thread), and `handed_out_bytes`
/// the bytes of the buffers handed out since the previous collection
/// (`_total_allocations`, the refilling threads' `_allocated_size`). Wastes
/// and the allocation in words. `None` when no live thread refilled:
/// HotSpot's `publish` returns early then (`_total_allocations == 0`), and
/// the counters keep the previous collection's values. Pure.
fn tlab_perf_values(
    totals: &cratonvm_gc::tlab::TlabTotals,
    handed_out_bytes: u64,
) -> Option<[i64; 10]> {
    if totals.threads == 0 {
        return None;
    }
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let words = |bytes: u64| n(bytes / 8);
    Some([
        i64::from(totals.threads),
        n(totals.refills),
        i64::from(totals.max_refills),
        words(handed_out_bytes),
        words(totals.gc_waste_bytes),
        words(totals.max_gc_waste_bytes),
        words(totals.refill_waste_bytes),
        words(totals.max_refill_waste_bytes),
        n(totals.slow_allocations),
        i64::from(totals.max_slow_allocations),
    ])
}

/// zt t16/pl7 (`CRATONVM_PERF_HOTSPOT_COUNTERS`; `zgc-zt12pl4-*` item 1): at a
/// collection's start, with the world stopped (`run_collection_pause`, right
/// after `gc_event_start`): difference every live thread's TLAB cycle
/// counters (`Tlab::cycle_mark`: refills, slow allocations, refill and
/// collection waste) against the previous collection's marks, and the VM's
/// refill-byte total against its mark, and keep HotSpot's ten
/// `ThreadLocalAllocStats::publish` values ([`tlab_perf_values`]) for the
/// perf-data writers. Every collector: HotSpot keeps these counters on each
/// collector that uses TLABs, and every collection here passes this door.
/// One relaxed load unless the image has the counters.
///
/// What the figures are on this VM:
/// - a thread's refills are its granted refills (`tlab_alloc_shaped_inner`);
/// - its slow allocations are the misses that kept the buffer
///   (`Tlab::keep_on_miss`), which only the opt-in share sizer makes and
///   never on ZGC, so `slowAlloc` is 0 there;
/// - waste is a tail buried under a filler; a tail a sink took back (ZGC's
///   free list) is not waste, so on ZGC the wastes are 0 unless a sink
///   refused a tail -- HotSpot ZGC fills the tail and counts it;
/// - `alloc` is every buffer handed out since the last collection, including
///   those of a thread that died since (HotSpot loses those with the
///   thread's statistics); the other figures cover the live threads.
pub(crate) fn publish_tlab_stats(shared: &crate::vm::SharedVm) {
    let wedge = &shared.mem.tlab_wedge;
    let state = &wedge.tlab_perf;
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    if !state.armed.load(relaxed) {
        return;
    }
    let totals = state
        .marks
        .publish(shared.threads.thread_registry.tlab_cycle_snapshots());
    let total = wedge.refill_bytes_total.load(relaxed);
    let handed_out = total.wrapping_sub(state.refill_bytes_mark.swap(total, relaxed));
    if let Some(values) = tlab_perf_values(&totals, handed_out) {
        for (cell, value) in state.published.iter().zip(values) {
            cell.store(value, relaxed);
        }
    }
}

/// zt t16/pl7: the values [`publish_tlab_stats`] kept, for a perf-data
/// write; `None` before the image has the counters.
fn published_tlab_values(shared: &crate::vm::SharedVm) -> Option<[i64; 10]> {
    let state = &shared.mem.tlab_wedge.tlab_perf;
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    if !state.armed.load(relaxed) {
        return None;
    }
    let mut out = [0i64; 10];
    for (slot, cell) in out.iter_mut().zip(state.published.iter()) {
        *slot = cell.load(relaxed);
    }
    Some(out)
}

/// zt t16/pl7: the class-trace timers `ClassTraceCounters::snapshot` gives,
/// in its order, as HotSpot's `classLoader.cpp` names them (each a
/// `NEWPERFTICKCOUNTER`: ticks, monotonic).
const CLASS_TIMER_COUNTERS: [&str; 8] = [
    "sun.cls.classLinkedTime",
    "sun.cls.classLinkedTime.self",
    "sun.cls.classVerifyTime",
    "sun.cls.classVerifyTime.self",
    "sun.cls.classInitTime",
    "sun.cls.classInitTime.self",
    "sun.cls.defineAppClassTime",
    "sun.cls.defineAppClassTime.self",
];

/// `CRATONVM_PERF_CLASS_TIMERS` (zt t16/pl7 opt-in; zt t26/rt2 default ON,
/// `=0` kill, page `zgc-zt12pl4-perf-counters-what-is-left-*` item 1): the
/// image gets the eight class-trace timers of [`CLASS_TIMER_COUNTERS`] and
/// `sun.cls.time` their self times' sum, as HotSpot 25 publishes them by
/// default. `=0`: the names are absent and every class event is inert (no
/// clock read). Read once, when the image gets its counters.
fn class_timers_enabled() -> bool {
    true
}

/// zt t16/pl7: this VM's class-trace figures, for a
/// `cratonvm_types::perf_class_trace::ClassTraceTime` event; `None` (an
/// inert event: no clock read) under `CRATONVM_PERF_CLASS_TIMERS=0` (default
/// ON since zt t26/rt2; [`class_timers_enabled`]), i.e. unless the image got
/// its timers ([`add_hotspot_counters`]). One load.
#[inline]
pub(crate) fn class_trace(
    shared: &crate::vm::SharedVm,
) -> Option<std::sync::Arc<cratonvm_types::perf_class_trace::ClassTraceCounters>> {
    shared.debug.class_trace.get().cloned()
}

/// zt pl4 item 1 (`CRATONVM_PERF_GC_CAUSE_CONCURRENT`, default ON, `=0`
/// kill): `sun.gc.cause` names the cause of a cycle in progress. HotSpot's
/// ZGC sets it for the whole cycle (`ZGCCauseSetter` in the driver), so a
/// Java thread reads it while a concurrent mark runs; a stop-the-world cycle
/// shows nobody anything but `No GC`. This VM opens a concurrent mark only
/// under the opt-in `CRATONVM_ZGC_CONC_START`: the mark-start pause that
/// opens one writes its trigger's cause ([`note_concurrent_cycle_opened`]),
/// and every collection's pause refresh writes `No GC` back. Off: `No GC`
/// always, as before.
pub(crate) fn gc_cause_concurrent_enabled() -> bool {
    true
}

/// zt pl4 item 1: a mark-start pause opened a concurrent cycle for `cause`
/// (HotSpot's spelling): `sun.gc.cause` reads it until the cycle's closing
/// collection ([`gc_cause_concurrent_enabled`]). A no-op without an image.
pub(crate) fn note_concurrent_cycle_opened(shared: &crate::vm::SharedVm, cause: &'static str) {
    if !gc_cause_concurrent_enabled() {
        return;
    }
    if let Some(file) = shared.debug.perf_data_file.lock().as_ref() {
        file.write_extras(&ExtraValues {
            gc_cause_now: Some(cause),
            ..ExtraValues::default()
        });
    }
}

/// zt t14/pl5: `sun.gc.cause` / `sun.gc.lastCause`'s byte length, HotSpot's
/// `create_string_variable(SUN_GC, "cause", 80, ...)` (`collectedHeap.cpp`).
const GC_CAUSE_CAPACITY: usize = 80;

/// zt t14/pl5: HotSpot's `GCCause::to_string(_no_gc)`, both cause counters'
/// value before the first collection and `sun.gc.cause`'s between two.
const NO_GC_CAUSE: &str = "No GC";

/// zt t14/pl5: one aligned 8-byte store of `value` at `offset` of the image
/// at `base`; nothing for an offset not inside it (`usize::MAX` included).
fn store_long(base: usize, offset: usize, value: i64) {
    if offset == usize::MAX || offset % 8 != 0 || offset + 8 > PERF_DATA_SIZE {
        return;
    }
    // SAFETY: the caller holds `with_base`'s guards, so `base` maps (or owns)
    // PERF_DATA_SIZE bytes; `offset + 8` lies inside and is 8-byte aligned.
    let cell = unsafe { &*((base + offset) as *const std::sync::atomic::AtomicI64) };
    cell.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// zt t15/pl6: [`store_long`] that keeps the larger of the stored and the new
/// value (a counter that must never go back).
fn store_long_max(base: usize, offset: usize, value: i64) {
    if offset == usize::MAX || offset % 8 != 0 || offset + 8 > PERF_DATA_SIZE {
        return;
    }
    // SAFETY: as `store_long`.
    let cell = unsafe { &*((base + offset) as *const std::sync::atomic::AtomicI64) };
    cell.fetch_max(value, std::sync::atomic::Ordering::Relaxed);
}

/// zt t14/pl5: rewrite the string counter of `capacity` bytes at `offset` of
/// the image at `base` with `text`: at most `capacity - 1` bytes of it, the
/// rest NUL. Nothing for an offset not inside the image.
fn store_string(base: usize, offset: usize, capacity: usize, text: &str) {
    if offset == usize::MAX || capacity == 0 || offset.saturating_add(capacity) > PERF_DATA_SIZE {
        return;
    }
    let n = text.len().min(capacity - 1);
    // SAFETY: the caller holds `with_base`'s guards, so `base` maps (or owns)
    // PERF_DATA_SIZE bytes, and `[offset, offset + capacity)` lies inside;
    // `text` is a distinct allocation.
    unsafe {
        let at = (base + offset) as *mut u8;
        std::ptr::copy_nonoverlapping(text.as_ptr(), at, n);
        std::ptr::write_bytes(at.add(n), 0, capacity - n);
    }
}

/// zt t14/pl5: a `sun.*` `long` counter.
fn long_entry(name: &str, value: i64, units: u8, variability: u8) -> PerfEntry {
    PerfEntry {
        name: name.to_string(),
        value: PerfValue::Long(value),
        supported: false,
        units,
        variability,
    }
}

/// zt t14/pl5 (`CRATONVM_PERF_HOTSPOT_COUNTERS`, default ON, `=0` kill;
/// `zgc-zt12pl4-perf-counters-what-is-left-*` item 1): append to this VM's
/// perf-data image the HotSpot 25 counters it lacked that have a real source
/// here, with HotSpot's names, units and variability (the census rows), as
/// HotSpot adds counters to its perf memory during start-up
/// ([`PerfDataFile::append_entries`]). Called once, right after the image is
/// made, when the VM's system properties exist.
///
/// Every collector:
/// - `sun.gc.metaspace.minCapacity` / `sun.gc.compressedclassspace.minCapacity`
///   (constant 0, HotSpot's `MetaspacePerfCounters` minimum) and their
///   `maxCapacity` (the reserved figures, written by the sampler);
/// - `sun.rt.createVmEndTime` / `sun.rt.vmInitDoneTime` (0 until the VM
///   finishes its boot, [`record_vm_init_done`]);
/// - `sun.rt.jvmVersion` ([`jvm_version_word`] of this VM's `java.vm.version`)
///   and `sun.rt.internalVersion` ([`internal_version`]);
/// - StatSampler's property counters this image did not have:
///   `java.property.{java.class.path, java.home, java.library.path,
///   java.version, jdk.debug}` and `sun.property.sun.boot.library.path`, each
///   the VM's property value now (HotSpot reads `System.getProperty` at
///   `StatSampler::engage`), left out when the property is unset, as HotSpot
///   leaves it out.
///
/// A ZGC-shaped image ([`zgc_image_shape`]) also gets HotSpot's
/// `ZServiceabilityCounters` sizes (`generation.<g>.capacity`, written by the
/// sampler; `minCapacity` / `maxCapacity`; `space.0.initCapacity` /
/// `maxCapacity`, from `[MinHeapSize, InitialHeapSize, MaxHeapSize]`, young
/// `min` / `init` and old 0, both `max`), the pause collectors'
/// `lastEntryTime` / `lastExitTime` and `sun.gc.cause` / `sun.gc.lastCause`
/// ([`zgc_last_collections`]).
///
/// zt t15/pl6 adds, on every collector:
/// - `ThreadService`'s `java.threads.started` / `live` / `livePeak` /
///   `daemon` (the registry's JMM roster, written by the sampler);
/// - `sun.cls.initializedClasses`, `linkedClasses` and `verifiedClasses`
///   (the VM's initialization and link counts; no class is pre-verified here,
///   so a link verifies, as an unshared class's does in HotSpot), and
///   `sun.cls.sharedClassLoadTime` (0: no class is shared here);
/// - with the JIT on (`VmConfig::jit_enabled`; HotSpot under `-Xint` has no
///   `CompileBroker` counters): `java.ci.totalTime`, `sun.ci.totalCompiles`,
///   `osrCompiles`, `standardCompiles`, `lastMethod`, `lastType`
///   ([`compile_values`]) and the constant `sun.ci.threads` (the compile
///   workers, `compiler_thread_counts`, as HotSpot's C1 plus C2 count).
///
/// zt t16/pl7 adds, on every collector:
/// - `ThreadLocalAllocStats`' ten `sun.gc.tlab.*` counters
///   ([`TLAB_COUNTERS`], published at each collection by
///   [`publish_tlab_stats`]);
/// - `sun.cls.defineAppClasses`, `appClassBytes` and
///   `unsafeDefineClassCalls`, counted by the define natives (written by the
///   sampler);
/// - unless `CRATONVM_PERF_CLASS_TIMERS=0` (default ON since zt t26/rt2), the class-trace
///   timers `classLinkedTime`, `classVerifyTime`, `classInitTime`,
///   `defineAppClassTime` and their `.self` ([`CLASS_TIMER_COUNTERS`],
///   `cratonvm_types::perf_class_trace`), and `sun.cls.time` becomes their
///   self times' sum.
///
/// zt t18/pl8 adds, with the JIT on: `sun.ci.standardTime`, `osrTime`,
/// `totalBailouts`, `lastFailedType` and `lastFailedMethod`
/// ([`compile_extra_values`]); `java.ci.totalTime` is in nanoseconds since.
///
/// zt pl4 item 1 adds, with the JIT on: `sun.ci.standardBytes`, `osrBytes`,
/// `lastSize`, `nmethodSize`, `nmethodCodeSize` ([`compile_size_values`])
/// and HotSpot 25's constant `totalInvalidates` / `lastInvalidatedMethod` /
/// `lastInvalidatedType`.
///
/// Last, on every collector and with or without the JIT (HotSpot creates them
/// at start-up under `-Xint` too): the dependency checker's
/// `sun.ci.findWitnessAnywhere` / `findWitnessAnywhereSteps` /
/// `findWitnessIn`, the dependency buckets' `sun.ci.nmethodBuckets*`, and
/// `sun.cls.secondarySuperHashTime` / `methodBytes`
/// ([`DEPENDENCY_COUNTERS`], written by the sampler; page
/// `docs/feature-designs/generative-gc-architecture.md`).
///
/// zt t29/cb adds `sun.cls.sysClassBytes` unless
/// `CRATONVM_PERF_CLASS_BYTES_CUMULATIVE=0` ([`class_bytes_cumulative_enabled`]).
pub(crate) fn add_hotspot_counters(shared: &crate::vm::SharedVm, zgc: bool) {
    if !true
        || shared.debug.perf_data_file.lock().is_none()
    {
        return;
    }
    let config = &shared.config;
    let identity = crate::vm::vm_init::vm_identity_properties(config);
    let identity_of = |key: &str| {
        identity
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let properties = shared.system_properties.read().clone();
    let property = |key: &str| properties.get(key).cloned();

    // The counters' kinds: a variable and a constant of the given units.
    let var = |name: &str, units: u8| long_entry(name, 0, units, VARIABILITY_VARIABLE);
    let fixed =
        |name: &str, value: i64, units: u8| long_entry(name, value, units, VARIABILITY_CONSTANT);
    let variables = [
        var("sun.rt.createVmEndTime", UNITS_NONE),
        var("sun.rt.vmInitDoneTime", UNITS_NONE),
        var("sun.gc.metaspace.maxCapacity", UNITS_BYTES),
        var("sun.gc.compressedclassspace.maxCapacity", UNITS_BYTES),
    ];
    let vm_version = identity_of("java.vm.version");
    let vm_name = property("java.vm.name").unwrap_or_else(|| identity_of("java.vm.name"));
    let runtime_version = property("java.runtime.version")
        .or_else(|| property("java.version"))
        .unwrap_or_default();
    let internal = internal_version(&vm_name, &vm_version, &runtime_version);
    let jvm_version = jvm_version_word(&vm_version);
    let mut constants = vec![
        fixed("sun.gc.metaspace.minCapacity", 0, UNITS_BYTES),
        fixed("sun.gc.compressedclassspace.minCapacity", 0, UNITS_BYTES),
        fixed("sun.rt.jvmVersion", jvm_version, UNITS_NONE),
        PerfEntry::string_constant("sun.rt.internalVersion", &internal, false),
    ];
    // StatSampler's supported `java.property.*` names this image lacked, then
    // its unsupported `sun.property.*` one.
    let supported = [
        "java.class.path",
        "java.home",
        "java.library.path",
        "java.version",
        "jdk.debug",
    ];
    for key in supported {
        if let Some(value) = property(key) {
            let name = format!("java.property.{key}");
            constants.push(PerfEntry::string_constant(&name, &value, true));
        }
    }
    if let Some(value) = property("sun.boot.library.path") {
        let name = "sun.property.sun.boot.library.path";
        constants.push(PerfEntry::string_constant(name, &value, false));
    }
    let zgc_shape = zgc_image_shape(zgc);
    let [min, initial, max] = crate::vm::vm_exec::gci_i42_mx_hotspot_heap_sizes(
        config.min_heap_size,
        config.initial_heap_size,
        config.max_heap_size,
    );
    let cause = |name: &str| PerfEntry {
        name: name.to_string(),
        value: PerfValue::Str {
            text: NO_GC_CAUSE.to_string(),
            capacity: GC_CAUSE_CAPACITY,
        },
        supported: false,
        units: UNITS_STRING,
        variability: VARIABILITY_VARIABLE,
    };
    let zgc_variables = [
        var("sun.gc.generation.0.capacity", UNITS_BYTES),
        var("sun.gc.generation.1.capacity", UNITS_BYTES),
        var("sun.gc.collector.0.lastEntryTime", UNITS_TICKS),
        var("sun.gc.collector.0.lastExitTime", UNITS_TICKS),
        var("sun.gc.collector.2.lastEntryTime", UNITS_TICKS),
        var("sun.gc.collector.2.lastExitTime", UNITS_TICKS),
        cause("sun.gc.cause"),
        cause("sun.gc.lastCause"),
    ];
    let size = |name: &str, value: i64| fixed(name, value, UNITS_BYTES);
    let zgc_constants = [
        size("sun.gc.generation.0.minCapacity", min),
        size("sun.gc.generation.0.maxCapacity", max),
        size("sun.gc.generation.0.space.0.initCapacity", initial),
        size("sun.gc.generation.0.space.0.maxCapacity", max),
        size("sun.gc.generation.1.minCapacity", 0),
        size("sun.gc.generation.1.maxCapacity", max),
        size("sun.gc.generation.1.space.0.initCapacity", 0),
        size("sun.gc.generation.1.space.0.maxCapacity", max),
    ];

    // zt t15/pl6: `ThreadService`'s counters (`java.*`: supported), the
    // class link / init counts, and the JIT's.
    let java = |name: &str, units: u8, variability: u8| PerfEntry {
        supported: true,
        ..long_entry(name, 0, units, variability)
    };
    let events = |name: &str| long_entry(name, 0, UNITS_EVENTS, VARIABILITY_MONOTONIC);
    let thread_variables = [
        java("java.threads.started", UNITS_EVENTS, VARIABILITY_MONOTONIC),
        java("java.threads.live", UNITS_NONE, VARIABILITY_VARIABLE),
        java("java.threads.livePeak", UNITS_NONE, VARIABILITY_VARIABLE),
        java("java.threads.daemon", UNITS_NONE, VARIABILITY_VARIABLE),
    ];
    let class_variables = [
        events("sun.cls.initializedClasses"),
        events("sun.cls.linkedClasses"),
        events("sun.cls.verifiedClasses"),
    ];
    // HotSpot's CDS load timer: no class is shared here, so it stays 0, as
    // `java.cls.sharedLoadedClasses` does (HotSpot's own `-Xshare:off` value).
    let class_constants = [long_entry(
        "sun.cls.sharedClassLoadTime",
        0,
        UNITS_TICKS,
        VARIABILITY_MONOTONIC,
    )];
    let jit = config.jit_enabled;
    let g1_image = shared.mem.heap.g1_gc_thread_cpu_ns().is_some();
    let compile_variables = [
        java("java.ci.totalTime", UNITS_TICKS, VARIABILITY_MONOTONIC),
        events("sun.ci.totalCompiles"),
        events("sun.ci.osrCompiles"),
        events("sun.ci.standardCompiles"),
        long_entry(
            "sun.ci.lastType",
            CI_NO_COMPILE,
            UNITS_NONE,
            VARIABILITY_VARIABLE,
        ),
        PerfEntry {
            name: "sun.ci.lastMethod".to_string(),
            value: PerfValue::Str {
                text: String::new(),
                capacity: CI_LAST_METHOD_CAPACITY,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_VARIABLE,
        },
    ];
    let (c1_workers, c2_workers) = cratonvm_jit::tiered::compiler_thread_counts();
    let compile_workers = i64::try_from(c1_workers.saturating_add(c2_workers)).unwrap_or(0);
    // HotSpot's own units for it (`create_constant(SUN_CI, "threads",
    // PerfData::U_Bytes, ...)`).
    let compile_constants = [fixed("sun.ci.threads", compile_workers, UNITS_BYTES)];
    // zt t16/pl7: `ThreadLocalAllocStats`' counters (every collector), 0
    // until a collection publishes ([`publish_tlab_stats`]).
    let tlab_variables = TLAB_COUNTERS.map(|(name, units)| var(name, units));
    // zt t16/pl7: `ClassLoader`'s define counters (`classLoader.cpp`:
    // `defineAppClasses` a tick counter that counts events, as
    // `PerfClassTraceTime` increments it; `appClassBytes` a byte counter;
    // `unsafeDefineClassCalls` an event counter), counted by the define
    // natives (`NativeSystemAccess::note_jvm_define_class` /
    // `note_unsafe_define_class_call`).
    let define_variables = [
        long_entry(
            "sun.cls.defineAppClasses",
            0,
            UNITS_TICKS,
            VARIABILITY_MONOTONIC,
        ),
        long_entry(
            "sun.cls.appClassBytes",
            0,
            UNITS_BYTES,
            VARIABILITY_MONOTONIC,
        ),
        events("sun.cls.unsafeDefineClassCalls"),
    ];
    // zt t16/pl7 (`CRATONVM_PERF_CLASS_TIMERS`; two clock reads per class
    // event): the class-trace timers. zt t26/rt2: default ON (`=0` kill),
    // as HotSpot 25 publishes them by default (t16 `pl7_values_T` SAME,
    // t20 `fl1_ct_*` CPU T/D 1.011 over 6 ABBA pairs). Under the kill the
    // names are absent, as a timer that never runs would read a false 0.
    let class_timers = class_timers_enabled();
    let timer_variables =
        CLASS_TIMER_COUNTERS.map(|name| long_entry(name, 0, UNITS_TICKS, VARIABILITY_MONOTONIC));
    // zt t18/pl8 (the JIT on): `CompileBroker`'s per-kind times (`Ticks`
    // counters), its bailout count and the last bailed compile's type and
    // method (`compile_extra_values`).
    let ticks = |name: &str| long_entry(name, 0, UNITS_TICKS, VARIABILITY_MONOTONIC);
    let compile_extra_variables = [
        ticks("sun.ci.standardTime"),
        ticks("sun.ci.osrTime"),
        events("sun.ci.totalBailouts"),
        long_entry(
            "sun.ci.lastFailedType",
            CI_NO_COMPILE,
            UNITS_NONE,
            VARIABILITY_VARIABLE,
        ),
        PerfEntry {
            name: "sun.ci.lastFailedMethod".to_string(),
            value: PerfValue::Str {
                text: String::new(),
                capacity: CI_LAST_METHOD_CAPACITY,
            },
            supported: false,
            units: UNITS_STRING,
            variability: VARIABILITY_VARIABLE,
        },
    ];

    let mut guard = shared.debug.perf_data_file.lock();
    let Some(file) = guard.as_mut() else {
        return;
    };
    let at = file.append_entries(&variables);
    file.extra.init_times = [at[0], at[1]];
    file.extra.metaspace_max = [at[2], at[3]];
    file.append_entries(&constants);
    if zgc_shape {
        let at = file.append_entries(&zgc_variables);
        file.extra.generation_capacity = [at[0], at[1]];
        file.extra.collector_times = [at[2], at[3], at[4], at[5]];
        file.extra.gc_cause = [at[6], at[7]];
        file.append_entries(&zgc_constants);
    }
    // zt t15/pl6: after t14/pl5's, so its layout is unchanged.
    let at = file.append_entries(&thread_variables);
    file.extra.threads = [at[0], at[1], at[2], at[3]];
    let at = file.append_entries(&class_variables);
    file.extra.class_events = [at[0], at[1], at[2]];
    file.append_entries(&class_constants);
    if jit {
        let at = file.append_entries(&compile_variables);
        file.extra.compiles = [at[0], at[1], at[2], at[3], at[4]];
        file.extra.last_method = at[5];
        file.append_entries(&compile_constants);
    }
    // zt t16/pl7: after t15/pl6's, so its layout is unchanged. Armed only
    // once they are in the image: until then a collection publishes nothing.
    let at = file.append_entries(&tlab_variables);
    file.extra.tlab = std::array::from_fn(|i| at[i]);
    if at.iter().any(|&o| o != usize::MAX) {
        shared
            .mem
            .tlab_wedge
            .tlab_perf
            .armed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let at = file.append_entries(&define_variables);
    file.extra.define_counts = [at[0], at[1], at[2]];
    if class_timers {
        let at = file.append_entries(&timer_variables);
        file.extra.class_times = std::array::from_fn(|i| at[i]);
        if at.iter().any(|&o| o != usize::MAX) {
            let _ = shared.debug.class_trace.set(std::sync::Arc::default());
        }
    }
    // zt t18/pl8: after t16/pl7's, so its layout is unchanged.
    if jit {
        let at = file.append_entries(&compile_extra_variables);
        file.extra.compile_extras = [at[0], at[1], at[2], at[3]];
        file.extra.last_failed_method = at[4];
    }
    // zt t29/cb (perf-counter page item 1): `ClassLoader`'s
    // `sun.cls.sysClassBytes` (a byte counter: every class file the boot
    // loader reads outside the CDS archive, the jimage's included), after
    // t18/pl8's, so its layout is unchanged. Under the kill the name is
    // absent, as before.
    if class_bytes_cumulative_enabled() {
        let at = file.append_entries(&[long_entry(
            "sun.cls.sysClassBytes",
            0,
            UNITS_BYTES,
            VARIABILITY_MONOTONIC,
        )]);
        file.extra.sys_class_bytes = at[0];
    }
    // zt pl4 item 1 (the JIT on): the rest of `CompileBroker`'s counters,
    // after t29/cb's, so its layout is unchanged. `sun.ci.standardBytes` /
    // `osrBytes` / `lastSize` / `nmethodSize` / `nmethodCodeSize` from the
    // published compiles' sizes ([`compile_size_values`]). The three
    // invalidation names are HotSpot 25's constants: its
    // `collect_statistics` books an invalidation under `else if
    // (!task->is_success())` after `if (!success)` with `success =
    // task->is_success()`, an arm that cannot run, so `totalInvalidates` is
    // 0, `lastInvalidatedMethod` empty and `lastInvalidatedType`
    // `no_compile` for the life of every HotSpot 25 VM.
    if jit {
        let bytes = |name: &str| long_entry(name, 0, UNITS_BYTES, VARIABILITY_MONOTONIC);
        let at = file.append_entries(&[
            bytes("sun.ci.standardBytes"),
            bytes("sun.ci.osrBytes"),
            long_entry("sun.ci.lastSize", CI_NO_COMPILE, UNITS_BYTES, VARIABILITY_VARIABLE),
            bytes("sun.ci.nmethodSize"),
            bytes("sun.ci.nmethodCodeSize"),
        ]);
        file.extra.compile_sizes = [at[0], at[1], at[2], at[3], at[4]];
        file.append_entries(&[
            long_entry("sun.ci.totalInvalidates", 0, UNITS_EVENTS, VARIABILITY_MONOTONIC),
            PerfEntry {
                name: "sun.ci.lastInvalidatedMethod".to_string(),
                value: PerfValue::Str {
                    text: String::new(),
                    capacity: CI_LAST_METHOD_CAPACITY,
                },
                supported: false,
                units: UNITS_STRING,
                variability: VARIABILITY_VARIABLE,
            },
            long_entry(
                "sun.ci.lastInvalidatedType",
                CI_NO_COMPILE,
                UNITS_NONE,
                VARIABILITY_VARIABLE,
            ),
        ]);
    }
    // zt pl4 item 1 (with the class timers): `ClassLoader`'s
    // `appClassLoadTime` / `.self` (`Ticks`), `appClassLoadCount`
    // (`Events`) and `sysClassLoadTime` (`Ticks`), all monotonic counters,
    // after the compile sizes, so the layout before is unchanged.
    if class_timers {
        let at = file.append_entries(&[
            long_entry("sun.cls.appClassLoadTime", 0, UNITS_TICKS, VARIABILITY_MONOTONIC),
            long_entry("sun.cls.appClassLoadTime.self", 0, UNITS_TICKS, VARIABILITY_MONOTONIC),
            events("sun.cls.appClassLoadCount"),
            long_entry("sun.cls.sysClassLoadTime", 0, UNITS_TICKS, VARIABILITY_MONOTONIC),
        ]);
        file.extra.class_loads = [at[0], at[1], at[2], at[3]];
    }
    // zt pl4 item 3: HotSpot's G1 creates its worker groups' CPU counters
    // (`CPUTimeCounters::create_counter`, `Ticks` units, written in
    // nanoseconds); its ZGC and Serial create none.
    if g1_image && cratonvm_gc::thread_cpu::gc_thread_cpu_on() {
        let cpu = |name: &str| long_entry(name, 0, UNITS_TICKS, VARIABILITY_MONOTONIC);
        let at = file.append_entries(&[
            cpu("sun.threads.cpu_time.gc_conc_mark"),
            cpu("sun.threads.cpu_time.gc_conc_refine"),
            cpu("sun.threads.cpu_time.gc_parallel_workers"),
            cpu("sun.threads.cpu_time.gc_service"),
        ]);
        file.extra.g1_thread_cpu = [at[0], at[1], at[2], at[3]];
    }
    // The dependency, subtype-table and method-size counters
    // ([`DEPENDENCY_COUNTERS`]), last, so the layout before is unchanged.
    let entries: Vec<PerfEntry> = DEPENDENCY_COUNTERS
        .iter()
        .map(|&(name, units, variability)| long_entry(name, 0, units, variability))
        .collect();
    let at = file.append_entries(&entries);
    file.extra.dependency = std::array::from_fn(|i| at[i]);
    drop(guard);
    // zt pl4 item 1: the class manager times its class-path door with the
    // same figures (`ClassManager::set_class_trace`), after the image's lock
    // is let go (the sampler takes the class manager's under it never).
    let counters = class_trace(shared);
    shared
        .classes
        .class_manager
        .write()
        .set_class_trace(counters);
}

/// zt pl4 item 1: `[sun.ci.standardBytes, osrBytes, lastSize, nmethodSize,
/// nmethodCodeSize]` from the JIT's statistics (`CompilationStats`'
/// published-compile sizes, booked by `CompilerCore::finish` with the
/// counts). Pure but for the relaxed loads.
fn compile_size_values(stats: &cratonvm_jit::tiered::CompilationStats) -> [i64; 5] {
    let load = |cell: &std::sync::atomic::AtomicU64| {
        i64::try_from(cell.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(i64::MAX)
    };
    [
        load(&stats.standard_bytes),
        load(&stats.osr_bytes),
        load(&stats.last_size),
        load(&stats.nmethod_size),
        load(&stats.nmethod_code_size),
    ]
}

/// zt t29/cb (`zgc-zt12pl4-perf-counters-what-is-left-FIXED-20261023.md` item 1):
/// `CRATONVM_PERF_CLASS_BYTES_CUMULATIVE` (default ON, `=0` kill). On:
/// `sun.cls.unloadedBytes` counts the model bytes of every unloaded class
/// (the unload transaction, `memory::gc::unload_dead_class_metadata`) and
/// `sun.cls.loadedBytes` is the live figure plus that, both cumulative as
/// HotSpot's; and `sun.cls.sysClassBytes` is added, the class-file bytes the
/// boot loader defined from its search (image, appended and patch entries;
/// `ClassManager::sys_class_path_bytes`). Off: `loadedBytes` the live
/// figure, `unloadedBytes` 0, no `sysClassBytes`. The sampler reads it once
/// per VM; the unload transaction once per transaction with dead classes.
pub(crate) fn class_bytes_cumulative_enabled() -> bool {
    true
}

/// zt t14/pl5: HotSpot's `Abstract_VM_Version::jvm_version()` for a
/// `java.vm.version` of the form `<major>.<minor>.<security>+<build>`
/// (missing parts 0, a build's suffix such as `-LTS` ignored): the four
/// numbers' low bytes, major first. Pure.
fn jvm_version_word(version: &str) -> i64 {
    let (numbers, build) = version.split_once('+').unwrap_or((version, ""));
    let leading = |s: &str| {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<i64>().unwrap_or(0) & 0xFF
    };
    let mut parts = numbers.split('.');
    let mut next = || parts.next().map_or(0, leading);
    let (major, minor, security) = (next(), next(), next());
    (major << 24) | (minor << 16) | (security << 8) | leading(build)
}

/// The binary's build stamp, `(time, compiler)`, set once by the launcher
/// ([`set_build_stamp`]); unset in an embedding.
static BUILD_STAMP: std::sync::OnceLock<(&'static str, &'static str)> = std::sync::OnceLock::new();

/// Record the binary's build stamp for `sun.rt.internalVersion`: `time` as
/// HotSpot's `HOTSPOT_BUILD_TIME` (`YYYY-MM-DDTHH:MM:SSZ`), `compiler` as its
/// `HOTSPOT_BUILD_COMPILER` (`rustc 1.x.y`). The launcher calls it with what
/// its build script derived (`vm-cli/build.rs`: `SOURCE_DATE_EPOCH`, else the
/// commit time). A property of the executable, like HotSpot's, so one per
/// process; the first call wins and an empty part leaves the stamp unset.
pub fn set_build_stamp(time: &'static str, compiler: &'static str) {
    if !time.is_empty() && !compiler.is_empty() {
        let _ = BUILD_STAMP.set((time, compiler));
    }
}

/// zt t14/pl5: `sun.rt.internalVersion`, HotSpot's
/// `Abstract_VM_Version::internal_vm_info_string()` shape
/// (`<vm name> (<vm release>) for <os>-<cpu> JRE (<version string>)`) with
/// this VM's own name, release and runtime version, then HotSpot 25's
/// `, built on <time> with <compiler>` from the binary's build stamp
/// ([`set_build_stamp`]); without one (an embedding, a build with no git and
/// no `SOURCE_DATE_EPOCH`) the string ends at the JRE version.
fn internal_version(vm_name: &str, vm_version: &str, runtime_version: &str) -> String {
    internal_version_with(
        vm_name,
        vm_version,
        runtime_version,
        BUILD_STAMP.get().copied(),
    )
}

/// [`internal_version`] with the stamp given. Pure.
fn internal_version_with(
    vm_name: &str,
    vm_version: &str,
    runtime_version: &str,
    stamp: Option<(&str, &str)>,
) -> String {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "bsd"
    };
    let cpu = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        std::env::consts::ARCH
    };
    let built = stamp.map_or(String::new(), |(time, compiler)| {
        format!(", built on {time} with {compiler}")
    });
    format!("{vm_name} ({vm_version}) for {os}-{cpu} JRE ({runtime_version}){built}")
}

/// zt t14/pl5: ZGC's pause collectors' last pause and the last cycle's
/// cause, from each bean's last collection (the record
/// `GarbageCollectorMXBean.getLastGcInfo()` reads): `[collector 0 entry,
/// exit, collector 2 entry, exit]` in ticks (`hrt_frequency`)
/// since the VM began (`DiagnosticCounters::start_instant`, the origin of
/// `GcInfo.getStartTime()`), 0 for a collector that has not run; and the
/// `GCCause` of whichever cycle bean (`ZGC Minor Cycles`, `ZGC Major Cycles`)
/// ended last, `None` before the first collection. HotSpot's
/// `TraceCollectorStats` stamps `lastEntryTime` / `lastExitTime` around each
/// pause (`os::elapsed_counter`), and its `GCCauseSetter` leaves
/// `sun.gc.lastCause` at the cause of the collection that ended last and
/// `sun.gc.cause` at `No GC` between collections; a collection here runs with
/// the world stopped (concurrent marking is opt-in), so no Java reader sees
/// one in progress and `sun.gc.cause` stays `No GC`.
///
/// A bean's last record in `pending` (a collection's records not yet booked
/// as each bean's last) wins over the queue's.
fn zgc_last_collections(
    shared: &crate::vm::SharedVm,
    pending: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) -> ([i64; 4], Option<&'static str>) {
    use cratonvm_gc::gc_metrics as m;
    let mut times = [0i64; 4];
    let Some(beans) = shared.mem.backend_gc_beans() else {
        return (times, None);
    };
    let queue = beans.notifications();
    let last = |name: &str| {
        pending
            .iter()
            .rev()
            .find(|r| r.collector == name)
            .map(|r| (r.start, r.end, r.cause))
            .or_else(|| queue.last(name).map(|r| (r.start, r.end, r.cause)))
    };
    let origin = shared.debug.diagnostic_counters.start_instant();
    // zt t21/pl10: in ticks of `hrt_frequency`.
    let ticks = |at: std::time::Instant| {
        ticks_from_nanos(
            u64::try_from(at.saturating_duration_since(origin).as_nanos()).unwrap_or(u64::MAX),
        )
    };
    for (slot, name) in [
        (0, m::JMX_ZGC_MINOR_PAUSES_COLLECTOR),
        (2, m::JMX_ZGC_MAJOR_PAUSES_COLLECTOR),
    ] {
        if let Some((start, end, _)) = last(name) {
            times[slot] = ticks(start);
            times[slot + 1] = ticks(end);
        }
    }
    let mut latest: Option<(std::time::Instant, &'static str)> = None;
    let cycles = [
        m::JMX_ZGC_MINOR_CYCLES_COLLECTOR,
        m::JMX_ZGC_MAJOR_CYCLES_COLLECTOR,
    ];
    for name in cycles {
        if let Some((_, end, cause)) = last(name) {
            if latest.is_none_or(|(at, _)| end > at) {
                latest = Some((end, cause));
            }
        }
    }
    (times, latest.map(|(_, cause)| cause))
}

/// zt t14/pl5: the ZGC half of an [`ExtraValues`] write
/// ([`zgc_last_collections`]); `None` for both off ZGC.
fn zgc_extra_values(
    shared: &crate::vm::SharedVm,
    zgc: bool,
    pending: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) -> (Option<[i64; 4]>, Option<&'static str>) {
    if !zgc {
        return (None, None);
    }
    let (times, cause) = zgc_last_collections(shared, pending);
    (Some(times), cause)
}





/// zt t20/pl9 (`CRATONVM_PERF_DEFINE_COUNTS_CLASS_PATH`, default ON, `=0`
/// kill; t16 defect D6): see [`define_count_values`]. Off: the define
/// natives' counts alone, as before.
fn define_counts_class_path_enabled() -> bool {
    true
}

/// zt t20/pl9 (D6): `sun.cls.defineAppClasses`, `appClassBytes`,
/// `unsafeDefineClassCalls`. `natives`: the define natives' counts
/// (`DiagnosticCounters::app_classes_defined` / `app_class_bytes` /
/// `unsafe_define_class_calls`). `class_path`: the application loader's
/// class-path defines and their bytes (`ClassManager::app_class_path_defines`):
/// HotSpot's application loader defines every class-path class through
/// `defineClass1` / `defineClass2` (`jvm_define_class_common` counts it), and
/// this VM's defines them in `ClassManager::load_class` in both modes, so its
/// total missed them (t16 rows `pl7_values_*`, 9/9: `app_loader_counted=false`,
/// `before=2/866` against HotSpot's `5/14536`). A class goes through exactly
/// one of the two doors, so the sum counts each define once. `with_class_path`
/// false: the natives' counts alone. Pure.
fn define_count_values(
    natives: [i64; 3],
    class_path: (u64, u64),
    with_class_path: bool,
) -> [i64; 3] {
    if !with_class_path {
        return natives;
    }
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    [
        natives[0].saturating_add(n(class_path.0)),
        natives[1].saturating_add(n(class_path.1)),
        natives[2],
    ]
}

/// zt t20/pl9 (D5): `sun.gc.metaspace.maxCapacity` and
/// `sun.gc.compressedclassspace.maxCapacity` from the model's reserved
/// figures, as HotSpot 25 holds them. `MetaspacePerfCounters::update` writes
/// `reserved()`: the class space's alone for the second, the class plus the
/// non-class space's (`MetaspaceCombinedStats`) for the first. HotSpot writes
/// them at two points only: `universe_init`
/// (`MetaspaceCounters::initialize_performance_counters`, right after
/// `Metaspace::global_initialize`), and each collection's end (ZGC:
/// `ZServiceabilityCounters::update_sizes`, from every pause tracer). At
/// `universe_init` the non-class list is still empty (an expandable
/// `VirtualSpaceList` reserves its first 64 MiB node at the first metadata
/// allocation), so until the first collection the combined figure is the
/// class space's reservation: 1073741824 with the defaults, which t16 rows
/// `pl5_values_*` read on HotSpot before any collection, against 1140850688
/// here. After one, the non-class nodes count (t9 `x6_3`'s HotSpot log:
/// `GC(0) ... 1088M reserved`). `collections`: the collectors' invocations the
/// image shows. Without a class space the combined figure is 0 until then, as
/// HotSpot's under `-XX:-UseCompressedClassPointers`. `before_gc` false: the
/// combined reservation at every pass. Pure.
fn metaspace_max_values(
    metadata: &crate::vm::vm_init::MetaspaceStats,
    collections: u64,
    before_gc: bool,
) -> [i64; 2] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let class_reserved = metadata.class_space.map_or(0, |(_, _, reserved)| reserved);
    let combined = if before_gc && collections == 0 {
        class_reserved
    } else {
        metadata.reserved
    };
    [n(combined), n(class_reserved)]
}

/// zt t22/x18 (review of t21/pl10; `CRATONVM_PERF_HRT_VM_ORIGIN`, default ON,
/// `=0` kill): the instant `sun.os.hrt.ticks` counts from. HotSpot samples
/// `os::elapsed_counter()` into it, the origin of `Perf.highResCounter()` and of
/// `sun.gc.collector.<n>.lastEntryTime` / `lastExitTime`, which pl10 moved to
/// this VM's start (`vm_start`); the ticks counted from the sampler's own start
/// (`sampler_start`, after the boot), so `hrt.ticks - lastExitTime` was short by
/// the boot and `jstat -t`'s `Timestamp` (ticks over the frequency) was not the
/// uptime. `=0`: the sampler's start, as before. Pure.
fn hrt_ticks_origin(
    vm_start: std::time::Instant,
    sampler_start: std::time::Instant,
    vm_origin: bool,
) -> std::time::Instant {
    if vm_origin {
        vm_start
    } else {
        sampler_start
    }
}

/// zt t21/pl10: `[sun.gc.metaspace.capacity, used, maxCapacity,
/// sun.gc.compressedclassspace.capacity, used, maxCapacity]`.
type MetaspaceFigures = [i64; 6];



/// zt t21/pl10 (`zgc-zt12pl4-perf-counters-what-is-left-*` item 10): the six
/// metaspace counters as HotSpot 25 holds them. `MetaspacePerfCounters::update`
/// writes `committed()`, `reserved()` and `used()` of the combined and the
/// class-space statistics together, at two points only
/// (`metaspaceCounters.cpp`): `universe_init`
/// (`MetaspaceCounters::initialize_performance_counters`, right after
/// `Metaspace::global_initialize`, before any class is loaded: nothing is
/// committed or used yet, and only the class space is reserved, item 9 D5),
/// and each collection pause's end (ZGC: `ZServiceabilityCounters::update_sizes`
/// from every `ZServiceabilityPauseTracer`). Between collections the figures
/// stand still, so `jstat -gc` `MC` / `MU` / `CCSC` / `CCSU` read 0 before the
/// first collection and the last collection's figures after it. This sampler
/// wrote the current figures at every pass.
///
/// With `at_gc`: 0 for the capacities and uses (and [`metaspace_max_values`]'
/// pre-collection reservations) while `collections` (the invocations the
/// image shows) is 0; after that, the figures of the first pass that saw the
/// current count, held in `held` until the count moves. That pass walked the
/// metadata afresh (the sampler's walk is due whenever the count moved), so
/// the held figures are those right after the collection, at most one
/// sampler period late (HotSpot writes them at the pause; the event refreshes
/// have no metadata figures). Without `at_gc`: the current figures. Pure but
/// for `held`.
fn metaspace_counter_values(
    metadata: &crate::vm::vm_init::MetaspaceStats,
    collections: u64,
    before_gc: bool,
    at_gc: bool,
    held: &mut Option<(u64, MetaspaceFigures)>,
) -> MetaspaceFigures {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let [max, class_max] = metaspace_max_values(metadata, collections, before_gc);
    let (class_committed, class_used) = metadata
        .class_space
        .map_or((0, 0), |(used, committed, _)| (committed, used));
    let current = [
        n(metadata.committed),
        n(metadata.used),
        max,
        n(class_committed),
        n(class_used),
        class_max,
    ];
    if !at_gc {
        return current;
    }
    if collections == 0 {
        *held = None;
        return [0, 0, max, 0, 0, class_max];
    }
    match *held {
        Some((seen, figures)) if seen == collections => figures,
        _ => {
            *held = Some((collections, current));
            current
        }
    }
}

/// zt t21/pl10: [`metaspace_counter_values`]' capacities and uses into the
/// [`LIVE_COUNTERS`] values.
fn put_metaspace_figures(values: &mut [i64; LIVE_COUNT], figures: &MetaspaceFigures) {
    values[LIVE_METASPACE] = figures[0];
    values[LIVE_METASPACE + 1] = figures[1];
    values[LIVE_CCS] = figures[3];
    values[LIVE_CCS + 1] = figures[4];
}

/// zt t14/pl5: `sun.rt.createVmEndTime` and `sun.rt.vmInitDoneTime`, both
/// `now` in milliseconds since the epoch (as `sun.rt.createVmBeginTime`),
/// when this VM finishes its boot (`SharedVm::raise_vm_init_level` reaching
/// 4, or an embedder's `mark_boot_finished`): HotSpot sets them at the end of
/// `Threads::create_vm` (`Management::record_vm_init_completed`,
/// `record_vm_startup_time`), after `System.initPhase3`. Written once;
/// nothing without the counters.
pub(crate) fn record_vm_init_done(shared: &crate::vm::SharedVm) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let mut guard = shared.debug.perf_data_file.lock();
    let Some(file) = guard.as_mut() else {
        return;
    };
    file.write_extras(&ExtraValues {
        init_times: Some([now, now]),
        ..ExtraValues::default()
    });
    // Once: a later boot mark (level 4 after an embedder's latch) finds
    // nothing to write.
    file.extra.init_times = [usize::MAX; 2];
    // zt t18/pl8: one sampler pass at the boot's end, so the sampled
    // figures are current when `main` starts rather than up to one interval
    // later.
    if let Some(handle) = file
        .sampler
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        // zt t20/pl9: asked for, so the park does not take the unpark for a
        // spurious return (`park_for_interval`).
        handle
            .wake
            .store(true, std::sync::atomic::Ordering::Release);
        handle.thread.thread().unpark();
    };
}

/// `java.rt.vmArgs`: HotSpot's `Arguments::jvm_args()`, the options joined
/// by single spaces (`build_jvm_args`), from `VmConfig::jvm_args` when the
/// launcher recorded them (interpreter round i4 wave 84, lane L1: `jps -v`
/// listed only the `-D` options before); else, for an embedder, the `-D`
/// options as before. `java.rt.vmFlags` stays empty: HotSpot fills it only
/// from a flags file (`-XX:Flags=`, `.hotspotrc` with
/// `-XX:+ReadVMOptionsFile`), which this VM does not read.
fn vm_args_text(config: &crate::config::VmConfig) -> String {
    if !config.jvm_args.is_empty() {
        return config.jvm_args.join(" ");
    }
    config
        .system_properties
        .iter()
        .filter(|(k, _)| k != "sun.java.command" && !k.starts_with("sun.java.launcher"))
        .map(|(k, v)| format!("-D{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Drop for PerfDataFile {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The directory HotSpot uses: `<tmp>/hsperfdata_<user>`, `<tmp>` being
/// `/tmp` (`os::get_temp_directory` on Linux; the user's temporary
/// directory on macOS).
#[cfg(unix)]
fn perf_data_dir(user: &str) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    let tmp = std::env::temp_dir();
    #[cfg(not(target_os = "macos"))]
    let tmp = std::path::PathBuf::from("/tmp");
    tmp.join(format!("hsperfdata_{user}"))
}

/// The effective user's name (`getpwuid_r(geteuid())`), as HotSpot names
/// the directory.
#[cfg(unix)]
fn effective_user_name() -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: `passwd` is plain C data; all-zero is a valid placeholder that
    // `getpwuid_r` overwrites.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is live for the call and `buf.len()` is its size.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` points into `buf`, NUL-terminated by `getpwuid_r`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

/// Write `bytes` as `<dir>/<pid>` (the directory made 0755 when missing,
/// the file 0600): under a staging name first, then linked, so a reader
/// never maps a partial file. `None` (with a warning) when it cannot.
#[cfg(unix)]
pub(crate) fn write_perf_data_file(pid: u32, bytes: &[u8]) -> Option<PerfDataFile> {
    let user = effective_user_name()?;
    write_perf_data_in_dir(&perf_data_dir(&user), pid, bytes)
}

/// Linux can reuse a crashed VM's PID before the next sweep. A publication
/// predating this process is stale even though kill(pid, 0) succeeds.
#[cfg(target_os = "linux")]
fn current_process_birth() -> Option<std::time::SystemTime> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let tail = stat.rsplit_once(')')?.1;
    let ticks = tail.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let mut uptime: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut uptime) } != 0 {
        return None;
    }
    let age_ns = (uptime.tv_sec as u128 * 1_000_000_000 + uptime.tv_nsec as u128)
        .checked_sub(ticks as u128 * 1_000_000_000 / hz as u128)?;
    std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_nanos(u64::try_from(age_ns).ok()?))
}
#[cfg(all(unix, not(target_os = "linux")))]
fn current_process_birth() -> Option<std::time::SystemTime> {
    None
}

/// Publication owns one inode, never overwrites a live VM's file, and refuses
/// symlink/writable/foreign directories. This also keeps embedded VMs in one
/// process from deleting each other's publication at teardown.
#[cfg(unix)]
fn write_perf_data_in_dir(dir: &std::path::Path, pid: u32, bytes: &[u8]) -> Option<PerfDataFile> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    let uid = unsafe { libc::geteuid() };
    if !dir.exists() {
        if let Err(error) = std::fs::DirBuilder::new().mode(0o755).create(dir) {
            if !dir.exists() {
                tracing::warn!("perf data: cannot create {}: {error}", dir.display());
                return None;
            }
        }
    }
    let metadata = std::fs::symlink_metadata(dir).ok()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        tracing::warn!("perf data: unsafe directory {}", dir.display());
        return None;
    }
    // As HotSpot does at startup, sweep only our regular numeric files of
    // dead processes. EPERM means alive; neither symlinks nor other users'
    // files are candidates. SIGKILL deliberately leaves a stale publication.
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(dead) = name.parse::<i32>() else {
            continue;
        };
        if dead <= 0 {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if dead == std::process::id() as i32 {
            if meta.is_file()
                && meta.uid() == uid
                && current_process_birth()
                    .zip(meta.modified().ok())
                    .is_some_and(|(birth, modified)| modified < birth)
            {
                if let Ok(file) = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(entry.path())
                {
                    let same = file
                        .metadata()
                        .is_ok_and(|m| (m.dev(), m.ino()) == (meta.dev(), meta.ino()));
                    // An active CratonVM in this process retains the flock,
                    // including when its timestamp was changed externally.
                    if same
                        && unsafe {
                            libc::flock(
                                std::os::fd::AsRawFd::as_raw_fd(&file),
                                libc::LOCK_EX | libc::LOCK_NB,
                            )
                        } == 0
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            continue;
        }
        if meta.is_file()
            && meta.uid() == uid
            && unsafe { libc::kill(dead, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let path = dir.join(pid.to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let staging = dir.join(format!("{pid}.{nonce}.tmp"));
    let written = (|| -> std::io::Result<((u64, u64), std::fs::File)> {
        // Read as well as write (wave 86): the live counters map it shared.
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staging)?;
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        file.write_all(bytes)?;
        let metadata = file.metadata()?;
        // link publishes complete bytes atomically and, unlike rename, never
        // overwrites a live publication (including another VM in this PID).
        std::fs::hard_link(&staging, &path)?;
        Ok(((metadata.dev(), metadata.ino()), file))
    })();
    let _ = std::fs::remove_file(&staging);
    match written {
        Ok((identity, file)) => Some(PerfDataFile {
            path,
            identity,
            view: std::sync::Mutex::new(UnixView::map(&file)),
            _lease: Some(file),
            memory: std::sync::Mutex::new(None),
            live_offsets: Vec::new(),
            extra: ExtraOffsets::ABSENT,
            sampler: std::sync::Mutex::new(None),
            attached: std::sync::atomic::AtomicBool::new(false),
        }),
        Err(error) => {
            tracing::warn!("perf data: cannot write {}: {error}", path.display());
            None
        }
    }
}

#[cfg(windows)]
#[path = "perf_data_windows.rs"]
mod windows;

#[cfg(windows)]
pub(crate) fn write_perf_data_file(pid: u32, bytes: &[u8]) -> Option<PerfDataFile> {
    let (path, mapping) = windows::publish(pid, bytes)?;
    Some(PerfDataFile {
        path,
        mapping: std::sync::Mutex::new(Some(mapping)),
        memory: std::sync::Mutex::new(None),
        live_offsets: Vec::new(),
        extra: ExtraOffsets::ABSENT,
        sampler: std::sync::Mutex::new(None),
        attached: std::sync::atomic::AtomicBool::new(false),
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn write_perf_data_file(_pid: u32, _bytes: &[u8]) -> Option<PerfDataFile> {
    None
}

/// zt t10/pl2: the largest foreign perf-data image [`attach_foreign`] maps
/// (a sanity bound; HotSpot's default `PerfDataMemorySize` is far below it).
const FOREIGN_MAX_SIZE: usize = 64 << 20;

/// zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 5): map
/// another VM's published perf data read-only for `Perf.attach(<lvmid>)`
/// (HotSpot's `PerfMemory::attach` for a foreign id maps that VM's
/// perf memory, which is how jvmstat's local monitoring, and with it
/// `VirtualMachine.list()`'s names, reads other VMs from inside a Java
/// program). `(base, length)`; `None` for lvmid 0 or a negative one, or when
/// this user has no such VM's data (`<tmp>/hsperfdata_<user>/<pid>`, a
/// regular file, on Unix; the named mapping `hsperfdata_<user>_<pid>` on
/// Windows), or it cannot be mapped. The caller records the view and
/// releases it with [`detach_foreign`] only.
pub(crate) fn attach_foreign(lvmid: i32) -> Option<(usize, usize)> {
    let pid = u32::try_from(lvmid).ok().filter(|&pid| pid != 0)?;
    attach_foreign_pid(pid)
}

#[cfg(unix)]
fn attach_foreign_pid(pid: u32) -> Option<(usize, usize)> {
    let user = effective_user_name()?;
    map_foreign_file(&perf_data_dir(&user).join(pid.to_string()))
}

/// zt t10/pl2: map the perf-data file at `path` read-only, whole: a regular
/// file (not a link to one) of [`PROLOGUE_SIZE`] to [`FOREIGN_MAX_SIZE`]
/// bytes.
///
/// zt t11/pl3 (t11/x8 C2 / P2): checked as the publisher checks its own
/// directory, and on the descriptor rather than the name. The parent
/// directory must be a real directory owned by this effective user and not
/// group- or world-writable; the file is opened with `O_NOFOLLOW` (a link in
/// the last component fails the open), and the OPEN descriptor's `fstat`
/// must be a regular file owned by this effective user, not group- or
/// world-writable. zt t12/x9: and a single link (`st_nlink == 1`, HotSpot's
/// `is_file_secure`).
///
/// zt t14/x11 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 10): the
/// directory is checked through a descriptor too, as HotSpot's
/// `open_directory_secure` does: `open(dir, O_RDONLY|O_DIRECTORY|O_NOFOLLOW)`
/// (a link as the directory fails the open), `fstat` on that descriptor,
/// then the file is opened RELATIVE to it (`openat(dirfd, name,
/// O_RDONLY|O_NOFOLLOW)`), so a rename of the checked directory between the
/// check and the open can no longer hand over another one. Before, the
/// directory was `lstat`ed by name and the file opened by its full path,
/// with a `(dev, ino)` match against the name's `lstat` to catch a file swap.
#[cfg(unix)]
fn map_foreign_file(path: &std::path::Path) -> Option<(usize, usize)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir_name = std::ffi::CString::new(path.parent()?.as_os_str().as_bytes()).ok()?;
    let file_name = std::ffi::CString::new(path.file_name()?.as_bytes()).ok()?;
    // SAFETY: a NUL-terminated path; a negative result is checked.
    let dir_fd = unsafe {
        libc::open(
            dir_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        return None;
    }
    // SAFETY: a descriptor this call just opened and nothing else owns.
    let dir = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(dir_fd) });
    let dir_meta = dir.metadata().ok()?;
    if !foreign_entry_is_safe(dir_meta.is_dir(), dir_meta.uid(), dir_meta.mode(), uid) {
        return None;
    }
    // SAFETY: `dir` is an open directory descriptor and `file_name` is
    // NUL-terminated; a negative result is checked.
    let file_fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            file_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if file_fd < 0 {
        return None;
    }
    // SAFETY: as `dir`.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(file_fd) });
    let opened = file.metadata().ok()?;
    if !foreign_entry_is_safe(opened.is_file(), opened.uid(), opened.mode(), uid)
        || opened.nlink() > 1
    {
        return None;
    }
    let len = usize::try_from(opened.len()).ok()?;
    if !(PROLOGUE_SIZE..=FOREIGN_MAX_SIZE).contains(&len) {
        return None;
    }
    // SAFETY: a fresh read-only mapping of an open descriptor of `len`
    // bytes; the result is checked against MAP_FAILED. The descriptor may
    // close after the call: the mapping keeps the file.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            std::os::fd::AsRawFd::as_raw_fd(&file),
            0,
        )
    };
    if base == libc::MAP_FAILED || base.is_null() {
        return None;
    }
    Some((base as usize, len)) // Cast: the mapping's address
}

/// zt t11/pl3 (t11/x8 P2): a foreign attach's directory or file is safe to
/// read: of the expected kind (`kind_ok`), owned by `euid`, and with no
/// group or other write bit (the publisher's own directory rule). Pure.
#[cfg(unix)]
fn foreign_entry_is_safe(kind_ok: bool, owner: u32, mode: u32, euid: u32) -> bool {
    kind_ok && owner == euid && mode & 0o022 == 0
}

#[cfg(windows)]
fn attach_foreign_pid(pid: u32) -> Option<(usize, usize)> {
    windows::attach_foreign(pid, PROLOGUE_SIZE, FOREIGN_MAX_SIZE)
}

#[cfg(not(any(unix, windows)))]
fn attach_foreign_pid(_pid: u32) -> Option<(usize, usize)> {
    None
}

/// zt pl4 item 5 (`CRATONVM_PERF_ATTACH_HOTSPOT_LOOKUP`, default ON, `=0`
/// kill: this user's directory only, as before): `Perf.attach0(<lvmid>)` of
/// another VM as HotSpot 25's `mmap_attach_shared` (`perfMemory_posix.cpp`)
/// does it, step for step:
///
/// 1. `kill(pid, 0)`: no such process is `IllegalArgumentException:
///    Process not found`, any other failure (`EPERM`: another user's
///    process, for a non-root caller) an `IOException` with the C library's
///    text;
/// 2. the owner: every `hsperfdata*` directory under the temporary directory
///    that is a secure directory (`is_statbuf_secure`: a real directory, no
///    group or other write bit, owned by this effective user unless it is
///    root) is searched for a regular, non-empty file named after the id; the
///    newest (`st_ctime`, strictly later) names the user. On Linux the id's
///    PID-namespace pid (`/proc/<pid>/status` `NSpid:`) is looked up under
///    `/proc/<pid>/root/tmp` (a container), and a miss is retried there with
///    the id itself (a container without its own PID namespace). No user is
///    `IllegalArgumentException: Could not map vmid to user Name`;
/// 3. that user's directory, insecure: `Process not found`; the file opened
///    with `O_NOFOLLOW`: `ENOENT` is `Process not found`, `EACCES`
///    `Permission denied` (both `IllegalArgumentException`), anything else
///    an `IOException` with the C library's text; a file with more than one
///    link is no view at all (`Ok(None)`: a zero-capacity buffer);
/// 4. its size: zero or not a page multiple is `IOException: Invalid
///    PerfMemory size`; a failed map `OutOfMemoryError: Could not map
///    PerfMemory`.
#[cfg(unix)]
pub(crate) fn attach_foreign_hotspot(
    lvmid: i32,
) -> Result<Option<(usize, usize)>, cratonvm_native_api::ForeignPerfAttachFailure> {
    use cratonvm_native_api::ForeignPerfAttachFailure as F;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let vmid = lvmid;
    let not_found = || F::IllegalArgument("Process not found".to_string());
    let errno_text = |err: &std::io::Error| {
        // SAFETY: `strerror` returns a static NUL-terminated string.
        let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(err.raw_os_error().unwrap_or(0))) };
        text.to_string_lossy().into_owned()
    };
    let mut nspid = foreign_namespace_pid(vmid);
    let user = match foreign_owner_name(vmid, nspid)? {
        Some(user) => Some(user),
        None if cfg!(target_os = "linux") => {
            let user = foreign_owner_name(vmid, vmid)?;
            if user.is_some() {
                nspid = vmid;
            }
            user
        }
        None => None,
    };
    let Some(user) = user else {
        return Err(F::IllegalArgument("Could not map vmid to user Name".to_string()));
    };
    let dir = foreign_tmp_dir(vmid, nspid).join(format!("hsperfdata_{user}"));
    if !foreign_dir_is_secure(&dir) {
        return Err(not_found());
    }
    let file_name = dir.join((if nspid == -1 { vmid } else { nspid }).to_string());
    let c_name = std::ffi::CString::new(file_name.as_os_str().as_bytes()).map_err(|_| not_found())?;
    // SAFETY: a NUL-terminated path; a negative result is checked.
    let fd = unsafe { libc::open(c_name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::ENOENT) => not_found(),
            Some(libc::EACCES) => F::IllegalArgument("Permission denied".to_string()),
            _ => F::Io(errno_text(&err)),
        });
    }
    // SAFETY: a descriptor this call just opened and nothing else owns.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // `is_file_secure`: a failed `fstat` or a second link is no view.
    match file.metadata() {
        Ok(meta) if meta.nlink() <= 1 => {}
        _ => return Ok(None),
    }
    let size = file
        .metadata()
        .map_err(|_| F::Io("Could not determine PerfMemory size".to_string()))?
        .len();
    // SAFETY: `sysconf` has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096).max(1);
    if size == 0 || size % page != 0 {
        return Err(F::Io("Invalid PerfMemory size".to_string()));
    }
    let len = usize::try_from(size).map_err(|_| F::Io("Invalid PerfMemory size".to_string()))?;
    // SAFETY: a fresh read-only mapping of an open descriptor of `len`
    // bytes; the result is checked against MAP_FAILED. The descriptor may
    // close after the call: the mapping keeps the file.
    let base = unsafe {
        libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
    };
    if base == libc::MAP_FAILED || base.is_null() {
        return Err(F::OutOfMemory("Could not map PerfMemory".to_string()));
    }
    Ok(Some((base as usize, len))) // Cast: the mapping's address
}

/// zt pl4 item 5: HotSpot's temporary directory for a foreign VM `vmid`
/// whose PID-namespace pid is `nspid` (`get_user_tmp_dir`): `/proc/<vmid>/root`
/// before it on Linux for a containerised process (`nspid != -1`).
#[cfg(unix)]
fn foreign_tmp_dir(vmid: i32, nspid: i32) -> std::path::PathBuf {
    let tmp = perf_data_dir("").parent().map_or_else(|| "/tmp".into(), std::path::Path::to_path_buf);
    if cfg!(target_os = "linux") && nspid != -1 {
        let relative = tmp.strip_prefix("/").unwrap_or(&tmp).to_path_buf();
        std::path::PathBuf::from(format!("/proc/{vmid}/root")).join(relative)
    } else {
        tmp
    }
}

/// zt pl4 item 5: HotSpot's `os::Linux::get_namespace_pid`: the second pid
/// of `/proc/<vmid>/status`'s `NSpid:` line (the process's pid in its own
/// PID namespace), or -1 (one pid, no line, no file; not Linux).
#[cfg(unix)]
fn foreign_namespace_pid(vmid: i32) -> i32 {
    if !cfg!(target_os = "linux") {
        return -1;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{vmid}/status")) else {
        return -1;
    };
    namespace_pid_from_status(&status)
}

/// zt pl4 item 5: [`foreign_namespace_pid`]'s parse of a status text. Pure.
#[cfg_attr(not(unix), allow(dead_code))]
fn namespace_pid_from_status(status: &str) -> i32 {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("NSpid:") {
            let mut pids = rest.split_whitespace().map(|p| p.parse::<i32>());
            return match (pids.next(), pids.next()) {
                (Some(Ok(_)), Some(Ok(nspid))) => nspid,
                _ => -1,
            };
        }
    }
    -1
}

/// zt pl4 item 5: HotSpot's `get_user_name_slow(vmid, nspid)`: the user of
/// the newest non-empty regular file named after the id (`nspid`, or `vmid`
/// when -1) in any secure `hsperfdata*` directory of the temporary directory
/// ([`foreign_tmp_dir`]); `Ok(None)` when none (or the directory cannot be
/// read). The process check comes first.
#[cfg(unix)]
fn foreign_owner_name(
    vmid: i32,
    nspid: i32,
) -> Result<Option<String>, cratonvm_native_api::ForeignPerfAttachFailure> {
    use cratonvm_native_api::ForeignPerfAttachFailure as F;
    use std::os::unix::fs::MetadataExt;
    // SAFETY: signal 0 sends nothing; the result is checked.
    if unsafe { libc::kill(vmid, 0) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(if err.raw_os_error() == Some(libc::ESRCH) {
            F::IllegalArgument("Process not found".to_string())
        } else {
            // SAFETY: `strerror` returns a static NUL-terminated string.
            let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(err.raw_os_error().unwrap_or(0))) };
            F::Io(text.to_string_lossy().into_owned())
        });
    }
    let search = if nspid == -1 { vmid } else { nspid };
    let tmp = foreign_tmp_dir(vmid, nspid);
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return Ok(None);
    };
    let mut newest: Option<(i64, String)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("hsperfdata") {
            continue;
        }
        let Some((_, user)) = name.split_once('_') else {
            continue;
        };
        let dir = tmp.join(name);
        if !foreign_dir_is_secure(&dir) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name();
            if file_name.to_str().and_then(filename_pid) != Some(search) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(file.path()) else {
                continue;
            };
            if !meta.file_type().is_file() || meta.len() == 0 {
                continue;
            }
            let ctime = meta.ctime();
            if newest.as_ref().is_none_or(|(oldest, _)| ctime > *oldest) {
                newest = Some((ctime, user.to_string()));
            }
        }
    }
    Ok(newest.map(|(_, user)| user))
}

/// zt pl4 item 5: HotSpot's `filename_to_pid`: a name of decimal digits
/// only (a leading digit, no leftover) as a pid. Pure.
#[cfg_attr(not(unix), allow(dead_code))]
fn filename_pid(name: &str) -> Option<i32> {
    if !name.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    name.parse::<i32>().ok()
}

/// zt pl4 item 5: HotSpot's `is_directory_secure` (`lstat`, then
/// `is_statbuf_secure`): a real directory (not a link) with no group or
/// other write bit, owned by this effective user unless that user is root.
#[cfg(unix)]
fn foreign_dir_is_secure(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` has no preconditions.
    let euid = unsafe { libc::geteuid() };
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.file_type().is_dir()
            && meta.mode() & 0o022 == 0
            && (euid == 0 || meta.uid() == euid)
    })
}

/// zt pl4 item 5: [`attach_foreign_hotspot`]'s switch.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn attach_hotspot_lookup_enabled() -> bool {
    true
}

/// zt t14/pl5 (`CRATONVM_PERF_ATTACH_MISSING_THROWS`, default ON, `=0` kill;
/// `zgc-zt12pl4-*` item 5, rows `pl4_fa_missing` / `_missingC`: HotSpot
/// threw `IllegalArgumentException`, CratonVM answered a zero-capacity
/// buffer): the message of the `IllegalArgumentException` HotSpot 25's
/// `PerfMemory::attach` throws when it finds no perf data for `lvmid`, for a
/// foreign attach [`attach_foreign`] mapped nothing for; `None` when the data
/// is there (a failure past the lookup keeps the zero-capacity answer). Its
/// lookup (`get_user_name_slow`) finds the id's file only in a secure
/// `hsperfdata_<user>` directory, as a regular file (`stat`) of non-zero
/// size, and throws `Could not map vmid to user Name` (POSIX) / `Could not
/// map vmid to user name` (Windows) otherwise. This looks in this user's
/// directory only; HotSpot also finds another user's file, and then fails
/// the open with `Permission denied` (still an `IllegalArgumentException`).
pub(crate) fn foreign_attach_missing_message(lvmid: i32) -> Option<&'static str> {
    if foreign_data_present(lvmid) {
        None
    } else {
        Some(FOREIGN_MISSING_MESSAGE)
    }
}

#[cfg(windows)]
const FOREIGN_MISSING_MESSAGE: &str = "Could not map vmid to user name";
#[cfg(not(windows))]
const FOREIGN_MISSING_MESSAGE: &str = "Could not map vmid to user Name";

/// zt t14/pl5: does this user have perf data for `lvmid` (a positive id), as
/// HotSpot's lookup finds it ([`foreign_attach_missing_message`])?
fn foreign_data_present(lvmid: i32) -> bool {
    let Some(pid) = u32::try_from(lvmid).ok().filter(|&pid| pid != 0) else {
        return false;
    };
    foreign_data_present_pid(pid)
}

#[cfg(unix)]
fn foreign_data_present_pid(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(user) = effective_user_name() else {
        return false;
    };
    let dir = perf_data_dir(&user);
    // SAFETY: `geteuid` has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir_safe = std::fs::symlink_metadata(&dir)
        .is_ok_and(|d| foreign_entry_is_safe(d.is_dir(), d.uid(), d.mode(), uid));
    let file = std::fs::metadata(dir.join(pid.to_string()));
    dir_safe && file.is_ok_and(|f| f.is_file() && f.len() > 0)
}

#[cfg(windows)]
fn foreign_data_present_pid(pid: u32) -> bool {
    windows::foreign_data_present(pid)
}

#[cfg(not(any(unix, windows)))]
fn foreign_data_present_pid(_pid: u32) -> bool {
    false
}

/// zt t11/pl3 (t11/x8 C4 / P4): `Perf.detach(address)`'s decision. This
/// VM's own view (`own`, whose base `Perf.attach(<own pid>)` also hands to
/// the JDK's cleaner) is never released; a recorded foreign view at
/// `address` is taken out of `views` (so it is released once); any other
/// address (a zero-capacity buffer, an unknown one) takes nothing.
pub(crate) fn take_foreign_view(
    views: &mut Vec<(usize, usize)>,
    own: Option<usize>,
    address: usize,
) -> Option<(usize, usize)> {
    if own == Some(address) {
        return None;
    }
    let at = views.iter().position(|&(base, _)| base == address)?;
    Some(views.swap_remove(at))
}

/// zt t10/pl2: release a view [`attach_foreign`] returned as `(base, len)`.
/// The caller removes it from its record first, so each view is released
/// once.
/// zt pl4 item 6: unmap every recorded foreign view (`views`, emptied) when
/// the VM that recorded them is torn down (`Drop for SharedVm`), unless
/// `CRATONVM_PERF_FOREIGN_VIEWS_AT_TEARDOWN=0` (they then live until the
/// process ends, as before). Returns how many it unmapped.
pub(crate) fn release_foreign_views_at_teardown(views: &mut Vec<(usize, usize)>) -> usize {
    if views.is_empty()
    {
        return 0;
    }
    let taken = std::mem::take(views);
    for &(base, len) in &taken {
        detach_foreign(base, len);
    }
    taken.len()
}

pub(crate) fn detach_foreign(base: usize, len: usize) {
    #[cfg(unix)]
    {
        // SAFETY: `base` / `len` are a live mapping `attach_foreign` made,
        // which the caller just took out of its record (unmapped once).
        unsafe {
            libc::munmap(base as *mut libc::c_void, len);
        }
    }
    #[cfg(windows)]
    {
        let _ = len;
        windows::detach_foreign(base);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (base, len);
}

/// zt pl4 items 5 and 6: the attach lookup's pure parts and the teardown
/// release.
#[cfg(test)]
mod zt_pl4_attach_tests;

#[cfg(test)]
mod tests;
