// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The rest of the VM-side JFR events (SBR5 lane `jfr`, round 5 follow-up):
//! GC cycles in the detail HotSpot writes them, safepoints, compilations,
//! deoptimizations, class unloading, the periodic configuration and statistics
//! events, and the method sampler behind `jdk.ExecutionSample` /
//! `jdk.NativeMethodSample`.
//!
//! Every event here is a fact the VM already holds (the GC site's heap numbers
//! and phase marks, the JIT's compile times, the module registry, the thread
//! registry, the JIT cache). An event type whose data the VM does not have is
//! *not* written; the per-type list with the reason is on
//! `docs/internal/jdk-only/SBR5-JFR-cost-and-uncovered-events-UNFIXABLE-20261006.md`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::{ObjectRef, Value};

use crate::jfr_backend::{
    class_by_name_locked, intern_stack, method_by_name_locked, now_ticks, register_thread, Backend,
    BACKEND,
};
use crate::jfr_chunk::{s, ModuleRec, PackageRec, ThreadRec, Val};
use crate::jfr_vm_events::{
    admit, any_recording, append, ensure_vm_thread, enum_key, fields, proc_file,
};

// ---------------------------------------------------------------------------
// shared counters (kept whether or not a recording runs)
// ---------------------------------------------------------------------------

static COMPILE_COUNT: AtomicI64 = AtomicI64::new(0);
static OSR_COUNT: AtomicI64 = AtomicI64::new(0);
static BAILOUT_COUNT: AtomicI64 = AtomicI64::new(0);
static OSR_BYTES: AtomicI64 = AtomicI64::new(0);
static STD_BYTES: AtomicI64 = AtomicI64::new(0);
static TOTAL_COMPILE_NANOS: AtomicI64 = AtomicI64::new(0);
static PEAK_COMPILE_NANOS: AtomicI64 = AtomicI64::new(0);

/// `enum_key`, naming the pool's own single field from the metadata.
fn enum_key_auto(b: &mut Backend, pool: &str, text: &str) -> i64 {
    let field = b
        .meta
        .as_ref()
        .and_then(|m| m.by_name(pool))
        .and_then(|t| t.fields.first())
        .map(|f| f.name.clone())
        .unwrap_or_else(|| "name".to_owned());
    enum_key(b, pool, &field, text)
}

fn named_thread(b: &mut Backend, id: i64, name: &str) -> i64 {
    b.tables.threads.entry(id).or_insert(ThreadRec {
        os_name: name.to_owned(),
        os_tid: 0,
        java_name: String::new(),
        java_tid: 0,
        group: 0,
        is_virtual: false,
    });
    b.needs.threads.insert(id);
    id
}

// ---------------------------------------------------------------------------
// GC cycles
// ---------------------------------------------------------------------------

/// One finished collection, as the GC site knows it.
pub struct GcCycle<'a> {
    pub gc_id: i32,
    pub name: &'a str,
    pub cause: &'a str,
    pub start_nanos: u64,
    pub duration_nanos: u64,
    pub heap_before: i64,
    pub committed_before: i64,
    pub heap_after: i64,
    pub committed_after: i64,
    pub max: i64,
    pub old: bool,
    /// gci i12/ge (`gci-i11go-...-review-findings-20261013` item 3): the
    /// collection is recorded as `jdk.OldGarbageCollection` (else
    /// `jdk.YoungGarbageCollection`). The caller decides it from the door and
    /// the backend (HotSpot's full-collection operations); it was chosen here
    /// by `cause.contains("System.gc")`, so a `jcmd GC.run`, heap-inspection or
    /// heap-dump full collection was recorded as a young one.
    pub old_event: bool,
    pub tenuring_threshold: Option<i32>,
    /// the collector's phase marks: (name, start nanos, duration nanos)
    pub phases: &'a [(&'a str, u64, u64)],
    /// class-metadata bytes in use (both spaces)
    pub metaspace_used: i64,
    /// gci i33/ms: HotSpot's other `jdk.MetaspaceSummary` figures,
    /// `[committed, reserved, class used, class committed, class reserved]`
    /// (the VM's `vm_init::metaspace_stats`); `None` = the no-class-space
    /// shape (`committed == reserved == used`, `classSpace` all 0).
    pub metaspace_spaces: Option<[i64; 5]>,
    /// gci i11/go: a G1 pause's `jdk.G1GarbageCollection.type`
    /// (`jdk.types.G1YCType`: `Normal`, `Concurrent Start`, `Prepare Mixed`,
    /// `Mixed`); `None` off G1 (no such event).
    pub g1_type: Option<&'a str>,
    /// gci i11/go: the `jdk.ExecuteVMOperation.operation` HotSpot 25 names
    /// for this collection (`G1CollectForAllocation`, `G1CollectFull`,
    /// `SerialCollectForAllocation`, ...); `None` keeps the historical
    /// `Collect` / `GC_HeapInspection` spelling.
    pub vm_operation: Option<&'a str>,
    /// gci i12/ge (`gci-i12ge-...-review-findings-20261013` item 2): the
    /// longest single pause of a collection made of several pauses (a G1
    /// full collection reported as one, whose `duration_nanos` is the pauses'
    /// sum); `None` = one pause (`longestPause` = the duration).
    pub longest_pause_nanos: Option<u64>,
    /// zr z4/jf: the ZGC-only facts of this collection; `None` on every other
    /// collector and under `CRATONVM_ZGC_JFR_EVENTS=0` (the pre-z4 shape).
    pub zgc: Option<&'a ZgcCycleJfr<'a>>,
    /// zs s7/jf: the collection's stop-the-world time as named pauses
    /// `(name, start nanos, duration nanos)`, contiguous over the collection
    /// (ZGC: HotSpot's `Pause Mark Start (Major)` / `Pause Mark End` /
    /// `Pause Relocate Start`). Empty: one `GC Pause` row over the whole
    /// duration, as before. `sumOfPauses` / `longestPause` follow the rows.
    pub pauses: &'a [(&'a str, u64, u64)],
    /// zs s8/jf2: the thread the collection's own events are committed on
    /// (ZGC: [`ZGC_DRIVER_MAJOR`] / [`ZGC_DRIVER_MINOR`]); `None` = `VM Thread`
    /// (every other collector, and `CRATONVM_ZGC_JFR_DRIVER_THREADS=0`).
    pub gc_thread: Option<&'static str>,
    /// gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1):
    /// the Java thread that asked for this collection -- HotSpot's
    /// `VM_Operation::calling_thread()`, written as
    /// `jdk.ExecuteVMOperation.caller` (`main` for its `System.gc()`, the
    /// allocating thread for an allocation pause). `None` (a VM-internal or
    /// concurrent-cycle pause, or a caller the site did not name) keeps the
    /// historical `caller` 0.
    pub caller: Option<JfrCallerThread<'a>>,
}

/// zs s8/jf2: HotSpot 25 ZGC's collector threads, as JFR names them
/// (`eventThread` of its `GarbageCollection`, `GCPhasePause`,
/// `GCPhaseConcurrent`, `ZYoung/ZOldGarbageCollection`, `ZRelocationSet*`).
pub const ZGC_DRIVER_MAJOR: &str = "ZDriverMajor";
pub const ZGC_DRIVER_MINOR: &str = "ZDriverMinor";

/// zs s8/jf2: the pool entry of a collector thread `name` (a non-Java thread
/// of its own, next to the uncommitter's), or `vm_thread` when `None`.
fn gc_event_thread(b: &mut Backend, name: Option<&'static str>, vm_thread: i64) -> i64 {
    match name {
        Some(n) if n == ZGC_DRIVER_MINOR => named_thread(b, ZGC_DRIVER_MINOR_THREAD_ID, n),
        Some(n) => named_thread(b, ZGC_DRIVER_MAJOR_THREAD_ID, n),
        None => vm_thread,
    }
}

/// zs s7/jf: `(sumOfPauses, longestPause)` of a collection written as
/// `pauses` rows (empty: the whole `duration` is one pause).
fn pause_totals(pauses: &[(&str, u64, u64)], duration: i64) -> (i64, i64) {
    if pauses.is_empty() {
        return (duration, duration);
    }
    let sum = pauses.iter().map(|p| p.2).fold(0u64, u64::saturating_add);
    let longest = pauses.iter().map(|p| p.2).max().unwrap_or(0);
    let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    (i(sum), i(longest))
}

/// gci i12/ge: [`gc_cycle`]'s `jdk.GarbageCollection.longestPause`, in the
/// collection's own time unit (nanoseconds): the caller's longest pause,
/// capped at the duration, else the duration. Pure.
fn gc_longest_pause(c: &GcCycle<'_>) -> i64 {
    let duration = c.duration_nanos as i64;
    match c.longest_pause_nanos {
        Some(n) => i64::try_from(n).unwrap_or(i64::MAX).min(duration),
        None => duration,
    }
}

/// gci i11/go: the operation name of [`gc_cycle`]'s `jdk.ExecuteVMOperation`
/// row: the caller's, else the historical spelling. Pure.
///
/// zt t2/jf (residual R6): a ZGC collection written as ONE safepoint (no
/// named pause rows: `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES=0`, or a split that
/// did not fit) is named by the operation that opens its cycle on HotSpot 25
/// (`ZMarkStartYoungAndOld` for a major, `ZMarkStartYoung` for a minor), not
/// `Collect`, which HotSpot's ZGC never runs. Under
/// `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS=0`: `Collect`, as before.
fn gc_vm_operation<'a>(c: &GcCycle<'a>) -> &'a str {
    if let (None, Some(z)) = (c.vm_operation, c.zgc) {
        if z.pause_vm_operations {
            return if z.major {
                "ZMarkStartYoungAndOld"
            } else {
                "ZMarkStartYoung"
            };
        }
    }
    c.vm_operation
        .unwrap_or(if c.old { "GC_HeapInspection" } else { "Collect" })
}

/// HotSpot 25's default `-XX:MetaspaceSize` on 64-bit (`21 * M`), which
/// `MetaspaceGC::post_initialize` makes the initial `capacity_until_GC`.
const HOTSPOT_METASPACE_SIZE: i64 = 21 * 1024 * 1024;

/// HotSpot's metaspace commit granule (`Metaspace::commit_alignment()`, 64 KiB
/// on 64-bit), the unit `capacity_until_GC` moves in.
const HOTSPOT_METASPACE_COMMIT_ALIGNMENT: i64 = 64 * 1024;

/// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 2): the
/// `jdk.MetaspaceSummary.gcThreshold` of a collection with `used` metadata
/// bytes -- HotSpot's `MetaspaceGC::capacity_until_GC()` as its default
/// policy leaves it: `MetaspaceSize` (21 MiB) while `used` fits under it at
/// `MinMetaspaceFreeRatio` (40 % free), else `compute_new_size`'s
/// `used / (1 - 0.40)` rounded up to the commit granule. This VM applies
/// neither `-XX:MetaspaceSize` (accepted, not applied) nor HotSpot's
/// allocation-time expansion steps, so a run whose metadata outgrows 12.6 MiB
/// reports the policy's floor, not HotSpot's exact step; below that -- every
/// small program -- it is HotSpot's value. It was always 0. Pure.
///
/// gci i31/hp: this is `compute_new_size`'s floor alone; the rows read
/// [`metaspace_threshold_step`], which applies it after a full collection.
fn metaspace_gc_threshold(used: i64) -> i64 {
    let used = used.max(0);
    // `used / (1 - MinMetaspaceFreeRatio / 100)`, in integers: `used * 100 / 60`.
    let min_desired = used.saturating_mul(100).saturating_add(59) / 60;
    let a = HOTSPOT_METASPACE_COMMIT_ALIGNMENT;
    let aligned = min_desired.saturating_add(a - 1) / a * a;
    aligned.max(HOTSPOT_METASPACE_SIZE)
}

/// gci i31/hp (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 2, vi20
/// Batch 3 pk `S-threshold`): one collection's `(Before GC, After GC)`
/// `gcThreshold` and the `capacity_until_GC` it leaves, from the value the
/// previous collection left (`cap`, 0 = none yet: `MetaspaceSize`).
///
/// HotSpot keeps `capacity_until_GC` as state, it does not recompute it per
/// row: it starts at `MetaspaceSize`, moves up at allocation time only when
/// committed metadata would pass it (`MetaspaceGC::inc_capacity_until_GC`;
/// modelled as the `used` figure rounded up to the commit granule -- HotSpot
/// steps by `MinMetaspaceExpansion` / `MaxMetaspaceExpansion`), and is
/// re-derived by `compute_new_size` (`used / 0.6`, never down here: the
/// shrink factor of HotSpot's first shrink-qualified collection is 0) only
/// after a full collection. The Before-GC row was `used / 0.6` on every
/// collection: a program with 12.6..21 MiB of metadata before its first full
/// collection read above HotSpot's `MetaspaceSize` (pk23: 23658496 vs
/// 22020096). Pure.
fn metaspace_threshold_step(cap: i64, used: i64, full: bool) -> (i64, i64) {
    let a = HOTSPOT_METASPACE_COMMIT_ALIGNMENT;
    let used = used.max(0);
    let cap = if cap <= 0 { HOTSPOT_METASPACE_SIZE } else { cap };
    let before = if used > cap {
        used.saturating_add(a - 1) / a * a
    } else {
        cap
    };
    let after = if full {
        before.max(metaspace_gc_threshold(used))
    } else {
        before
    };
    (before, after)
}

/// gci i33/ms (`gci-i33ms-metaspace-spaces-and-limit-FIXED-20261016`):
/// `jdk.MetaspaceSummary`'s three `(committed, used, reserved)` structs --
/// `metaspace` (both spaces), `dataSpace` (the non-class space) and
/// `classSpace` (the compressed class space), as HotSpot's
/// `MetaspaceUtils::get_combined_statistics` fills them -- from the total used
/// and [`GcCycle::metaspace_spaces`]. Without the spaces: the pre-i33 rows
/// (`used` three times, the class space 0). Pure.
fn metaspace_summary_spaces(used: i64, spaces: Option<[i64; 5]>) -> [(i64, i64, i64); 3] {
    match spaces {
        Some([committed, reserved, class_used, class_committed, class_reserved]) => [
            (committed, used, reserved),
            (
                committed.saturating_sub(class_committed),
                used.saturating_sub(class_used),
                reserved.saturating_sub(class_reserved),
            ),
            (class_committed, class_used, class_reserved),
        ],
        None => [(used, used, used), (used, used, used), (0, 0, 0)],
    }
}

/// gci i12/ge: the young / old event [`gc_cycle`] files a collection under,
/// from the caller's [`GcCycle::old_event`] alone. Pure.
fn gc_kind_event(c: &GcCycle<'_>) -> &'static str {
    if c.old_event {
        "jdk.OldGarbageCollection"
    } else {
        "jdk.YoungGarbageCollection"
    }
}

/// zr z4/jf: what a ZGC collection adds to [`gc_cycle`]'s events. HotSpot ZGC
/// writes `jdk.ZOldGarbageCollection` (not the Serial/Parallel/G1
/// `jdk.OldGarbageCollection` / `jdk.YoungGarbageCollection`), and the
/// relocation set and uncommit events of the cycle.
pub struct ZgcCycleJfr<'a> {
    /// The collection was a whole-heap (`ZGC Major`) collection.
    pub major: bool,
    /// `(total, empty, relocate)` bytes of the relocation-set selection.
    pub relocation_set: Option<(i64, i64, i64)>,
    /// One `(page type, candidatePages, total, empty, selectedPages,
    /// relocate)` row per page size class.
    pub relocation_groups: &'a [(&'a str, i64, i64, i64, i64, i64)],
    /// `(bytes, nanos)` returned to the OS inside the pause.
    pub uncommit: Option<(i64, i64)>,
    /// zs s7/jf: the tenuring threshold of the collection's
    /// `jdk.ZYoungGarbageCollection`; `None` writes none (and under
    /// `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS=0`). A young (`ZGC Minor`)
    /// collection of the generational mode carries its promotion age; zt
    /// t1/jf: a whole-heap (`ZGC Major`) collection carries the threshold of
    /// the young collection HotSpot 25's major runs inside it
    /// (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`, the VM's
    /// `zgc_young_tenuring_threshold`).
    pub young_tenuring_threshold: Option<i32>,
    /// zs s7/jf: `jdk.GCReferenceStatistics` counts (soft, weak, final,
    /// phantom); `None` when the reference pass did not state them.
    pub reference_counts: Option<[i64; 4]>,
    /// zs s10/jf3 (ported by zt t1/jf): a concurrent cycle's span `(start
    /// nanos, duration nanos)` from its mark-start pause to this
    /// collection's end, for the cycle's own events (`GarbageCollection`,
    /// `ZOld/ZYoungGarbageCollection`): HotSpot's span the whole cycle.
    /// `None`: the collection's span. The heap summaries, the relocation set
    /// and the collection-wide safepoint rows keep the collection's times.
    pub cycle_span: Option<(u64, u64)>,
    /// zt t2/jf (residual R2): with [`Self::cycle_span`], the heap's `(used,
    /// committed)` bytes at the cycle's mark-start pause: the `Before GC`
    /// `jdk.GCHeapSummary` is written at the span's start with them, as
    /// HotSpot traces the heap before the cycle. `None`: the collection's
    /// own before figures and start.
    pub cycle_heap_before: Option<(i64, i64)>,
    /// zs s10/jf3: write HotSpot 25's all-zero
    /// `jdk.MetaspaceChunkFreeListSummary` rows
    /// (`CRATONVM_ZGC_JFR_METASPACE_CHUNK_FREE_LISTS`).
    pub metaspace_chunk_free_lists: bool,
    /// zt t1/jf: the stop-the-world time is written as HotSpot 25 ZGC's VM
    /// operations, one safepoint (`SafepointBegin`, `ExecuteVMOperation`,
    /// `SafepointEnd`) per named pause row, named by
    /// [`zgc_pause_vm_operation`] and requested by the collector thread
    /// (`CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS`). Only with named pause rows;
    /// otherwise the one collection-wide safepoint as before.
    pub pause_vm_operations: bool,
}

/// zt t1/jf: HotSpot 25 ZGC's VM operation (`jdk.types.VMOperationType`)
/// for one of its pauses (`ZStatPhasePause` name), as generational ZGC runs
/// them (`VM_ZMarkStartYoungAndOld`, `VM_ZMarkStartYoung`,
/// `VM_ZMarkEndYoung` / `VM_ZMarkEndOld`, `VM_ZRelocateStartYoung` /
/// `VM_ZRelocateStartOld`). A whole-heap collection's mark end and relocate
/// start are written as its old generation's (this collector's one
/// whole-heap pause ends both generations' marks). Pure.
pub fn zgc_pause_vm_operation(pause: &str, major: bool) -> &'static str {
    match pause {
        "Pause Mark Start (Major)" => "ZMarkStartYoungAndOld",
        "Pause Mark Start" => "ZMarkStartYoung",
        "Pause Mark End" if major => "ZMarkEndOld",
        "Pause Mark End" => "ZMarkEndYoung",
        _ if major => "ZRelocateStartOld",
        _ => "ZRelocateStartYoung",
    }
}

/// zt t1/jf: one stop-the-world VM operation's safepoint rows on the real
/// backend -- `SafepointBegin` at `start`, `ExecuteVMOperation` (`op`,
/// requested by the pool thread `caller`, 0 = none) over `duration`,
/// `SafepointEnd` at its end -- under a fresh safepoint id. `t` is the VM
/// thread. Times are backend ticks.
fn safepoint_operation_rows(
    b: &mut Backend,
    t: i64,
    caller: i64,
    op: &str,
    start: i64,
    duration: i64,
) {
    b.safepoint_id += 1;
    let sp = b.safepoint_id;
    if admit(b, "jdk.SafepointBegin", 0).is_some() {
        append(
            b,
            "jdk.SafepointBegin",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(0)),
                ("eventThread", Val::Ref(t)),
                ("safepointId", Val::Long(sp)),
            ]),
        );
    }
    if admit(b, "jdk.ExecuteVMOperation", duration).is_some() {
        let op = enum_key_auto(b, "jdk.types.VMOperationType", op);
        append(
            b,
            "jdk.ExecuteVMOperation",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("operation", Val::Ref(op)),
                ("safepoint", Val::Bool(true)),
                ("blocking", Val::Bool(true)),
                ("caller", Val::Ref(caller)),
                ("safepointId", Val::Long(sp)),
            ]),
        );
    }
    if admit(b, "jdk.SafepointEnd", 0).is_some() {
        append(
            b,
            "jdk.SafepointEnd",
            &fields(vec![
                ("startTime", Val::Long(start.saturating_add(duration))),
                ("duration", Val::Long(0)),
                ("eventThread", Val::Ref(t)),
                ("safepointId", Val::Long(sp)),
            ]),
        );
    }
}

/// zs s7/jf: HotSpot's `ReferenceType` texts, in `jdk.GCReferenceStatistics`
/// order.
pub const REFERENCE_TYPE_NAMES: [&str; 4] = [
    "Soft reference",
    "Weak reference",
    "Final reference",
    "Phantom reference",
];

fn virtual_space(committed: i64, max: i64) -> Val {
    Val::Struct(fields(vec![
        ("start", Val::Long(0)),
        ("committedEnd", Val::Long(committed)),
        ("committedSize", Val::Long(committed)),
        ("reservedEnd", Val::Long(max)),
        ("reservedSize", Val::Long(max)),
    ]))
}

