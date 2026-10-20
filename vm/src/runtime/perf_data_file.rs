// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot's jvmstat perf-data file, `/tmp/hsperfdata_<user>/<pid>`
//! (`VmConfig::perf_data_file`, `CRATONVM_PERF_DATA_FILE`, default on;
//! interpreter round i3 wave 81, lane L1;
//! `docs/internal/fixed-bugs/interpreter-i59-external-disabled-attach-discovery-FIXED-20261011.md`).
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

/// The `sun.os.hrt.frequency` this VM's tick counters use: nanoseconds
/// (HotSpot's on Linux; its Windows value is the performance counter's
/// frequency, which `jstat` only divides by).
pub(crate) const HRT_FREQUENCY: i64 = 1_000_000_000;

/// The counters `jstat -gc` and `jstat -class` read (`jstat_options`), kept
/// live in the file (interpreter round i4 wave 86, lane L1): name, units,
/// variability, the `supported` flag. Indices are the `LIVE_*` constants;
/// [`sample_live_counters`] fills them in this order.
pub(crate) const LIVE_COUNTERS: [(&str, u8, u8, bool); 28] = [
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
];
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

/// `sun.os.hrt.frequency` and the [`LIVE_COUNTERS`] at zero, in that order.
pub(crate) fn live_entries() -> Vec<PerfEntry> {
    let mut entries = vec![PerfEntry {
        name: "sun.os.hrt.frequency".to_string(),
        value: PerfValue::Long(HRT_FREQUENCY),
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
}

/// The [`LIVE_COUNTERS`] values for `ticks` nanoseconds since the VM began
/// and the two halves of a sample: the collectors' times in ticks
/// ([`HRT_FREQUENCY`]), the young generation as eden plus the two survivor
/// spaces, the old generation, metaspace as the class metadata (gci i33/ms:
/// capacity the committed figure and the compressed class space its own,
/// `vm_init::metaspace_stats`; `capacity == used` and the class space 0
/// under `CRATONVM_METASPACE_HOTSPOT_SPACES=0`), the
/// classes loaded as the cumulative total (`java.cls.loadedClasses` counts
/// every class ever loaded) and their bytes as the class metadata, the
/// shared (CDS) counters 0, and `sun.cls.time` 0 (no class-load timer).
pub(crate) fn live_values(ticks: i64, heap: &HeapCounters, classes: &ClassCounters) -> [i64; 28] {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let ms_ticks = |ms: u64| n(ms).saturating_mul(HRT_FREQUENCY / 1000);
    let mut v = [0i64; 28];
    v[LIVE_TICKS] = ticks;
    for (i, &(count, time_ms)) in heap.collectors.iter().enumerate() {
        v[LIVE_COLLECTOR + 2 * i] = n(count);
        v[LIVE_COLLECTOR + 2 * i + 1] = ms_ticks(time_ms);
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
    v[LIVE_LOADED_BYTES] = n(classes.metadata_bytes);
    v
}

/// How often the sampler thread refreshes the live counters: HotSpot's
/// `PerfDataSamplingInterval` default (50 ms) is for its tick counter alone;
/// the GC and class counters it updates at the events themselves. One
/// refresh every 100 ms keeps `jstat <pid> <interval>` (whose interval is at
/// least 1 ms, usually 1000) current to a tenth of a second.
const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

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
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => {
            for (slot, c) in s.collectors.iter_mut().zip(h.jmx_collectors().iter()) {
                *slot = (c.count, c.time_ms);
            }
            for p in h.jmx_memory_pools().iter() {
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
        heap => {
            if let Some(beans) = shared.mem.backend_gc_beans() {
                for (slot, c) in s.collectors.iter_mut().zip(beans.collectors().iter()) {
                    *slot = (c.count, c.time_ms);
                }
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
    let started = std::time::Instant::now();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = std::sync::Arc::clone(&stop);
    let stopped = move || stopping.load(std::sync::atomic::Ordering::Acquire);
    let spawned = std::thread::Builder::new()
        .name("cratonvm-perf-data".to_string())
        .spawn(move || {
            let mut heap = HeapCounters::default();
            let mut metadata = crate::vm::vm_init::MetaspaceStats::default();
            let mut metadata_at: Option<(std::time::Instant, u64)> = None;
            loop {
                // Parked, not slept: `stop_sampler` unparks it (wave 87).
                std::thread::park_timeout(SAMPLE_INTERVAL);
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
                    // space), the walk `heap_summary` reports from.
                    metadata =
                        crate::vm::vm_init::metaspace_stats(&shared.classes.class_manager.read());
                    metadata_at = Some((std::time::Instant::now(), collections));
                }
                let unloaded = shared
                    .debug
                    .diagnostic_counters
                    .classes_unloaded
                    .load(std::sync::atomic::Ordering::Relaxed);
                let live_classes = shared.classes.class_manager.read().loaded_count() as u64; // Widening: usize -> u64
                let classes = ClassCounters {
                    loaded_total: live_classes + unloaded,
                    unloaded,
                    metadata_bytes: metadata.used,
                    metadata_committed: metadata.committed,
                    class_space: metadata
                        .class_space
                        .map_or((0, 0), |(used, committed, _)| (committed, used)),
                };
                let ticks = i64::try_from(started.elapsed().as_nanos()).unwrap_or(i64::MAX);
                let values = live_values(ticks, &heap, &classes);
                let guard = shared.debug.perf_data_file.lock();
                let Some(file) = guard.as_ref() else {
                    return;
                };
                file.write_live(&values);
            }
        });
    let Ok(thread) = spawned else {
        return;
    };
    if let Some(file) = shared.debug.perf_data_file.lock().as_ref() {
        *file.sampler.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(SamplerHandle { stop, thread });
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
    let Some(SamplerHandle { stop, thread }) = handle else {
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
    while !thread.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if thread.is_finished() {
        let _ = thread.join();
    }
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
    #[cfg(unix)]
    _lease: std::fs::File,
    /// The file mapped shared and writable, through which the live counters
    /// are updated in place (interpreter round i4 wave 86, lane L1); `None`
    /// when it could not be mapped, or once removed.
    #[cfg(unix)]
    view: std::sync::Mutex<Option<UnixView>>,
    #[cfg(windows)]
    mapping: std::sync::Mutex<Option<windows::Mapping>>,
    /// The absolute offsets of the [`LIVE_COUNTERS`]' `long` data in the file,
    /// in that order (`usize::MAX` for one left out); empty for a file
    /// written without them.
    live_offsets: Vec<usize>,
    /// The live-counter sampler of this VM's file ([`start_sampler`]), for
    /// [`stop_sampler`] to stop and join at the VM's teardown (interpreter
    /// round i4 wave 87, lane L1).
    sampler: std::sync::Mutex<Option<SamplerHandle>>,
}

/// A running sampler: its stop flag (per VM, shared with the thread only)
/// and its thread.
#[derive(Debug)]
struct SamplerHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
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
        let Some(base) = base else {
            return;
        };
        for (&offset, &value) in self.live_offsets.iter().zip(values) {
            if offset == usize::MAX || offset % 8 != 0 || offset + 8 > PERF_DATA_SIZE {
                continue;
            }
            // SAFETY: `base` maps PERF_DATA_SIZE bytes for as long as the
            // guard above is held; `offset + 8` lies inside it and is 8-byte
            // aligned (the mapping is page-aligned), so this is one aligned
            // 64-bit store, as HotSpot's `PerfLongVariant` writes its value.
            unsafe {
                (*((base + offset) as *const std::sync::atomic::AtomicI64))
                    .store(value, std::sync::atomic::Ordering::Relaxed);
            }
        }
        #[cfg(any(unix, windows))]
        drop(guard);
    }

    /// Unlink the file now (idempotent).
    pub(crate) fn remove(&self) {
        #[cfg(windows)]
        drop(
            self.mapping
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        #[cfg(unix)]
        drop(self.view.lock().unwrap_or_else(|e| e.into_inner()).take());
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
pub(crate) fn create_for_vm(config: &crate::config::VmConfig) -> Option<PerfDataFile> {
    let given = |key: &str| {
        config
            .system_properties
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let identity = crate::vm::vm_init::vm_identity_properties(config.compatibility_mode);
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
    // `jstat -gc` / `-class` read, kept current by `start_sampler`.
    let first_live = entries.len() + 1;
    entries.extend(live_entries());
    let (bytes, offsets) = encode_perf_data_with_offsets(&entries, 1);
    let mut file = write_perf_data_file(std::process::id(), &bytes)?;
    file.live_offsets = offsets.get(first_live..).map(<[usize]>::to_vec).unwrap_or_default();
    Some(file)
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
            _lease: file,
            live_offsets: Vec::new(),
            sampler: std::sync::Mutex::new(None),
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
        live_offsets: Vec::new(),
        sampler: std::sync::Mutex::new(None),
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn write_perf_data_file(_pid: u32, _bytes: &[u8]) -> Option<PerfDataFile> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn publication_owns_its_inode_and_refuses_unsafe_paths() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = std::env::temp_dir().join(format!(
            "cvm-perf-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let dir = root.join("safe");
        let pid = std::process::id();
        let owned = write_perf_data_in_dir(&dir, pid, b"complete").unwrap();
        assert_eq!(std::fs::read(&owned.path).unwrap(), b"complete");
        assert_eq!(
            std::fs::metadata(&owned.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_perf_data_in_dir(&dir, pid, b"other VM").is_none());
        // Replacement is a different lease: old teardown cannot remove it.
        std::fs::rename(&owned.path, dir.join("old")).unwrap();
        std::fs::write(&owned.path, b"replacement").unwrap();
        owned.remove();
        assert_eq!(std::fs::read(&owned.path).unwrap(), b"replacement");
        let unsafe_dir = root.join("writable");
        std::fs::create_dir(&unsafe_dir).unwrap();
        std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(write_perf_data_in_dir(&unsafe_dir, pid, b"unsafe").is_none());
        let alias = root.join("alias");
        symlink(&dir, &alias).unwrap();
        assert!(write_perf_data_in_dir(&alias, pid, b"unsafe").is_none());
        std::fs::remove_file(&owned.path).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, b"preserve").unwrap();
        symlink(&victim, &owned.path).unwrap();
        assert!(write_perf_data_in_dir(&dir, pid, b"unsafe").is_none());
        assert_eq!(std::fs::read(&victim).unwrap(), b"preserve");
        std::fs::remove_file(&owned.path).unwrap();
        let final_lease = write_perf_data_in_dir(&dir, pid, b"final").unwrap();
        final_lease.remove();
        assert!(!final_lease.path.exists());
        #[cfg(target_os = "linux")]
        {
            std::fs::write(&final_lease.path, b"stale reused PID").unwrap();
            let old = std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH);
            std::fs::File::open(&final_lease.path)
                .unwrap()
                .set_times(old)
                .unwrap();
            let replacement = write_perf_data_in_dir(&dir, pid, b"reused PID").unwrap();
            assert_eq!(std::fs::read(&replacement.path).unwrap(), b"reused PID");
            // An old timestamp alone never steals a live embedded VM's lease.
            std::fs::File::open(&replacement.path)
                .unwrap()
                .set_times(old)
                .unwrap();
            assert!(write_perf_data_in_dir(&dir, pid, b"steal").is_none());
            replacement.remove();
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Wave 84 (lane L1): a string constant longer than
    /// `PerfMaxStringConstLength` is cut at 1024 bytes and still ends in a
    /// NUL; `java.rt.vmArgs` is the launcher's option list when it gave one.
    #[test]
    fn long_string_constants_are_cut_as_hotspots_and_vm_args_are_the_options() {
        let long = "x".repeat(3000);
        let entry = PerfEntry::string_constant("java.rt.vmArgs", &long, true);
        assert_eq!(
            entry.value,
            PerfValue::Str {
                text: long.clone(),
                capacity: PERF_MAX_STRING_CONST_LENGTH + 1
            }
        );
        let bytes = encode_entry(&entry);
        let data_at = i32::from_ne_bytes(bytes[16..20].try_into().unwrap()) as usize;
        assert_eq!(bytes[data_at + 1023], b'x');
        assert_eq!(bytes[data_at + 1024], 0);

        let mut config = crate::config::VmConfig::default();
        config.system_properties = vec![("x".to_string(), "y".to_string())];
        assert_eq!(vm_args_text(&config), "-Dx=y");
        config.jvm_args = vec!["-Xmx64m".to_string(), "-Dx=y".to_string()];
        assert_eq!(vm_args_text(&config), "-Xmx64m -Dx=y");
    }

    /// Wave 86 (lane L1): the live counters lay out as `long` entries whose
    /// recorded data offsets are 8-aligned and hold their values, and a
    /// sample maps onto the names `jstat -gc` / `-class` read.
    #[test]
    fn live_counters_have_aligned_offsets_and_jstat_names() {
        let entries = live_entries();
        assert_eq!(entries.len(), LIVE_COUNTERS.len() + 1);
        assert_eq!(entries[0].name, "sun.os.hrt.frequency");
        let (file, offsets) = encode_perf_data_with_offsets(&entries, 1);
        let long_at = |at: usize| i64::from_ne_bytes(file[at..at + 8].try_into().unwrap());
        assert_eq!(long_at(offsets[0]), HRT_FREQUENCY);
        assert!(offsets.iter().all(|&at| at % 8 == 0 && at + 8 <= PERF_DATA_SIZE));
        let heap = HeapCounters {
            collectors: [(3, 20), (1, 7), (0, 0)],
            eden: (1000, 400),
            s0: (100, 10),
            s1: (100, 0),
            old: (5000, 2500),
        };
        let classes = ClassCounters {
            loaded_total: 700,
            unloaded: 2,
            metadata_bytes: 4096,
            metadata_committed: 65536,
            class_space: (65536, 1024),
        };
        let values = live_values(9, &heap, &classes);
        let value = |name: &str| {
            let i = LIVE_COUNTERS.iter().position(|c| c.0 == name).unwrap();
            values[i]
        };
        assert_eq!(value("sun.os.hrt.ticks"), 9);
        assert_eq!(value("sun.gc.collector.0.invocations"), 3);
        assert_eq!(value("sun.gc.collector.0.time"), 20 * HRT_FREQUENCY / 1000);
        assert_eq!(value("sun.gc.collector.1.invocations"), 1);
        assert_eq!(value("sun.gc.generation.0.space.0.capacity"), 1000);
        assert_eq!(value("sun.gc.generation.0.space.0.used"), 400);
        assert_eq!(value("sun.gc.generation.0.space.1.used"), 10);
        assert_eq!(value("sun.gc.generation.0.space.2.capacity"), 100);
        assert_eq!(value("sun.gc.generation.1.space.0.used"), 2500);
        assert_eq!(value("sun.gc.metaspace.used"), 4096);
        // gci i33/ms: capacity is the committed figure; the class space its own.
        assert_eq!(value("sun.gc.metaspace.capacity"), 65536);
        assert_eq!(value("sun.gc.compressedclassspace.capacity"), 65536);
        assert_eq!(value("sun.gc.compressedclassspace.used"), 1024);
        assert_eq!(value("java.cls.loadedClasses"), 700);
        assert_eq!(value("java.cls.unloadedClasses"), 2);
        assert_eq!(value("sun.cls.loadedBytes"), 4096);
        assert_eq!(value("sun.cls.time"), 0);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The prologue HotSpot 25.0.4 wrote (MEASURED, the host oracle of
    /// 2026-10-09, default flags: used 12184, time stamp 21849324, 185
    /// entries).
    #[cfg(target_endian = "little")]
    #[test]
    fn the_prologue_matches_hotspots_bytes() {
        assert_eq!(
            hex(&encode_prologue(12184, 21_849_324, 185)),
            "cafec0c001020001982f000000000000ec644d010000000020000000b9000000"
        );
    }

    /// Entry #0 (`sun.rt.createVmBeginTime`, a `long`) and entry #12
    /// (`sun.rt.jvmCapabilities`, default and `-XX:+DisableAttachMechanism`)
    /// byte for byte as HotSpot 25.0.4 wrote them (MEASURED).
    #[cfg(target_endian = "little")]
    #[test]
    fn entries_match_hotspots_bytes() {
        let begin = vm_entries(1_791_557_291_013, true, "", &[], "")[0].clone();
        assert_eq!(
            hex(&encode_entry(&begin)),
            "3800000014000000000000004a0001033000000073756e2e72742e637265617465566d426567696e54696d650000000005cc2221a1010000"
        );
        let caps = |attachable: bool| vm_entries(0, attachable, "", &[], "")[1].clone();
        let head = "700000001400000041000000420005012b00000073756e2e72742e6a766d4361706162696c697469657300";
        let tail = format!("{}0000000000", "30".repeat(62));
        let default = format!("{head}3131{tail}");
        assert_eq!(default.len(), 224);
        assert_eq!(hex(&encode_entry(&caps(true))), default);
        assert_eq!(
            hex(&encode_entry(&caps(false))),
            format!("{head}3031{tail}")
        );
    }

    /// `java.property.java.vm.name` lays out as HotSpot's entry #171 did
    /// (MEASURED: length 72, vector 25, flags 1, units 5, variability 1,
    /// data offset 47), and the file is a walkable v2.0 buffer whose entries
    /// end at the first zero length.
    #[test]
    fn a_string_property_and_the_whole_file_lay_out_as_hotspots() {
        let entry = PerfEntry::string_constant(
            "java.property.java.vm.name",
            "OpenJDK 64-Bit Server VM",
            true,
        );
        let bytes = encode_entry(&entry);
        let int_of = |at: usize| i32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(bytes.len(), 72);
        assert_eq!(
            (int_of(0), int_of(4), int_of(8), int_of(16)),
            (72, 20, 25, 47)
        );
        assert_eq!(&bytes[12..16], &[b'B', 1, 5, 1]);
        assert_eq!(&bytes[47..47 + 24], b"OpenJDK 64-Bit Server VM");
        assert_eq!(bytes[47 + 24], 0);

        let entries = vm_entries(
            1,
            false,
            "Main a b",
            &[
                ("java.vm.name", "cratonvm"),
                ("java.vm.version", "25.0.1+8"),
            ],
            "-Dx=y",
        );
        let file = encode_perf_data(&entries, 7);
        assert_eq!(file.len(), PERF_DATA_SIZE);
        assert_eq!(&file[0..4], &[0xca, 0xfe, 0xc0, 0xc0]);
        let int_at = |at: usize| i32::from_ne_bytes(file[at..at + 4].try_into().unwrap());
        assert_eq!(int_at(24), 32);
        assert_eq!(int_at(28), entries.len() as i32);
        // Walk it as `PerfDataBuffer.getNextMonitorEntry` does.
        let mut at = 32usize;
        let mut names = Vec::new();
        while at < file.len() && int_at(at) != 0 {
            let length = int_at(at) as usize;
            assert_eq!(at % 4, 0);
            assert_eq!(length % 8, 0);
            let name_at = at + int_at(at + 4) as usize;
            let end = file[name_at..].iter().position(|b| *b == 0).unwrap();
            names.push(String::from_utf8(file[name_at..name_at + end].to_vec()).unwrap());
            at += length;
        }
        assert_eq!(int_at(8) as usize, at, "used");
        assert_eq!(
            names,
            [
                "sun.rt.createVmBeginTime",
                "sun.rt.jvmCapabilities",
                "sun.rt.javaCommand",
                "java.property.java.vm.name",
                "java.property.java.vm.version",
                "java.rt.vmArgs",
                "java.rt.vmFlags",
            ]
        );
    }
}