/// Write the events of one collection: `GarbageCollection`, `GCPhasePause` (+
/// `GCPhasePauseLevel1` per phase mark), `Young`/`OldGarbageCollection`,
/// `GCHeapSummary` before and after (gci i12/ge: `SystemGC` is [`system_gc`]'s,
/// once per request), and the stop-the-world as
/// `SafepointBegin`/`SafepointEnd`/`ExecuteVMOperation`.
pub fn gc_cycle(vm: usize, c: &GcCycle<'_>) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = c.start_nanos as i64 - offset;
    let duration = c.duration_nanos as i64;
    let end = start + duration;
    // zt t1/jf: a ZGC collection with named pauses writes one safepoint per
    // pause (HotSpot 25 ZGC's VM operations) instead of one for the whole
    // collection. Every other collector: `false`, the rows below unchanged.
    let per_pause_safepoints = !c.pauses.is_empty() && c.zgc.is_some_and(|z| z.pause_vm_operations);
    BACKEND.with(vm, |b| {
        // zt t5/jf: the explicit request this collection serves ends here.
        if c.zgc.is_some() {
            b.last_zgc_collection_end = Some(end);
        }
        let t = ensure_vm_thread(b);
        // zt t1/jf: per-pause safepoints take their own ids
        // (`safepoint_operation_rows`), so none is spent on the collection.
        let sp = if per_pause_safepoints {
            0
        } else {
            b.safepoint_id += 1;
            b.safepoint_id
        };
        if !per_pause_safepoints && admit(b, "jdk.SafepointBegin", 0).is_some() {
            append(
                b,
                "jdk.SafepointBegin",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(0)),
                    ("eventThread", Val::Ref(t)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
        if !per_pause_safepoints && admit(b, "jdk.ExecuteVMOperation", duration).is_some() {
            let op = enum_key_auto(b, "jdk.types.VMOperationType", gc_vm_operation(c));
            // gci i20/ob: HotSpot's `post_vm_operation_event` sets `caller`
            // to the requesting thread's JFR id; 0 when the site named none.
            let caller = c
                .caller
                .as_ref()
                .and_then(|who| caller_thread_ref(b, who))
                .unwrap_or(0);
            append(
                b,
                "jdk.ExecuteVMOperation",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(duration)),
                    ("eventThread", Val::Ref(t)),
                    ("operation", Val::Ref(op)),
                    ("safepoint", Val::Bool(true)),
                    ("blocking", Val::Bool(true)),
                    ("caller", Val::Ref(caller)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
        // zs s8/jf2: the collection's own events on the collector's thread
        // (HotSpot ZGC: `ZDriverMajor` / `ZDriverMinor`); the safepoint rows
        // stay on `VM Thread`, as HotSpot's do.
        let g = gc_event_thread(b, c.gc_thread, t);
        // zs s10/jf3: a ZGC concurrent cycle's own events span the cycle.
        let (cstart, cdur) = match c.zgc.and_then(|z| z.cycle_span) {
            Some((s, d)) => (s as i64 - offset, i64::try_from(d).unwrap_or(i64::MAX)),
            None => (start, duration),
        };
        if admit(b, "jdk.GarbageCollection", cdur).is_some() {
            let name_key = enum_key_auto(b, "jdk.types.GCName", c.name);
            let cause_key = enum_key_auto(b, "jdk.types.GCCause", c.cause);
            // zs s7/jf: named pause rows (ZGC) decide both; otherwise one
            // pause over the duration, gci i12/ge's longest single pause.
            let (sum, longest) = if c.pauses.is_empty() {
                (duration, gc_longest_pause(c))
            } else {
                pause_totals(c.pauses, duration)
            };
            append(
                b,
                "jdk.GarbageCollection",
                &fields(vec![
                    ("startTime", Val::Long(cstart)),
                    ("duration", Val::Long(cdur)),
                    ("eventThread", Val::Ref(g)),
                    ("gcId", Val::Int(c.gc_id)),
                    ("name", Val::Ref(name_key)),
                    ("cause", Val::Ref(cause_key)),
                    ("sumOfPauses", Val::Long(sum)),
                    ("longestPause", Val::Long(longest)),
                ]),
            );
        }
        // zs s7/jf: named pauses (ZGC: HotSpot's pause names) in place of the
        // one `GC Pause` row.
        // zt t1/jf: HotSpot ZGC's VM operations are requested by its driver
        // thread (`VM_Operation::calling_thread()`), not by the Java thread
        // that asked for the collection.
        let pause_caller = if per_pause_safepoints && c.gc_thread.is_some() {
            g
        } else if per_pause_safepoints {
            c.caller
                .as_ref()
                .and_then(|who| caller_thread_ref(b, who))
                .unwrap_or(0)
        } else {
            0
        };
        let major = c.zgc.is_some_and(|z| z.major);
        for &(name, p_start, p_dur) in c.pauses {
            let p_start = p_start as i64 - offset;
            let p_dur = i64::try_from(p_dur).unwrap_or(i64::MAX);
            if per_pause_safepoints {
                let op = zgc_pause_vm_operation(name, major);
                safepoint_operation_rows(b, t, pause_caller, op, p_start, p_dur);
            }
            if admit(b, "jdk.GCPhasePause", p_dur).is_some() {
                append(
                    b,
                    "jdk.GCPhasePause",
                    &fields(vec![
                        ("startTime", Val::Long(p_start)),
                        ("duration", Val::Long(p_dur)),
                        ("eventThread", Val::Ref(g)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("name", s(name)),
                    ]),
                );
            }
        }
        if c.pauses.is_empty() && admit(b, "jdk.GCPhasePause", duration).is_some() {
            append(
                b,
                "jdk.GCPhasePause",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(duration)),
                    ("eventThread", Val::Ref(g)),
                    ("gcId", Val::Int(c.gc_id)),
                    ("name", s("GC Pause")),
                ]),
            );
        }
        for (name, p_start, p_dur) in c.phases {
            let p_start = *p_start as i64 - offset;
            let p_dur = *p_dur as i64;
            if admit(b, "jdk.GCPhasePauseLevel1", p_dur).is_some() {
                append(
                    b,
                    "jdk.GCPhasePauseLevel1",
                    &fields(vec![
                        ("startTime", Val::Long(p_start)),
                        ("duration", Val::Long(p_dur)),
                        ("eventThread", Val::Ref(g)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("name", s(name)),
                    ]),
                );
            }
        }
        if let Some(z) = c.zgc {
            // zt t3/jf (zt2x1 item 4): HotSpot's major runs its young
            // collection first, so that event ends before the old one; here
            // a major's young event ends where its mark-end pause starts (the
            // whole-heap mark's end, the earliest instant the young half is
            // marked). Without a named `Pause Mark End` row: the cycle's span.
            // zt t4/x3: `CRATONVM_ZGC_JFR_MAJOR_YOUNG_AT_MARK_END=0` keeps
            // the cycle's span (the t1 shape); no host row has run either.
            let young = if zgc_jfr_major_young_at_mark_end_enabled() {
                zgc_young_span(c.pauses, offset, (cstart, cdur), z.major)
            } else {
                (cstart, cdur)
            };
            zgc_cycle_events(b, g, c, z, (start, duration), (cstart, cdur), young);
        }
        // gci i12/ge: the kind is the caller's (`GcCycle::old_event`); the
        // `tenuringThreshold` field belongs to the young event only (a G1
        // `System.gc()` pause wrote it onto its old row).
        // zr z4/jf: a ZGC collection writes its own events instead (above).
        // (dev decided the kind by `c.old || cause.contains("System.gc")`; the
        // gci rule, the caller's `old_event`, is kept at the 2026-10-15 merge.)
        let kind_event = gc_kind_event(c);
        if c.zgc.is_none() && admit(b, kind_event, duration).is_some() {
            let mut v = vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("gcId", Val::Int(c.gc_id)),
            ];
            if let (false, Some(th)) = (c.old_event, c.tenuring_threshold) {
                v.push(("tenuringThreshold", Val::Int(th)));
            }
            append(b, kind_event, &fields(v));
        }
        // gci i11/go: HotSpot G1 also records `jdk.G1GarbageCollection` (the
        // pause type) for every young / mixed pause.
        if let Some(g1_type) = c.g1_type {
            if admit(b, "jdk.G1GarbageCollection", duration).is_some() {
                let type_key = enum_key_auto(b, "jdk.types.G1YCType", g1_type);
                append(
                    b,
                    "jdk.G1GarbageCollection",
                    &fields(vec![
                        ("startTime", Val::Long(start)),
                        ("duration", Val::Long(duration)),
                        ("eventThread", Val::Ref(t)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("type", Val::Ref(type_key)),
                    ]),
                );
            }
        }
        if admit(b, "jdk.GCHeapSummary", 0).is_some() {
            // zt t2/jf (R2): a ZGC concurrent cycle's `Before GC` is the heap
            // at its mark-start pause, at the cycle's start.
            let (before_at, before_used, before_committed) =
                match c.zgc.and_then(|z| z.cycle_span.and(z.cycle_heap_before)) {
                    Some((used, committed)) => (cstart, used, committed),
                    None => (start, c.heap_before, c.committed_before),
                };
            for (when, at, used, committed) in [
                ("Before GC", before_at, before_used, before_committed),
                ("After GC", end, c.heap_after, c.committed_after),
            ] {
                let key = enum_key_auto(b, "jdk.types.GCWhen", when);
                append(
                    b,
                    "jdk.GCHeapSummary",
                    &fields(vec![
                        ("startTime", Val::Long(at)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("when", Val::Ref(key)),
                        ("heapSpace", virtual_space(committed, c.max)),
                        ("heapUsed", Val::Long(used)),
                    ]),
                );
            }
        }
        // gci i31/hp: `capacity_until_GC` is per-VM state, moved on every
        // recorded collection whether or not the summary event is enabled.
        // gci i33/ms: HotSpot's `MetaspaceGC` steps `capacity_until_GC` on
        // COMMITTED metaspace (`compute_new_size`'s `used_after_gc` is
        // `MetaspaceUtils::committed_bytes()`; an allocation raises it when
        // the commit passes it), so the step reads the committed figure when
        // the VM states one (else the used figure, as before).
        let (threshold_before, threshold_after) = metaspace_threshold_step(
            b.metaspace_capacity_until_gc,
            c.metaspace_spaces.map_or(c.metaspace_used, |s| s[0]),
            c.old_event,
        );
        b.metaspace_capacity_until_gc = threshold_after;
        if admit(b, "jdk.MetaspaceSummary", 0).is_some() {
            // gci i33/ms: `(committed, used, reserved)` of both spaces, the
            // non-class space (`dataSpace`) and the class space.
            let [metaspace, data_space, class_space] = metaspace_summary_spaces(
                c.metaspace_used,
                c.metaspace_spaces,
            );
            let sizes = |(committed, used, reserved): (i64, i64, i64)| {
                Val::Struct(fields(vec![
                    ("committed", Val::Long(committed)),
                    ("used", Val::Long(used)),
                    ("reserved", Val::Long(reserved)),
                ]))
            };
            for (when, at, threshold) in [
                ("Before GC", start, threshold_before),
                ("After GC", end, threshold_after),
            ] {
                let key = enum_key_auto(b, "jdk.types.GCWhen", when);
                append(
                    b,
                    "jdk.MetaspaceSummary",
                    &fields(vec![
                        ("startTime", Val::Long(at)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("when", Val::Ref(key)),
                        ("gcThreshold", Val::Long(threshold)),
                        ("metaspace", sizes(metaspace)),
                        ("dataSpace", sizes(data_space)),
                        ("classSpace", sizes(class_space)),
                    ]),
                );
            }
        }
        // zs s10/jf3: HotSpot 25 writes `jdk.MetaspaceChunkFreeListSummary`
        // per collection, Before and After GC, for `Metadata` and `Class`,
        // every count and size 0 (its elastic metaspace has kept no chunk
        // free lists since JDK 16; this VM has none either). ZGC only here;
        // the other collectors are out of this lane's scope (filed).
        if c.zgc.is_some_and(|z| z.metaspace_chunk_free_lists)
            && admit(b, "jdk.MetaspaceChunkFreeListSummary", 0).is_some()
        {
            for (when, at) in [("Before GC", start), ("After GC", end)] {
                for kind in ["Metadata", "Class"] {
                    let when_key = enum_key_auto(b, "jdk.types.GCWhen", when);
                    let kind_key = enum_key_auto(b, "jdk.types.MetadataType", kind);
                    append(
                        b,
                        "jdk.MetaspaceChunkFreeListSummary",
                        &fields(vec![
                            ("startTime", Val::Long(at)),
                            ("gcId", Val::Int(c.gc_id)),
                            ("when", Val::Ref(when_key)),
                            ("metadataType", Val::Ref(kind_key)),
                            ("specializedChunks", Val::Long(0)),
                            ("specializedChunksTotalSize", Val::Long(0)),
                            ("smallChunks", Val::Long(0)),
                            ("smallChunksTotalSize", Val::Long(0)),
                            ("mediumChunks", Val::Long(0)),
                            ("mediumChunksTotalSize", Val::Long(0)),
                            ("humongousChunks", Val::Long(0)),
                            ("humongousChunksTotalSize", Val::Long(0)),
                        ]),
                    );
                }
            }
        }
        // gci i12/ge (`gci-i12ge-...-review-findings-20261013` item 1):
        // `jdk.SystemGC` is no longer written here, once per `System.gc()`
        // PAUSE (two per G1 `System.gc()`, and for VM-internal collections
        // too), but once per request by [`system_gc`].
        if !per_pause_safepoints && admit(b, "jdk.SafepointEnd", 0).is_some() {
            append(
                b,
                "jdk.SafepointEnd",
                &fields(vec![
                    ("startTime", Val::Long(end)),
                    ("duration", Val::Long(0)),
                    ("eventThread", Val::Ref(t)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
    });
}

/// gci i12/ge: one explicit GC request's `jdk.SystemGC` (HotSpot's `JVM_GC`:
/// the event spans the collection; `invokedConcurrent` is
/// `-XX:+ExplicitGCInvokesConcurrent`). Called once per non-ignored
/// `System.gc()` / `Runtime.gc()` after its collection, outside every pause.
/// `eventThread` is the backend's VM thread; [`system_gc_by`] names the
/// calling Java thread instead, as HotSpot does (gci i19/ob).
pub fn system_gc(vm: usize, invoked_concurrent: bool, start_nanos: u64, duration_nanos: u64) {
    system_gc_by(vm, None, invoked_concurrent, start_nanos, duration_nanos, 0);
}

/// gci i19/ob (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` residual
/// r1): the Java thread that asked for a collection, as the VM's thread
/// registry knows it -- `Thread.tid` (the JFR thread id this backend keys
/// every Java thread by, see `jfr_backend::jfr_thread_id`), its name and its
/// OS thread id (0 = unknown). Built by the caller from the registry, so no
/// `NativeContext` is needed at the collection site.
#[derive(Debug, Clone, Copy)]
pub struct JfrCallerThread<'a> {
    pub java_tid: i64,
    pub name: &'a str,
    pub os_tid: i64,
}

/// The JFR thread id of `caller`, recording it in the thread pool on first
/// sight (an earlier `register_thread` of the same Java thread made the same
/// key, so its record is kept) and marking it used by this chunk. A `java_tid`
/// below 1 is not a Java thread id: `None`, and the caller falls back to the
/// VM thread.
fn caller_thread_ref(b: &mut Backend, caller: &JfrCallerThread<'_>) -> Option<i64> {
    let tid = caller.java_tid;
    if tid < 1 {
        return None;
    }
    b.tables.threads.entry(tid).or_insert_with(|| ThreadRec {
        os_name: caller.name.to_owned(),
        os_tid: caller.os_tid,
        java_name: caller.name.to_owned(),
        java_tid: tid,
        group: 0,
        is_virtual: false,
    });
    b.needs.threads.insert(tid);
    Some(tid)
}

/// [`system_gc`] with HotSpot's `eventThread`: `JVM_GC` commits
/// `EventSystemGC` on the calling Java thread. `caller == None` (or an
/// unknown tid) keeps the backend's VM thread. `stack_trace`: the request's
/// stack-trace pool id, taken where its frames were walkable
/// ([`system_gc_stack_trace`], gci i23/pk); `0` writes none (a VM-internal
/// request, or no recording wanted one when the request began).
pub fn system_gc_by(
    vm: usize,
    caller: Option<JfrCallerThread<'_>>,
    invoked_concurrent: bool,
    start_nanos: u64,
    duration_nanos: u64,
    stack_trace: i64,
) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = i64::try_from(duration_nanos).unwrap_or(i64::MAX);
    BACKEND.with(vm, |b| {
        // zt t5/jf: on ZGC, end with the request's collection.
        let duration = zgc_system_gc_duration(b, start, duration);
        if admit(b, "jdk.SystemGC", duration).is_none() {
            return;
        }
        let t = match caller.as_ref().and_then(|c| caller_thread_ref(b, c)) {
            Some(t) => t,
            None => ensure_vm_thread(b),
        };
        append(
            b,
            "jdk.SystemGC",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack_trace)),
                ("invokedConcurrent", Val::Bool(invoked_concurrent)),
            ]),
        );
    });
}

/// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 3): the
/// stack-trace pool id of an explicit GC request's `jdk.SystemGC` row, taken
/// by the requesting native (`NativeContextImpl::force_gc`) before the
/// collection, while its frames are walkable; `0` when no backend recording
/// has `jdk.SystemGC` enabled with stack traces (HotSpot's `default.jfc` and
/// `profile.jfc`: `stackTrace=true`). HotSpot's `JVM_GC` records the stack
/// from the `Runtime.gc()` native frame down; the capture lists the Java
/// frames below the running native, so that frame is put on top when the
/// capture lacks it ([`system_gc_stack_frames`]).
pub fn system_gc_stack_trace(ctx: &mut dyn NativeContext) -> i64 {
    if !any_recording() {
        return 0;
    }
    // zt t1/jf: on ZGC with `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER` on, the
    // door writes no row (`force_gc_as_system_gc` writes it, walking the
    // stack itself), so the walk and the interned stack here would be
    // unused. `None` on every other collector: unchanged there.
    if ctx.jfr_system_gc_on_caller().is_some() {
        return 0;
    }
    let vm = ctx.vm_identity();
    // `i64::MAX`: enabled and stack-wanted, whatever the threshold (the
    // duration is not known yet; `system_gc_by` applies it).
    if BACKEND.with(vm, |b| admit(b, "jdk.SystemGC", i64::MAX)) != Some(true) {
        return 0;
    }
    let frames = system_gc_stack_frames(ctx.capture_stack_trace());
    intern_stack(ctx, &frames, 0, 0)
}

/// [`system_gc_stack_trace`]'s frames, innermost first: `captured` (as
/// `capture_stack_trace` lists it, outermost first) reversed, under a
/// `java.lang.Runtime.gc()` native frame unless its innermost frame is
/// already that one. Pure.
fn system_gc_stack_frames(
    mut captured: Vec<cratonvm_native_api::StackTraceEntry>,
) -> Vec<cratonvm_native_api::StackTraceEntry> {
    captured.reverse();
    let has_native_top = captured.first().is_some_and(|f| {
        f.method_name.as_ref() == "gc"
            && matches!(f.class_name.as_ref(), "java.lang.Runtime" | "java/lang/Runtime")
    });
    if !has_native_top {
        captured.insert(
            0,
            cratonvm_native_api::StackTraceEntry {
                class_name: std::sync::Arc::from("java.lang.Runtime"),
                method_name: std::sync::Arc::from("gc"),
                method_descriptor: Some(std::sync::Arc::from("()V")),
                source_file: Some(std::sync::Arc::from("Runtime.java")),
                line_number: -2,
                byte_code_index: -1,
                class_id: None,
                method_index: None,
            },
        );
    }
    captured
}

/// gci i19/ob (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` residual
/// r3): `jdk.ConcurrentModeFailure` for a G1 concurrent cycle a full
/// collection aborted. HotSpot 25 commits it from
/// `G1ConcurrentMark::concurrent_cycle_end` when `has_aborted()` (beside the
/// `Concurrent Mark Abort` log line), through
/// `GCTracer::report_concurrent_mode_failure`, with the cycle's `gcId`; the
/// JDK 25 `default.jfc` enables the type. The only field is `gcId`
/// (`startTime` / `eventThread` are written as the metadata declares them).
/// Call outside every pause.
pub fn concurrent_mode_failure(vm: usize, gc_id: i32) {
    if !any_recording() {
        return;
    }
    let start = now_ticks();
    BACKEND.with(vm, |b| {
        if admit(b, "jdk.ConcurrentModeFailure", 0).is_none() {
            return;
        }
        let t = ensure_vm_thread(b);
        append(
            b,
            "jdk.ConcurrentModeFailure",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("eventThread", Val::Ref(t)),
                ("gcId", Val::Int(gc_id)),
            ]),
        );
    });
}

/// gci i9/jc: one concurrent-cycle pause row, `jdk.GCPhasePause` (`Pause Init
/// Mark` / G1's `GC Pause`, `Pause Remark`, `Pause Cleanup`), under the
/// cycle's `gcId`. The legacy recorder had these rows alone
/// (`gci-i8jf-jfr-cycle-rows-never-reach-the-jdk-jfr-backend-FIXED-20261017`).
/// Call outside every pause (takes the backend's lock).
pub fn gc_phase_pause(vm: usize, gc_id: i32, name: &str, start_nanos: u64, duration_nanos: u64) {
    gc_phase(vm, "jdk.GCPhasePause", gc_id, name, start_nanos, duration_nanos, None);
}

/// gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
/// one stop-the-world VM operation that is not a collection of its own -- a
/// G1 Remark or Cleanup pause, which HotSpot 25 runs as `VM_G1PauseRemark` /
/// `VM_G1PauseCleanup` -- as [`gc_cycle`] writes a collection's stop:
/// `SafepointBegin`, `ExecuteVMOperation` (`operation`, `caller` 0: HotSpot's
/// requester is the concurrent mark thread, not a Java thread this backend
/// names) and `SafepointEnd`, sharing `gc_cycle`'s safepoint counter. Call
/// outside every pause (takes the backend's lock).
pub fn vm_operation_pause(vm: usize, operation: &str, start_nanos: u64, duration_nanos: u64) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = i64::try_from(duration_nanos).unwrap_or(i64::MAX);
    let end = start.saturating_add(duration);
    BACKEND.with(vm, |b| {
        let t = ensure_vm_thread(b);
        b.safepoint_id += 1;
        let sp = b.safepoint_id;
        if admit(b, "jdk.SafepointBegin", 0).is_some() {
            append(
                b,
                "jdk.SafepointBegin",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(0)),
                    ("eventThread", Val::Ref(t)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
        if admit(b, "jdk.ExecuteVMOperation", duration).is_some() {
            let op = enum_key_auto(b, "jdk.types.VMOperationType", operation);
            append(
                b,
                "jdk.ExecuteVMOperation",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(duration)),
                    ("eventThread", Val::Ref(t)),
                    ("operation", Val::Ref(op)),
                    ("safepoint", Val::Bool(true)),
                    ("blocking", Val::Bool(true)),
                    ("caller", Val::Ref(0)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
        if admit(b, "jdk.SafepointEnd", 0).is_some() {
            append(
                b,
                "jdk.SafepointEnd",
                &fields(vec![
                    ("startTime", Val::Long(end)),
                    ("duration", Val::Long(0)),
                    ("eventThread", Val::Ref(t)),
                    ("safepointId", Val::Long(sp)),
                ]),
            );
        }
    });
}

/// gci i9/jc: one concurrent phase of a cycle, `jdk.GCPhaseConcurrent`
/// (`Concurrent Mark`, G1's `Concurrent Mark From Roots`, `Concurrent
/// Sweep`), under the cycle's `gcId`. HotSpot's `default.jfc` and
/// `profile.jfc` enable the type with a 0 ms threshold. Call outside every
/// pause.
pub fn gc_phase_concurrent(vm: usize, gc_id: i32, name: &str, start_nanos: u64, duration_nanos: u64) {
    gc_phase(vm, "jdk.GCPhaseConcurrent", gc_id, name, start_nanos, duration_nanos, None);
}

/// zt t3/jf (zt2x1 item 2): a ZGC pause written outside its collection's
/// rows (a concurrent cycle's mark-start pause whose cycle never closed) as
/// HotSpot 25 ZGC runs it: one safepoint with `ExecuteVMOperation`
/// `operation`, requested by the collector thread `thread`
/// ([`gc_event_thread`]; `None` = the VM thread), as [`gc_cycle`]'s
/// per-pause rows (`safepoint_operation_rows`). Call outside every pause.
pub fn zgc_vm_operation_pause(
    vm: usize,
    operation: &str,
    start_nanos: u64,
    duration_nanos: u64,
    thread: Option<&'static str>,
) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = i64::try_from(duration_nanos).unwrap_or(i64::MAX);
    BACKEND.with(vm, |b| {
        let t = ensure_vm_thread(b);
        let caller = gc_event_thread(b, thread, t);
        safepoint_operation_rows(b, t, caller, operation, start, duration);
    });
}

/// `CRATONVM_ZGC_JFR_MAJOR_YOUNG_AT_MARK_END` -- default ON, `=0` kill (zt
/// t4/x3: t3/jf changed the major's `jdk.ZYoungGarbageCollection` span by
/// default with no switch of its own; `CRATONVM_ZGC_JFR_MAJOR_YOUNG=0` drops
/// the event, it does not restore its old span). On: [`zgc_young_span`].
/// `=0`: the young event of a major spans the whole cycle, as before.
/// Read per collection.
fn zgc_jfr_major_young_at_mark_end_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_MAJOR_YOUNG_AT_MARK_END")
}

/// zt t3/jf (zt2x1 item 4): the `(start, duration)` ticks of a ZGC
/// collection's `jdk.ZYoungGarbageCollection`: for a major with a named
/// `Pause Mark End` row, from the cycle's start to that row's start (clamped
/// into the cycle); otherwise the cycle's span `(cstart, cdur)`. `offset` is
/// the epoch-to-ticks offset the rows' nanos are converted by. Pure.
fn zgc_young_span(
    pauses: &[(&str, u64, u64)],
    offset: i64,
    (cstart, cdur): (i64, i64),
    major: bool,
) -> (i64, i64) {
    match pauses.iter().find(|p| p.0 == "Pause Mark End") {
        Some(&(_, mark_end, _)) if major => {
            let at = mark_end as i64 - offset;
            (cstart, at.saturating_sub(cstart).clamp(0, cdur.max(0)))
        }
        _ => (cstart, cdur),
    }
}

/// zr z4/jf: the ZGC rows of one collection (see [`ZgcCycleJfr`]). Field
/// names are HotSpot 25's; the JDK's metadata in the chunk decides order and
/// encoding, so a name it does not declare is dropped.
///
/// `jdk.ZYoungGarbageCollection` is written when the VM states its
/// threshold ([`ZgcCycleJfr::young_tenuring_threshold`]). `(start,
/// duration)` is the collection's, `(cstart, cdur)` the cycle's (zs s10/jf3:
/// the same unless a concurrent cycle's span was given), which the two
/// generation events take.
fn zgc_cycle_events(
    b: &mut Backend,
    t: i64,
    c: &GcCycle<'_>,
    z: &ZgcCycleJfr<'_>,
    (start, duration): (i64, i64),
    (cstart, cdur): (i64, i64),
    (ystart, ydur): (i64, i64),
) {
    if z.major && admit(b, "jdk.ZOldGarbageCollection", cdur).is_some() {
        append(
            b,
            "jdk.ZOldGarbageCollection",
            &fields(vec![
                ("startTime", Val::Long(cstart)),
                ("duration", Val::Long(cdur)),
                // zs s8/jf2: HotSpot's carries its collector thread.
                ("eventThread", Val::Ref(t)),
                ("gcId", Val::Int(c.gc_id)),
            ]),
        );
    }
    // zs s7/jf: a young (`ZGC Minor`) collection of the generational mode;
    // zt t1/jf: and the young collection inside a whole-heap one, when the
    // VM states its threshold (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`).
    if let Some(threshold) = z.young_tenuring_threshold {
        if admit(b, "jdk.ZYoungGarbageCollection", ydur).is_some() {
            append(
                b,
                "jdk.ZYoungGarbageCollection",
                &fields(vec![
                    ("startTime", Val::Long(ystart)),
                    ("duration", Val::Long(ydur)),
                    ("eventThread", Val::Ref(t)),
                    ("gcId", Val::Int(c.gc_id)),
                    ("tenuringThreshold", Val::Int(threshold)),
                ]),
            );
        }
    }
    // zs s7/jf: the reference pass's counts, one row per type, at the end of
    // the collection (HotSpot commits them after its reference processing).
    if let Some(counts) = z.reference_counts {
        if admit(b, "jdk.GCReferenceStatistics", 0).is_some() {
            // (Majors only: the VM passes `None` for a minor -- zs s8/jf2.)
            let at = start + duration;
            for (name, count) in REFERENCE_TYPE_NAMES.iter().zip(counts) {
                let ty = enum_key_auto(b, "jdk.types.ReferenceType", name);
                append(
                    b,
                    "jdk.GCReferenceStatistics",
                    &fields(vec![
                        ("startTime", Val::Long(at)),
                        ("gcId", Val::Int(c.gc_id)),
                        ("type", Val::Ref(ty)),
                        ("count", Val::Long(count)),
                    ]),
                );
            }
        }
    }
    if let Some((total, empty, relocate)) = z.relocation_set {
        if admit(b, "jdk.ZRelocationSet", 0).is_some() {
            append(
                b,
                "jdk.ZRelocationSet",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(0)),
                    ("eventThread", Val::Ref(t)),
                    ("total", Val::Long(total)),
                    ("empty", Val::Long(empty)),
                    ("relocate", Val::Long(relocate)),
                ]),
            );
        }
        if admit(b, "jdk.ZRelocationSetGroup", 0).is_some() {
            for &(page_type, candidates, total, empty, selected, relocate) in z.relocation_groups {
                let ty = enum_key_auto(b, "jdk.types.ZPageTypeType", page_type);
                append(
                    b,
                    "jdk.ZRelocationSetGroup",
                    &fields(vec![
                        ("startTime", Val::Long(start)),
                        ("duration", Val::Long(0)),
                        ("eventThread", Val::Ref(t)),
                        ("type", Val::Ref(ty)),
                        ("candidatePages", Val::Long(candidates)),
                        ("total", Val::Long(total)),
                        ("empty", Val::Long(empty)),
                        ("selectedPages", Val::Long(selected)),
                        ("relocate", Val::Long(relocate)),
                    ]),
                );
            }
        }
    }
    if let Some((bytes, nanos)) = z.uncommit {
        if admit(b, "jdk.ZUncommit", nanos).is_some() {
            append(
                b,
                "jdk.ZUncommit",
                &fields(vec![
                    ("startTime", Val::Long(start)),
                    ("duration", Val::Long(nanos)),
                    ("eventThread", Val::Ref(t)),
                    ("uncommitted", Val::Long(bytes)),
                ]),
            );
        }
    }
}

/// zr z4/jf: one GC phase row outside a collection's own events --
/// `jdk.GCPhasePause` for a ZGC mark-start pause, `jdk.GCPhaseConcurrent` for
/// the concurrent marking a cycle really ran. `event` is the JFR type name;
/// `thread` the collector thread it is committed on (zs s8/jf2, see
/// [`GcCycle::gc_thread`]; `None` = `VM Thread`).
///
/// gci i9/jc: also the writer of [`gc_phase_pause`] / [`gc_phase_concurrent`]
/// (the `GC Pause` block of [`gc_cycle`] with the event type and name as
/// parameters; times are epoch nanos as [`gc_cycle`]'s).
pub fn gc_phase(
    vm: usize,
    event: &str,
    gc_id: i32,
    name: &str,
    start_nanos: u64,
    duration_nanos: u64,
    thread: Option<&'static str>,
) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = duration_nanos as i64;
    BACKEND.with(vm, |b| {
        if admit(b, event, duration).is_none() {
            return;
        }
        let vm_thread = ensure_vm_thread(b);
        let t = gc_event_thread(b, thread, vm_thread);
        append(
            b,
            event,
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("gcId", Val::Int(gc_id)),
                ("name", s(name)),
            ]),
        );
    });
}

/// Ids given to a stalling thread the Java side never registered: far above
/// any `Thread.tid`, so they cannot collide with one.
const STALL_THREAD_ID_BASE: i64 = 1 << 40;

/// zr z4/jf: `jdk.ZAllocationStall` -- `thread_name` (VM thread id `vm_tid`)
/// could not allocate and waited `duration_nanos` for the collection that
/// served it. The event thread is the Java thread already in the pool under
/// that name (the Java side registers started threads), else a pool entry of
/// its own.
///
/// `type` and `size` are HotSpot's page type and the page size the stalled
/// page allocation asked for: `page` (zs s7/jf), the page HotSpot would have
/// asked for to serve the request this VM's arena refused. `None` (no refusal
/// on record, or `CRATONVM_ZGC_JFR_STALL_REQUEST=0`) writes `size = 0` and
/// `type` as no value (`null`) rather than invented; see
/// `docs/internal/gc/zgc-zr4jf-allocation-stall-event-has-no-size-FIXED-20261019.md`.
pub fn zgc_allocation_stall(
    vm: usize,
    vm_tid: u64,
    thread_name: &str,
    start_nanos: u64,
    duration_nanos: u64,
    page: Option<(&str, i64)>,
    ctx: Option<&mut dyn NativeContext>,
) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = duration_nanos as i64;
    let Some(stack_on) = BACKEND.with(vm, |b| admit(b, "jdk.ZAllocationStall", duration)) else {
        return;
    };
    // zs s8/jf2: with the stalling thread's context, the event names that
    // thread's own pool entry and carries its Java stack (HotSpot commits it
    // on the stalling thread, stack included). Taken before the backend lock:
    // `intern_stack` takes it itself.
    let (java_thread, stack) = match ctx {
        Some(ctx) => {
            let t = ctx
                .current_thread_object_if_any()
                .map(|o| register_thread(&mut *ctx, o));
            let stack = if stack_on {
                let mut frames = ctx.capture_stack_trace();
                frames.reverse();
                intern_stack(&mut *ctx, &frames, 0, 0)
            } else {
                0
            };
            (t, stack)
        }
        None => (None, 0),
    };
    BACKEND.with(vm, |b| {
        let known = java_thread.or_else(|| {
            b.tables
                .threads
                .iter()
                .find(|(_, r)| r.java_tid != 0 && r.java_name == thread_name)
                .map(|(id, _)| *id)
        });
        let t = match known {
            Some(id) => {
                b.needs.threads.insert(id);
                id
            }
            None => named_thread(
                b,
                STALL_THREAD_ID_BASE + (vm_tid as i64 & 0xFF_FFFF_FFFF),
                thread_name,
            ),
        };
        let (ty, size) = match page {
            Some((name, size)) => (enum_key_auto(b, "jdk.types.ZPageTypeType", name), size),
            None => (0, 0),
        };
        append(
            b,
            "jdk.ZAllocationStall",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack)),
                ("type", Val::Ref(ty)),
                ("size", Val::Long(size)),
            ]),
        );
    });
}

/// zs s7/jf: `jdk.SystemGC` as HotSpot's `JVM_GC` commits it -- on the Java
/// thread that called `Runtime.gc()`, with that thread's stack, spanning the
/// call from `start_ticks` (read before the collection) to now. Written only
/// when the VM answers that the collection it ran leaves `jdk.SystemGC` to
/// the caller (`NativeContext::jfr_system_gc_on_caller`: ZGC, an explicit GC
/// that is not ignored, `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER` on).
pub fn system_gc_on_caller(
    ctx: &mut dyn NativeContext,
    start_ticks: i64,
    invoked_concurrent: bool,
) {
    let vm = ctx.vm_identity();
    let end = now_ticks();
    // zt t5/jf: end with the request's collection, not after the door's
    // drains (`zgc_system_gc_duration`).
    let duration = BACKEND.with(vm, |b| {
        zgc_system_gc_duration(b, start_ticks, (end - start_ticks).max(0))
    });
    let Some(stack_on) = BACKEND.with(vm, |b| admit(b, "jdk.SystemGC", duration)) else {
        return;
    };
    let Some(thread) = ctx.current_thread_object_if_any() else {
        return;
    };
    let t = register_thread(ctx, thread);
    let stack = if stack_on {
        // zs s8/jf2: HotSpot commits the event in `JVM_GC`, under the native
        // `java.lang.Runtime.gc()` frame, which tops its stack whatever the
        // Java caller (`System.gc()`, `MemoryMXBean.gc()`); the capture does not
        // include the running native's own frame, so it is put back
        // (`CRATONVM_ZGC_JFR_SYSTEM_GC_NATIVE_FRAME`, default ON, `=0` kill) --
        // by gci i23/pk's [`system_gc_stack_frames`], the one copy of that
        // frame. `capture_stack_trace` lists the outermost frame first; JFR
        // goes innermost first.
        let frames = if system_gc_native_frame_enabled() {
            system_gc_stack_frames(ctx.capture_stack_trace())
        } else {
            let mut frames = ctx.capture_stack_trace();
            frames.reverse();
            frames
        };
        intern_stack(ctx, &frames, 0, 0)
    } else {
        0
    };
    BACKEND.with(vm, |b| {
        append(
            b,
            "jdk.SystemGC",
            &fields(vec![
                ("startTime", Val::Long(start_ticks)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(stack)),
                ("invokedConcurrent", Val::Bool(invoked_concurrent)),
            ]),
        );
    });
}

/// zt t5/jf: a `jdk.SystemGC` row's duration in ticks, from `start` and the
/// writer's own `duration`: when the last ZGC collection
/// ([`Backend::last_zgc_collection_end`], taken here) ended inside the
/// request, the row ends there, as HotSpot 25's `JVM_GC` commits right after
/// `collect` returns. The door's row ended after the collection's own event
/// emission and post-pause work, the caller-side row after the door's drains
/// too (local HotSpot 25.0.3 `Zt1JfSystemGcSpan`: 0.1-0.8 ms after the
/// collection; this VM 16-24 ms). `CRATONVM_ZGC_JFR_SYSTEM_GC_ENDS_WITH_COLLECTION=0`,
/// no ZGC collection on record, or one outside the request: unchanged. Off
/// ZGC the field is never set, so every other collector's row is unchanged.
fn zgc_system_gc_duration(b: &mut Backend, start: i64, duration: i64) -> i64 {
    let last = b.last_zgc_collection_end.take();
    match last {
        Some(end)
            if end >= start
                && end <= start.saturating_add(duration)
                && cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_ZGC_JFR_SYSTEM_GC_ENDS_WITH_COLLECTION",
                ) =>
        {
            end - start
        }
        _ => duration,
    }
}

/// `CRATONVM_ZGC_JFR_SYSTEM_GC_NATIVE_FRAME` -- default ON (zs s8/jf2): see
/// [`system_gc_on_caller`]. Read once per process.
fn system_gc_native_frame_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_SYSTEM_GC_NATIVE_FRAME")
    })
}

/// zs s7/jf: an explicit GC request from Java (`Runtime.gc()`, the
/// management beans' `gc()`): `ctx.force_gc()`, plus its `jdk.SystemGC` on
/// the calling thread when the VM leaves that event to the caller
/// ([`system_gc_on_caller`]). One relaxed load when no recording runs.
pub fn force_gc_as_system_gc(ctx: &mut dyn NativeContext) {
    let jfr = if any_recording() {
        ctx.jfr_system_gc_on_caller().map(|c| (now_ticks(), c))
    } else {
        None
    };
    // zt t3/jf (`CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR`): on ZGC the door
    // writes the row; record the calling thread first, read off its `Thread`
    // object (`isVirtual`, name), so the door's `caller_thread_ref` finds
    // this record instead of making a platform-thread one. ZGC only: the
    // other collectors keep their rows byte for byte.
    if jfr.is_none() && any_recording() && ctx.heap_is_zgc() {
        if let Some(thread) = ctx.current_thread_object_if_any() {
            let _ = register_thread(ctx, thread);
        }
    }
    ctx.force_gc();
    if let Some((start, invoked_concurrent)) = jfr {
        system_gc_on_caller(ctx, start, invoked_concurrent);
    }
}

/// Id of the uncommitter thread's pool entry (zr z4/un), next to the stall
/// range so it cannot collide with a `Thread.tid`.
const UNCOMMITTER_THREAD_ID: i64 = STALL_THREAD_ID_BASE - 1;
/// zs s8/jf2: ids of the two ZGC collector-thread pool entries, beside it.
const ZGC_DRIVER_MAJOR_THREAD_ID: i64 = STALL_THREAD_ID_BASE - 2;
const ZGC_DRIVER_MINOR_THREAD_ID: i64 = STALL_THREAD_ID_BASE - 3;

/// zr z4/un: `jdk.ZUncommit` from the heap's uncommitter thread, outside any
/// collection (HotSpot emits it from its `ZUncommitter` thread): `bytes`
/// returned to the OS over `duration_nanos` starting at `start_nanos`.
pub fn zgc_uncommit(vm: usize, start_nanos: u64, duration_nanos: u64, bytes: u64) {
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = duration_nanos as i64;
    BACKEND.with(vm, |b| {
        if admit(b, "jdk.ZUncommit", duration).is_none() {
            return;
        }
        let t = named_thread(b, UNCOMMITTER_THREAD_ID, "ZUncommitter");
        append(
            b,
            "jdk.ZUncommit",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                ("uncommitted", Val::Long(bytes as i64)),
            ]),
        );
    });
}

// ---------------------------------------------------------------------------
// JIT: compilations, deoptimizations; class unloading
// ---------------------------------------------------------------------------

const COMPILER_THREAD_ID: i64 = 2;

/// A method finished compiling (called from the JIT's compile paths whether or
/// not a recording runs: the counters feed `jdk.CompilerStatistics`).
#[allow(clippy::too_many_arguments)]
pub fn compilation(
    vm: usize,
    class: &str,
    method: &str,
    descriptor: &str,
    start_nanos: u64,
    duration_nanos: u64,
    code_size: i64,
    osr: bool,
    succeeded: bool,
    level: i32,
) {
    COMPILE_COUNT.fetch_add(1, Ordering::Relaxed);
    if !succeeded {
        BAILOUT_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    if osr {
        OSR_COUNT.fetch_add(1, Ordering::Relaxed);
        OSR_BYTES.fetch_add(code_size, Ordering::Relaxed);
    } else {
        STD_BYTES.fetch_add(code_size, Ordering::Relaxed);
    }
    TOTAL_COMPILE_NANOS.fetch_add(duration_nanos as i64, Ordering::Relaxed);
    PEAK_COMPILE_NANOS.fetch_max(duration_nanos as i64, Ordering::Relaxed);
    if !any_recording() {
        return;
    }
    let offset = crate::jfr_backend::epoch_to_ticks_offset();
    let start = start_nanos as i64 - offset;
    let duration = duration_nanos as i64;
    BACKEND.with(vm, |b| {
        if admit(b, "jdk.Compilation", duration).is_none() {
            return;
        }
        let t = named_thread(b, COMPILER_THREAD_ID, "C2 CompilerThread0");
        let m = method_by_name_locked(b, &class.replace('.', "/"), method, descriptor);
        let compiler = enum_key_auto(
            b,
            "jdk.types.CompilerType",
            if level >= 4 { "c2" } else { "c1" },
        );
        append(
            b,
            "jdk.Compilation",
            &fields(vec![
                ("startTime", Val::Long(start)),
                ("duration", Val::Long(duration)),
                ("eventThread", Val::Ref(t)),
                (
                    "compileId",
                    Val::Int(COMPILE_COUNT.load(Ordering::Relaxed) as i32),
                ),
                ("compiler", Val::Ref(compiler)),
                ("method", Val::Ref(m)),
                ("compileLevel", Val::Int(level)),
                ("succeded", Val::Bool(succeeded)),
                ("isOsr", Val::Bool(osr)),
                ("codeSize", Val::Long(code_size)),
                ("inlinedBytes", Val::Long(0)),
                ("arenaBytes", Val::Long(0)),
            ]),
        );
    });
}

/// A compiled method trapped back to the interpreter. `method_key` is the JIT's
/// `Class.method:descriptor` key.
pub fn deoptimization(vm: usize, method_key: &str, reason: &str, action: &str, bci: i32) {
    if !any_recording() {
        return;
    }
    let now = now_ticks();
    let (class, method, descriptor) = split_method_key(method_key);
    BACKEND.with(vm, |b| {
        if admit(b, "jdk.Deoptimization", 0).is_none() {
            return;
        }
        let t = named_thread(b, COMPILER_THREAD_ID + 1, "main");
        let m = method_by_name_locked(b, &class, &method, &descriptor);
        let compiler = enum_key_auto(b, "jdk.types.CompilerType", "c2");
        let reason_key = enum_key_auto(b, "jdk.types.DeoptimizationReason", reason);
        let action_key = enum_key_auto(b, "jdk.types.DeoptimizationAction", action);
        let instruction = enum_key_auto(b, "jdk.types.Bytecode", "unknown");
        append(
            b,
            "jdk.Deoptimization",
            &fields(vec![
                ("startTime", Val::Long(now)),
                ("eventThread", Val::Ref(t)),
                ("stackTrace", Val::Ref(0)),
                ("compileId", Val::Int(0)),
                ("compiler", Val::Ref(compiler)),
                ("method", Val::Ref(m)),
                ("lineNumber", Val::Int(0)),
                ("bci", Val::Int(bci)),
                ("instruction", Val::Ref(instruction)),
                ("reason", Val::Ref(reason_key)),
                ("action", Val::Ref(action_key)),
            ]),
        );
    });
}

/// `Class.method:descriptor` (or `Class::method(desc)`) into its three parts.
fn split_method_key(key: &str) -> (String, String, String) {
    let (head, descriptor) = match key.find('(') {
        Some(i) => (&key[..i], key[i..].to_owned()),
        None => match key.rsplit_once(':') {
            Some((h, d)) => (h, d.to_owned()),
            None => (key, String::new()),
        },
    };
    let head = head.trim_end_matches(':');
    let (class, method) = match head.rsplit_once("::") {
        Some((c, m)) => (c, m),
        None => head.rsplit_once('.').unwrap_or(("", head)),
    };
    (class.replace('.', "/"), method.to_owned(), descriptor)
}

/// A class was unloaded. `loader` is its defining loader's flat id
/// (`ClassLoaderId::to_native_id`, as `NativeContext::loader_id_of_class`).
///
/// gci i12/ul F5: called INSIDE the collection pause
/// (`memory::gc::report_unloaded_classes`), so the backend is taken with
/// `try_with`: a peer frozen by the pause, or this thread further up its
/// stack, may hold it, and a blocking take would stall the pause. A busy
/// backend drops the event, as the legacy recorder beside it does. The
/// `definingClassLoader` is the class's loader constant (it was null).
pub fn class_unloaded(vm: usize, class: &str, loader: i32) {
    if !any_recording() {
        return;
    }
    let now = now_ticks();
    let _ = BACKEND.try_with(vm, |b| {
        if admit(b, "jdk.ClassUnload", 0).is_none() {
            return;
        }
        let t = ensure_vm_thread(b);
        let c = class_by_name_locked(b, &class.replace('.', "/"));
        let defining = crate::jfr_backend::loader_ref(b, loader);
        append(
            b,
            "jdk.ClassUnload",
            &fields(vec![
                ("startTime", Val::Long(now)),
                ("eventThread", Val::Ref(t)),
                ("unloadedClass", Val::Ref(c)),
                ("definingClassLoader", Val::Ref(defining)),
            ]),
        );
    });
}

// ---------------------------------------------------------------------------
// periodic events
// ---------------------------------------------------------------------------

fn runtime_memory(ctx: &mut dyn NativeContext) -> (i64, i64, i64) {
    let rt = match ctx.invoke_static(
        "java/lang/Runtime",
        "getRuntime",
        "()Ljava/lang/Runtime;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => return (0, 0, 0),
    };
    let mut scope = NativeHandleScope::new(ctx);
    let h = scope.root(rt);
    let mut get = |scope: &mut NativeHandleScope<'_>, name: &str| -> i64 {
        let r = scope.get(&h);
        match scope.invoke_virtual(r, name, "()J", &[]) {
            Ok(Some(Value::Long(v))) => v,
            _ => 0,
        }
    };
    let total = get(&mut scope, "totalMemory");
    let free = get(&mut scope, "freeMemory");
    let max = get(&mut scope, "maxMemory");
    (total - free, total, max)
}

/// `CRATONVM_JFR_GC_CONFIG_HOTSPOT` -- default ON, `=0` kill. gci i42/mx
/// (`gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-FIXED-20261016`): the
/// periodic GC configuration and heap-usage events answer HotSpot 25's values
/// where the VM holds them -- `jdk.GCHeapMemoryPoolUsage` one row per heap
/// pool ([`heap_pool_usage_rows`]), `jdk.GCConfiguration`'s GC thread counts
/// ([`hotspot_gc_threads`]) and explicit-GC flags, `jdk.GCHeapConfiguration`'s
/// and the flag events' `MaxHeapSize` / `InitialHeapSize`. Off: the rows this
/// VM wrote before (one `Java heap` pool row, the CPU count as both thread
/// counts, `false` / `false`, `Runtime.maxMemory()` / `totalMemory()`). Read
/// per periodic emission (once per chunk), never on a hot path.
fn jfr_gc_config_hotspot_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JFR_GC_CONFIG_HOTSPOT")
}

/// zt t2/jf: HotSpot 25's code heaps, as `jdk.types.CodeBlobType` and the
/// `CodeHeap` memory pools name them (`CodeCache::get_code_heap_name`).
const CODE_HEAP_NAMES: [&str; 3] = [
    "CodeHeap 'non-nmethods'",
    "CodeHeap 'profiled nmethods'",
    CODE_HEAP_NON_PROFILED,
];
/// The heap this VM's JIT code is reported in (`jmx.rs`: every byte of
/// mapped JIT code is `CodeHeap 'non-profiled nmethods'`).
const CODE_HEAP_NON_PROFILED: &str = "CodeHeap 'non-profiled nmethods'";

/// `CRATONVM_ZGC_JFR_CODE_HEAP_ROWS` -- default ON (zt t2/jf): on ZGC the
/// real backend's `jdk.CodeCacheStatistics` is one row per HotSpot code heap
/// with its `codeBlobType` key and the non-profiled heap's
/// `unallocatedCapacity`, and `jdk.CodeCacheConfiguration` states the
/// code-cache cap as `reservedSize` / `nonProfiledSize` -- the figures the
/// `CodeHeap` pool beans report. Other collectors keep the generic rows
/// (outside the ZGC round's scope). `=0`: the generic rows. Read per call.
fn zgc_jfr_code_heap_rows(ctx: &dyn NativeContext) -> bool {
    ctx.heap_is_zgc()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_CODE_HEAP_ROWS")
}

/// gci i42/mx: HotSpot 25's `(ParallelGCThreads, ConcGCThreads)` for the
/// collector whose JFR old-collector name is `old_collector`, on `ncpus`
/// active processors, as `jdk.GCConfiguration` reports them with the flags at
/// their defaults. Serial (`SerialOld`, this VM's Generational backend): no GC
/// worker gang, both flags stay `0`. G1 (`G1Old`): `G1Arguments::initialize`
/// sets `ParallelGCThreads` to `WorkerPolicy::parallel_worker_threads()`
/// (`ncpus` up to 8, then `8 + (ncpus - 8) * 5 / 8`, the x86 denominator) and
/// `G1ConcurrentMark` sets `ConcGCThreads` to
/// `max((ParallelGCThreads + 2) / 4, 1)`. `None` for any other collector (ZGC,
/// out of the round's scope: the caller keeps its historical figures). Pure.
fn hotspot_gc_threads(old_collector: &str, ncpus: i32) -> Option<(i32, i32)> {
    match old_collector {
        "SerialOld" => Some((0, 0)),
        "G1Old" => {
            let n = ncpus.max(1);
            let parallel = if n <= 8 { n } else { 8 + (n - 8) * 5 / 8 };
            Some((parallel, ((parallel + 2) / 4).max(1)))
        }
        _ => None,
    }
}

/// gci i42/mx: `jdk.GCHeapConfiguration.compressedOopsMode`, HotSpot's
/// `CompressedOops::mode_to_string(CompressedOops::mode())` for this process's
/// narrow-oop state. With compressed oops OFF (this VM's default, and what a
/// row's HotSpot twin runs under `-XX:-UseCompressedOops`) HotSpot's base and
/// shift stay 0, so its mode is `UnscaledNarrowOop`, `32-bit`; on, the base
/// and shift decide (`Zero based`, the disjoint test of a base aligned to
/// `4G << LogMinObjAlignmentInBytes` = 32 GiB, `Non-zero based`) -- the same
/// test as the `gc,init` `Compressed Oops:` line. Pure.
fn hotspot_narrow_oop_mode(enabled: bool, base: u64, shift: usize) -> &'static str {
    const DISJOINT_ALIGN: u64 = 1 << 35;
    if !enabled || (base == 0 && shift == 0) {
        "32-bit"
    } else if base == 0 {
        "Zero based"
    } else if base % DISJOINT_ALIGN == 0 {
        "Non-zero disjoint base"
    } else {
        "Non-zero based"
    }
}

/// gci i42/mx: `jdk.GCHeapMemoryPoolUsage`'s rows, `(name, used, committed,
/// max)`: one per HEAP pool, in the collector's pool order, as HotSpot's
/// periodic request walks `MemoryService`'s pools and writes every
/// `is_heap()` one under its own name (`G1 Eden Space`, `Tenured Gen`, ...)
/// with `get_memory_usage()`. `-1` (the JMM's undefined `max`) is written as
/// is: HotSpot's `size_t` `undefined_size` has the same 64 bits. Empty when
/// the context describes no heap pools (the caller then writes its historical
/// single row). Pure.
fn heap_pool_usage_rows(
    pools: &[cratonvm_native_api::GcMemoryPoolInfo],
) -> Vec<(&'static str, i64, i64, i64)> {
    pools
        .iter()
        .map(|p| (p.name, p.used, p.committed, p.max))
        .collect()
}

/// `CRATONVM_JFR_GC_FLAG_CONFIG_HOTSPOT` -- default ON, `=0` kill. gci i43/tj
/// (`gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-FIXED-20261016` items
/// 5-7): `jdk.YoungGenerationConfiguration`, `jdk.GCSurvivorConfiguration`,
/// `jdk.GCTLABConfiguration` and `jdk.GCConfiguration.gcTimeRatio` answer the
/// HotSpot 25 flag values for this command line ([`gc_command_line_flags`],
/// [`hotspot_young_generation_configuration`], [`hotspot_gc_time_ratio`],
/// `NativeSystemAccess::hotspot_tlab_configuration`).
/// Off: the rows written before (`totalMemory() / 3`, `maxMemory() / 3`,
/// `newRatio = 2`; the constants 15 / 7, `true` / 2048 / 64, and 99). Read per
/// periodic emission (once per chunk), never on a hot path.
fn jfr_gc_flag_config_hotspot_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JFR_GC_FLAG_CONFIG_HOTSPOT")
}

/// gci i43/tj: the young-sizing, tenuring and `GCTimeRatio` flags a command
/// line set (the TLAB flags come validated from `VmConfig::tlab_flags`), as
/// HotSpot's `Arguments::parse_each_vm_init_arg` leaves them
/// (`FLAG_SET_CMDLINE`, last occurrence wins). `None` = the flag is at its
/// default (`FLAG_IS_DEFAULT`), which the ergonomics below distinguish from
/// an explicit value. `-Xmn<size>` sets both `NewSize` and `MaxNewSize` at
/// its position. A value HotSpot's parser would refuse never reaches a
/// running VM (the launcher refuses it, or will: lane i43/lf), so text that
/// does not parse is skipped here rather than guessed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GcCommandLineFlags {
    new_size: Option<u64>,
    max_new_size: Option<u64>,
    new_ratio: Option<u64>,
    max_tenuring_threshold: Option<u64>,
    initial_tenuring_threshold: Option<u64>,
    gc_time_ratio: Option<u64>,
    /// gci i47w/j43: `-XX:G1HeapRegionSize` (G1's `MaxNewSize` is a whole
    /// number of regions, [`hotspot_g1_max_new_size`]).
    g1_heap_region_size: Option<u64>,
}

/// gci i43/tj: HotSpot's `atojulong` for a flag or `-Xmn` value: decimal
/// digits, or `0x` / `0X` hex digits, then at most one of `k K m M g G t T`
/// (binary multiples), then the end. `None` on anything else or on overflow.
/// Pure.
fn hotspot_size_value(text: &str) -> Option<u64> {
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (text, 10),
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let n = u64::from_str_radix(&digits[..end], radix).ok()?;
    let shift = match &digits[end..] {
        "" => 0,
        "k" | "K" => 10,
        "m" | "M" => 20,
        "g" | "G" => 30,
        "t" | "T" => 40,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

/// gci i43/tj: [`GcCommandLineFlags`] from the VM's options
/// (`NativeSystemAccess::jvm_input_arguments`, HotSpot's
/// `Arguments::jvm_args_array()`), in order. Pure.
fn gc_command_line_flags(args: &[String]) -> GcCommandLineFlags {
    let mut f = GcCommandLineFlags::default();
    for arg in args {
        if let Some(size) = arg.strip_prefix("-Xmn") {
            if let Some(n) = hotspot_size_value(size) {
                f.new_size = Some(n);
                f.max_new_size = Some(n);
            }
            continue;
        }
        let Some((name, text)) = arg
            .strip_prefix("-XX:")
            .and_then(|flag| flag.split_once('='))
        else {
            continue;
        };
        let slot = match name {
            "NewSize" => &mut f.new_size,
            "MaxNewSize" => &mut f.max_new_size,
            "NewRatio" => &mut f.new_ratio,
            "MaxTenuringThreshold" => &mut f.max_tenuring_threshold,
            "InitialTenuringThreshold" => &mut f.initial_tenuring_threshold,
            "GCTimeRatio" => &mut f.gc_time_ratio,
            "G1HeapRegionSize" => &mut f.g1_heap_region_size,
            _ => continue,
        };
        if let Some(n) = hotspot_size_value(text) {
            *slot = Some(n);
        }
    }
    f
}

/// gci i43/tj: HotSpot 25's default `NewSize`, `ScaleForWordSize(1*M)` on a
/// 64-bit VM (`align_down(1M * 13 / 10, HeapWordSize)`).
const HOTSPOT_DEFAULT_NEW_SIZE: u64 = 1_363_144;
/// gci i43/tj: Serial's `GenAlignment` (`SpaceAlignment`, 64 KiB).
const HOTSPOT_GEN_ALIGNMENT: u64 = 64 * 1024;
/// gci i43/tj: HotSpot's default `NewRatio`.
const HOTSPOT_DEFAULT_NEW_RATIO: u64 = 2;

/// gci i47w/j43: HotSpot 25's G1 `MaxNewSize` after
/// `G1YoungGenSizer::adjust_max_new_size` (`G1Policy::init`), which
/// `FLAG_SET_ERGO`s it to the sizer's maximum young length times the region
/// size for the heap's `max_num_regions()` -- so the flag is never at its
/// default and `jdk.YoungGenerationConfiguration.maxSize` is that figure, not
/// `Long.MIN_VALUE` (HotSpot 25.0.3, `-XX:+UseG1GC -Xms256m -Xmx256m`:
/// 160 432 128 = 153 one-MiB regions, row `tj43_c_g1`). The sizer kind, as
/// `G1YoungGenSizer::G1YoungGenSizer`: a command-line `NewRatio` without
/// `NewSize` / `MaxNewSize` is `regions / (NewRatio + 1)`; a command-line
/// `MaxNewSize` (raised to a larger `NewSize`) is `MaxNewSize / region`;
/// `NewSize` alone is `max(NewSize / region, G1MaxNewSizePercent of the
/// regions)`; nothing is the 60 % default. Each length is at least 1.
/// The region size is `G1HeapRegion::setup_heap_region_size`'s
/// (`-XX:G1HeapRegionSize`, else `clamp(MaxHeapSize / 2048, 1M, 32M)`,
/// rounded up to a power of two) and `MaxHeapSize` is aligned up to
/// `max(region, 2 MiB)` (the card table's heap alignment). Pure.
fn hotspot_g1_max_new_size(max_heap: u64, new_size: u64, flags: &GcCommandLineFlags) -> u64 {
    const M: u64 = 1024 * 1024;
    const G1_MAX_NEW_SIZE_PERCENT: u64 = 60;
    let region = match flags.g1_heap_region_size {
        // `G1HeapRegionBounds::max_size()` (512 MiB) bounds the flag's range.
        Some(r) if r > 0 => r.min(512 * M),
        _ => (max_heap / 2048).clamp(M, 32 * M),
    }
    .max(1)
    .next_power_of_two();
    let heap = max_heap.div_ceil(region.max(2 * M)).saturating_mul(region.max(2 * M));
    let regions = heap / region;
    let len = |bytes: u64| (bytes / region).max(1);
    let default_max = (regions.saturating_mul(G1_MAX_NEW_SIZE_PERCENT) / 100).max(1);
    let max_new_size = flags.max_new_size.map(|m| m.max(new_size));
    let length = match (flags.new_size.is_some(), max_new_size, flags.new_ratio) {
        (false, None, Some(ratio)) => (regions / ratio.saturating_add(1)).max(1),
        (_, Some(m), _) => len(m),
        (true, None, _) => default_max.max(len(new_size)),
        (false, None, None) => default_max,
    };
    length.saturating_mul(region)
}

/// gci i43/tj: `jdk.YoungGenerationConfiguration`'s `(minSize, maxSize,
/// newRatio)` as HotSpot 25 writes them (`GCYoungGenerationConfiguration`:
/// `NewSize`, `MaxNewSize` -- for G1 the value `G1YoungGenSizer` set
/// ergonomically, [`hotspot_g1_max_new_size`] -- and `NewRatio`) for the collector whose
/// JFR old-collector name is `old_collector`, with HotSpot's
/// `[MinHeapSize, InitialHeapSize, MaxHeapSize]` `heap` and the command line's
/// `flags`. `None` for a collector this round does not model (ZGC): the caller
/// keeps its historical row. Pure.
///
/// Serial (`SerialOld`) runs `GenArguments::initialize_heap_flags_and_sizes`
/// then `initialize_size_info`: `NewSize` bounded to leave a granule of the
/// initial and maximum heap and floored at `3 * SpaceAlignment`; the maximum
/// young size `MaxHeapSize / (NewRatio + 1)` aligned down (at least one
/// granule) and clamped to `[NewSize, MaxNewSize]` unless `MaxNewSize` was
/// given; with `InitialHeapSize == MaxHeapSize` the young generation is fixed
/// at that maximum (or at a command-line `NewSize`), otherwise its initial size
/// is `InitialHeapSize / (NewRatio + 1)` clamped the same way (or a
/// command-line `NewSize` bounded by the initial heap); the two flags are then
/// written back. G1 (`G1Old`) leaves `NewSize` as given and writes the
/// sizer's `MaxNewSize` ([`hotspot_g1_max_new_size`]).
fn hotspot_young_generation_configuration(
    old_collector: &str,
    heap: [i64; 3],
    flags: &GcCommandLineFlags,
) -> Option<(i64, i64, i64)> {
    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let ratio = flags.new_ratio.unwrap_or(HOTSPOT_DEFAULT_NEW_RATIO);
    let ratio_out = to_i64(ratio);
    match old_collector {
        "G1Old" => {
            let new_size = flags.new_size.unwrap_or(HOTSPOT_DEFAULT_NEW_SIZE);
            let max_heap = u64::try_from(heap[2]).unwrap_or(0);
            let max_young = hotspot_g1_max_new_size(max_heap, new_size, flags);
            Some((to_i64(new_size), to_i64(max_young), ratio_out))
        }
        "SerialOld" => {
            const A: u64 = HOTSPOT_GEN_ALIGNMENT;
            let [_, initial_heap, max_heap] = heap.map(|v| u64::try_from(v).unwrap_or(0));
            let align_down = |v: u64| v - v % A;
            // `bound_minus_alignment(desired, maximum, alignment)`.
            let bound = |desired: u64, maximum: u64| desired.min(maximum.saturating_sub(A));
            // `scale_by_NewRatio_aligned`: `align_down_bounded(base / (NewRatio + 1))`.
            let scale = |base: u64| align_down(base / ratio.saturating_add(1)).max(A);
            let smallest_new_size = 3 * A;
            let new_cmdline = flags.new_size.is_some();
            let mut new_size = flags.new_size.unwrap_or(HOTSPOT_DEFAULT_NEW_SIZE);
            // GenArguments::initialize_heap_flags_and_sizes.
            if new_cmdline && new_size >= initial_heap {
                new_size = bound(new_size, initial_heap);
            }
            new_size = smallest_new_size.max(align_down(bound(new_size, max_heap)));
            let mut max_new_size = flags.max_new_size;
            if let Some(m) = max_new_size {
                if m >= max_heap {
                    let smaller = max_heap.saturating_sub(A);
                    max_new_size = Some(smaller);
                    new_size = new_size.min(smaller);
                } else if m < new_size {
                    new_size = m;
                } else {
                    max_new_size = Some(align_down(m));
                }
            }
            // GenArguments::initialize_size_info.
            let mut max_young = match max_new_size {
                None => scale(max_heap).max(new_size),
                Some(m) => m,
            };
            let initial_young = if max_heap == initial_heap {
                if new_cmdline {
                    max_young = new_size;
                }
                max_young
            } else if new_cmdline {
                max_young.min(bound(new_size, initial_heap))
            } else {
                scale(initial_heap).max(new_size).min(max_young)
            };
            Some((to_i64(initial_young), to_i64(max_young), ratio_out))
        }
        _ => None,
    }
}

/// gci i43/tj: `jdk.GCConfiguration.gcTimeRatio`, HotSpot 25's `GCTimeRatio`
/// after ergonomics (`None` for a collector this round does not model: the
/// caller keeps 99). The flag's default is 99; `G1Arguments::initialize`
/// replaces a default (or `0`) value by G1's own goal, 12 (about 8 % of the
/// time in GC). A command-line value stands otherwise. Pure.
fn hotspot_gc_time_ratio(old_collector: &str, command_line: Option<u64>) -> Option<i32> {
    let as_int = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    match (old_collector, command_line) {
        ("G1Old", None | Some(0)) => Some(12),
        ("G1Old" | "SerialOld", Some(v)) => Some(as_int(v)),
        ("SerialOld", None) => Some(99),
        _ => None,
    }
}

/// gci i43/tj (`gci-i42mx-...` item 8, the `origin` half): did the VM's
/// options (`args`, HotSpot's `jvm_args()`) set flag `flag`? A boolean flag
/// by `-XX:+F` / `-XX:-F`, any other by `-XX:F=..`; `MaxHeapSize` also by
/// `-Xmx`, `InitialHeapSize` by `-Xms`, and `UseSerialGC` by this VM's own
/// spelling of the collector it reports as Serial, `-XX:±UseGenerationalGC`.
/// Pure.
fn flag_on_command_line(flag: &str, args: &[String]) -> bool {
    let named = |arg: &str, name: &str| {
        arg.strip_prefix("-XX:").is_some_and(|rest| {
            rest.strip_prefix('+').or_else(|| rest.strip_prefix('-')) == Some(name)
                || rest
                    .strip_prefix(name)
                    .is_some_and(|value| value.starts_with('='))
        })
    };
    args.iter().any(|arg| {
        let arg = arg.as_str();
        named(arg, flag)
            || match flag {
                "MaxHeapSize" => arg.starts_with("-Xmx"),
                "InitialHeapSize" => arg.starts_with("-Xms"),
                "UseSerialGC" => named(arg, "UseGenerationalGC"),
                _ => false,
            }
    })
}

/// gci i43/tj (`gci-i42mx-...` item 8): the `jdk.types.FlagValueOrigin` name
/// HotSpot 25 gives flag `flag` of the flag events this VM writes, with the
/// VM's options `args`, the collector whose JFR old-collector name is
/// `old_collector` and `MaxHeapSize` `max_heap`: `Command line` for a flag
/// the options set ([`flag_on_command_line`]); `Ergonomic` for one HotSpot's
/// argument processing sets with `FLAG_SET_ERGO` (`MaxHeapSize` /
/// `InitialHeapSize` in `Arguments::set_heap_size`, the selected collector's
/// `Use*GC` in `GCConfig::select_gc_ergonomically`, `CICompilerCount` in
/// `CompilationPolicy::initialize`, `UseCompressedOops` in
/// `Arguments::set_use_compressed_oops` for a heap under the 32 GiB
/// narrow-oop limit, except under ZGC); `Default` for every other one. The
/// row used to say `Ergonomic` for all of them. Pure.
fn hotspot_flag_origin(
    flag: &str,
    args: &[String],
    old_collector: &str,
    max_heap: i64,
) -> &'static str {
    if flag_on_command_line(flag, args) {
        return "Command line";
    }
    let ergonomic = match flag {
        "MaxHeapSize" | "InitialHeapSize" | "CICompilerCount" => true,
        // gci i47w/j43: `G1Arguments::initialize` sets `ParallelGCThreads`
        // with `FLAG_SET_DEFAULT`, so its origin stays `Default` (HotSpot
        // 25.0.3, row `tj43_c_g1`); it is not listed here.
        "UseG1GC" => old_collector == "G1Old",
        "UseSerialGC" => old_collector == "SerialOld",
        "UseZGC" => old_collector == "ZGC Major",
        "UseCompressedOops" => old_collector != "ZGC Major" && max_heap < (32_i64 << 30),
        _ => false,
    };
    if ergonomic {
        "Ergonomic"
    } else {
        "Default"
    }
}

fn module_ref(b: &mut Backend, name: &str) -> i64 {
    let key = b.tables.symbol(name);
    b.tables.modules.entry(key).or_insert(ModuleRec {
        name: name.to_owned(),
        version: String::new(),
        location: String::new(),
        loader: 0,
    });
    b.needs.modules.insert(key);
    key
}

fn package_ref(b: &mut Backend, name: &str, module: i64) -> i64 {
    let key = b.tables.symbol(name);
    b.tables.packages.entry(key).or_insert(PackageRec {
        name: name.to_owned(),
        module,
        exported: true,
    });
    b.needs.packages.insert(key);
    key
}

fn one(ts: i64, pairs: Vec<(&str, Val)>) -> Vec<Vec<(String, Val)>> {
    let mut all = vec![("startTime", Val::Long(ts))];
    all.extend(pairs);
    vec![fields(all)]
}

/// Producers for the periodic types `jfr_vm_events::emit_periodic` has no arm for.
pub fn periodic(
    ctx: &mut dyn NativeContext,
    name: &str,
    ts: i64,
) -> Option<Vec<Vec<(String, Val)>>> {
    let vm = ctx.vm_identity();
    Some(match name {
        "jdk.ThreadCPULoad" => {
            let threads = ctx.enumerate_threads(2048);
            let now = now_ticks();
            let mut out = Vec::new();
            for t in threads {
                let Some(os) = ctx.thread_os_tid(t) else {
                    continue;
                };
                let Some(stat) = proc_file(&format!("/proc/self/task/{os}/stat")) else {
                    continue;
                };
                let Some(after) = stat.rsplit_once(')').map(|x| x.1) else {
                    continue;
                };
                let parts: Vec<&str> = after.split_whitespace().collect();
                let (Some(u), Some(sy)) = (
                    parts.get(11).and_then(|v| v.parse::<u64>().ok()),
                    parts.get(12).and_then(|v| v.parse::<u64>().ok()),
                ) else {
                    continue;
                };
                let jt = register_thread(ctx, t);
                let started_ago = {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    (now_ms - crate::jfr_backend::vm_start_millis()).max(1) * 1_000_000
                };
                let prev = BACKEND.with(vm, |b| b.thread_cpu.insert(jt, (u, sy, now)));
                // First sample of a thread: its CPU since the VM started (as HotSpot's).
                let prev = prev.or(Some((0, 0, now - started_ago)));
                if let Some((u0, s0, t0)) = prev {
                    let ticks = ((now - t0).max(1) as f64) / 1e9 * 100.0;
                    let user = (((u.saturating_sub(u0)) as f64) / ticks).min(1.0) as f32;
                    let system = (((sy.saturating_sub(s0)) as f64) / ticks).min(1.0) as f32;
                    out.push(fields(vec![
                        ("startTime", Val::Long(ts)),
                        ("eventThread", Val::Ref(jt)),
                        ("user", Val::Float(user)),
                        ("system", Val::Float(system)),
                    ]));
                }
            }
            out
        }
        "jdk.ClassLoaderStatistics" => {
            let mut counts: std::collections::BTreeMap<i32, (i64, i64)> = Default::default();
            for id in ctx.list_loaded_class_ids() {
                let e = counts.entry(ctx.loader_id_of_class(id)).or_default();
                e.0 += 1;
                if ctx.is_class_hidden(id) {
                    e.1 += 1;
                }
            }
            let rows: Vec<(i32, (i64, i64))> = counts.into_iter().collect();
            BACKEND.with(vm, |b| {
                rows.iter()
                    .map(|(loader, (n, hidden))| {
                        let me = crate::jfr_backend::loader_ref(b, *loader);
                        let parent = if *loader <= 0 {
                            0
                        } else {
                            crate::jfr_backend::loader_ref(b, loader - 1)
                        };
                        fields(vec![
                            ("startTime", Val::Long(ts)),
                            ("classLoader", Val::Ref(me)),
                            ("parentClassLoader", Val::Ref(parent)),
                            ("classLoaderData", Val::Long(i64::from(*loader) + 1)),
                            ("classCount", Val::Long(*n)),
                            ("chunkSize", Val::Long(0)),
                            ("blockSize", Val::Long(0)),
                            ("hiddenClassCount", Val::Long(*hidden)),
                            ("hiddenChunkSize", Val::Long(0)),
                            ("hiddenBlockSize", Val::Long(0)),
                        ])
                    })
                    .collect()
            })
        }
        "jdk.ModuleRequire" => {
            let mut pairs: Vec<(String, String)> = Vec::new();
            for m in ctx.module_names() {
                for (req, _, _) in ctx.module_requires(&m) {
                    pairs.push((m.clone(), req));
                }
            }
            BACKEND.with(vm, |b| {
                pairs
                    .iter()
                    .map(|(m, r)| {
                        let (a, c) = (module_ref(b, m), module_ref(b, r));
                        fields(vec![
                            ("startTime", Val::Long(ts)),
                            ("source", Val::Ref(a)),
                            ("requiredModule", Val::Ref(c)),
                        ])
                    })
                    .collect()
            })
        }
        "jdk.ModuleExport" => {
            let mut rows: Vec<(String, String, Vec<String>)> = Vec::new();
            for m in ctx.module_names() {
                for (pkg, targets) in ctx.module_exports(&m) {
                    rows.push((m.clone(), pkg, targets));
                }
            }
            BACKEND.with(vm, |b| {
                let mut out = Vec::new();
                for (m, pkg, targets) in &rows {
                    let module = module_ref(b, m);
                    let p = package_ref(b, pkg, module);
                    if targets.is_empty() {
                        out.push(fields(vec![
                            ("startTime", Val::Long(ts)),
                            ("exportedPackage", Val::Ref(p)),
                            ("targetModule", Val::Ref(0)),
                        ]));
                    }
                    for t in targets {
                        let tm = module_ref(b, t);
                        out.push(fields(vec![
                            ("startTime", Val::Long(ts)),
                            ("exportedPackage", Val::Ref(p)),
                            ("targetModule", Val::Ref(tm)),
                        ]));
                    }
                }
                out
            })
        }
        "jdk.CompilerStatistics" => one(
            ts,
            vec![
                (
                    "compileCount",
                    Val::Int(COMPILE_COUNT.load(Ordering::Relaxed) as i32),
                ),
                (
                    "bailoutCount",
                    Val::Int(BAILOUT_COUNT.load(Ordering::Relaxed) as i32),
                ),
                ("invalidatedCount", Val::Int(0)),
                (
                    "osrCompileCount",
                    Val::Int(OSR_COUNT.load(Ordering::Relaxed) as i32),
                ),
                (
                    "standardCompileCount",
                    Val::Int(
                        (COMPILE_COUNT.load(Ordering::Relaxed) - OSR_COUNT.load(Ordering::Relaxed))
                            as i32,
                    ),
                ),
                (
                    "osrBytesCompiled",
                    Val::Long(OSR_BYTES.load(Ordering::Relaxed)),
                ),
                (
                    "standardBytesCompiled",
                    Val::Long(STD_BYTES.load(Ordering::Relaxed)),
                ),
                ("nmethodsSize", Val::Long(0)),
                ("nmethodCodeSize", Val::Long(0)),
                (
                    "peakTimeSpent",
                    Val::Long(PEAK_COMPILE_NANOS.load(Ordering::Relaxed)),
                ),
                (
                    "totalTimeSpent",
                    Val::Long(TOTAL_COMPILE_NANOS.load(Ordering::Relaxed)),
                ),
            ],
        ),
        "jdk.GCConfiguration" => {
            let (young, old) = ctx.jfr_collector_names();
            let threads = std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(1);
            // gci i42/mx: HotSpot's thread flags and explicit-GC flags.
            let hotspot = jfr_gc_config_hotspot_enabled();
            // zs s2/ob: on ZGC the figures the command line states (explicit
            // thread counts, `UseDynamicNumberOfGCThreads`, the two explicit-GC
            // flags) replace the generic ones; HotSpot 25.0.3 (local) writes
            // the flag values (`-XX:ParallelGCThreads=3 -XX:ConcGCThreads=1
            // -XX:+DisableExplicitGC`: `3`, `1`, `isExplicitGCDisabled = true`).
            // (gci i42/mx's `hotspot_gc_threads` leaves ZGC out.)
            let z = ctx.jfr_zgc_configuration();
            let to_i32 = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
            let (parallel, concurrent) = match z {
                Some(z) => (
                    z.parallel_gc_threads.map_or(threads, to_i32),
                    z.conc_gc_threads.map_or((threads / 4).max(1), to_i32),
                ),
                None => hotspot
                    .then(|| hotspot_gc_threads(old, ctx.available_processor_count()))
                    .flatten()
                    .unwrap_or((threads, (threads / 4).max(1))),
            };
            let (explicit_disabled, explicit_concurrent) = match z {
                Some(z) => (z.explicit_gc_disabled, z.explicit_gc_concurrent),
                None if hotspot => ctx.explicit_gc_flags(),
                None => (false, false),
            };
            let dynamic_gc_threads = z.is_none_or(|z| z.dynamic_gc_threads);
            // gci i43/tj: HotSpot's `GCTimeRatio` after G1's ergonomics.
            let gc_time_ratio = if jfr_gc_flag_config_hotspot_enabled() {
                let flags = gc_command_line_flags(&ctx.jvm_input_arguments());
                hotspot_gc_time_ratio(old, flags.gc_time_ratio).unwrap_or(99)
            } else {
                99
            };
            let (yk, ok) = BACKEND.with(vm, |b| {
                (
                    enum_key_auto(b, "jdk.types.GCName", young),
                    enum_key_auto(b, "jdk.types.GCName", old),
                )
            });
            one(
                ts,
                vec![
                    ("youngCollector", Val::Ref(yk)),
                    ("oldCollector", Val::Ref(ok)),
                    ("parallelGCThreads", Val::Int(parallel)),
                    ("concurrentGCThreads", Val::Int(concurrent)),
                    ("usesDynamicGCThreads", Val::Bool(dynamic_gc_threads)),
                    ("isExplicitGCConcurrent", Val::Bool(explicit_concurrent)),
                    ("isExplicitGCDisabled", Val::Bool(explicit_disabled)),
                    ("pauseTarget", Val::Long(i64::MIN)),
                    ("gcTimeRatio", Val::Int(gc_time_ratio)),
                ],
            )
        }
        "jdk.GCHeapConfiguration" => {
            let (_, total, max) = runtime_memory(ctx);
            // gci i42/mx: HotSpot's `MinHeapSize` / `InitialHeapSize` /
            // `MaxHeapSize`, the figures the `gc,init` block prints
            // (`hotspot_heap_sizes`): `totalMemory()` (twice) moves as the heap
            // grows and `maxMemory()` is `-Xmx` less one survivor space on
            // Serial. And the narrow-oop state this process runs with
            // (`hotspot_narrow_oop_mode`): compressed oops are off by default,
            // where the row said `Non-zero disjoint base`.
            //
            // zs s2/ob: on ZGC, HotSpot 25's `MinHeapSize` / `InitialHeapSize`
            // / `MaxHeapSize` (local 25.0.3 `-Xmx256m`: `minSize = 8,0 MB`,
            // `initialSize = 254,0 MB`, `-Xms64m`: `minSize = 64,0 MB`) and
            // its narrow-oop mode without compressed oops, `32-bit`, whatever
            // the gci i42/mx switch says.
            let hotspot = jfr_gc_config_hotspot_enabled();
            let z = ctx.jfr_zgc_configuration();
            let [min, initial, max] = match z {
                Some(z) => [z.min_heap, z.initial_heap, z.max_heap],
                None if hotspot => ctx.hotspot_heap_sizes(),
                None => [total, total, max],
            };
            let compressed =
                z.is_none() && hotspot && cratonvm_types::narrow_oop::narrow_oops_enabled();
            let mode_name = if z.is_some() {
                "32-bit"
            } else if hotspot {
                hotspot_narrow_oop_mode(
                    compressed,
                    cratonvm_types::narrow_oop::narrow_base(),
                    cratonvm_types::narrow_oop::narrow_shift(),
                )
            } else {
                "Non-zero disjoint base"
            };
            let mode = BACKEND.with(vm, |b| {
                enum_key_auto(b, "jdk.types.NarrowOopMode", mode_name)
            });
            one(
                ts,
                vec![
                    ("minSize", Val::Long(min)),
                    ("maxSize", Val::Long(max)),
                    ("initialSize", Val::Long(initial)),
                    ("usesCompressedOops", Val::Bool(compressed)),
                    ("compressedOopsMode", Val::Ref(mode)),
                    ("objectAlignment", Val::Long(8)),
                    ("heapAddressBits", Val::Int(if compressed { 32 } else { 64 })),
                ],
            )
        }
        "jdk.CodeCacheConfiguration" if zgc_jfr_code_heap_rows(ctx) => {
            // zt t2/jf: on ZGC the sizes the `CodeHeap` pool beans report
            // (`jmx.rs` `non_heap_pool_from`): the one non-profiled heap's
            // `max` is the JIT's code-cache cap, the other two are empty
            // with no `max`. `0` where no cap is in force, as before.
            let cap = ctx
                .jit_code_cache_bytes()
                .and_then(|(_, cap)| cap)
                .map_or(0, |c| i64::try_from(c).unwrap_or(i64::MAX));
            one(
                ts,
                vec![
                    ("initialSize", Val::Long(0)),
                    ("reservedSize", Val::Long(cap)),
                    ("nonNMethodSize", Val::Long(0)),
                    ("profiledSize", Val::Long(0)),
                    ("nonProfiledSize", Val::Long(cap)),
                    ("expansionSize", Val::Long(0)),
                    ("minBlockLength", Val::Long(0)),
                    ("startAddress", Val::Long(0)),
                    ("reservedTopAddress", Val::Long(0)),
                ],
            )
        }
        "jdk.CodeCacheConfiguration" => {
            // The VM's JIT keeps one arena with no segmented heaps or addresses:
            // only the sizes it can state (none are reserved up front) are written.
            one(
                ts,
                vec![
                    ("initialSize", Val::Long(0)),
                    ("reservedSize", Val::Long(0)),
                    ("nonNMethodSize", Val::Long(0)),
                    ("profiledSize", Val::Long(0)),
                    ("nonProfiledSize", Val::Long(0)),
                    ("expansionSize", Val::Long(0)),
                    ("minBlockLength", Val::Long(0)),
                    ("startAddress", Val::Long(0)),
                    ("reservedTopAddress", Val::Long(0)),
                ],
            )
        }
        "jdk.CodeCacheStatistics" if zgc_jfr_code_heap_rows(ctx) => {
            // zt t2/jf: on ZGC one row per HotSpot code heap, named as the
            // `CodeHeap` pool beans are (`jdk.types.CodeBlobType`; the
            // generic row wrote the name as an inline string into a
            // constant-pool field, so it read back `null`), with the
            // non-profiled heap's `unallocatedCapacity` = the pool's `max -
            // used` (0 without a cap). The empty heaps write zeros.
            let n = COMPILE_COUNT.load(Ordering::Relaxed) as i32;
            let (used, cap) = ctx.jit_code_cache_bytes().unwrap_or((0, None));
            let free = cap.map_or(0, |c| {
                i64::try_from(c.saturating_sub(used)).unwrap_or(i64::MAX)
            });
            let keys = BACKEND.with(vm, |b| {
                CODE_HEAP_NAMES.map(|name| enum_key_auto(b, "jdk.types.CodeBlobType", name))
            });
            keys.iter()
                .zip(CODE_HEAP_NAMES)
                .map(|(&key, name)| {
                    let live = name == CODE_HEAP_NON_PROFILED;
                    let (count, unallocated) = if live { (n, free) } else { (0, 0) };
                    fields(vec![
                        ("startTime", Val::Long(ts)),
                        ("codeBlobType", Val::Ref(key)),
                        ("startAddress", Val::Long(0)),
                        ("reservedTopAddress", Val::Long(0)),
                        ("entryCount", Val::Int(count)),
                        ("methodCount", Val::Int(count)),
                        ("adaptorCount", Val::Int(0)),
                        ("unallocatedCapacity", Val::Long(unallocated)),
                        ("fullCount", Val::Int(0)),
                    ])
                })
                .collect()
        }
        "jdk.CodeCacheStatistics" => {
            let n = COMPILE_COUNT.load(Ordering::Relaxed) as i32;
            vec![fields(vec![
                ("startTime", Val::Long(ts)),
                ("codeBlobType", s("CodeCache")),
                ("startAddress", Val::Long(0)),
                ("reservedTopAddress", Val::Long(0)),
                ("entryCount", Val::Int(n)),
                ("methodCount", Val::Int(n)),
                ("adaptorCount", Val::Int(0)),
                ("unallocatedCapacity", Val::Long(0)),
                ("fullCount", Val::Int(0)),
            ])]
        }
        "jdk.DoubleFlag" => {
            let origin = BACKEND.with(vm, |b| {
                enum_key_auto(b, "jdk.types.FlagValueOrigin", "Default")
            });
            vec![fields(vec![
                ("startTime", Val::Long(ts)),
                ("name", s("G1ConcMarkStepDurationMillis")),
                ("value", Val::Double(10.0)),
                ("origin", Val::Ref(origin)),
            ])]
        }
        "jdk.StringFlag" => {
            let origin = BACKEND.with(vm, |b| {
                enum_key_auto(b, "jdk.types.FlagValueOrigin", "Default")
            });
            vec![fields(vec![
                ("startTime", Val::Long(ts)),
                ("name", s("CompileCommandFile")),
                ("value", s("")),
                ("origin", Val::Ref(origin)),
            ])]
        }
        "jdk.GCTLABConfiguration" => {
            // gci i43/tj: HotSpot's `UseTLAB`, `MinTLABSize` and
            // `TLABRefillWasteFraction` as the launcher validated them
            // (`VmConfig::tlab_flags`, lane i43/lf's accessor; defaults
            // true / 2048 / 64), where the row was the defaults.
            let (uses_tlabs, min_tlab_size, refill_waste_fraction) =
                if jfr_gc_flag_config_hotspot_enabled() {
                    ctx.hotspot_tlab_configuration()
                } else {
                    (true, 2048, 64)
                };
            one(
                ts,
                vec![
                    ("usesTLABs", Val::Bool(uses_tlabs)),
                    ("minTLABSize", Val::Long(min_tlab_size)),
                    ("tlabRefillWasteLimit", Val::Long(refill_waste_fraction)),
                ],
            )
        }
        "jdk.GCSurvivorConfiguration" => {
            // gci i43/tj: HotSpot's `MaxTenuringThreshold` /
            // `InitialTenuringThreshold` flags (defaults 15 / 7) as the
            // command line set them; the launcher already refuses a value
            // outside HotSpot's `0..=16` (gci i23/tn).
            let flags = if jfr_gc_flag_config_hotspot_enabled() {
                gc_command_line_flags(&ctx.jvm_input_arguments())
            } else {
                GcCommandLineFlags::default()
            };
            let as_int = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
            one(
                ts,
                vec![
                    (
                        "maxTenuringThreshold",
                        Val::Int(flags.max_tenuring_threshold.map_or(15, as_int)),
                    ),
                    (
                        "initialTenuringThreshold",
                        Val::Int(flags.initial_tenuring_threshold.map_or(7, as_int)),
                    ),
                ],
            )
        }
        "jdk.YoungGenerationConfiguration" => {
            // gci i43/tj: HotSpot's `NewSize` / `MaxNewSize` / `NewRatio`
            // after its young-generation ergonomics, from the `gc,init` heap
            // sizes and the command line, where the row was a third of
            // `totalMemory()` / `maxMemory()` and a constant ratio.
            let hotspot = if jfr_gc_flag_config_hotspot_enabled() {
                let (_, old) = ctx.jfr_collector_names();
                let flags = gc_command_line_flags(&ctx.jvm_input_arguments());
                hotspot_young_generation_configuration(old, ctx.hotspot_heap_sizes(), &flags)
            } else {
                None
            };
            let (min, max, ratio) = match hotspot {
                Some(row) => row,
                None => {
                    let (_, total, max) = runtime_memory(ctx);
                    (total / 3, max / 3, 2)
                }
            };
            one(
                ts,
                vec![
                    ("minSize", Val::Long(min)),
                    ("maxSize", Val::Long(max)),
                    ("newRatio", Val::Int(i32::try_from(ratio).unwrap_or(i32::MAX))),
                ],
            )
        }
        "jdk.GCHeapMemoryUsage" => {
            let (used, committed, max) = runtime_memory(ctx);
            one(
                ts,
                vec![
                    ("used", Val::Long(used)),
                    ("committed", Val::Long(committed)),
                    ("max", Val::Long(max)),
                ],
            )
        }
        "jdk.GCHeapMemoryPoolUsage" => {
            // gci i42/mx: one row per heap pool, as HotSpot.
            let rows = if jfr_gc_config_hotspot_enabled() {
                heap_pool_usage_rows(&ctx.gc_memory_pools())
            } else {
                Vec::new()
            };
            if !rows.is_empty() {
                return Some(
                    rows.into_iter()
                        .map(|(name, used, committed, max)| {
                            fields(vec![
                                ("startTime", Val::Long(ts)),
                                ("name", s(name)),
                                ("used", Val::Long(used)),
                                ("committed", Val::Long(committed)),
                                ("max", Val::Long(max)),
                            ])
                        })
                        .collect(),
                );
            }
            let (used, committed, max) = runtime_memory(ctx);
            one(
                ts,
                vec![
                    ("name", s("Java heap")),
                    ("used", Val::Long(used)),
                    ("committed", Val::Long(committed)),
                    ("max", Val::Long(max)),
                ],
            )
        }
        "jdk.BooleanFlag"
        | "jdk.UnsignedLongFlag"
        | "jdk.LongFlag"
        | "jdk.IntFlag"
        | "jdk.UnsignedIntFlag" => {
            let (young, old) = ctx.jfr_collector_names();
            let (_, total, max) = runtime_memory(ctx);
            let threads = std::thread::available_parallelism()
                .map(|n| n.get() as i64)
                .unwrap_or(1);
            let g1 = old == "G1Old";
            let serial = old == "SerialOld";
            let zgc = old == "ZGC Major";
            let _ = young;
            // gci i42/mx: `MaxHeapSize` is `-Xmx` and `InitialHeapSize` `-Xms`
            // (not `maxMemory()` / `totalMemory()`), `ParallelGCThreads`
            // HotSpot's per collector -- the values `jdk.GCConfiguration` and
            // `jdk.GCHeapConfiguration` now report.
            let hotspot = jfr_gc_config_hotspot_enabled();
            let (max, total, threads) = if hotspot {
                let parallel = hotspot_gc_threads(old, ctx.available_processor_count())
                    .map_or(threads, |(p, _)| i64::from(p));
                let [_, initial, max] = ctx.hotspot_heap_sizes();
                (max, initial, parallel)
            } else {
                (max, total, threads)
            };
            // gci i42/mx: and `UseCompressedOops` as this process runs.
            let compressed_oops = hotspot && cratonvm_types::narrow_oop::narrow_oops_enabled();
            // gci i43/tj (lane i43/lf hunk H-tj1): `UseTLAB` as the command
            // line set it, the value `jdk.GCTLABConfiguration` reports.
            let use_tlab =
                !jfr_gc_flag_config_hotspot_enabled() || ctx.hotspot_tlab_configuration().0;
            // (name, value) pairs of the flags the VM can state
            let rows: Vec<(&str, Val)> = match name {
                "jdk.BooleanFlag" => vec![
                    ("UseG1GC", Val::Bool(g1)),
                    ("UseSerialGC", Val::Bool(serial)),
                    ("UseZGC", Val::Bool(zgc)),
                    ("UseParallelGC", Val::Bool(false)),
                    ("UseCompressedOops", Val::Bool(compressed_oops)),
                    ("UseTLAB", Val::Bool(use_tlab)),
                    ("TieredCompilation", Val::Bool(true)),
                    ("UseStringDeduplication", Val::Bool(false)),
                ],
                "jdk.UnsignedLongFlag" => vec![
                    ("MaxHeapSize", Val::Long(max)),
                    ("InitialHeapSize", Val::Long(total)),
                ],
                "jdk.LongFlag" => vec![("MaxJavaStackTraceDepth", Val::Long(1024))],
                "jdk.IntFlag" => vec![("CICompilerCount", Val::Int(1))],
                _ => vec![("ParallelGCThreads", Val::Int(threads as i32))],
            };
            // gci i43/tj (page item 8, `origin`): HotSpot's origin per flag
            // (`hotspot_flag_origin`), where every row said `Ergonomic`.
            let origin_args = if jfr_gc_flag_config_hotspot_enabled() {
                Some(ctx.jvm_input_arguments())
            } else {
                None
            };
            rows.into_iter()
                .map(|(n, v)| {
                    let origin_name = match &origin_args {
                        Some(args) => hotspot_flag_origin(n, args, old, max),
                        None => "Ergonomic",
                    };
                    let origin = BACKEND.with(vm, |b| {
                        enum_key_auto(b, "jdk.types.FlagValueOrigin", origin_name)
                    });
                    fields(vec![
                        ("startTime", Val::Long(ts)),
                        ("name", s(n)),
                        ("value", v),
                        ("origin", Val::Ref(origin)),
                    ])
                })
                .collect()
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// the method sampler
// ---------------------------------------------------------------------------

const SAMPLER_CLASS: &str = "jdk/jfr/internal/CratonSampler";
/// HotSpot samples at most this many threads per period.
const MAX_SAMPLED_THREADS: usize = 5;

fn put_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}

fn utf8(v: &mut Vec<u8>, t: &str) {
    v.push(1);
    put_u16(v, t.len() as u16);
    v.extend_from_slice(t.as_bytes());
}

/// `final class CratonSampler implements Runnable { public void run() {
/// sampleLoop(); } private static native void sampleLoop(); }`, assembled by
/// hand: the VM needs one Java thread whose body is a native.
fn sampler_class_bytes() -> Vec<u8> {
    let mut v: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    put_u16(&mut v, 16);
    utf8(&mut v, SAMPLER_CLASS); // 1
    v.extend_from_slice(&[7, 0, 1]); // 2 Class #1
    utf8(&mut v, "java/lang/Object"); // 3
    v.extend_from_slice(&[7, 0, 3]); // 4
    utf8(&mut v, "java/lang/Runnable"); // 5
    v.extend_from_slice(&[7, 0, 5]); // 6
    utf8(&mut v, "<init>"); // 7
    utf8(&mut v, "()V"); // 8
    v.extend_from_slice(&[12, 0, 7, 0, 8]); // 9 NameAndType
    v.extend_from_slice(&[10, 0, 4, 0, 9]); // 10 Methodref Object.<init>
    utf8(&mut v, "run"); // 11
    utf8(&mut v, "sampleLoop"); // 12
    v.extend_from_slice(&[12, 0, 12, 0, 8]); // 13 NameAndType
    v.extend_from_slice(&[10, 0, 2, 0, 13]); // 14 Methodref sampleLoop
    utf8(&mut v, "Code"); // 15
    put_u16(&mut v, 0x0031); // public final super
    put_u16(&mut v, 2);
    put_u16(&mut v, 4);
    put_u16(&mut v, 1);
    put_u16(&mut v, 6); // implements Runnable
    put_u16(&mut v, 0); // fields
    put_u16(&mut v, 3); // methods
    let code_method = |v: &mut Vec<u8>, name: u16, max_stack: u16, code: &[u8]| {
        put_u16(v, 0x0001);
        put_u16(v, name);
        put_u16(v, 8);
        put_u16(v, 1);
        put_u16(v, 15);
        v.extend_from_slice(&((2 + 2 + 4 + code.len() + 2 + 2) as u32).to_be_bytes());
        put_u16(v, max_stack);
        put_u16(v, 1);
        v.extend_from_slice(&(code.len() as u32).to_be_bytes());
        v.extend_from_slice(code);
        put_u16(v, 0);
        put_u16(v, 0);
    };
    code_method(&mut v, 7, 1, &[0x2A, 0xB7, 0x00, 0x0A, 0xB1]); // <init>
    code_method(&mut v, 11, 0, &[0xB8, 0x00, 0x0E, 0xB1]); // run
    put_u16(&mut v, 0x010A); // private static native
    put_u16(&mut v, 12);
    put_u16(&mut v, 8);
    put_u16(&mut v, 0);
    put_u16(&mut v, 0); // class attributes
    v
}

/// `JVM.setMethodSamplingPeriod(type, periodMillis)`: start the sampler thread
/// when a period is set (`> 0`) and none runs; a zero period lets it end.
pub fn set_sampling_period(ctx: &mut dyn NativeContext, period_ms: i64) {
    let vm = ctx.vm_identity();
    let start = BACKEND.with(vm, |b| {
        b.sampler_period_ms = period_ms;
        b.sampler_stop = period_ms <= 0;
        if period_ms > 0 && !b.sampler_running {
            b.sampler_running = true;
            true
        } else {
            false
        }
    });
    if !start {
        return;
    }
    if !spawn_sampler(ctx) {
        BACKEND.with(vm, |b| b.sampler_running = false);
    }
}

fn spawn_sampler(ctx: &mut dyn NativeContext) -> bool {
    if ctx.class_id_by_name(SAMPLER_CLASS).is_none()
        && ctx
            .define_class_from_bytes(SAMPLER_CLASS, &sampler_class_bytes())
            .is_none()
    {
        return false;
    }
    let mut scope = NativeHandleScope::new(ctx);
    let Ok(Some(Value::Object(Some(runnable)))) =
        scope.new_object_initialized(SAMPLER_CLASS, "()V", &[])
    else {
        return false;
    };
    let runnable = scope.root(runnable);
    let name = scope.create_string("JFR Method Sampler");
    let name = scope.root(name);
    let (r, n) = (scope.get(&runnable), scope.get(&name));
    let Ok(Some(Value::Object(Some(thread)))) = scope.new_object_initialized(
        "java/lang/Thread",
        "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        &[Value::Object(Some(r)), Value::Object(Some(n))],
    ) else {
        return false;
    };
    let thread = scope.root(thread);
    let t = scope.get(&thread);
    let _ = scope.invoke_virtual(t, "setDaemon", "(Z)V", &[Value::Int(1)]);
    let t = scope.get(&thread);
    scope.thread_start(t).is_ok()
}

fn nat_sampler_loop(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let vm = ctx.vm_identity();
    let me = ctx.current_thread_object();
    crate::jfr::exclude_thread(&*ctx, me).ok();
    loop {
        let (period, stop) = BACKEND.with(vm, |b| (b.sampler_period_ms, b.sampler_stop));
        if stop || period <= 0 {
            break;
        }
        // HotSpot's floor is 1 ms; this VM walks other threads' stacks under a
        // frame-trace pause, so never sample faster than every 10 ms.
        let nap = Duration::from_millis(period.max(10) as u64);
        // A blocked region, or a stop-the-world waits for this thread forever.
        //
        // Interpreter round i3 wave 83, lane L6: the period is the VM's own
        // sampler idling, not a Java sleep. HotSpot's sampler is a VM thread
        // waiting inside the VM, and HotSpot's VM-service threads that are
        // Java threads (`Service Thread`, `Signal Dispatcher`, `Notification
        // Thread`) read RUNNABLE when idle: a VM wait, not TIMED_WAITING.
        ctx.begin_vm_wait_region();
        ctx.run_blocked_wait(&mut || std::thread::sleep(nap));
        ctx.end_blocking_region();
        if !any_recording() {
            continue;
        }
        // Re-read after the region: a moving GC may have relocated the thread object.
        let me = ctx.current_thread_object();
        sample_once(ctx, me);
    }
    BACKEND.with(vm, |b| b.sampler_running = false);
    Ok(None)
}

fn sample_once(ctx: &mut dyn NativeContext, me: ObjectRef) {
    let vm = ctx.vm_identity();
    let all = ctx.enumerate_threads(4096);
    let mut candidates: Vec<ObjectRef> = Vec::new();
    for t in all {
        if t != me && ctx.thread_run_state(t) == 1 {
            candidates.push(t);
        }
    }
    if candidates.is_empty() {
        return;
    }
    let cursor = BACKEND.with(vm, |b| {
        b.sample_cursor = b.sample_cursor.wrapping_add(MAX_SAMPLED_THREADS);
        b.sample_cursor
    });
    let n = candidates.len();
    let picked: Vec<ObjectRef> = (0..MAX_SAMPLED_THREADS.min(n))
        .map(|i| candidates[(cursor + i) % n])
        .collect();
    let mut scope = NativeHandleScope::new(ctx);
    let handles: Vec<_> = picked.iter().map(|&t| scope.root(t)).collect();
    let refs: Vec<ObjectRef> = handles.iter().map(|h| scope.get(h)).collect();
    SAMPLING_DUMP.store(true, std::sync::atomic::Ordering::Relaxed);
    scope.begin_thread_dump(&refs);
    let mut traces = Vec::new();
    for h in &handles {
        let t = scope.get(h);
        traces.push(scope.thread_stack_trace(t));
    }
    scope.end_thread_dump();
    SAMPLING_DUMP.store(false, std::sync::atomic::Ordering::Relaxed);
    let now = now_ticks();
    for (h, frames) in handles.iter().zip(traces) {
        if frames.is_empty() {
            continue;
        }
        let t = scope.get(h);
        let jt = register_thread(&mut *scope, t);
        let native = frames.first().is_some_and(|f| f.line_number == -2);
        let name = if native {
            "jdk.NativeMethodSample"
        } else {
            "jdk.ExecutionSample"
        };
        if BACKEND.with(vm, |b| admit(b, name, 0)).is_none() {
            continue;
        }
        let stack = intern_stack(&mut *scope, &frames, 0, 0);
        BACKEND.with(vm, |b| {
            let state = enum_key_auto(
                b,
                "jdk.types.ThreadState",
                if native {
                    "STATE_IN_NATIVE"
                } else {
                    "STATE_RUNNABLE"
                },
            );
            append(
                b,
                name,
                &fields(vec![
                    ("startTime", Val::Long(now)),
                    ("sampledThread", Val::Ref(jt)),
                    ("stackTrace", Val::Ref(stack)),
                    ("state", Val::Ref(state)),
                ]),
            );
        });
    }
}

/// Register the sampler's native (called with the rest of the backend).
pub fn register_sampler_native(registry: &mut NativeMethodRegistry) {
    registry.register_with_kind(
        SAMPLER_CLASS,
        "sampleLoop",
        "()V",
        nat_sampler_loop,
        NativeKind::Bridge,
    );
}

fn nat_set_method_sampling_period(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let ty = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let period = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let vm = ctx.vm_identity();
    let effective = BACKEND.with(vm, |b| {
        b.sampler_periods.insert(ty, period);
        // PlatformEventType routes a sampling type's enabled bit through its period.
        b.enabled.insert(ty, period > 0);
        b.sampler_periods
            .values()
            .copied()
            .filter(|p| *p > 0)
            .min()
            .unwrap_or(0)
    });
    set_sampling_period(ctx, effective);
    Ok(None)
}

/// Register the sampler's natives (under the caller's `Bridge` category).
pub fn register_sampler_natives(registry: &mut NativeMethodRegistry) {
    registry.register_with_kind(
        "jdk/jfr/internal/JVM",
        "setMethodSamplingPeriod",
        "(JJ)V",
        nat_set_method_sampling_period,
        NativeKind::Bridge,
    );
}

static REDEFINITION_ID: AtomicI64 = AtomicI64::new(0);

/// `Instrumentation.retransformClasses` finished for these classes (their
/// `jdk.types.Class` keys): `jdk.RetransformClasses` plus a `jdk.ClassRedefinition`
/// per class, all under one redefinition id.
pub fn retransformed(vm: usize, start_ticks: i64, class_refs: &[i64]) {
    let now = now_ticks();
    let id = REDEFINITION_ID.fetch_add(1, Ordering::Relaxed) + 1;
    BACKEND.with(vm, |b| {
        let t = ensure_vm_thread(b);
        if admit(b, "jdk.RetransformClasses", now - start_ticks).is_some() {
            append(
                b,
                "jdk.RetransformClasses",
                &fields(vec![
                    ("startTime", Val::Long(start_ticks)),
                    ("duration", Val::Long(now - start_ticks)),
                    ("eventThread", Val::Ref(t)),
                    ("stackTrace", Val::Ref(0)),
                    ("classCount", Val::Int(class_refs.len() as i32)),
                    ("redefinitionId", Val::Long(id)),
                ]),
            );
        }
        if admit(b, "jdk.ClassRedefinition", 0).is_some() {
            for c in class_refs {
                b.needs.classes.insert(*c);
                append(
                    b,
                    "jdk.ClassRedefinition",
                    &fields(vec![
                        ("startTime", Val::Long(now)),
                        ("redefinedClass", Val::Ref(*c)),
                        ("classModificationCount", Val::Int(1)),
                        ("redefinitionId", Val::Long(id)),
                    ]),
                );
            }
        }
    });
}

/// HotSpot writes a thread's last `jdk.ThreadCPULoad` as it exits: its CPU since
/// the previous sample (or since the VM started) over the wall time of that span.
pub fn thread_cpu_load_at_exit(ctx: &mut dyn NativeContext, thread: ObjectRef) {
    let vm = ctx.vm_identity();
    if BACKEND
        .with(vm, |b| admit(b, "jdk.ThreadCPULoad", 0))
        .is_none()
    {
        return;
    }
    let Some(stat) = proc_file("/proc/thread-self/stat") else {
        return;
    };
    let Some(after) = stat.rsplit_once(')').map(|x| x.1) else {
        return;
    };
    let parts: Vec<&str> = after.split_whitespace().collect();
    let (Some(u), Some(sy)) = (
        parts.get(11).and_then(|v| v.parse::<u64>().ok()),
        parts.get(12).and_then(|v| v.parse::<u64>().ok()),
    ) else {
        return;
    };
    let now = now_ticks();
    let jt = register_thread(ctx, thread);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let since_vm = (now_ms - crate::jfr_backend::vm_start_millis()).max(1) * 1_000_000;
    BACKEND.with(vm, |b| {
        let (u0, s0, t0) = b.thread_cpu.remove(&jt).unwrap_or((0, 0, now - since_vm));
        let ticks = ((now - t0).max(1) as f64) / 1e9 * 100.0;
        let user = ((u.saturating_sub(u0) as f64) / ticks).min(1.0) as f32;
        let system = ((sy.saturating_sub(s0) as f64) / ticks).min(1.0) as f32;
        append(
            b,
            "jdk.ThreadCPULoad",
            &fields(vec![
                ("startTime", Val::Long(now)),
                ("eventThread", Val::Ref(jt)),
                ("user", Val::Float(user)),
                ("system", Val::Float(system)),
            ]),
        );
    });
}

static SAMPLING_DUMP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True while the sampler's own stop-the-world stack dump runs: the pause that
/// waits for a busy thread there is routine, not a diagnostic worth a warning.
pub fn sampling_in_progress() -> bool {
    SAMPLING_DUMP.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_keys_split_into_class_method_descriptor() {
        assert_eq!(
            split_method_key("java.util.HashMap.put:(Ljava/lang/Object;)V"),
            (
                "java/util/HashMap".to_owned(),
                "put".to_owned(),
                "(Ljava/lang/Object;)V".to_owned()
            )
        );
        assert_eq!(
            split_method_key("p/C::m(I)I"),
            ("p/C".to_owned(), "m".to_owned(), "(I)I".to_owned())
        );
    }

    #[test]
    fn the_sampler_class_is_a_well_formed_runnable() {
        let b = sampler_class_bytes();
        assert_eq!(&b[..8], &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]);
        // constant pool count 16, then access flags public final super after the pool
        assert_eq!(u16::from_be_bytes([b[8], b[9]]), 16);
        let parsed = cratonvm_reader::read_class(&b).expect("parses");
        assert_eq!(&*parsed.this_class, SAMPLER_CLASS);
        assert_eq!(parsed.methods.len(), 3);
        assert!(parsed
            .interfaces
            .iter()
            .any(|i| &**i == "java/lang/Runnable"));
    }
}

#[cfg(test)]
mod gci_i11_go_tests {
    //! gci i11/go: the `jdk.ExecuteVMOperation` name of a collection.
    use super::*;

    fn cycle(old: bool, vm_operation: Option<&'static str>) -> GcCycle<'static> {
        GcCycle {
            gc_id: 1,
            name: "G1New",
            cause: "G1 Evacuation Pause",
            start_nanos: 0,
            duration_nanos: 0,
            heap_before: 0,
            committed_before: 0,
            heap_after: 0,
            committed_after: 0,
            max: 0,
            old,
            old_event: old,
            tenuring_threshold: None,
            phases: &[],
            metaspace_used: 0,
            metaspace_spaces: None,
            g1_type: Some("Normal"),
            vm_operation,
            longest_pause_nanos: None,
            zgc: None,
            pauses: &[],
            gc_thread: None,
            caller: None,
        }
    }

    /// gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1):
    /// `jdk.ExecuteVMOperation.caller` is the collection's requesting Java
    /// thread when the site names one (recorded under its tid, as
    /// `jdk.SystemGC`'s `eventThread`), else 0 as before.
    #[test]
    fn gci_i20_ob_execute_vm_operation_names_the_requesting_thread() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn gc_cycle(").expect("gc_cycle");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("(\"caller\", Val::Ref(caller))"));
        assert!(!body.contains("(\"caller\", Val::Ref(0))"));
        assert!(body.contains(".and_then(|who| caller_thread_ref(b, who))"));
        // The literal the VM builds carries the field (default None here).
        let mut c = cycle(false, Some("G1CollectForAllocation"));
        assert!(c.caller.is_none());
        c.caller = Some(JfrCallerThread {
            java_tid: 3,
            name: "main",
            os_tid: 7,
        });
        let mut b = Backend::default();
        let who = c.caller.as_ref().expect("set");
        assert_eq!(caller_thread_ref(&mut b, who), Some(3));
    }

    /// gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
    /// a non-collection VM-operation pause writes `gc_cycle`'s stop rows in
    /// `gc_cycle`'s order, under the next shared safepoint id, with the
    /// caller's operation name -- and no collection row.
    #[test]
    fn gci_i23pk_vm_operation_pause_writes_the_stop_rows_only() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src
            .find("\npub fn vm_operation_pause(")
            .expect("vm_operation_pause");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        let begin = body
            .find("admit(b, \"jdk.SafepointBegin\", 0)")
            .expect("begin");
        let op = body
            .find("admit(b, \"jdk.ExecuteVMOperation\", duration)")
            .expect("operation");
        let end = body.find("admit(b, \"jdk.SafepointEnd\", 0)").expect("end");
        assert!(begin < op && op < end);
        assert!(body.contains("b.safepoint_id += 1;"));
        assert!(body.contains("enum_key_auto(b, \"jdk.types.VMOperationType\", operation)"));
        for absent in [
            "jdk.GarbageCollection",
            "jdk.GCPhasePause",
            "jdk.GCHeapSummary",
        ] {
            assert!(!body.contains(absent), "{absent}");
        }
    }

    /// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 2):
    /// `gcThreshold` is HotSpot's default `capacity_until_GC` -- 21 MiB for
    /// a small program, the 40 %-free floor on the 64 KiB granule above it --
    /// and never 0 again.
    #[test]
    fn gci_i23pk_the_metaspace_gc_threshold_is_hotspots_default_policy() {
        let mib = 1024 * 1024;
        assert_eq!(metaspace_gc_threshold(0), 21 * mib);
        assert_eq!(metaspace_gc_threshold(-5), 21 * mib);
        assert_eq!(metaspace_gc_threshold(8 * mib), 21 * mib);
        // 12.6 MiB is the last size whose 40 %-free floor fits in 21 MiB.
        assert_eq!(metaspace_gc_threshold(21 * mib * 60 / 100), 21 * mib);
        // 30 MiB used: 50 MiB floor, already granule-aligned.
        assert_eq!(metaspace_gc_threshold(30 * mib), 50 * mib);
        // One byte more rounds up to the next granule.
        assert_eq!(
            metaspace_gc_threshold(30 * mib + 1),
            50 * mib + 64 * 1024
        );
        assert!(metaspace_gc_threshold(i64::MAX) > 0, "saturates, no panic");
        // gci i31/hp: the rows read `metaspace_threshold_step`, which applies
        // this policy after a full collection only.
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        assert!(src.contains(concat!("(\"gcThreshold\", Val::Long(", "threshold)),")));
        assert!(!src.contains(concat!(
            "Val::Long(metaspace_gc_threshold(",
            "c.metaspace_used))"
        )));
    }

    /// gci i31/hp (vi20 Batch 3 pk `S-threshold`): `capacity_until_GC` is
    /// state -- `MetaspaceSize` until the metadata passes it, raised by
    /// `compute_new_size` after a full collection only, kept by the next
    /// collection's Before-GC row.
    #[test]
    fn gci_i31hp_the_metaspace_threshold_moves_like_capacity_until_gc() {
        let mib = 1024 * 1024;
        let ms = 21 * mib;
        // pk23 S: ~14.2 MiB before the first (full) System.gc: Before GC is
        // `MetaspaceSize` (HotSpot 22020096), not `used / 0.6`.
        let used = 23658496 * 60 / 100;
        let (before, after) = metaspace_threshold_step(0, used, true);
        assert_eq!(before, ms);
        assert_eq!(after, 23658496);
        // The next collection's Before GC keeps what the full one left.
        assert_eq!(metaspace_threshold_step(after, used, false), (after, after));
        // A young collection never applies the 40 %-free policy.
        assert_eq!(metaspace_threshold_step(0, used, false), (ms, ms));
        // Small metadata: `MetaspaceSize` throughout.
        assert_eq!(metaspace_threshold_step(0, 8 * mib, true), (ms, ms));
        // Metadata past the threshold expands it at allocation time (granule).
        let (b, a) = metaspace_threshold_step(ms, 30 * mib + 1, false);
        assert_eq!(b, 30 * mib + 64 * 1024);
        assert_eq!(a, b);
        assert_eq!(metaspace_threshold_step(ms, 30 * mib, true).1, 50 * mib);
        // Never below what the previous collection left (no shrink modelled).
        assert_eq!(metaspace_threshold_step(50 * mib, mib, true), (50 * mib, 50 * mib));
        assert_eq!(metaspace_threshold_step(-1, -1, true), (ms, ms));
    }

    /// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 3):
    /// the `jdk.SystemGC` stack starts at the `Runtime.gc()` native frame
    /// (added when the capture, which lists the frames below the running
    /// native, lacks it; kept once when present) and lists innermost first;
    /// `system_gc_by` writes the id it is handed.
    #[test]
    fn gci_i23pk_the_system_gc_stack_starts_at_runtime_gc() {
        use cratonvm_native_api::StackTraceEntry;
        use std::sync::Arc;
        let at = |class: &str, method: &str| StackTraceEntry {
            class_name: Arc::from(class),
            method_name: Arc::from(method),
            method_descriptor: None,
            source_file: None,
            line_number: 7,
            byte_code_index: 0,
            class_id: None,
            method_index: None,
        };
        // Outermost first, as `capture_stack_trace` lists it.
        let frames =
            system_gc_stack_frames(vec![at("Main", "main"), at("java.lang.System", "gc")]);
        let names: Vec<(&str, &str)> = frames
            .iter()
            .map(|f| (f.class_name.as_ref(), f.method_name.as_ref()))
            .collect();
        assert_eq!(
            names,
            [
                ("java.lang.Runtime", "gc"),
                ("java.lang.System", "gc"),
                ("Main", "main")
            ]
        );
        assert_eq!(frames[0].line_number, -2, "a native frame");
        assert_eq!(frames[0].method_descriptor.as_deref(), Some("()V"));
        let present = system_gc_stack_frames(vec![
            at("Main", "main"),
            at("java/lang/Runtime", "gc"),
        ]);
        assert_eq!(present.len(), 2, "not doubled");
        assert_eq!(present[0].class_name.as_ref(), "java/lang/Runtime");
        assert_eq!(system_gc_stack_frames(Vec::new()).len(), 1);
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn system_gc_by(").expect("system_gc_by");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("(\"stackTrace\", Val::Ref(stack_trace)),"));
    }

    /// gci i12/ge (findings page item 2): `longestPause` is the caller's
    /// longest pause (capped at the duration), else the duration.
    #[test]
    fn gci_i12_ge_the_longest_pause_is_the_callers() {
        let mut c = cycle(true, Some("G1CollectFull"));
        c.duration_nanos = 9_000;
        assert_eq!(gc_longest_pause(&c), 9_000);
        c.longest_pause_nanos = Some(4_000);
        assert_eq!(gc_longest_pause(&c), 4_000);
        c.longest_pause_nanos = Some(12_000);
        assert_eq!(gc_longest_pause(&c), 9_000, "never above the sum");
    }

    /// gci i12/ge (findings page item 1): `jdk.SystemGC` is written once per
    /// request by `system_gc`, no longer by `gc_cycle` per pause.
    #[test]
    fn gci_i12_ge_system_gc_rows_are_per_request() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn gc_cycle(").expect("gc_cycle");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(!body.contains("admit(b, \"jdk.SystemGC\""));
        // gci i19/ob: the row is written by `system_gc_by` (`system_gc`
        // delegates with no caller).
        let start = src.find("\npub fn system_gc_by(").expect("system_gc_by");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("(\"invokedConcurrent\", Val::Bool(invoked_concurrent))"));
        let start = src.find("\npub fn system_gc(").expect("system_gc");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("system_gc_by(vm, None,"));
    }

    /// gci i19/ob (`gci-i12ge-...-review-findings-20261013` residual r1): the
    /// caller is recorded under its Java tid (the key `register_thread` uses),
    /// an existing record of that tid is kept, and a tid below 1 is refused so
    /// the row falls back to the VM thread.
    #[test]
    fn gci_i19_ob_system_gc_names_the_calling_java_thread() {
        let mut b = Backend::default();
        let main = JfrCallerThread {
            java_tid: 3,
            name: "main",
            os_tid: 4242,
        };
        assert_eq!(caller_thread_ref(&mut b, &main), Some(3));
        let rec = b.tables.threads.get(&3).expect("recorded");
        assert_eq!(rec.java_name, "main");
        assert_eq!(rec.os_name, "main");
        assert_eq!(rec.java_tid, 3);
        assert_eq!(rec.os_tid, 4242);
        assert!(b.needs.threads.contains(&3));
        // A record made earlier for the same tid wins (same key, same thread).
        let renamed = JfrCallerThread {
            java_tid: 3,
            name: "other",
            os_tid: 1,
        };
        assert_eq!(caller_thread_ref(&mut b, &renamed), Some(3));
        assert_eq!(b.tables.threads.get(&3).map(|r| r.java_name.as_str()), Some("main"));
        // Not a Java thread id: no record, the caller uses the VM thread.
        let none = JfrCallerThread {
            java_tid: 0,
            name: "x",
            os_tid: 0,
        };
        assert_eq!(caller_thread_ref(&mut b, &none), None);
        assert!(!b.tables.threads.contains_key(&0));
    }

    /// gci i19/ob (residual r3): the `jdk.ConcurrentModeFailure` writer exists,
    /// carries the cycle's `gcId`, and is admitted by name like every row.
    #[test]
    fn gci_i19_ob_concurrent_mode_failure_row_carries_the_gc_id() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src
            .find("\npub fn concurrent_mode_failure(")
            .expect("concurrent_mode_failure");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains("admit(b, \"jdk.ConcurrentModeFailure\", 0)"));
        assert!(body.contains("(\"gcId\", Val::Int(gc_id))"));
    }

    /// gci i12/ge (`gci-i11go-...-review-findings-20261013` item 3): the
    /// event kind is the caller's `old_event`, never a substring of the cause.
    #[test]
    fn gci_i12_ge_the_event_kind_is_the_callers() {
        let mut c = cycle(false, None);
        c.cause = "System.gc()";
        assert_eq!(gc_kind_event(&c), "jdk.YoungGarbageCollection");
        c.old_event = true;
        c.cause = "Diagnostic Command";
        assert_eq!(gc_kind_event(&c), "jdk.OldGarbageCollection");
        // `old` (the operation fallback's input) does not decide the kind.
        c.old = true;
        c.old_event = false;
        assert_eq!(gc_kind_event(&c), "jdk.YoungGarbageCollection");
    }

    /// zt t1/jf: HotSpot 25 ZGC's VM operation per pause name -- the young
    /// generation's for a minor, the old one's (after the both-generation
    /// mark start) for a major.
    #[test]
    fn zt1jf_zgc_pause_vm_operations_are_hotspots() {
        let major = [
            ("Pause Mark Start (Major)", "ZMarkStartYoungAndOld"),
            ("Pause Mark End", "ZMarkEndOld"),
            ("Pause Relocate Start", "ZRelocateStartOld"),
        ];
        for (pause, op) in major {
            assert_eq!(zgc_pause_vm_operation(pause, true), op);
        }
        let minor = [
            ("Pause Mark Start", "ZMarkStartYoung"),
            ("Pause Mark End", "ZMarkEndYoung"),
            ("Pause Relocate Start", "ZRelocateStartYoung"),
        ];
        for (pause, op) in minor {
            assert_eq!(zgc_pause_vm_operation(pause, false), op);
        }
    }

    /// zt t1/jf: one per-pause safepoint takes exactly one fresh id (a
    /// backend that is not recording writes nothing but still numbers it,
    /// as `gc_cycle` does), and `gc_cycle` takes the per-pause path only for
    /// a ZGC collection that asks for it: every other collector keeps its
    /// one collection-wide safepoint.
    #[test]
    fn zt1jf_per_pause_safepoints_are_zgc_only() {
        let mut b = Backend {
            safepoint_id: 41,
            ..Default::default()
        };
        safepoint_operation_rows(&mut b, 1, 0, "ZMarkEndOld", 0, 5);
        assert_eq!(b.safepoint_id, 42);
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn gc_cycle(").expect("gc_cycle");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains(
            "!c.pauses.is_empty() && c.zgc.is_some_and(|z| z.pause_vm_operations)"
        ));
        assert!(body.contains("if !per_pause_safepoints && admit(b, \"jdk.SafepointBegin\", 0)"));
        assert!(body.contains("if !per_pause_safepoints && admit(b, \"jdk.SafepointEnd\", 0)"));
        // The G1 literal of this module has no ZGC half: the old path.
        let c = cycle(false, None);
        assert!(c.zgc.is_none());
    }

    #[test]
    fn gci_i11_go_the_callers_operation_name_wins() {
        assert_eq!(
            gc_vm_operation(&cycle(false, Some("G1CollectForAllocation"))),
            "G1CollectForAllocation"
        );
        assert_eq!(gc_vm_operation(&cycle(true, Some("G1CollectFull"))), "G1CollectFull");
        // No name (ZGC): the historical spelling.
        assert_eq!(gc_vm_operation(&cycle(false, None)), "Collect");
        assert_eq!(gc_vm_operation(&cycle(true, None)), "GC_HeapInspection");
    }

    /// zt t2/jf: the ZGC code-heap rows name exactly the JMX `CodeHeap`
    /// pools (the MXBean and the JFR row describe the same heaps), the JIT's
    /// code in the non-profiled one; and the generic row's inline-string
    /// `codeBlobType` (a constant-pool field: it read `null`) is not what the
    /// ZGC arm writes.
    #[test]
    fn zt2jf_code_heap_rows_name_the_jmx_code_heap_pools() {
        let pools = cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES;
        for name in CODE_HEAP_NAMES {
            assert!(pools.contains(&name), "{name}");
        }
        assert_eq!(CODE_HEAP_NAMES[2], CODE_HEAP_NON_PROFILED);
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let arm = src
            .find("\"jdk.CodeCacheStatistics\" if zgc_jfr_code_heap_rows(ctx) => {")
            .expect("the ZGC arm");
        let body: String = src[arm..].chars().take(2_000).collect();
        assert!(body.contains("(\"codeBlobType\", Val::Ref(key)),"));
        assert!(body.contains("enum_key_auto(b, \"jdk.types.CodeBlobType\", name)"));
    }

    /// zt t4/x3: the young-span change has its own kill; default ON.
    #[test]
    fn zt4x3_the_major_young_span_switch_follows_an_override() {
        use cratonvm_types::flags::with_thread_overrides;
        let key = "CRATONVM_ZGC_JFR_MAJOR_YOUNG_AT_MARK_END";
        with_thread_overrides(&[(key, None)], || {
            assert!(zgc_jfr_major_young_at_mark_end_enabled(), "default ON")
        });
        with_thread_overrides(&[(key, Some("0"))], || {
            assert!(!zgc_jfr_major_young_at_mark_end_enabled(), "=0 kill")
        });
    }

    /// zt t5/jf: a `jdk.SystemGC` row ends with the ZGC collection recorded
    /// inside it (taken once); a collection outside the request, or none,
    /// leaves the writer's duration.
    #[test]
    fn zt5jf_a_system_gc_row_ends_with_its_collection() {
        let d = |b: &mut Backend| zgc_system_gc_duration(b, 1_000, 30_000);
        let mut b = Backend::default();
        assert_eq!(d(&mut b), 30_000, "none on record");
        b.last_zgc_collection_end = Some(5_000);
        assert_eq!(d(&mut b), 4_000);
        assert_eq!(b.last_zgc_collection_end, None, "taken");
        assert_eq!(d(&mut b), 30_000, "taken once");
        b.last_zgc_collection_end = Some(500);
        assert_eq!(d(&mut b), 30_000, "before the request");
        b.last_zgc_collection_end = Some(40_000);
        assert_eq!(d(&mut b), 30_000, "after it");
    }

    /// zt t3/jf (zt2x1 item 4): a major's young event ends at its mark-end
    /// pause, inside the cycle; a minor, or a major without named rows,
    /// spans the cycle.
    #[test]
    fn zt3jf_a_majors_young_event_ends_at_its_mark_end() {
        let rows = [
            ("Pause Mark Start (Major)", 1_000u64, 100u64),
            ("Pause Mark End", 1_100, 600),
            ("Pause Relocate Start", 1_700, 300),
        ];
        assert_eq!(zgc_young_span(&rows, 0, (1_000, 1_000), true), (1_000, 100));
        assert_eq!(zgc_young_span(&rows, 100, (900, 1_000), true), (900, 100));
        assert_eq!(zgc_young_span(&rows, 0, (1_000, 1_000), false), (1_000, 1_000));
        assert_eq!(zgc_young_span(&[], 0, (1_000, 1_000), true), (1_000, 1_000));
        // A row before the span start clamps to 0, never negative.
        assert_eq!(zgc_young_span(&rows, 0, (5_000, 1_000), true), (5_000, 0));
    }

    /// zt t2/jf (R6): a ZGC collection written as one safepoint names the
    /// operation that opens its cycle; `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS=0`
    /// (`pause_vm_operations: false`) keeps `Collect`; a caller's name wins.
    #[test]
    fn zt2jf_a_one_row_zgc_collection_names_its_opening_operation() {
        let z = |major: bool, on: bool| ZgcCycleJfr {
            major,
            relocation_set: None,
            relocation_groups: &[],
            uncommit: None,
            young_tenuring_threshold: None,
            reference_counts: None,
            cycle_span: None,
            cycle_heap_before: None,
            metaspace_chunk_free_lists: false,
            pause_vm_operations: on,
        };
        let (major, minor, off) = (z(true, true), z(false, true), z(true, false));
        let mut c = cycle(false, None);
        c.zgc = Some(&major);
        assert_eq!(gc_vm_operation(&c), "ZMarkStartYoungAndOld");
        c.zgc = Some(&minor);
        assert_eq!(gc_vm_operation(&c), "ZMarkStartYoung");
        c.zgc = Some(&off);
        assert_eq!(gc_vm_operation(&c), "Collect");
        c.zgc = Some(&major);
        c.vm_operation = Some("HeapDumper");
        assert_eq!(gc_vm_operation(&c), "HeapDumper");
    }
}

/// gci i42/mx: the periodic GC configuration events' HotSpot values.
#[cfg(test)]
mod gci_i42_mx_tests {
    use super::{heap_pool_usage_rows, hotspot_gc_threads};

    #[test]
    fn gci_i42_mx_gc_thread_flags_are_hotspots_per_collector() {
        // Serial: no worker gang, both flags 0, whatever the CPU count.
        assert_eq!(hotspot_gc_threads("SerialOld", 8), Some((0, 0)));
        assert_eq!(hotspot_gc_threads("SerialOld", 64), Some((0, 0)));
        // G1: ParallelGCThreads = ncpus up to 8, then 8 + (n - 8) * 5 / 8;
        // ConcGCThreads = max((P + 2) / 4, 1).
        assert_eq!(hotspot_gc_threads("G1Old", 1), Some((1, 1)));
        assert_eq!(hotspot_gc_threads("G1Old", 4), Some((4, 1)));
        assert_eq!(hotspot_gc_threads("G1Old", 6), Some((6, 2)));
        assert_eq!(hotspot_gc_threads("G1Old", 8), Some((8, 2)));
        assert_eq!(hotspot_gc_threads("G1Old", 16), Some((13, 3)));
        assert_eq!(hotspot_gc_threads("G1Old", 32), Some((23, 6)));
        assert_eq!(hotspot_gc_threads("G1Old", 72), Some((48, 12)));
        // A non-positive count reads as one processor.
        assert_eq!(hotspot_gc_threads("G1Old", 0), Some((1, 1)));
        // Any other collector keeps the caller's figures.
        assert_eq!(hotspot_gc_threads("ZGC Major", 8), None);
        assert_eq!(hotspot_gc_threads("N/A", 8), None);
    }

    #[test]
    fn gci_i42_mx_heap_pool_usage_is_one_row_per_heap_pool() {
        let pool = |name: &'static str, used: i64, committed: i64, max: i64| {
            cratonvm_native_api::GcMemoryPoolInfo {
                name,
                init: -1,
                used,
                committed,
                max,
                collection_used: -1,
                collection_committed: -1,
                manager_names: &[],
            }
        };
        let rows = heap_pool_usage_rows(&[
            pool("G1 Eden Space", 4 << 20, 24 << 20, -1),
            pool("G1 Survivor Space", 1 << 20, 1 << 20, -1),
            pool("G1 Old Gen", 9 << 20, 40 << 20, 256 << 20),
        ]);
        assert_eq!(
            rows,
            vec![
                ("G1 Eden Space", 4 << 20, 24 << 20, -1),
                ("G1 Survivor Space", 1 << 20, 1 << 20, -1),
                ("G1 Old Gen", 9 << 20, 40 << 20, 256 << 20),
            ]
        );
        // No pools described: no rows (the caller writes its historical row).
        assert!(heap_pool_usage_rows(&[]).is_empty());
    }

    /// The periodic writer reads the switch and the new context answers at
    /// each of the four events it changes.
    #[test]
    fn gci_i42_mx_the_periodic_writer_uses_the_hotspot_answers() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn periodic(").expect("periodic");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.matches("jfr_gc_config_hotspot_enabled()").count() >= 4);
        assert!(body.contains("heap_pool_usage_rows(&ctx.gc_memory_pools())"));
        assert!(body.contains("ctx.explicit_gc_flags()"));
        assert!(body.matches("ctx.hotspot_heap_sizes()").count() >= 2);
        assert!(body.contains("(\"isExplicitGCDisabled\", Val::Bool(explicit_disabled))"));
        assert!(body.contains("(\"usesCompressedOops\", Val::Bool(compressed))"));
        assert!(body.contains("(\"UseCompressedOops\", Val::Bool(compressed_oops))"));
    }

    #[test]
    fn gci_i42_mx_narrow_oop_mode_is_hotspots_spelling() {
        use super::hotspot_narrow_oop_mode;
        // Off (this VM's default; HotSpot `-XX:-UseCompressedOops`): base and
        // shift 0, `UnscaledNarrowOop`.
        assert_eq!(hotspot_narrow_oop_mode(false, 0, 0), "32-bit");
        assert_eq!(hotspot_narrow_oop_mode(false, 1 << 35, 3), "32-bit");
        assert_eq!(hotspot_narrow_oop_mode(true, 0, 0), "32-bit");
        assert_eq!(hotspot_narrow_oop_mode(true, 0, 3), "Zero based");
        assert_eq!(hotspot_narrow_oop_mode(true, 1 << 35, 3), "Non-zero disjoint base");
        assert_eq!(hotspot_narrow_oop_mode(true, 0x8_0000_0000 + 0x1000, 3), "Non-zero based");
    }
}

/// gci i43/tj: the young / survivor / TLAB configuration events and
/// `gcTimeRatio` answer HotSpot 25's flag values.
#[cfg(test)]
mod gci_i43_tj_tests {
    use super::{
        gc_command_line_flags, hotspot_gc_time_ratio, hotspot_size_value,
        hotspot_young_generation_configuration, GcCommandLineFlags,
    };

    const M: i64 = 1024 * 1024;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gci_i43_tj_size_values_parse_as_hotspots_atojulong() {
        assert_eq!(hotspot_size_value("64m"), Some(64 << 20));
        assert_eq!(hotspot_size_value("64M"), Some(64 << 20));
        assert_eq!(hotspot_size_value("1G"), Some(1 << 30));
        assert_eq!(hotspot_size_value("2k"), Some(2048));
        assert_eq!(hotspot_size_value("5"), Some(5));
        assert_eq!(hotspot_size_value("0x10"), Some(16));
        assert_eq!(hotspot_size_value("18446744073709551615"), Some(u64::MAX));
        assert_eq!(hotspot_size_value("20000000t"), None, "overflow");
        assert_eq!(hotspot_size_value(""), None);
        assert_eq!(hotspot_size_value("m"), None);
        assert_eq!(hotspot_size_value("12x"), None);
        assert_eq!(hotspot_size_value("12mb"), None);
        assert_eq!(hotspot_size_value("+5"), None);
    }

    /// Last occurrence wins; `-Xmn` sets both young bounds at its position;
    /// text that does not parse leaves the flag at its default.
    #[test]
    fn gci_i43_tj_command_line_flags_are_read_in_order() {
        let f = gc_command_line_flags(&args(&[
            "-Xmx256m",
            "-Xmn32m",
            "-XX:NewSize=16m",
            "-XX:MaxTenuringThreshold=5",
            "-XX:+UseG1GC",
            "-XX:GCTimeRatio=bad",
            "-XX:NewRatio=3",
            "-Xmn",
            "-Dx=-XX:InitialTenuringThreshold=3",
        ]));
        assert_eq!(
            f,
            GcCommandLineFlags {
                new_size: Some(16 << 20),
                max_new_size: Some(32 << 20),
                new_ratio: Some(3),
                max_tenuring_threshold: Some(5),
                initial_tenuring_threshold: None,
                gc_time_ratio: None,
                g1_heap_region_size: None,
            }
        );
        assert_eq!(gc_command_line_flags(&[]), GcCommandLineFlags::default());
    }

    /// HotSpot Serial's `GenArguments` arithmetic: `-Xms256m -Xmx256m` fixes
    /// the young generation at `align_down(256M / 3, 64K)` = 89 456 640 (the
    /// i42/mx page's figure); a smaller `-Xms` starts it at a third of that;
    /// `-Xmn` and `NewRatio` move both.
    #[test]
    fn gci_i43_tj_serial_young_configuration_is_genarguments() {
        let none = GcCommandLineFlags::default();
        let fixed = [256 * M, 256 * M, 256 * M];
        assert_eq!(
            hotspot_young_generation_configuration("SerialOld", fixed, &none),
            Some((89_456_640, 89_456_640, 2))
        );
        assert_eq!(
            hotspot_young_generation_configuration("SerialOld", [8 * M, 64 * M, 256 * M], &none),
            Some((22_347_776, 89_456_640, 2))
        );
        let xmn = gc_command_line_flags(&args(&["-Xmn64m"]));
        assert_eq!(
            hotspot_young_generation_configuration("SerialOld", fixed, &xmn),
            Some((64 * M, 64 * M, 2))
        );
        let ratio = gc_command_line_flags(&args(&["-XX:NewRatio=3"]));
        assert_eq!(
            hotspot_young_generation_configuration("SerialOld", fixed, &ratio),
            Some((64 * M, 64 * M, 3))
        );
        // A `MaxNewSize` at or above the heap leaves room for one granule of
        // old generation.
        let huge = gc_command_line_flags(&args(&["-Xmn256m"]));
        assert_eq!(
            hotspot_young_generation_configuration("SerialOld", fixed, &huge),
            Some((256 * M - 65_536, 256 * M - 65_536, 2))
        );
    }

    /// HotSpot G1 writes `NewSize` as given (default
    /// `ScaleForWordSize(1M)`) and the `MaxNewSize` `G1YoungGenSizer` set
    /// (at the defaults 60 % of the regions: 153 MiB at `-Xmx256m`, HotSpot
    /// 25.0.3 row `tj43_c_g1`; never `Long.MIN_VALUE`), a command-line
    /// `MaxNewSize` raised to a larger `NewSize`. ZGC keeps the caller's row.
    #[test]
    fn gci_i43_tj_g1_young_configuration_is_the_flags() {
        let heap = [256 * M, 256 * M, 256 * M];
        assert_eq!(
            hotspot_young_generation_configuration("G1Old", heap, &GcCommandLineFlags::default()),
            Some((1_363_144, 160_432_128, 2))
        );
        let xmn = gc_command_line_flags(&args(&["-Xmn64m"]));
        assert_eq!(
            hotspot_young_generation_configuration("G1Old", heap, &xmn),
            Some((64 * M, 64 * M, 2))
        );
        let crossed = gc_command_line_flags(&args(&["-XX:NewSize=32m", "-XX:MaxNewSize=16m"]));
        assert_eq!(
            hotspot_young_generation_configuration("G1Old", heap, &crossed),
            Some((32 * M, 32 * M, 2))
        );
        assert_eq!(
            hotspot_young_generation_configuration("ZGC Major", heap, &xmn),
            None
        );
    }

    /// gci i47w/j43: G1's `maxSize` is `G1YoungGenSizer`'s ergonomic
    /// `MaxNewSize` (`adjust_max_new_size`) for every sizer kind, a whole
    /// number of regions; HotSpot 25.0.3 answered 160 432 128 at
    /// `-XX:+UseG1GC -Xms256m -Xmx256m` where this wrote `Long.MIN_VALUE`.
    #[test]
    fn gci_i47w_j43_g1_young_max_is_the_sizers_max_new_size() {
        use super::hotspot_g1_max_new_size as max_new;
        const U: u64 = 1024 * 1024;
        let f = |list: &[&str]| gc_command_line_flags(&args(list));
        // Defaults: 60 % of 256 one-MiB regions, rounded down.
        assert_eq!(max_new(256 * U, 1_363_144, &f(&[])), 153 * U);
        // A -Xmx that is not a multiple of the 2 MiB heap alignment.
        assert_eq!(max_new(255 * U, 1_363_144, &f(&[])), 153 * U);
        // NewRatio alone fixes the length: 256 / 4.
        assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:NewRatio=3"])), 64 * U);
        // NewSize with NewRatio: NewSize wins (the sizer's warning case);
        // NewSize alone keeps the 60 % maximum unless it is larger.
        let ns16 = f(&["-XX:NewRatio=3", "-XX:NewSize=16m"]);
        assert_eq!(max_new(256 * U, 16 * U, &ns16), 153 * U);
        assert_eq!(max_new(256 * U, 200 * U, &f(&["-XX:NewSize=200m"])), 200 * U);
        // A command-line MaxNewSize is whole regions, at least one, and
        // raised to a larger (even default) NewSize first.
        assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:MaxNewSize=35127296"])), 33 * U);
        assert_eq!(max_new(256 * U, 1_363_144, &f(&["-XX:MaxNewSize=1m"])), U);
        let gt = f(&["-XX:NewSize=64m", "-XX:MaxNewSize=32m"]);
        assert_eq!(max_new(256 * U, 64 * U, &gt), 64 * U);
        // Region size: clamp(MaxHeapSize / 2048, 1M, 32M), or the flag.
        assert_eq!(max_new(1024 * U, 1_363_144, &f(&[])), 614 * U);
        assert_eq!(max_new(16384 * U, 1_363_144, &f(&[])), 1228 * 8 * U);
        let r4 = f(&["-XX:G1HeapRegionSize=4m"]);
        assert_eq!(max_new(256 * U, 1_363_144, &r4), 38 * 4 * U);
        // Through the event's answer.
        assert_eq!(
            hotspot_young_generation_configuration("G1Old", [256 * M, 256 * M, 256 * M], &ns16),
            Some((16 * M, 153 * M, 3))
        );
    }

    /// `GCTimeRatio`: Serial keeps the flag (99 by default); G1 replaces a
    /// default or zero value by 12 (`G1Arguments::initialize`).
    #[test]
    fn gci_i43_tj_gc_time_ratio_is_hotspots_per_collector() {
        assert_eq!(hotspot_gc_time_ratio("SerialOld", None), Some(99));
        assert_eq!(hotspot_gc_time_ratio("SerialOld", Some(4)), Some(4));
        assert_eq!(hotspot_gc_time_ratio("G1Old", None), Some(12));
        assert_eq!(hotspot_gc_time_ratio("G1Old", Some(0)), Some(12));
        assert_eq!(hotspot_gc_time_ratio("G1Old", Some(19)), Some(19));
        assert_eq!(hotspot_gc_time_ratio("ZGC Major", None), None);
    }

    /// The periodic writer reads the switch and the new answers at each of
    /// the four events it changes.
    #[test]
    fn gci_i43_tj_the_periodic_writer_uses_the_flag_answers() {
        let src = include_str!("jfr_vm_more.rs").replace("\r\n", "\n");
        let start = src.find("\npub fn periodic(").expect("periodic");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        assert!(body.matches("jfr_gc_flag_config_hotspot_enabled()").count() >= 4);
        assert!(body.contains("hotspot_gc_time_ratio(old, flags.gc_time_ratio)"));
        assert!(body.contains("(\"gcTimeRatio\", Val::Int(gc_time_ratio))"));
        assert!(body.contains("ctx.hotspot_tlab_configuration()"));
        assert!(body.contains("flags.max_tenuring_threshold.map_or(15, as_int)"));
        assert!(body.contains("flags.initial_tenuring_threshold.map_or(7, as_int)"));
        assert!(body.contains(
            "hotspot_young_generation_configuration(old, ctx.hotspot_heap_sizes(), &flags)"
        ));
        assert!(body.contains("Some(args) => hotspot_flag_origin(n, args, old, max),"));
    }

    /// Item 8's `origin`: `Command line` for what the options set,
    /// `Ergonomic` for what HotSpot's argument processing sets, `Default`
    /// otherwise.
    #[test]
    fn gci_i43_tj_flag_origins_are_hotspots() {
        use super::hotspot_flag_origin as origin;
        let g1 = args(&["-XX:+UseG1GC", "-Xms256m", "-Xmx256m", "-XX:-UseCompressedOops"]);
        assert_eq!(origin("UseG1GC", &g1, "G1Old", 256 * M), "Command line");
        assert_eq!(origin("UseSerialGC", &g1, "G1Old", 256 * M), "Default");
        assert_eq!(origin("MaxHeapSize", &g1, "G1Old", 256 * M), "Command line");
        assert_eq!(origin("InitialHeapSize", &g1, "G1Old", 256 * M), "Command line");
        assert_eq!(origin("UseCompressedOops", &g1, "G1Old", 256 * M), "Command line");
        // gci i47w/j43: `FLAG_SET_DEFAULT` in `G1Arguments::initialize`
        // (HotSpot 25.0.3 row `tj43_c_g1`).
        assert_eq!(origin("ParallelGCThreads", &g1, "G1Old", 256 * M), "Default");
        assert_eq!(origin("CICompilerCount", &g1, "G1Old", 256 * M), "Ergonomic");
        assert_eq!(origin("UseTLAB", &g1, "G1Old", 256 * M), "Default");
        assert_eq!(origin("MaxJavaStackTraceDepth", &g1, "G1Old", 256 * M), "Default");

        let gen = args(&["-XX:+UseGenerationalGC", "-Xmx64m", "-XX:-UseTLAB"]);
        assert_eq!(origin("UseSerialGC", &gen, "SerialOld", 64 * M), "Command line");
        assert_eq!(origin("InitialHeapSize", &gen, "SerialOld", 64 * M), "Ergonomic");
        assert_eq!(origin("ParallelGCThreads", &gen, "SerialOld", 64 * M), "Default");
        assert_eq!(origin("UseTLAB", &gen, "SerialOld", 64 * M), "Command line");
        assert_eq!(origin("UseCompressedOops", &gen, "SerialOld", 64 * M), "Ergonomic");
        assert_eq!(origin("UseCompressedOops", &gen, "SerialOld", 32 << 30), "Default");

        // Nothing given: the selected collector is ergonomic; a flag whose
        // name only starts like another's is not it.
        let none = args(&["-XX:UseG1GCFoo=1"]);
        assert_eq!(origin("UseSerialGC", &none, "SerialOld", 64 * M), "Ergonomic");
        assert_eq!(origin("UseG1GC", &none, "SerialOld", 64 * M), "Default");
        assert_eq!(origin("MaxHeapSize", &none, "SerialOld", 64 * M), "Ergonomic");
    }
}

/// gci i33/ms: `jdk.MetaspaceSummary`'s spaces.
#[cfg(test)]
mod gci_i33_ms_tests {
    use super::metaspace_summary_spaces;

    #[test]
    fn gci_i33ms_metaspace_summary_splits_the_class_space() {
        const K: i64 = 1024;
        let g = 1024 * K * K;
        let [metaspace, data, class] =
            metaspace_summary_spaces(5_000, Some([320 * K, 1088 * K * K, 1_200, 128 * K, g]));
        assert_eq!(metaspace, (320 * K, 5_000, 1088 * K * K));
        assert_eq!(data, (192 * K, 3_800, 64 * K * K));
        assert_eq!(class, (128 * K, 1_200, g));
        // The no-class-space shape: the pre-i33 rows.
        assert_eq!(
            metaspace_summary_spaces(5_000, None),
            [(5_000, 5_000, 5_000), (5_000, 5_000, 5_000), (0, 0, 0)]
        );
    }
}
