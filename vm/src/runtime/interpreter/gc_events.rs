// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-collection accounting shared by every GC door in `gc_and_alloc`.
//!
//! Split out of `gc_and_alloc.rs` (gen r4/plumbing, 2026-09-23) so the
//! observability half of a collection -- the VM's collection ordinal, the JFR
//! GC events and the `-Xlog:gc` line -- lives in one place that the door
//! functions only call into.

use super::*;

// ---------------------------------------------------------------------------
// Per-collection accounting: the cycle ordinal, JFR GC events, `-Xlog:gc`
// ---------------------------------------------------------------------------
//
// gen r4/plumbing (2026-09-23). Until this section existed the three GC doors
// disagreed about what a collection leaves behind:
//
// * `gc_cycle_count` was bumped by `maybe_gc` and `maybe_gc_forced` but not by
//   `force_gc_from_native`, so `System.gc()` collections were uncounted;
// * exactly ONE of the six initiator arms (`maybe_gc`, single-threaded) emitted
//   JFR events, with `gcId` the literal `1` on every collection, the name
//   `"YoungGC"`, the cause `"Allocation Failure"` whatever the cause, G1's
//   tenuring threshold `15`, a heap summary whose `committed` was `used` and
//   whose `max` was `2 * used`, and no `jdk.OldGarbageCollection` ever
//   (`docs/feature-designs/generative-gc-architecture.md`);
// * `-Xlog:gc` initialised the unified logger and then nothing ever logged to
//   it: `unified_logging::gc_info` had no caller, so `-Xlog:gc` — the flag a
//   HotSpot user reaches for first — printed nothing, on every backend.
//
// Every initiator arm now brackets its collection with `gc_event_start` /
// `gc_event_finish`, so the three cannot drift again. The default path pays
// one relaxed-ish load (`cratonvm_jfr::is_enabled`) and one `OnceLock` read
// (the unified logger) per COLLECTION, and nothing per allocation.

/// Which VM door initiated a collection — the `cause` a JFR
/// `jdk.GarbageCollection` event and an `-Xlog:gc` line carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GcDoor {
    /// `maybe_gc`: the young-occupancy trigger (`needs_gc`) or a latched
    /// `gc_requested`.
    AllocationThreshold,
    /// `maybe_gc_forced`: an allocation that failed outright. (The forced
    /// door's pause runs on `AllocationThreshold` for an occupancy verdict,
    /// on the requester's door for G1's full-collection compaction since gci
    /// i10/dl, and on [`Self::G1CompactionPause`] for G1's recovery
    /// compaction since gci i11/go: `forced_site_door`.)
    AllocationFailure,
    /// `force_gc_from_native`: `System.gc()`, `Runtime.gc()`, JMX `gc()`.
    SystemGc,
    /// `force_gc_for_class_metadata`: the `-XX:MaxMetaspaceSize` refusal's
    /// full collection. Not an explicit GC (gc-common w8-g; HotSpot's
    /// `GCCause::_metadata_GC_threshold`).
    MetadataThreshold,
    /// `jcmd GC.run` (`gc_and_alloc::run_diagnostic_command_gc`): HotSpot's
    /// `GCCause::_dcmd_gc_run`, a full collection that
    /// `-XX:+DisableExplicitGC` does not gate. gci i2/oo.
    DiagnosticCommand,
    /// The full collection a live-only class histogram runs first
    /// (`jcmd GC.class_histogram` without `-all`, `jmap -histo:live`):
    /// HotSpot's `VM_GC_HeapInspection`, `GCCause::_heap_inspection`. gci
    /// i9/dc (`gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017`; it was
    /// labelled `Diagnostic Command`).
    HeapInspection,
    /// The full collection a live-only heap dump runs first (`GC.heap_dump`
    /// without `-all`, `jmap -dump:live`): HotSpot's `VM_HeapDumper`,
    /// `GCCause::_heap_dump`. gci i9/dc.
    HeapDump,
    /// G1 only: the compacting recovery pause of the allocation-failure
    /// ladder (`g1_last_ditch_recovery_pause`, site
    /// `g1-last-ditch-compaction`), the collection HotSpot G1 runs under
    /// `GCCauseSetter(.., GCCause::_g1_compaction_pause)` in
    /// `satisfy_failed_allocation_helper` before it throws
    /// `OutOfMemoryError`. An allocation failure in every other respect: it
    /// samples productivity and is judged for the overhead streak
    /// (`door_samples_productivity`) and it is a forced ladder rung. gci
    /// i11/go (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (a)).
    G1CompactionPause,
    /// G1 only: a forced allocation pause a HUMONGOUS request asked for (the
    /// interpreter's array ladder, site `g1-humongous-alloc`,
    /// `gc_alloc_array_unsampled`): HotSpot's `GCCause::_g1_humongous_allocation`,
    /// which `attempt_allocation_humongous` passes for every collection it
    /// asks for. An allocation failure in every other respect (samples
    /// productivity, a forced ladder rung, drives the mark cycle). gci i11/go
    /// (i10rh finding 4).
    G1HumongousAllocation,
    /// JVMTI `ForceGarbageCollection` (`NativeContext::force_gc_for_jvmti`):
    /// HotSpot's `collect(GCCause::_jvmti_force_gc)`, `JvmtiEnv
    /// ForceGarbageCollection`, a full collection that
    /// `-XX:+DisableExplicitGC` does not gate and that commits no
    /// `jdk.SystemGC`. gci i27/ob (`gci-i15rb-...-review-findings-20261014`
    /// finding 4's residual: it reported `System.gc()`).
    JvmtiForceGc,
    /// G1 only, zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`): the young
    /// pause of G1's periodic collection (`-XX:G1PeriodicGCInterval`, the
    /// service thread's `gc_and_alloc::g1_periodic_collection`): HotSpot's
    /// `try_collect(GCCause::_g1_periodic_collection)` under the default
    /// `-XX:+G1PeriodicGCInvokesConcurrent`, a `Concurrent Start` young pause
    /// (or `Prepare Mixed` / `Mixed` in a mixed phase) whose cycle nobody
    /// waits for. Not an explicit GC.
    G1PeriodicCollection,
    /// G1 only, zt t31/x10: the periodic collection under
    /// `-XX:-G1PeriodicGCInvokesConcurrent`, HotSpot's `Pause Full (G1
    /// Periodic Collection)` (`force_full_collection` on this door). Not an
    /// explicit GC.
    G1PeriodicFull,
}

impl GcDoor {
    /// HotSpot's `GCCause` spelling. Both allocation doors are HotSpot's
    /// "Allocation Failure" (a young generation that is full, or would be) —
    /// the VM-internal distinction between them is already counted per door by
    /// `gc_entry_census`, and inventing a cause string JMC does not know would
    /// buy nothing. This is the Serial-shaped (Generational) and ZGC spelling;
    /// G1 reports through [`Self::cause_on`].
    pub(super) fn cause(self) -> &'static str {
        match self {
            GcDoor::AllocationThreshold | GcDoor::AllocationFailure => "Allocation Failure",
            GcDoor::SystemGc => "System.gc()",
            GcDoor::MetadataThreshold => "Metadata GC Threshold",
            GcDoor::DiagnosticCommand => "Diagnostic Command",
            GcDoor::HeapInspection => "Heap Inspection Initiated GC",
            GcDoor::HeapDump => "Heap Dump Initiated GC",
            GcDoor::G1CompactionPause => G1_COMPACTION_PAUSE_CAUSE,
            GcDoor::G1HumongousAllocation => "G1 Humongous Allocation",
            GcDoor::JvmtiForceGc => "JvmtiEnv ForceGarbageCollection",
            GcDoor::G1PeriodicCollection | GcDoor::G1PeriodicFull => "G1 Periodic Collection",
        }
    }

    /// The cause a collection on `heap` reports (JMX notification
    /// `gcCause`, JFR `jdk.GarbageCollection.cause`, the `-Xlog:gc` line).
    /// gci i11/go (`gci-i10rh-...-review-findings-20261013` finding 4): HotSpot
    /// 25 G1 never says `Allocation Failure` -- an evacuation pause a full
    /// young generation (or a failed allocation) asks for is
    /// `GCCause::_g1_inc_collection_pause`, `G1 Evacuation Pause`. Every other
    /// door, and every other backend, is [`Self::cause`]. Pure.
    pub(super) fn cause_on(self, heap: &crate::memory::vm_heap::VmHeap) -> &'static str {
        g1_door_cause(
            self,
            heap.is_g1(),
        )
    }
}

/// HotSpot's `GCCause::_g1_compaction_pause` spelling.
const G1_COMPACTION_PAUSE_CAUSE: &str = "G1 Compaction Pause";
/// HotSpot's `GCCause::_g1_inc_collection_pause` spelling.
const G1_EVACUATION_PAUSE_CAUSE: &str = "G1 Evacuation Pause";

/// [`GcDoor::cause_on`], pure: on G1 both allocation doors are `G1 Evacuation
/// Pause`; otherwise [`GcDoor::cause`].
fn g1_door_cause(door: GcDoor, g1: bool) -> &'static str {
    match door {
        GcDoor::AllocationThreshold | GcDoor::AllocationFailure if g1 => G1_EVACUATION_PAUSE_CAUSE,
        _ => door.cause(),
    }
}

/// Pre-collection facts for [`gc_event_finish`]. Only captured when some
/// consumer (a JFR recording, an `-Xlog:gc` rule) is armed, or when the heap
/// keeps per-bean GC statistics (gen r4w3/obs2: the generational backend).
pub(super) struct GcEventStart {
    t0: std::time::Instant,
    wall_start_ns: u64,
    heap_used_before: usize,
    /// gen r5w2/obs6: the committed heap BEFORE the collection, for the JFR
    /// `Before GC` heap summary (it carried the after-collection figure, so
    /// a collection that grew or shrank the heap reported the new size on
    /// both edges). `0` unless `events`.
    heap_committed_before: usize,
    /// zt t31/x10 (item 1): the ZGC slides' cumulative copy volume before
    /// the collection (`VmHeap::zgc_slide_moved_bytes_total`), for its
    /// `Compacted` figure; `0` unless `events`, and off ZGC.
    zgc_moved_before: usize,
    /// A JFR recording or `-Xlog:gc` is armed: emit the events.
    events: bool,
    /// gen r4w3/obs2: the JMX beans before the collection, on a heap that
    /// describes them. See [`record_gc_notification`].
    beans: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    /// gc-common w3-f: `--verbose:gc` is on — print this pause's
    /// [`render_pause_line`] (time-to-safepoint and take-over coverage).
    pause_line: Option<PauseSafepointFacts>,
    /// `(end, heap_after, committed)`, stamped by [`gc_event_seal`] while the
    /// world is still stopped. `None` if the door did not seal (then
    /// [`gc_event_finish`] samples late, as before gc-common w5-a).
    /// `heap_after` / `committed` are `0` unless `events` (nobody reads them).
    sealed: Option<(std::time::Instant, usize, usize)>,
    /// gc-common w5-a: the JMX beans AFTER the collection, stamped by
    /// [`gc_event_seal`] inside the pause when `beans` is `Some` — so the
    /// `GcInfo` a `GarbageCollectionNotificationInfo` / `getLastGcInfo()`
    /// carries (its end time, duration and after-usage) is the collection's,
    /// not the collection's plus the released mutators' allocation.
    beans_after: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    /// gc-common w7-f: this collection has been booked on the heap's
    /// [`BackendGcBeans`](cratonvm_gc::gc_metrics::BackendGcBeans) (by the
    /// seal, or by [`gc_event_finish`] for a door that did not seal), so it is
    /// never booked twice.
    backend_counted: bool,
    /// gcd d10/o retirement: `VmHeap::young_only_cycles` at the start, so the
    /// collection can tell a ZGC young cycle (`ZGC Minor ...`) from a
    /// whole-heap one (`ZGC Major ...`): see [`GcEventStart::kind`].
    young_only_cycles_before: usize,
    /// gci i5g/g1o: `G1Collector::mixed_collection_count` at the start, on a
    /// G1 heap (`None` on every other), so the seal and the notification can
    /// tell a young-only G1 pause from a mixed one: see
    /// [`GcEventStart::affected_pools`].
    g1_mixed_before: Option<u64>,
    /// Wave 9 lane G (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
    /// `VmHeap::g1_old_collecting_pauses` at the start (`None` off G1), so
    /// the collection can tell a G1 young-only pause (which leaves
    /// `G1 Old Gen`'s collection usage and threshold alone on HotSpot 25)
    /// from a mixed one: see [`GcEventStart::affected_only`].
    g1_old_pauses_before: Option<u64>,
    /// gen r5w3/obs7: the process CPU `(user, system)` seconds at the start,
    /// for the `-Xlog:gc+cpu` line. `Some` only when that tag is on, on the
    /// Generational backend.
    cpu_start: Option<(f64, f64)>,
    /// gcf f4/r: the class-metadata bytes at the start, for the
    /// `-Xlog:gc+metaspace` line. `Some` only when that tag is on, on the
    /// Generational backend (the figure is a class-store walk). gci i33/ms:
    /// HotSpot's figures (committed, the class space), the same one walk.
    metaspace_before: Option<crate::vm::vm_init::MetaspaceStats>,
    /// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 12's
    /// info-only rest): the VM's unloaded-class count
    /// (`diagnostic_counters.classes_unloaded`) at the start, read only with
    /// `metaspace_before`. A collection that unloaded no class ends with the
    /// class store it started with, so its after-figure is `metaspace_before`
    /// and needs no second walk ([`metaspace_after_bytes`]). `0` otherwise.
    classes_unloaded_before: u64,
    /// gci i30/ob (`i28-tl-report.md` review item 6): this collection's
    /// `gc,tlab` debug `TLAB totals` line, computed at the start
    /// (`unified_logging::stash_tlab_totals`) and carried here, so a G1 young
    /// report deferred past another pause's start
    /// ([`G1DeferredYoungReport`]) still prints it. The per-VM stash it was
    /// read from at the writer is overwritten (or cleared) by every later
    /// collection's start. `None` unless `-Xlog:gc+tlab=debug`.
    tlab_totals: Option<String>,
    /// gci i30/ob hand-back 2 (vi34-ob TC: `GC(190)` printed no `TLAB
    /// totals`): the VM's allocated-bytes total (`bytes_allocated_total`) at
    /// the start, for the `cratonvm::tlab_census` INFO witness the writer
    /// prints per collection: two consecutive collections with the same figure
    /// had no allocation in between, so no refill and -- as on HotSpot -- no
    /// line. One relaxed load (already taken for the JFR totals).
    allocated_at_start: u64,
    /// gcg g3/x: the heap as HotSpot Serial's `Heap Before GC` debug block
    /// describes it (the `gc,exit` summary's figures, taken inside the pause
    /// at the start). `Some` only under `-Xlog:gc+heap=debug` (or
    /// `gc+metaspace=debug`), on the Generational backend.
    heap_debug_before: Option<crate::runtime::unified_logging::SerialExitHeap>,
    /// gcg g3/x: the same at the end, stamped by [`gc_event_seal`] inside the
    /// pause (or late by [`gc_event_finish`] for a door that did not seal).
    heap_debug_after: Option<crate::runtime::unified_logging::SerialExitHeap>,
    /// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
    /// item 9): the G1 heap as HotSpot G1's `Heap Before GC` debug block
    /// describes it ([`g1_heap_debug_sample`]). `Some` only under
    /// `-Xlog:gc+heap=debug` (or `gc+metaspace=debug`), on G1.
    g1_heap_debug_before: Option<crate::runtime::unified_logging::G1ExitHeap>,
    /// The same at the end, stamped by [`gc_event_seal`] inside the pause.
    g1_heap_debug_after: Option<crate::runtime::unified_logging::G1ExitHeap>,
    /// zr z1/tr: HotSpot 25 ZGC's causes for this collection, read from the
    /// heap's trigger latch at the start (the collection clears it):
    /// `(allocation-threshold door, allocation-failure door)`. `None` off ZGC
    /// and under `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`. See [`Self::cause`].
    zgc_causes: Option<(&'static str, &'static str)>,
    /// gcg h1/ob: the young half of a Generational pause that escalated into
    /// an unrequested major, taken by [`gc_event_seal`] inside the pause
    /// (`GenerationalHeap::take_escalation_split`). Reported as two
    /// collections under `CRATONVM_GC_ESCALATED_PAUSE_SPLIT`
    /// ([`escalated_pause_split`]).
    escalation: Option<cratonvm_gc::gen_heap::EscalationSplit>,
    /// zs s4/ob3: the phase boundaries this ZGC collection was booked with on
    /// its `... Pauses` bean (`BackendGcBeans::note_collection_split`), so
    /// its notifications are cut the same way. `None` off ZGC, under
    /// `CRATONVM_ZGC_PAUSE_PHASES=0`, and when booked as one pause.
    zgc_pause_split: Option<cratonvm_gc::gc_metrics::ZgcPauseSplit>,
    /// gci i11/go (`gci-i10rh-...` finding 5): a G1 heap's region census and
    /// young target (regions) at the start, for HotSpot G1's `gc,heap` lines
    /// (`G1HeapTransition`). `Some` only under `-Xlog:gc+heap` on G1.
    g1_regions_before: Option<G1RegionEdge>,
    /// The same at the end, stamped by [`gc_event_seal`] inside the pause
    /// (`None` for a door that did not seal: no `gc,heap` lines then).
    g1_regions_after: Option<G1RegionEdge>,
    /// gci i11/go (finding 9): `G1Collector::prepare_mixed_pause_count` at the
    /// start, on a G1 heap (`None` elsewhere).
    g1_prepare_before: Option<u64>,
    /// gci i11/go (finding 4): stamped by [`gc_event_seal`] inside a G1 pause
    /// while `-Xlog` / JFR are armed: no cycle is open and the trigger would
    /// open one now (`G1Collector::would_start_marking`), so the pause's mark
    /// drive starts a cycle -- HotSpot's `(Concurrent Start)`.
    g1_concurrent_start: bool,
    /// gci i13/gp: stamped by [`gc_event_seal`] beside `g1_concurrent_start`:
    /// no G1 cycle was marking at the end of this pause. Under
    /// `CRATONVM_G1_METADATA_CONCURRENT_START` a `Metadata GC Threshold` pause
    /// with no cycle running is the concurrent start whatever IHOP says
    /// (HotSpot's `force_concurrent_start_if_outside_cycle`), see
    /// [`Self::g1_pause_type`].
    g1_marking_idle: bool,
    /// gci i13/gp (`gci-i13ef-...-review-findings-20261013` finding 2): the
    /// G1 cycle record's sequence (`gc_metrics::last_g1_cycle`) at the start,
    /// `Some(0)` before any; `None` off G1 or without `-Xlog` / JFR.
    g1_cycle_seq_before: Option<u64>,
    /// gci i13/gp: stamped by [`gc_event_seal`]: this G1 pause's evacuation
    /// failed (a self-forwarded object), HotSpot 25's `(Evacuation Failure:
    /// Allocation)` end-line suffix ([`g1_pause_evacuation_failed`]).
    g1_evacuation_failed: bool,
    /// gci i12/ge (`gci-i10dl-...-door-residuals-20261013` (b), opt-in
    /// `CRATONVM_G1_FULL_GC_ONE_REPORT`): this pause is one of a G1 full
    /// collection's, folded into its ONE report -- `Some(the report's id)`.
    /// Such a pause takes no ordinal, books no bean, queues no notification
    /// and emits no `-Xlog` / JFR row of its own (`events` and `beans` are
    /// off); only its `--verbose:gc` pause line prints, under the report's
    /// id. `None` everywhere else.
    full_scope_id: Option<u64>,
    /// zt t31/zc11 (program page 1f residual R3): the failed ZGC Pause Mark
    /// End attempts `(pause_ns, continue_ns)` of the cycle this collection
    /// closed, taken by [`gc_event_seal`] inside the pause. The writer read
    /// the heap's log after the release, so a whole collection on another
    /// thread in between took (and printed) them instead. `None` off ZGC and
    /// for a door that did not seal (the writer then takes the log late).
    zgc_mark_continue: Option<Vec<(u64, u64)>>,
    /// zt t31/zc11 (1f R6): the concurrent phase of the cycle this collection
    /// closed (`ZgcRealHeap::take_last_concurrent_mark_nanos`), taken by
    /// [`gc_event_seal`] inside the pause; `0` for a stop-the-world mark, off
    /// ZGC, or a door that did not seal.
    zgc_conc_mark_ns: u64,
    /// zt t31/zc11: when the ZGC concurrent cycle this collection closes (or
    /// abandons) began its mark start (`ConcurrentCycleJfr::take_cycle_open_at`,
    /// taken by [`gc_event_seal`]); the `... Cycles` bean and the cycle
    /// notification record span from it. `None` off ZGC and for a collection
    /// no mark start preceded.
    zgc_cycle_start: Option<std::time::Instant>,
}

/// gci i11/go: one edge of a G1 pause as `G1HeapTransition` describes it --
/// the region census and the young target in regions at that edge.
#[derive(Clone, Copy, Debug)]
struct G1RegionEdge {
    census: cratonvm_gc::g1::G1RegionCensus,
    young_target_regions: usize,
    /// gci i43/gy: the collector's `-XX:SurvivorRatio` (8 unless typed).
    survivor_ratio: usize,
}

impl G1RegionEdge {
    /// One region-table walk under the regions read lock (uncontended with
    /// the world stopped) and one relaxed load. `None` off G1.
    fn sample(heap: &crate::memory::vm_heap::VmHeap) -> Option<Self> {
        Some(Self {
            census: heap.region_census()?,
            young_target_regions: heap.g1_young_target_regions()?,
            survivor_ratio: heap.g1_survivor_ratio()?,
        })
    }
}

impl GcEventStart {
    /// The `GCCause` this collection is reported under: the door's
    /// ([`GcDoor::cause`]), except an allocation-driven ZGC collection, which
    /// is HotSpot 25 ZGC's (`Allocation Rate`, `Warmup`, `Proactive`,
    /// `High Usage`; `Allocation Stall` for an allocation that failed) --
    /// HotSpot's ZGC never says `Allocation Failure` (zr z1/tr).
    ///
    /// Merge of origin/dev (2026-10-15): and gci i11/go's G1 spelling
    /// ([`GcDoor::cause_on`]: `G1 Evacuation Pause` for both allocation doors
    /// on G1; `g1_mixed_before` is `Some` exactly on a G1 heap).
    pub(super) fn cause(&self, door: GcDoor) -> &'static str {
        match (door, self.zgc_causes) {
            (GcDoor::AllocationThreshold, Some((rule, _))) => rule,
            (GcDoor::AllocationFailure, Some((_, stall))) => stall,
            _ => g1_door_cause(door, self.g1_mixed_before.is_some()),
        }
    }

    /// Which collection ran since this start: a young-only cycle (ZGC's
    /// opt-in generational mode's minor) when the heap's young-only cycle
    /// count moved, a whole-heap / ordinary one otherwise. Read after the
    /// collection (inside the pause by [`gc_event_seal`], or late by
    /// [`gc_event_finish`]: the count moves only inside a collection, and one
    /// thread reports one collection at a time).
    fn kind(&self, shared: &SharedVm) -> cratonvm_gc::gc_metrics::CollectionKind {
        collection_kind(
            self.young_only_cycles_before,
            shared.mem.heap.young_only_cycles(),
        )
    }

    /// Wave 9 lane G: the pools this collection affected when that is not
    /// every pool -- a G1 young-only pause's eden and survivor -- or `None`
    /// (all). Read after the collection, as [`Self::kind`] is.
    fn affected_only(&self, shared: &SharedVm) -> Option<&'static [&'static str]> {
        g1_young_only_pools(
            self.g1_old_pauses_before,
            shared.mem.heap.g1_old_collecting_pauses(),
        )
    }

    /// gci i5g/g1o (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage-FIXED-20261012`):
    /// the heap pools this collection affected, when not all of them. HotSpot
    /// 25 registers `G1 Old Gen` with the young manager (`G1 Young
    /// Generation`) as NOT always affected, and runs a young-only pause under
    /// `G1YoungGCMonitoringScope(.., all_memory_pools_affected = false)` (true
    /// for a mixed pause), so that pause's `GCMemoryManager::gc_end` neither
    /// stamps `G1 Old Gen`'s collection usage nor runs its collection-threshold
    /// check. `Some(eden, survivor)` for a G1 pause that recorded no `Mixed`
    /// collection since the start; `None` (every pool) for a mixed G1 pause and
    /// on every other backend. Read after the collection, like [`Self::kind`].
    ///
    /// Merge of origin/dev (2026-10-15): dev's wave 9 lane G fixed the same
    /// item on `G1Collector::old_collecting_pause_count`
    /// ([`Self::affected_only`]), which also moves on the full- and
    /// humongous-compaction pauses (not `Mixed` collections). A pause is
    /// young-only here only when BOTH counters stood still.
    fn affected_pools(&self, shared: &SharedVm) -> Option<&'static [&'static str]> {
        self.affected_only(shared)?;
        let before = self.g1_mixed_before?;
        let after = shared.mem.heap.g1_mixed_collection_count()?;
        g1_young_only_pause(before, after)
            .then_some(G1_YOUNG_ONLY_AFFECTED_POOLS)
    }

    /// gci i11/go: this collection's G1 pause type, `None` off G1
    /// ([`g1_young_pause_type`] over the two edges' counters and the
    /// concurrent-start decision [`gc_event_seal`] took inside the pause).
    /// Read after the collection, like [`Self::affected_pools`].
    fn g1_pause_type(&self, shared: &SharedVm, door: GcDoor) -> Option<G1YoungPauseType> {
        let before = self.g1_mixed_before?;
        if !shared.mem.heap.is_g1() {
            return None;
        }
        let prepare_moved = self
            .g1_prepare_before
            .is_some_and(|b| shared.mem.heap.g1_prepare_mixed_pause_count().unwrap_or(0) != b);
        let concurrent_start = (self.g1_concurrent_start
            && g1_door_drives_mark_cycle(door, true))
            || g1_metadata_pause_is_concurrent_start(door, self.g1_marking_idle, || {
                g1_metadata_concurrent_start_enabled()
            })
            // zt t31/x10: the periodic collection's forced start.
            || g1_periodic_pause_is_concurrent_start(door, self.g1_marking_idle)
            // gci cs48: the user-requested concurrent cycle's forced start.
            || g1_user_requested_concurrent_start(
                door,
                explicit_gc_invokes_concurrent(shared),
                self.g1_marking_idle,
                g1_concurrent_start_at_young_pause_enabled,
            );
        Some(g1_young_pause_type(
            before,
            shared.mem.heap.g1_mixed_collection_count().unwrap_or(0),
            prepare_moved,
            concurrent_start,
        ))
    }
}

/// gci i11/go (`gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-FIXED-20261016`
/// findings 4 and 9): HotSpot 25's `G1GCPauseType` for a young-generation
/// pause, `Pause Young (<type>)` with `G1GCPauseTypeHelper::to_string`.
/// `ConcurrentStart`: this VM opens a cycle in its own initial-mark pause right
/// after the evacuation pause (`g1_drive_mark_cycle`), so the evacuation pause
/// is labelled when, inside it, the trigger would open one
/// (`G1Collector::would_start_marking`, HotSpot's
/// `decide_on_concurrent_start_pause`). `PrepareMixed`: only under
/// `CRATONVM_G1_PREPARE_MIXED` (finding 9); without it the first pause after
/// cleanup is already mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum G1YoungPauseType {
    Normal,
    ConcurrentStart,
    PrepareMixed,
    Mixed,
}

impl G1YoungPauseType {
    /// The `-Xlog:gc` label (`Pause Young (%s)`). Pure.
    fn xlog_label(self) -> &'static str {
        match self {
            G1YoungPauseType::Normal => "Pause Young (Normal)",
            G1YoungPauseType::ConcurrentStart => "Pause Young (Concurrent Start)",
            G1YoungPauseType::PrepareMixed => "Pause Young (Prepare Mixed)",
            G1YoungPauseType::Mixed => "Pause Young (Mixed)",
        }
    }

    /// The `jdk.G1GarbageCollection.type` value (`jdk.types.G1YCType`, the
    /// same `G1GCPauseTypeHelper::to_string` names). Pure.
    fn jfr_type(self) -> &'static str {
        match self {
            G1YoungPauseType::Normal => "Normal",
            G1YoungPauseType::ConcurrentStart => "Concurrent Start",
            G1YoungPauseType::PrepareMixed => "Prepare Mixed",
            G1YoungPauseType::Mixed => "Mixed",
        }
    }
}

/// [`GcEventStart::g1_pause_type`], pure: `Mixed` iff the mixed-collection
/// count moved across the pause, else `PrepareMixed` iff the `Prepare Mixed`
/// count moved, else `ConcurrentStart` iff the pause will open a marking
/// cycle, else `Normal` (HotSpot: a mixed or prepare-mixed pause never starts
/// a cycle -- the marking of the phase it belongs to has finished).
fn g1_young_pause_type(
    mixed_before: u64,
    mixed_after: u64,
    prepare_moved: bool,
    concurrent_start: bool,
) -> G1YoungPauseType {
    if !g1_young_only_pause(mixed_before, mixed_after) {
        G1YoungPauseType::Mixed
    } else if prepare_moved {
        G1YoungPauseType::PrepareMixed
    } else if concurrent_start {
        G1YoungPauseType::ConcurrentStart
    } else {
        G1YoungPauseType::Normal
    }
}

/// gci i11/go: does a pause on `door` drive the G1 mark cycle right after it
/// (so a cycle its trigger wants opens there)? `maybe_gc`'s threshold pause
/// always (`MarkDoor::MaybeGc`); the forced allocation doors under
/// `CRATONVM_G1_ALLOC_MARK_DRIVE` (`alloc_drive`, default ON:
/// `MarkDoor::AllocFail`). A full-collection door runs its own cycle
/// (`g1_force_full_cycle`), HotSpot's `Pause Full`, not a concurrent start.
/// Pure.
fn g1_door_drives_mark_cycle(door: GcDoor, alloc_drive: bool) -> bool {
    match door {
        GcDoor::AllocationThreshold => true,
        GcDoor::AllocationFailure | GcDoor::G1CompactionPause | GcDoor::G1HumongousAllocation => {
            alloc_drive
        }
        GcDoor::SystemGc
        | GcDoor::MetadataThreshold
        | GcDoor::DiagnosticCommand
        | GcDoor::HeapInspection
        | GcDoor::HeapDump
        | GcDoor::JvmtiForceGc
        | GcDoor::G1PeriodicCollection
        | GcDoor::G1PeriodicFull => false,
    }
}

/// gci i13/gp (`gci-i12ge-...-review-findings-20261013` item 5): is a G1
/// pause on `door` the concurrent start of the metadata opt-in? A `Metadata
/// GC Threshold` pause that left no cycle marking (`marking_idle`) while
/// `CRATONVM_G1_METADATA_CONCURRENT_START` is on (`enabled`, asked last so the
/// flag is read only for that door): `force_gc_for_class_metadata` opens the
/// cycle right after it, as HotSpot's `force_concurrent_start_if_outside_cycle`
/// makes that pause a concurrent start whatever IHOP says. Pure but for
/// `enabled`.
fn g1_metadata_pause_is_concurrent_start(
    door: GcDoor,
    marking_idle: bool,
    enabled: impl FnOnce() -> bool,
) -> bool {
    door == GcDoor::MetadataThreshold && marking_idle && enabled()
}

/// zt t31/x10: is a G1 pause on `door` the concurrent start of a periodic
/// collection? A [`GcDoor::G1PeriodicCollection`] pause that found no cycle
/// marking (`marking_idle`): HotSpot 25's `try_collect` for
/// `_g1_periodic_collection` forces the concurrent start
/// (`decide_on_concurrent_start_pause`'s user-requested arm), and
/// `gc_and_alloc::g1_periodic_collection` opens the cycle right after it. A
/// pause of a mixed phase keeps its `Prepare Mixed` / `Mixed` label
/// ([`g1_young_pause_type`] asks those first). Pure.
fn g1_periodic_pause_is_concurrent_start(door: GcDoor, marking_idle: bool) -> bool {
    door == GcDoor::G1PeriodicCollection && marking_idle
}

/// gci i5g/g1o: the pools a young-only G1 pause affects (HotSpot 25's
/// `G1MonitoringSupport`: eden and survivor always, `G1 Old Gen` only when all
/// pools are). See [`GcEventStart::affected_pools`].
///
/// gci i20/mr finding 7: the one list dev's wave 9 lane G added as
/// `gc_metrics::G1_YOUNG_ONLY_POOLS` (the merge left two copies; the
/// young-only answer of [`GcEventStart::affected_only`] and of
/// [`GcEventStart::affected_pools`] must be the same pools).
const G1_YOUNG_ONLY_AFFECTED_POOLS: &[&str] = cratonvm_gc::gc_metrics::G1_YOUNG_ONLY_POOLS;

/// gci i5g/g1o: a G1 pause was young-only iff it recorded no `Mixed`
/// collection between its two edges. Pure.
fn g1_young_only_pause(mixed_before: u64, mixed_after: u64) -> bool {
    mixed_after == mixed_before
}

/// gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
/// does a G1 pause hold the next concurrent start back to the next young-only
/// pause? Iff it was `Mixed` (it recorded a `Mixed` collection) and
/// `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE` is on (`enabled`, read only
/// for a mixed pause). HotSpot 25: `G1Policy::decide_on_concurrent_start_pause`
/// starts a cycle only at a young-only pause; the pause that ends a mixed
/// phase only sets `initiate_conc_mark_if_possible`. Pure but for `enabled`.
fn g1_mixed_pause_defers_concurrent_start(
    mixed_before: u64,
    mixed_after: u64,
    enabled: impl FnOnce() -> bool,
) -> bool {
    !g1_young_only_pause(mixed_before, mixed_after) && enabled()
}

/// gci cs48: is `door` a G1 user-requested concurrent cause under
/// `-XX:+ExplicitGCInvokesConcurrent` -- `System.gc()` or `jcmd GC.run`
/// (HotSpot 25 `is_user_requested_concurrent_full_gc`)? Pure.
fn g1_door_is_user_requested(door: GcDoor) -> bool {
    matches!(door, GcDoor::SystemGc | GcDoor::DiagnosticCommand)
}

/// gci cs48 (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`,
/// vi28-dg E4): is a G1 pause on `door` the forced concurrent start of a
/// user-requested concurrent cycle? A user-requested door
/// ([`g1_door_is_user_requested`]) under `-XX:+ExplicitGCInvokesConcurrent`
/// (`explicit_concurrent`) with no cycle marking (`marking_idle`) while
/// `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE` is on (`enabled`, asked
/// last): HotSpot 25's `decide_on_concurrent_start_pause` makes that pause
/// `Concurrent Start` whatever IHOP says, unless a cycle is in progress (then
/// the request waits for it). `gc_and_alloc::force_full_collection` opens the
/// cycle right after it. Pure but for `enabled`.
fn g1_user_requested_concurrent_start(
    door: GcDoor,
    explicit_concurrent: bool,
    marking_idle: bool,
    enabled: impl FnOnce() -> bool,
) -> bool {
    g1_door_is_user_requested(door) && explicit_concurrent && marking_idle && enabled()
}

/// gci i5g/g1o: the rows `BackendGcBeans::note_collection_usage` stamps for a
/// collection that affected only `affected`: those pools at `pools`' usage,
/// every other pool written back with its own last collection usage, or `0`
/// before any (`MemoryPool::_after_gc_usage`'s initial `used` on HotSpot, and
/// what `getCollectionUsage()` reports for an unstamped pool here). Pure.
///
/// gci i14/mp: test-only since `BackendGcBeans::note_collection_usage_of`
/// stamps the affected pools itself (used AND committed, finding 3); kept as
/// the pure model of its `collection_used` rows the tests below pin.
#[cfg(test)]
fn affected_collection_usage_rows(
    pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
    affected: &[&'static str],
) -> Vec<cratonvm_gc::gc_metrics::JmxPoolUsage> {
    pools
        .iter()
        .map(|p| {
            if affected.contains(&p.name) {
                *p
            } else {
                cratonvm_gc::gc_metrics::JmxPoolUsage {
                    used: p.collection_used.unwrap_or(0),
                    ..*p
                }
            }
        })
        .collect()
}

impl GcEventStart {
    /// The collection's duration and — when sealed — its after-heap figures:
    /// `(duration_ns, Some((heap_after, committed)))`. ONE end instant for
    /// every consumer (the `[GC] pause:` line, `-Xlog:gc`, JFR), so the three
    /// cannot disagree (`common-w4f-gc-event-durations-sampled-after-release`).
    /// Unsealed, the end is "now" and the heap figures are left to the caller.
    fn end_facts(&self) -> (u64, Option<(usize, usize)>) {
        let (end, after) = match self.sealed {
            Some((end, after, committed)) => (end, Some((after, committed))),
            None => (std::time::Instant::now(), None),
        };
        let duration_ns =
            u64::try_from(end.saturating_duration_since(self.t0).as_nanos()).unwrap_or(u64::MAX);
        (duration_ns, after)
    }
}

/// The safepoint half of one collection pause, sampled by [`gc_event_start`]
/// for the `--verbose:gc` `[GC] pause:` line (gc-common w3-f, 2026-09-23;
/// `common-a-proposal-ttsp-reporting-FIXED-20260923.md` item 1 per pause, and the coverage
/// half of `docs/feature-designs/real-jdk-mode.md`).
///
/// Both are read INSIDE the pause, before the collection, which is what makes
/// them this pause's: the TTSP of a pause is recorded once, when the initiator
/// first observes its quota met (`GcBarrier::note_quota_met_locked`, reached
/// from every wait `stw_take_over_and_wait` ends on), and no other pause can
/// record until `complete_gc` releases this one; the take-over coverage
/// counters are reset at the top of `stw_take_over_and_wait` and only its own
/// passes add to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PauseSafepointFacts {
    /// Request accepted -> quota met, nanoseconds (`TtspStats::last_ns`).
    ttsp_ns: u64,
    /// The thread this pause waited for last (`TtspStats::last_straggler`,
    /// a registry `ThreadId.0`); `None` when the quota was met with no
    /// participating arrival. gc-common w5-a (`handoff-w4f-…` §2): the exit
    /// line named only the SLOWEST pause's straggler.
    straggler: Option<u64>,
    /// Quota met -> take-over returned, nanoseconds
    /// (`gc_quiescence::xt_post_quota_ns`): the frozen-peer frame walk, the
    /// helper-window pass and the skip-span publish. gc-common w4-a.
    xt_post_quota_ns: u64,
    /// The take-over passes' share of `ttsp_ns`
    /// (`gc_quiescence::xt_pass_ns`). gc-common w6-f.
    xt_pass_ns: u64,
    /// `gc_quiescence::xt_cycle_coverage()`: `(passes, taken_over,
    /// unclassified, roots, hw_windows, hw_roots)`.
    xt: (u64, u64, u64, u64, u64, u64),
}

/// The `-Xlog` tag sets a collection logs to (gen r5w3/obs7): `gc` (the
/// per-collection line), and on the Generational backend HotSpot Serial's
/// `gc,start`, `gc,heap` and `gc,cpu` lines ([`xlog_collection_lines`]).
const XLOG_COLLECTION_TAGS: &[crate::runtime::unified_logging::LogTag] = &[
    crate::runtime::unified_logging::LogTag::Gc,
    crate::runtime::unified_logging::LogTag::GcStart,
    crate::runtime::unified_logging::LogTag::GcHeap,
    crate::runtime::unified_logging::LogTag::GcCpu,
    // gcf f4/r: the per-collection `gc,metaspace` line.
    crate::runtime::unified_logging::LogTag::GcMetaspace,
    // gcg g4/yg: a full collection's `gc,phases` pairs
    // (`CRATONVM_XLOG_GC_FULL_PHASES`, default on since gcg g5/yg, `=0` kill).
    crate::runtime::unified_logging::LogTag::GcPhases,
    crate::runtime::unified_logging::LogTag::GcPhasesStart,
];

/// Is any per-collection event consumer armed right now?
///
/// gci i9/jc: a `jdk.jfr` backend recording (`Recording`, `RecordingStream`,
/// `jcmd JFR.start`) is one. It was missing, so with no legacy recording and
/// no `-Xlog:gc` rule `gc_event_finish` returned before
/// `emit_jfr_collection`, and the backend's `jfr_vm_more::gc_cycle` (its
/// `jdk.GarbageCollection`, `GC Pause`, heap summaries) never ran: such a
/// recording had no collection at all. One more relaxed load per collection.
fn gc_events_wanted() -> bool {
    cratonvm_jfr::is_enabled()
        || cratonvm_native_builtins_jfr::jfr_vm_events::any_recording()
        || crate::runtime::unified_logging::is_unified_logging_enabled(
            XLOG_COLLECTION_TAGS,
            crate::runtime::unified_logging::LogLevel::Info,
        )
        || xlog_age_on()
        // gcg g3/x: the deferred reference-processing block is flushed by the
        // collection's `-Xlog` lines, so a `gc+ref=debug`-only spec arms them.
        || crate::runtime::unified_logging::xlog_reference_lines_wanted()
}

/// gcj j1/yg (`docs/feature-designs/real-jdk-mode.md`):
/// does a `-Xlog:gc+tlab=debug` rule arm this collection's events on its own?
///
/// The `TLAB totals` line is computed at the collection's start only when
/// `events` holds (`unified_logging::tlab_totals_wanted`), and `events` was
/// [`gc_events_wanted`] alone, whose `-Xlog` arm asks the collection tags
/// ([`XLOG_COLLECTION_TAGS`]) at info. `gc+tlab` parses to the single tag
/// `LogTag::GcTlab` (`parse_tag_selector`), which is not one of them, so a
/// spec naming only `gc+tlab=debug` -- HotSpot prints `GC(n) TLAB totals:` at
/// every pause under it -- left `events` false and printed nothing: rows
/// `tj43_t_gen` / `tj43_t_g1` (vi43-tj T1), both logs empty. The same shape
/// `xlog_reference_lines_wanted` closed for `gc+ref=debug`.
///
/// Generational only (G1 is out of the gcj round's scope and keeps its
/// behaviour; ZGC prints no such line). `CRATONVM_XLOG_GEN_TLAB_TOTALS_ALONE=0`
/// restores the old gate. Pure; [`xlog_tlab_totals_alone_wanted`] is the
/// short-circuiting read.
fn tlab_totals_alone_arms_events(generational: bool, tlab_debug_on: bool, armed: bool) -> bool {
    generational && tlab_debug_on && armed
}

/// gcj j1/yg: [`tlab_totals_alone_arms_events`] for this VM, reading the
/// `-Xlog` rules only on a Generational heap and the kill switch only when a
/// `gc+tlab=debug` rule is armed (one flag read per such collection).
fn xlog_tlab_totals_alone_wanted(shared: &SharedVm) -> bool {
    let generational = shared.mem.heap.is_generational();
    let tlab_debug_on = generational
        && crate::runtime::unified_logging::is_unified_logging_enabled(
            &[crate::runtime::unified_logging::LogTag::GcTlab],
            crate::runtime::unified_logging::LogLevel::Debug,
        );
    let armed = tlab_debug_on
        && true;
    tlab_totals_alone_arms_events(generational, tlab_debug_on, armed)}

/// gcg g3/x: is `-Xlog` taking HotSpot Serial's `Heap Before GC` / `Heap
/// After GC` blocks (`gc,heap` at debug) or their `Metaspace` twins
/// (`gc,metaspace` at debug)?
fn xlog_heap_debug_on() -> bool {
    crate::runtime::unified_logging::is_unified_logging_enabled(
        &[
            crate::runtime::unified_logging::LogTag::GcHeap,
            crate::runtime::unified_logging::LogTag::GcMetaspace,
        ],
        crate::runtime::unified_logging::LogLevel::Debug,
    )
}

/// gcg g3/x: HotSpot's `invocations=<n> (full <m>)` of a Serial heap
/// snapshot: every collection (`Copy` + `MarkSweepCompact`) and the full ones.
fn serial_invocations(snap: &cratonvm_gc::gc_metrics::GcBeanSnapshot) -> (u64, u64) {
    use cratonvm_gc::gc_metrics::{JMX_SERIAL_OLD_COLLECTOR, JMX_SERIAL_YOUNG_COLLECTOR};
    let count = |name: &str| {
        snap.collectors
            .iter()
            .find(|c| c.name == name)
            .map_or(0, |c| c.count)
    };
    let full = count(JMX_SERIAL_OLD_COLLECTOR);
    (count(JMX_SERIAL_YOUNG_COLLECTOR).saturating_add(full), full)
}

/// gcg g3/x: HotSpot Serial's debug heap blocks around a collection
/// (`CollectedHeap::print_heap_before_gc` / `print_heap_after_gc`, measured on
/// 25.0.3): under `gc,heap` the header `GC(n) Heap <Before|After> GC
/// invocations=<i> (full <f>):` and the `gc,exit` summary's generation and
/// space lines, each prefixed `GC(n) `; under `gc,metaspace` the header
/// `GC(n) Metaspace <Before|After> GC invocations=<i> (full <f>):` and the
/// `Metaspace used ..` line (no `class space` line: HotSpot omits it under
/// `-XX:-UseCompressedClassPointers`, this VM's shape). Young and full
/// collections alike. Returns `(heap_lines, metaspace_lines)`.
fn render_xlog_heap_debug_blocks(
    gc_id: u64,
    when: &str,
    (invocations, full): (u64, u64),
    heap: &crate::runtime::unified_logging::SerialExitHeap,
) -> (Vec<String>, Vec<String>) {
    let lines = crate::runtime::unified_logging::serial_exit_heap_lines(heap);
    let header = |what: &str| {
        format!("GC({gc_id}) {what} {when} GC invocations={invocations} (full {full}):")
    };
    let mut heap_lines = vec![header("Heap")];
    let mut meta_lines = vec![header("Metaspace")];
    // `serial_exit_heap_lines`: `Heap`, six generation / space lines, then the
    // `Metaspace` line.
    for line in lines.iter().skip(1) {
        // gci i33/ms: the `class space` line too.
        if crate::runtime::unified_logging::is_metaspace_print_on_line(line) {
            meta_lines.push(format!("GC({gc_id}) {line}"));
        } else {
            heap_lines.push(format!("GC({gc_id}) {line}"));
        }
    }
    (heap_lines, meta_lines)
}

/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 9): the G1 figures of the debug `Heap Before|After GC` blocks
/// (`unified_logging::g1_exit_heap`: one region-table walk and one
/// class-store walk). `None` unless `events` and `-Xlog:gc+heap=debug` (or
/// `gc+metaspace=debug`) and the heap is G1.
fn g1_heap_debug_sample(
    shared: &SharedVm,
    events: bool,
) -> Option<crate::runtime::unified_logging::G1ExitHeap> {
    if !(events && shared.mem.heap.is_g1() && xlog_heap_debug_on()) {
        return None;
    }
    crate::runtime::unified_logging::g1_exit_heap(shared)
}

/// gci i14/hd: HotSpot G1's `invocations=<n> (full <m>)`: its young and full
/// pauses (`G1 Young Generation` + `G1 Old Generation`; the concurrent
/// cycle's remark and cleanup are not collections there either).
fn g1_invocations(snap: &cratonvm_gc::gc_metrics::GcBeanSnapshot) -> (u64, u64) {
    use cratonvm_gc::gc_metrics::{JMX_G1_OLD_COLLECTOR, JMX_G1_YOUNG_COLLECTOR};
    let count = |name: &str| {
        snap.collectors
            .iter()
            .find(|c| c.name == name)
            .map_or(0, |c| c.count)
    };
    let full = count(JMX_G1_OLD_COLLECTOR);
    (count(JMX_G1_YOUNG_COLLECTOR).saturating_add(full), full)
}

/// gci i14/hd: HotSpot G1's debug blocks (`CollectedHeap::print_heap_before_gc`
/// / `print_heap_after_gc` in the grammar Serial's were measured in,
/// [`render_xlog_heap_debug_blocks`]): under `gc,heap` the header `GC(n) Heap
/// <when> GC invocations=<i> (full <f>):` and `print_heap_on`'s two lines;
/// under `gc,metaspace` the header `GC(n) Metaspace <when> GC ...` and the
/// Metaspace line. Pure.
fn render_xlog_g1_heap_debug_blocks(
    gc_id: u64,
    when: &str,
    (invocations, full): (u64, u64),
    heap: &crate::runtime::unified_logging::G1ExitHeap,
) -> (Vec<String>, Vec<String>) {
    let lines = crate::runtime::unified_logging::g1_exit_heap_lines(heap);
    let header = |what: &str| {
        format!("GC({gc_id}) {what} {when} GC invocations={invocations} (full {full}):")
    };
    let mut heap_lines = vec![header("Heap")];
    let mut meta_lines = vec![header("Metaspace")];
    for line in lines.iter().skip(1) {
        // gci i33/ms: the `class space` line too.
        if crate::runtime::unified_logging::is_metaspace_print_on_line(line) {
            meta_lines.push(format!("GC({gc_id}) {line}"));
        } else {
            heap_lines.push(format!("GC({gc_id}) {line}"));
        }
    }
    (heap_lines, meta_lines)
}

/// gci i14/hd: print [`render_xlog_g1_heap_debug_blocks`] at debug, when
/// both the figures and the bean snapshot (for the invocation counts) exist.
fn log_g1_heap_debug(
    gc_id: u64,
    when: &str,
    heap: Option<&crate::runtime::unified_logging::G1ExitHeap>,
    beans: Option<&cratonvm_gc::gc_metrics::GcBeanSnapshot>,
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let (Some(heap), Some(beans)) = (heap, beans) else {
        return;
    };
    let (heap_lines, meta_lines) =
        render_xlog_g1_heap_debug_blocks(gc_id, when, g1_invocations(beans), heap);
    for line in heap_lines {
        log_unified(&[LogTag::GcHeap], LogLevel::Debug, &line);
    }
    for line in meta_lines {
        log_unified(&[LogTag::GcMetaspace], LogLevel::Debug, &line);
    }
}

/// gcf f4/r: is `-Xlog` taking `gc,age` at debug (HotSpot's `Desired survivor
/// size` line; the age table is at trace)?
fn xlog_age_on() -> bool {
    crate::runtime::unified_logging::is_unified_logging_enabled(
        &[crate::runtime::unified_logging::LogTag::GcAge],
        crate::runtime::unified_logging::LogLevel::Debug,
    )
}

/// gcg g5/yg: does a collection print the `gc,age` block it stashed? Only a
/// young pause does: HotSpot Serial's full collection prints none (vg3
/// `x3_heapdbg_full` printed one under `Pause Full (System.gc())`).
fn xlog_age_lines_printed_for(full: bool) -> bool {
    !full
}

/// Is `-Xlog` taking `tag` at info?
fn xlog_on(tag: crate::runtime::unified_logging::LogTag) -> bool {
    crate::runtime::unified_logging::is_unified_logging_enabled(
        &[tag],
        crate::runtime::unified_logging::LogLevel::Info,
    )
}

/// gci i30/ob hand-back 3 (vi34-ob `.hse`: under `-Xlog:gc+start,gc+tlab=debug`
/// and `-Xlog:gc+phases*=info,gc+start` HotSpot 25 printed NO `gc,start` line,
/// ours one per pause): HotSpot's start line is half of a `GCTraceTime(Info,
/// <tags>)` scope, whose `GCTraceTimeLoggerImpl` arms itself on the SCOPE's
/// own tag set (`LogImpl<tags>::is_level(Info)`) and only then writes the start
/// message on `<tags>,start`. So a start line needs BOTH tag sets selected:
/// `gc` and `gc,start` for `Pause Young` / `Pause Full` / `Pause Remark` /
/// `Pause Cleanup`, `gc,phases` and `gc,phases,start` for a phase. `-Xlog:gc*`
/// selects both; `-Xlog:gc+start` alone selects the start set only (an exact
/// tag set), so it prints nothing. The end line needs only the scope's set.
fn xlog_start_line_on(
    start: crate::runtime::unified_logging::LogTag,
    scope: crate::runtime::unified_logging::LogTag,
) -> bool {
    xlog_on(scope) && xlog_on(start)
}

thread_local! {
    /// zt t31/x10: the full-collection heap dumps this thread's door owes
    /// the `-Xlog` writer ([`FullGcDumpScope`]). Per-thread, per-collection
    /// scratch, not compatibility state.
    static FULL_GC_DUMP_SCOPE: std::cell::RefCell<FullGcDumpScope> =
        std::cell::RefCell::new(FullGcDumpScope::default());
}

/// zt t31/x10: what the `-Xlog` writer of this thread's full collection
/// prints for its heap dumps: the before-dump's console lines and trace
/// time (written before the pause, printed after the collection's
/// `gc,start` line), and the after-dump it runs itself.
#[derive(Default)]
struct FullGcDumpScope {
    before: Option<(u64, Vec<String>, u64)>,
    owed_after: Option<crate::threading::jvm_thread::ThreadId>,
    after_ran: bool,
}

/// zt t31/x10: will the `-Xlog` writer print this collection's `gc,start`
/// line (so a before-dump's lines wait for it)?
pub(super) fn full_gc_dump_before_deferred() -> bool {
    use crate::runtime::unified_logging::LogTag;
    xlog_start_line_on(LogTag::GcStart, LogTag::Gc)
}

/// zt t31/x10: will the `-Xlog` writer print this collection's `gc` end
/// line (so it runs the after-dump before it)?
pub(super) fn full_gc_dump_after_owed_to_writer() -> bool {
    xlog_on(crate::runtime::unified_logging::LogTag::Gc)
}

/// zt t31/x10: keep a before-dump's lines for the writer.
pub(super) fn defer_full_gc_dump_lines(gc_id: u64, lines: Vec<String>, elapsed_ns: u64) {
    FULL_GC_DUMP_SCOPE.with(|s| s.borrow_mut().before = Some((gc_id, lines, elapsed_ns)));
}

/// zt t31/x10: print the kept before-dump lines (the trace scope's start
/// line, the two console lines, the scope's end line) now. No-op when none
/// is kept. The writer calls it right after a collection's `gc,start` line.
pub(super) fn flush_deferred_full_gc_dump_lines() {
    let Some((gc_id, lines, elapsed)) = FULL_GC_DUMP_SCOPE.with(|s| s.borrow_mut().before.take())
    else {
        return;
    };
    full_gc_heap_dump_xlog(gc_id, true, None);
    {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }
    full_gc_heap_dump_xlog(gc_id, true, Some(elapsed));
}

/// zt t31/x10: the writer owes the after-dump of this thread's next full
/// collection, from `initiator`.
pub(super) fn owe_full_gc_dump_after(initiator: crate::threading::jvm_thread::ThreadId) {
    FULL_GC_DUMP_SCOPE.with(|s| {
        let mut s = s.borrow_mut();
        s.owed_after = Some(initiator);
        s.after_ran = false;
    });
}

/// zt t31/x10: take the owed after-dump (`None` when none is owed).
pub(super) fn take_owed_full_gc_dump_after() -> Option<crate::threading::jvm_thread::ThreadId> {
    FULL_GC_DUMP_SCOPE.with(|s| s.borrow_mut().owed_after.take())
}

/// zt t31/x10: did the writer run the owed after-dump? Taken (reset).
pub(super) fn take_full_gc_dump_after_ran() -> bool {
    FULL_GC_DUMP_SCOPE.with(|s| std::mem::take(&mut s.borrow_mut().after_ran))
}

/// zt t31/x10: the writer's point of a full collection's after-dump: run it
/// if one is owed (once).
fn run_owed_full_gc_dump_after(shared: &SharedVm, gc_id: u64) {
    let Some(tid) = take_owed_full_gc_dump_after() else {
        return;
    };
    FULL_GC_DUMP_SCOPE.with(|s| s.borrow_mut().after_ran = true);
    super::gc_and_alloc::run_full_gc_heap_dump_after_by_tid(shared, tid, gc_id);
}

/// zt t31/x10 (`CRATONVM_GC_FULL_GC_HEAP_DUMPS`, item 30h): HotSpot 25's
/// `CollectedHeap::full_gc_dump` scope, `GCTraceTime(Info, gc) tm("Heap Dump
/// (before full gc)")` (or `after`): the start line `GC(<n>) <label>` on
/// `gc,start` (`elapsed_ns` `None`) and the end line `GC(<n>) <label>
/// <t>ms` on `gc` (rows `x10_fd_cmd_*`, `-Xlog:gc,gc+start`).
pub(super) fn full_gc_heap_dump_xlog(gc_id: u64, before: bool, elapsed_ns: Option<u64>) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let label = if before {
        "Heap Dump (before full gc)"
    } else {
        "Heap Dump (after full gc)"
    };
    match elapsed_ns {
        None => {
            if xlog_start_line_on(LogTag::GcStart, LogTag::Gc) {
                log_unified(&[LogTag::GcStart], LogLevel::Info, &format!("GC({gc_id}) {label}"));
            }
        }
        Some(ns) => {
            if xlog_on(LogTag::Gc) {
                crate::runtime::unified_logging::gc_info(&format!(
                    "GC({gc_id}) {label} {:.3}ms",
                    ns as f64 / 1_000_000.0
                ));
            }
        }
    }
}

thread_local! {
    /// zr z3/t2: the classification of the forced (allocation-failure) door
    /// whose pause this thread is about to run -- `true` for an
    /// occupancy-verdict site (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
    /// Set only around `run_collection_pause` by `maybe_gc_forced_collected`
    /// and read by `gc_event_start` on the same thread, so the failure door's
    /// ZGC cause follows the site. A per-thread, per-call scratch value, not
    /// compatibility state.
    static ZGC_FORCED_DOOR_OCCUPANCY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// zt t31/zc11 (program page 1b): the cause of the ZGC mark start this
    /// thread is about to run for a requested concurrent cycle (`System.gc()`,
    /// `jcmd GC.run`, JVMTI, the metadata door). Set only around the mark
    /// start by `gc_and_alloc::zgc_system_gc_runs_concurrent_cycle` and read
    /// by `non_collection_pause_start` on the same thread. A per-call scratch
    /// value, not compatibility state.
    static ZGC_MARK_START_DOOR_CAUSE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// zt t31/zc11 (program page 1b): run `f` (a requested cycle's mark start)
/// with `door`'s cause as the mark-start pause's cause. HotSpot's cycle
/// carries its requester's `GCCause` from the first pause on (a
/// `System.gc()` cycle's `Pause Mark Start` notification says `System.gc()`);
/// the mark start reported the trigger rule's cause (`Warmup`, `Allocation
/// Rate`), which no rule had raised. Restores the previous value after.
pub(super) fn with_zgc_mark_start_door<R>(door: GcDoor, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<&'static str>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ZGC_MARK_START_DOOR_CAUSE.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(ZGC_MARK_START_DOOR_CAUSE.with(|c| c.replace(Some(door.cause()))));
    f()
}

/// The cause [`with_zgc_mark_start_door`] set; `None` outside it.
fn zgc_mark_start_door_cause() -> Option<&'static str> {
    ZGC_MARK_START_DOOR_CAUSE.with(std::cell::Cell::get)
}

/// zt t32 (successor page section 10): the cause of a requested cycle's mark
/// start on ANY thread -- this thread's door ([`with_zgc_mark_start_door`]),
/// else the VM's pending request (`ConcurrentCycleJfr::requested_cause`). A
/// requested cycle is a recorded every-trigger request, so a peer's
/// allocating poll can run its mark start first; that pause, and the cycle,
/// still carry the requester's cause. `None` for a rule-opened cycle.
pub(super) fn zgc_requested_mark_start_cause(shared: &SharedVm) -> Option<&'static str> {
    zgc_mark_start_door_cause()
        .or_else(|| zgc_concurrent_cycle_jfr(shared).and_then(|c| c.requested_cause()))
}

/// zr z3/t2: run `f` (the forced door's pause) with this thread's forced-door
/// classification set to `occupancy`, restoring the previous value after
/// (a nested forced collection inside a delivery cannot leak its value).
pub(super) fn with_zgc_forced_door_occupancy<R>(occupancy: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ZGC_FORCED_DOOR_OCCUPANCY.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(ZGC_FORCED_DOOR_OCCUPANCY.with(|c| c.replace(occupancy)));
    f()
}

/// The classification [`with_zgc_forced_door_occupancy`] set; `false` (a
/// failed allocation) outside it.
fn zgc_forced_door_occupancy() -> bool {
    ZGC_FORCED_DOOR_OCCUPANCY.with(std::cell::Cell::get)
}

/// zr z2/t2: the end of one mutator ALLOCATION STALL on ZGC -- a forced
/// (allocation-failure) collection that is not the compiled refill trigger's
/// occupancy cycle, measured from `t0` (taken before the pause was requested)
/// to now. Recorded on the heap's `ZgcMetrics` (`CRATONVM_ZGC_METRICS=1`
/// prints the stall count / total / max in the run summary; the TSV carries
/// them) -- the first production feed of those columns
/// (`zgc-zr1tr-allocation-stalls-are-not-recorded-FIXED-20261012.md`).
///
/// With `-Xlog:gc` (the exact `gc` tag set at `info`, as HotSpot's
/// `LogTarget(Info, gc)` for a verbose critical phase; `-verbose:gc` too)
/// also prints HotSpot ZGC's `Allocation Stall (<thread>) <ms>ms` line.
/// Default ON since zt t14/ca5; `CRATONVM_ZGC_XLOG_ALLOCATION_STALL=0` kills
/// the line, never the recording. A no-op off ZGC.
pub(super) fn zgc_allocation_stall_finish(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    t0: std::time::Instant,
    page_at_entry: Option<(&'static str, i64)>,
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let heap = &shared.mem.heap;
    if heap.is_generational() || heap.is_g1() {
        return;
    }
    let nanos = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
    heap.zgc_note_allocation_stall(nanos);
    // zr z4/jf: `jdk.ZAllocationStall` (a no-op unless a recording runs).
    zgc_allocation_stall_jfr(shared, thread, nanos, page_at_entry);
    if xlog_on(LogTag::Gc) && zgc_xlog_allocation_stall_enabled() {
        log_unified(
            &[LogTag::Gc],
            LogLevel::Info,
            &zgc_allocation_stall_line(&thread.name, nanos),
        );
    }
}

/// zki (page `zgc-zt23hunt4-transparent-huge-pages-flag-ignored` item 4):
/// on a ZGC heap backed by explicit hugetlbfs pages, HotSpot 25's lines for a
/// commit the pool refused after start-up -- the `Failed to commit memory
/// (Not enough space)` lines, then `Forced to lower max Java heap size` --
/// once, before the collection the failed allocation asks for (host row
/// `zki_f4`: after the running cycle's start line, before the stall
/// collection's). A no-op on any other heap (one load).
pub(super) fn zgc_report_hugetlb_refusals(shared: &SharedVm) {
    #[cfg(feature = "zgc")]
    {
        use crate::runtime::unified_logging as ul;
        let Some(report) = shared.mem.heap.zgc_take_hugetlb_refusal_report() else {
            return;
        };
        let uptime = shared.debug.diagnostic_counters.uptime_secs();
        for _ in 0..report.failed_commit_lines {
            ul::log_gc_error_line(ul::ZGC_COMMIT_FAILED_ERROR, uptime);
        }
        ul::log_gc_error_line(
            &ul::zgc_forced_lower_max_text(
                u64::try_from(report.max_capacity).unwrap_or(u64::MAX),
                u64::try_from(report.lowered_to).unwrap_or(u64::MAX),
            ),
            uptime,
        );
    }
    #[cfg(not(feature = "zgc"))]
    let _ = shared;
}

/// HotSpot ZGC's stall line (`ZStatCriticalPhase::register_end`, verbose
/// phase `Allocation Stall`): `Allocation Stall (main) 0.204ms`.
fn zgc_allocation_stall_line(thread_name: &str, nanos: u64) -> String {
    // Lossless for any stall shorter than 2^53 ns (104 days).
    let ms = nanos as f64 / 1_000_000.0;
    format!("Allocation Stall ({thread_name}) {ms:.3}ms")
}

/// `CRATONVM_ZGC_XLOG_ALLOCATION_STALL` -- default ON (zt t14/ca5), `=0`
/// kills the line. Opt-in until the t12 rows compared it with HotSpot 25.0.3
/// under `-Xlog:gc`: `[info][gc] Allocation Stall (main) N.NNNms` on both
/// (`t12-row-verdicts.md` "st", `st_T13a` SAME). A logging spelling, not
/// compatibility state. gci i20/ob: read from the flag snapshot per call
/// (once per reported stall, after `xlog_on`), not through a process
/// `OnceLock` latch (`per_vm_state_statics_ratchet`), so a per-thread test
/// override reaches it.
fn zgc_xlog_allocation_stall_enabled() -> bool {
    true
}

/// zt t24/hunt7 (`zgc-zt24hunt7-out-of-memory-xlog-line-missing-FIXED-20261021.md`):
/// a heap `OutOfMemoryError` (`Java heap space`) raised on ZGC prints HotSpot
/// 25 ZGC's `Out Of Memory (<thread>)` line under `-Xlog:gc` (the exact `gc`
/// tag set at `info`; `-verbose:gc` too), as `ZHeap::out_of_memory` does for
/// the allocation that failed. Measured on 25.0.3 (`sp4cpu_cap_D_jo`,
/// `Zr2CpCapacity grow -XX:+UseZGC -Xmx256m -Xlog:gc:stderr`):
/// `[1.722s][info][gc] Allocation Stall (main) 66.745ms`, then
/// `[1.722s][info][gc] Out Of Memory (main)`. This VM printed nothing.
/// `CRATONVM_ZGC_XLOG_OUT_OF_MEMORY=0` kills the line. A no-op off ZGC.
pub(super) fn zgc_out_of_memory_logged(shared: &SharedVm, thread_name: &str) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let heap = &shared.mem.heap;
    if heap.is_generational() || heap.is_g1() {
        return;
    }
    if xlog_on(LogTag::Gc) && zgc_xlog_out_of_memory_enabled() {
        log_unified(
            &[LogTag::Gc],
            LogLevel::Info,
            &zgc_out_of_memory_line(thread_name),
        );
    }
}

/// HotSpot ZGC's `ZHeap::out_of_memory` line: `Out Of Memory (main)`.
fn zgc_out_of_memory_line(thread_name: &str) -> String {
    format!("Out Of Memory ({thread_name})")
}

/// `CRATONVM_ZGC_XLOG_OUT_OF_MEMORY` -- default ON (zt t24/hunt7), `=0`
/// kills the line. Read per raised error, after `xlog_on`.
fn zgc_xlog_out_of_memory_enabled() -> bool {
    true
}

// ---------------------------------------------------------------------------
// zr z4/jf: ZGC's JFR events
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/zgc-architecture.md`.
// Both JFR backends: the real JDK backend (`jfr_vm_more`, a recording the
// JDK's own Java started: `jdk.jfr.Recording`, `jcmd JFR.start`) and the
// legacy recorder (`cratonvm_jfr`, `-XX:StartFlightRecording`). Every row is a
// fact the collector measured; nothing is emitted while no recording runs.

/// `CRATONVM_ZGC_JFR_EVENTS` -- default ON, `=0` restores the pre-z4 ZGC JFR
/// shape (no `GCPhasePauseLevel1`, mark-start, `Concurrent Mark`,
/// `ZAllocationStall`, `ZRelocationSet[Group]`, `ZUncommit`,
/// `ZOldGarbageCollection` rows; the real backend's `Old`/
/// `YoungGarbageCollection` row back on ZGC collections). An observability
/// spelling, not compatibility state. gci i20/ob: read from the flag snapshot
/// per call (every caller tests the ZGC heap or the event kind first; at most
/// a few reads per collection) instead of a process `OnceLock` latch.
fn zgc_jfr_events_enabled() -> bool {
    true
}



/// gci i20/ob: [`zgc_jfr_pause_level1_rows`] reading the switch only when the
/// collection has ZGC facts, so a Generational or G1 collection reads no flag.
fn zgc_jfr_pause_level1_rows_for(zgc_facts: bool) -> bool {
    !zgc_facts || zgc_jfr_pause_level1_rows(zgc_facts, true)
}







/// `CRATONVM_ZGC_JFR_DRIVER_THREADS` -- default ON (zs s8/jf2): a ZGC
/// collection's own JFR events (real backend) are committed on HotSpot's
/// collector thread names, `ZDriverMajor` for a whole-heap collection and its
/// mark-start pause / concurrent mark, `ZDriverMinor` for a young one; the
/// safepoint rows stay on `VM Thread`. `=0` = everything on `VM Thread`.
/// Read per call.
fn zgc_jfr_driver_threads_enabled() -> bool {
    true
}

/// zs s8/jf2: the collector thread a ZGC collection's events name
/// ([`zgc_jfr_driver_threads_enabled`]); `None` = `VM Thread`.
fn zgc_driver_thread(major: bool) -> Option<&'static str> {
    use cratonvm_native_builtins_jfr::jfr_vm_more::{ZGC_DRIVER_MAJOR, ZGC_DRIVER_MINOR};
    zgc_jfr_driver_threads_enabled().then_some(if major {
        ZGC_DRIVER_MAJOR
    } else {
        ZGC_DRIVER_MINOR
    })
}

/// `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS` -- default ON (zs s7/jf): a ZGC
/// collection writes `jdk.GCReferenceStatistics` (its reference pass's
/// counts), and a generational-mode `ZGC Minor` collection
/// `jdk.ZYoungGarbageCollection`. `=0` writes neither. Read per call.
pub(super) fn zgc_jfr_ref_and_young_enabled() -> bool {
    true
}





/// zs s7/jf: `NativeContext::jfr_system_gc_on_caller` -- `Some(invoked
/// concurrent)` when the `Runtime.gc()` native writes `jdk.SystemGC` itself:
/// ZGC with its JFR events on (the same test that sets
/// [`JfrCollection::zgc`], so exactly the collections that leave the event
/// out), an explicit GC this VM does not ignore, and the switch on.
pub(crate) fn zgc_jfr_system_gc_on_caller(shared: &SharedVm) -> Option<bool> {
    use crate::config::ExplicitGcPolicy;
    if !heap_is_zgc(shared)
        || !zgc_jfr_events_enabled()
        || true
    {
        return None;
    }
    match shared.config.explicit_gc {
        ExplicitGcPolicy::Disabled => None,
        policy => Some(policy == ExplicitGcPolicy::Concurrent),
    }
}





/// `CRATONVM_GC_JFR_REFERENCE_STATISTICS` -- default ON (g1gen/c1b): the
/// reference round's per-type counts become the collection's four
/// `jdk.GCReferenceStatistics` rows on G1 and Generational, as HotSpot 25
/// writes them on every collector. `=0`: none off ZGC (ZGC keeps
/// `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS`).
pub(crate) fn gc_jfr_reference_statistics_enabled() -> bool {
    true
}

/// zt t29/g1: whether this collection writes the
/// `jdk.MetaspaceChunkFreeListSummary` rows -- `zgc` is whether the heap is
/// ZGC, `zgc_facts` whether its ZGC JFR facts were made (the pre-t29 gate).
/// Pure over the two switches.
fn jfr_metaspace_chunk_free_lists(zgc: bool, zgc_facts: bool) -> bool {
    !zgc || (zgc_facts)
}

/// `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS` -- default ON (zt t1/jf): the real
/// backend writes a ZGC collection's stop-the-world time as HotSpot 25 ZGC's
/// VM operations: one safepoint (`SafepointBegin`, `ExecuteVMOperation`,
/// `SafepointEnd`) per named pause row, the operation
/// `jfr_vm_more::zgc_pause_vm_operation` names (`ZMarkStartYoungAndOld`,
/// `ZMarkEndOld`, `ZRelocateStartOld`; the `Young` ones for a minor) and the
/// collector thread as `caller`. It wrote one safepoint per collection,
/// operation `Collect` (`jfr_vm_more::gc_vm_operation`'s fallback; ZGC's
/// collections are never `old`), with the Java requester as `caller`. Only
/// with named pause rows (`CRATONVM_ZGC_JFR_HOTSPOT_PAUSES`); zt t2/jf (R6):
/// without them the one row is named by the cycle's opening operation
/// (`jfr_vm_more::gc_vm_operation`). `=0`: the one `Collect` row. Read per
/// call.
fn zgc_jfr_pause_vm_operations_enabled() -> bool {
    true
}

/// HotSpot 25 ZGC's pause names (`ZStatPhasePause`): the mark start of a major
/// (whole-heap) collection, of a young one, its mark end, its relocate start.
const ZGC_PAUSE_MARK_START_MAJOR: &str = "Pause Mark Start (Major)";
const ZGC_PAUSE_MARK_END: &str = "Pause Mark End";
const ZGC_PAUSE_RELOCATE_START: &str = "Pause Relocate Start";

/// zs s7/jf: a ZGC collection's one stop-the-world pause as HotSpot 25's pause
/// rows `(name, wall start ns, duration ns)`, cut at the pause's own phase
/// boundaries (the split zs s4/ob3 books the `... Pauses` bean with): `Pause
/// Mark Start (Major)` (`Pause Mark Start` for a young collection) up to the
/// end of the object-start snapshot -- absent when the cycle's mark start was
/// a pause of its own --, `Pause Mark End` up to the end of reference
/// processing, `Pause Relocate Start` to the end. The rows are contiguous and
/// sum to the pause. Empty (the caller writes the one `GC Pause` row) without
/// a split or with one that does not fit the pause.
fn zgc_hotspot_pause_rows(
    split: Option<&cratonvm_gc::gc_metrics::ZgcPauseSplit>,
    t0: std::time::Instant,
    wall_start_ns: u64,
    duration_ns: u64,
    major: bool,
) -> Vec<(&'static str, u64, u64)> {
    let Some(split) = split else {
        return Vec::new();
    };
    let end = t0 + std::time::Duration::from_nanos(duration_ns);
    if !split.fits(t0, end) {
        return Vec::new();
    }
    let (spans, n) = split.spans(t0, end);
    let mark_start = if major {
        ZGC_PAUSE_MARK_START_MAJOR
    } else {
        ZGC_PAUSE_MARK_START
    };
    let names: &[&'static str] = match n {
        3 => &[mark_start, ZGC_PAUSE_MARK_END, ZGC_PAUSE_RELOCATE_START],
        2 => &[ZGC_PAUSE_MARK_END, ZGC_PAUSE_RELOCATE_START],
        _ => return Vec::new(),
    };
    names
        .iter()
        .zip(spans.iter())
        .map(|(&name, &(s, e))| {
            (
                name,
                wall_start_ns.saturating_add(nanos_between(t0, s)),
                nanos_between(s, e),
            )
        })
        .collect()
}

/// `CRATONVM_ZGC_XLOG_SAFEPOINT_LINES` -- default ON, `=0` kill (zt
/// t22/hunt3): under `-Xlog:safepoint` a ZGC collection prints HotSpot 25
/// ZGC's `Safepoint "<op>"` lines -- the operations the real JFR backend
/// writes for it (`ZMarkStartYoungAndOld`, `ZMarkEndOld`,
/// `ZRelocateStartOld`; the `Young` ones for a minor), cut at its pause rows'
/// boundaries ([`zgc_safepoint_ops`]) -- and a concurrent cycle's own
/// mark-start pause prints `ZMarkStartYoungAndOld`, as its JFR row is named.
/// It printed no line for any ZGC pause (gci i45/sl had left ZGC "out of
/// scope"), so the default collector's `-Xlog:safepoint` was empty. `=0`: no
/// ZGC line, as before. Read once per ZGC pause, only under the tag.
fn zgc_xlog_safepoint_lines_enabled() -> bool {
    true
}

/// zt t22/hunt3: the operation of a ZGC concurrent cycle's own mark-start
/// pause under `-Xlog:safepoint` (`jfr_vm_more::zgc_pause_vm_operation` of
/// `Pause Mark Start (Major)`, as its JFR row is named).
const ZGC_MARK_START_MAJOR_OPERATION: &str = "ZMarkStartYoungAndOld";

/// zt t24/zc8: the operation of a failed Pause Mark End
/// (`NonCollectionPause::ZgcMarkEndContinue`) under `-Xlog:safepoint`
/// (`jfr_vm_more::zgc_pause_vm_operation` of a major's `Pause Mark End`).
const ZGC_MARK_END_OLD_OPERATION: &str = "ZMarkEndOld";

/// zt t22/hunt3: the HotSpot 25 operations one ZGC collection's pause prints
/// as under `-Xlog:safepoint`, and the instants between them: the names and
/// boundaries of its JFR pause rows ([`zgc_hotspot_pause_rows`],
/// `jfr_vm_more::zgc_pause_vm_operation`). Without a split (or with one of
/// another shape) the one operation that opens the cycle, as the JFR backend
/// names its one row then. `names` has one entry more than the cuts. Pure.
fn zgc_safepoint_ops(
    split: Option<&cratonvm_gc::gc_metrics::ZgcPauseSplit>,
    major: bool,
) -> (Vec<&'static str>, Vec<std::time::Instant>) {
    let op = |pause: &str| {
        cratonvm_native_builtins_jfr::jfr_vm_more::zgc_pause_vm_operation(pause, major)
    };
    let mark_start = if major {
        ZGC_PAUSE_MARK_START_MAJOR
    } else {
        ZGC_PAUSE_MARK_START
    };
    match split {
        Some(s) if s.pauses() == 3 => (
            vec![
                op(mark_start),
                op(ZGC_PAUSE_MARK_END),
                op(ZGC_PAUSE_RELOCATE_START),
            ],
            s.bounds().to_vec(),
        ),
        Some(s) if s.pauses() == 2 => (
            vec![op(ZGC_PAUSE_MARK_END), op(ZGC_PAUSE_RELOCATE_START)],
            s.bounds().to_vec(),
        ),
        _ => (vec![op(mark_start)], Vec::new()),
    }
}

/// zr z9/hp: whether a collection's level-1 pause rows are written. Off ZGC
/// (no ZGC facts: Generational, G1, or `CRATONVM_ZGC_JFR_EVENTS=0`) always;
/// on ZGC only with the HotSpot phase set killed.
fn zgc_jfr_pause_level1_rows(zgc_facts: bool, hotspot_phase_set: bool) -> bool {
    !zgc_facts || !hotspot_phase_set
}

/// Whether either JFR backend records.
fn any_jfr_recording() -> bool {
    cratonvm_jfr::is_enabled() || cratonvm_native_builtins_jfr::jfr_vm_events::any_recording()
}

/// The heap is ZGC (neither Generational nor G1), the test
/// [`zgc_allocation_stall_finish`] uses.
fn heap_is_zgc(shared: &SharedVm) -> bool {
    let heap = &shared.mem.heap;
    !heap.is_generational() && !heap.is_g1()
}

/// One ZGC collection's JFR facts, as the collector left them
/// (`ZgcRealHeap::take_jfr_cycle`), in the shapes the writers take.
#[derive(Debug, Default)]
struct ZgcCycleFacts {
    marks: Vec<(&'static str, u128)>,
    marked_concurrently: bool,
    relocation_set: Option<(i64, i64, i64)>,
    relocation_groups: Vec<(&'static str, i64, i64, i64, i64, i64)>,
    uncommit: Option<(i64, i64)>,
    /// zs s7/jf: `jdk.GCReferenceStatistics` (soft, weak, final, phantom).
    reference_counts: Option<[i64; 4]>,
    /// zs s10/jf3: the concurrent cycle this collection closed: its `(cycle
    /// id, mark-start pause wall start ns, duration ns)`, set by
    /// [`zgc_collection_jfr`] and consumed by [`zgc_cycle_shape`].
    initial_pause: Option<(u64, u64, u64)>,
    /// zt t2/jf (R2): that cycle's heap `(used, committed)` at its mark-start
    /// pause; `Some` only with [`Self::initial_pause`].
    cycle_heap_before: Option<(u64, u64)>,
}

#[cfg(feature = "zgc")]
fn zgc_take_cycle_facts(shared: &SharedVm) -> Option<ZgcCycleFacts> {
    let c = shared.mem.heap.zgc_take_jfr_cycle()?;
    let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let (relocation_set, relocation_groups) = match c.relocation_set {
        Some(r) => (
            Some((i(r.total), i(r.empty), i(r.relocate))),
            r.groups
                .iter()
                .map(|g| {
                    (
                        g.page_type,
                        i(g.candidate_pages),
                        i(g.total),
                        i(g.empty),
                        i(g.selected_pages),
                        i(g.relocate),
                    )
                })
                .collect(),
        ),
        None => (None, Vec::new()),
    };
    Some(ZgcCycleFacts {
        marks: c.marks,
        marked_concurrently: c.marked_concurrently,
        relocation_set,
        relocation_groups,
        uncommit: c.uncommit.map(|(b, n)| (i(b), i(n))),
        reference_counts: c
            .reference_counts
            .filter(|_| zgc_jfr_ref_and_young_enabled())
            .map(|r| r.map(i)),
        initial_pause: None,
        cycle_heap_before: None,
    })
}

#[cfg(not(feature = "zgc"))]
fn zgc_take_cycle_facts(_shared: &SharedVm) -> Option<ZgcCycleFacts> {
    None
}

/// The ZGC bean set's concurrent-cycle JFR state (the per-heap
/// `ConcurrentCycleJfr` the generational backend uses for the same purpose),
/// or `None` off ZGC / without beans.
pub(super) fn zgc_concurrent_cycle_jfr(
    shared: &SharedVm,
) -> Option<std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr>> {
    if !heap_is_zgc(shared) {
        return None;
    }
    let set = shared.mem.backend_gc_beans()?;
    Some(std::sync::Arc::clone(
        set.notifications().concurrent_cycle_jfr(),
    ))
}

/// The JFR half of a ZGC mark-start pause, prepared inside the pause by
/// [`non_collection_pause_start`] and emitted by [`non_collection_pause_finish`]
/// after the world is released. Owns everything (the finish has the timer
/// only).
struct ZgcMarkStartJfr {
    vm: usize,
    /// The id the collection that closes this cycle will take: the VM's next
    /// `gc_cycle_count`. Peeked, not taken -- the closing collection takes it
    /// (`gc_event_finish`), so the cycle's pause, its `Concurrent Mark` and its
    /// closing collection share one `gcId`, as HotSpot's do.
    gc_id: u64,
    wall_start_ns: u64,
    /// The legacy recorder's `jdk.GCPhasePause` type, resolved with `try_lock`
    /// inside the pause (`None`: no legacy recording, or the recorder busy).
    legacy_pause_type: Option<cratonvm_jfr::EventTypeId>,
    cycle: Option<std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr>>,
    /// Stamped at the seal: the pause opened a concurrent mark.
    opened: bool,
    /// zt t2/jf (R2): the heap's `(used, committed)` bytes as the pause
    /// found it, for the cycle's `Before GC` heap summary.
    heap_before: (u64, u64),
    /// zt t31/zc11 (program page 1f residual R1): this is a failed Pause Mark
    /// End attempt (`NonCollectionPause::ZgcMarkEndContinue`), not a mark
    /// start: its own `Pause Mark End` row under the cycle's id.
    mark_end_attempt: bool,
}

fn zgc_mark_start_jfr(shared: &SharedVm, kind: NonCollectionPause) -> Option<ZgcMarkStartJfr> {
    let mark_end_attempt = kind == NonCollectionPause::ZgcMarkEndContinue;
    let xlog_mark_start = kind == NonCollectionPause::ZgcMarkStart
        && zgc_xlog_pause_phases_enabled()
        && crate::runtime::unified_logging::xlog_full_gc_phases_enabled();
    if (kind != NonCollectionPause::ZgcMarkStart && !mark_end_attempt)
        || !zgc_jfr_events_enabled()
        || (!any_jfr_recording() && !xlog_mark_start)
    {
        return None;
    }
    let legacy_pause_type = if cratonvm_jfr::is_enabled() && any_jfr_recording() {
        shared
            .debug
            .flight_recorder
            .try_lock()
            .and_then(|r| cratonvm_jfr::builtin::gc_phase_pause_type(&r))
    } else {
        None
    };
    Some(ZgcMarkStartJfr {
        vm: shared.vm_identity,
        gc_id: shared
            .mem
            .gc_cycle_count
            .load(std::sync::atomic::Ordering::Relaxed),
        wall_start_ns: wall_clock_ns(),
        legacy_pause_type,
        cycle: zgc_concurrent_cycle_jfr(shared),
        opened: false,
        heap_before: (
            gc_event_heap_used(&shared.mem.heap) as u64,
            shared.mem.heap.committed_bytes() as u64,
        ),
        mark_end_attempt,
    })
}

/// HotSpot ZGC's name of the pause that opens a marking cycle.
const ZGC_PAUSE_MARK_START: &str = "Pause Mark Start";
/// HotSpot ZGC's name of its concurrent marking phase.
const ZGC_CONCURRENT_MARK: &str = "Concurrent Mark";
/// The level-0 `jdk.GCPhasePause` name of a ZGC collection's one pause (the
/// real backend's name since SBR5; a whole stop-the-world collection has no
/// HotSpot ZGC pause name -- its sub-phases are the level-1 rows).
const ZGC_COLLECTION_PAUSE: &str = "GC Pause";

fn emit_zgc_mark_start_jfr(row: ZgcMarkStartJfr, work_ns: u64) {
    // zt t31/zc11 (1f R1): a failed Pause Mark End attempt is a pause of the
    // open cycle, as HotSpot's `ZDriver` records each `pause_mark_end()`
    // that returns false: one `jdk.GCPhasePause` row `Pause Mark End` (and
    // its `ZMarkEndOld` operation) under the id the closing collection takes.
    // It had no row at all. Written now: the cycle keeps nothing for it.
    if row.mark_end_attempt {
        zgc_write_pause_rows(
            row.vm,
            row.legacy_pause_type,
            row.gc_id,
            ZGC_PAUSE_MARK_END,
            row.wall_start_ns,
            work_ns,
        );
        return;
    }
    // zs s10/jf3 (ported by zt t1/jf): a mark start that opened a concurrent
    // cycle is written by the collection that closes the cycle, as the first
    // of the cycle's pauses inside its `jdk.GarbageCollection` span
    // (HotSpot's shape, `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN`); a refused
    // one (no cycle) is written here as before.
    if row.opened {
        if let Some(cycle) = row.cycle.as_ref() {
            // zt t1/jf: a mark-start pause an earlier cycle left noted (that
            // cycle never closed) is still a pause: write it now rather than
            // lose it under this one.
            if let Some((stale_id, start, dur)) = cycle.take_any_initial_pause() {
                zgc_write_mark_start_rows(row.vm, row.legacy_pause_type, stale_id, start, dur);
            }
            cycle.open(row.gc_id);
            cycle.mark_started(row.gc_id, row.wall_start_ns.saturating_add(work_ns));
            cycle.note_initial_pause(row.gc_id, row.wall_start_ns, work_ns);
            // zt t2/jf (R2): the cycle's `Before GC` heap figures.
            let (used, committed) = row.heap_before;
            cycle.note_cycle_heap_before(row.gc_id, used, committed);
            return;
        }
    }
    if any_jfr_recording() {
        zgc_write_mark_start_rows(
            row.vm,
            row.legacy_pause_type,
            row.gc_id,
            row.wall_start_ns,
            work_ns,
        );
    }
    if let Some(cycle) = row.cycle {
        if row.opened {
            cycle.open(row.gc_id);
            cycle.mark_started(row.gc_id, row.wall_start_ns.saturating_add(work_ns));
        }
    }
}

/// A ZGC mark-start pause row on its own (the s7 shape: `Pause Mark Start`,
/// under the cycle's `gcId`), to the real backend and, when its type was
/// resolved inside the pause, the legacy recorder. Called with the world
/// running.
///
/// zt t3/jf (zt2x1 item 2): with `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN` on
/// (HotSpot's shape) the row is named `Pause Mark Start (Major)` -- this VM's
/// concurrent cycle is whole-heap -- and, under
/// `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS`, gets its own safepoint and
/// `ZMarkStartYoungAndOld` operation (real backend), as a joined row does.
/// Under the span's `=0` kill: the s7 shape (`Pause Mark Start`, no
/// operation).
fn zgc_write_mark_start_rows(
    vm: usize,
    legacy_pause_type: Option<cratonvm_jfr::EventTypeId>,
    gc_id: u64,
    start_ns: u64,
    dur_ns: u64,
) {
    let name = {
        ZGC_PAUSE_MARK_START_MAJOR
    };
    zgc_write_pause_rows(vm, legacy_pause_type, gc_id, name, start_ns, dur_ns);
}

/// [`zgc_write_mark_start_rows`]'s writer for any of a concurrent cycle's
/// pauses written on their own (`name`): the `jdk.GCPhasePause` row, its
/// operation in HotSpot's shape, the legacy recorder's row.
fn zgc_write_pause_rows(
    vm: usize,
    legacy_pause_type: Option<cratonvm_jfr::EventTypeId>,
    gc_id: u64,
    name: &'static str,
    start_ns: u64,
    dur_ns: u64,
) {
    let hotspot_shape = true;
    let id = gc_id as i32; // Truncation: deliberate wrap, as `gc_event_finish`.
    cratonvm_native_builtins_jfr::jfr_vm_more::gc_phase(
        vm,
        "jdk.GCPhasePause",
        id,
        name,
        start_ns,
        dur_ns,
        zgc_driver_thread(true),
    );
    if hotspot_shape && zgc_jfr_pause_vm_operations_enabled() {
        cratonvm_native_builtins_jfr::jfr_vm_more::zgc_vm_operation_pause(
            vm,
            cratonvm_native_builtins_jfr::jfr_vm_more::zgc_pause_vm_operation(name, true),
            start_ns,
            dur_ns,
            zgc_driver_thread(true),
        );
    }
    if let Some(ty) = legacy_pause_type {
        cratonvm_jfr::builtin::emit_gc_phase_pause_event_typed(ty, id, name, start_ns, dur_ns);
    }
}

/// zs s10/jf3: [`zgc_write_mark_start_rows`] from a collection's finish
/// (world running), the legacy recorder's type resolved under its lock.
fn zgc_emit_mark_start_row(shared: &SharedVm, gc_id: u64, start_ns: u64, dur_ns: u64) {
    let legacy = if cratonvm_jfr::is_enabled() {
        let jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::gc_phase_pause_type(&jfr)
    } else {
        None
    };
    zgc_write_mark_start_rows(shared.vm_identity, legacy, gc_id, start_ns, dur_ns);
}

/// zs s10/jf3: the closing collection `gc_id`'s cycle shape: the pause rows
/// with the cycle's mark-start pause first, and the cycle's span `(start,
/// duration)`. `initial` is the cycle's `(cycle id, start, duration)`. Rows
/// and span are unchanged (and the mark-start row is written on its own)
/// unless it is this collection's cycle, the collection has named rows, and
/// the pause started no later than the collection ended.
fn zgc_join_initial_pause(
    gc_id: u64,
    initial: Option<(u64, u64, u64)>,
    pauses: &mut Vec<(&'static str, u64, u64)>,
    major: bool,
    collection_end_ns: u64,
) -> Option<(u64, u64)> {
    let (cycle_id, start, dur) = initial?;
    if cycle_id != gc_id || pauses.is_empty() || start > collection_end_ns {
        return None;
    }
    pauses.insert(0, (zgc_mark_start_pause_name(major), start, dur));
    Some((start, collection_end_ns - start))
}

fn zgc_take_xlog_initial_pause(shared: &SharedVm) -> Option<(u64, u64, u64)> {
    let cycle = zgc_concurrent_cycle_jfr(shared)?;
    match cycle.take() {
        Some((cycle_id, _)) => {
            let initial = cycle.take_initial_pause(cycle_id);
            let _ = cycle.take_cycle_heap_before(cycle_id);
            initial.map(|(start, dur)| (cycle_id, start, dur))
        }
        None => {
            let _ = cycle.take_any_initial_pause();
            None
        }
    }
}

fn zgc_cycle_shape(
    shared: &SharedVm,
    gc_id: u64,
    initial: Option<(u64, u64, u64)>,
    pauses: &mut Vec<(&'static str, u64, u64)>,
    major: bool,
    collection_end_ns: u64,
) -> Option<(u64, u64)> {
    let joined = zgc_join_initial_pause(gc_id, initial, pauses, major, collection_end_ns);
    if joined.is_none() {
        if let Some((cycle_id, start, dur)) = initial {
            zgc_emit_mark_start_row(shared, cycle_id, start, dur);
        }
    }
    joined
}

/// HotSpot 25 ZGC's name of a cycle's mark-start pause: `Pause Mark Start
/// (Major)` for a whole-heap cycle, `Pause Mark Start` for a young one. Pure.
fn zgc_mark_start_pause_name(major: bool) -> &'static str {
    if major {
        ZGC_PAUSE_MARK_START_MAJOR
    } else {
        ZGC_PAUSE_MARK_START
    }
}

/// A ZGC collection's own rows beyond the shared ones: the cycle's
/// `Concurrent Mark` (only when this collection closed a concurrent mark a
/// recorded mark-start pause opened), and the facts for
/// [`emit_jfr_collection`]. Takes the collector's facts and the open cycle
/// either way, so neither outlives this collection.
fn zgc_collection_jfr(shared: &SharedVm, pause_start_ns: u64) -> Option<ZgcCycleFacts> {
    if !heap_is_zgc(shared) || !zgc_jfr_events_enabled() {
        return None;
    }
    let mut facts = zgc_take_cycle_facts(shared).unwrap_or_default();
    let cycle = zgc_concurrent_cycle_jfr(shared);
    let open = cycle.as_ref().and_then(|c| c.take());
    match open {
        Some((cycle_id, mark_start_ns)) => {
            // zs s10/jf3: the cycle's mark-start pause, for its span and rows.
            let initial = cycle.as_ref().and_then(|c| c.take_initial_pause(cycle_id));
            // zt t2/jf (R2): taken whatever happens, so it never outlives
            // this collection.
            let heap_before = cycle
                .as_ref()
                .and_then(|c| c.take_cycle_heap_before(cycle_id));
            if facts.marked_concurrently && mark_start_ns != 0 && mark_start_ns <= pause_start_ns {
                zgc_emit_concurrent_mark(
                    shared,
                    cycle_id,
                    mark_start_ns,
                    pause_start_ns - mark_start_ns,
                );
                match initial.filter(|&(start, _)| start <= pause_start_ns) {
                    Some((start, dur)) => {
                        facts.initial_pause = Some((cycle_id, start, dur));
                        facts.cycle_heap_before = heap_before;
                    }
                    None => {
                        if let Some((start, dur)) = initial {
                            zgc_emit_mark_start_row(shared, cycle_id, start, dur);
                        }
                    }
                }
            } else if let Some((start, dur)) = initial {
                // A cycle this collection did not close concurrently (a
                // `System.gc()` that abandoned it and marked afresh, a refused
                // certification). zt t5/jf: its mark-start pause carries the
                // id this collection takes (the mark start peeks the next
                // `gc_cycle_count`), so written on its own it was a pause row
                // of this collection lying before its `GarbageCollection`
                // (local row `jf2_t2_sysgc_conc`: `pause ... inside_gc=false`,
                // HotSpot `true`). It joins the collection's rows and span
                // instead, as a closed cycle's does (`zgc_cycle_shape` still
                // writes it on its own when the ids differ). Under
                // `CRATONVM_ZGC_JFR_STANDALONE_MARK_START_JOINS=0`: on its own.
                if start <= pause_start_ns {
                    facts.initial_pause = Some((cycle_id, start, dur));
                    facts.cycle_heap_before = heap_before;
                } else {
                    zgc_emit_mark_start_row(shared, cycle_id, start, dur);
                }
            }
        }
        None => {
            // zt t1/jf: no open cycle, but a noted mark-start pause (its
            // cycle was taken without one): write it rather than leave it to
            // be dropped by the next cycle.
            let stale = cycle.as_ref().and_then(|c| c.take_any_initial_pause());
            if let Some((id, start, dur)) = stale {
                zgc_emit_mark_start_row(shared, id, start, dur);
            }
        }
    }
    Some(facts)
}

fn zgc_emit_concurrent_mark(shared: &SharedVm, gc_id: u64, start_ns: u64, dur_ns: u64) {
    let id = gc_id as i32; // Truncation: deliberate wrap, as `gc_event_finish`.
    cratonvm_native_builtins_jfr::jfr_vm_more::gc_phase(
        shared.vm_identity,
        "jdk.GCPhaseConcurrent",
        id,
        ZGC_CONCURRENT_MARK,
        start_ns,
        dur_ns,
        zgc_driver_thread(true),
    );
    if cratonvm_jfr::is_enabled() {
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_gc_phase_concurrent_event(
            &mut jfr,
            id,
            ZGC_CONCURRENT_MARK,
            start_ns,
            dur_ns,
        );
    }
}

/// `jdk.ZAllocationStall` for one stall of `nanos` ending now, on both
/// backends. The event's `type` / `size` (HotSpot: the page allocation that
/// stalled) are, since zs s7/jf, the page HotSpot 25 ZGC would allocate for
/// the request this heap's arena refused before the stall
/// (`VmHeap::zgc_jfr_stall_page`); `null` / 0 when no refusal is on record
/// (a door that stalled without one) or under
/// `CRATONVM_ZGC_JFR_STALL_REQUEST=0`.
/// zs s8/jf2: the stall's page (`VmHeap::zgc_jfr_stall_page`) read by the
/// forced door BEFORE its collection -- the refusal it follows is then at
/// most one collection old, where after the ladder's further collections
/// (soft-clear rung, a sibling's pause) it can read as none. `None` off ZGC,
/// while no recording runs, and under `CRATONVM_ZGC_JFR_STALL_REQUEST=0`.
pub(super) fn zgc_stall_page_at_entry(shared: &SharedVm) -> Option<(&'static str, i64)> {
    if !heap_is_zgc(shared) || !zgc_jfr_events_enabled() || !any_jfr_recording() {
        return None;
    }
    zgc_stall_page_now(shared)
}

fn zgc_stall_page_now(shared: &SharedVm) -> Option<(&'static str, i64)> {
    shared
        .mem
        .heap
        .zgc_jfr_stall_page(shared.config.max_heap_size)
        .map(|(ty, size)| (ty, i64::try_from(size).unwrap_or(i64::MAX)))
}

fn zgc_allocation_stall_jfr(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    nanos: u64,
    page_at_entry: Option<(&'static str, i64)>,
) {
    if !zgc_jfr_events_enabled() || !any_jfr_recording() {
        return;
    }
    // zs s8/jf2: the record as the door saw it before its collection first
    // (the ladder's later collections age it out), else as it stands now.
    let page = page_at_entry.or_else(|| zgc_stall_page_now(shared));
    let start_ns = wall_clock_ns().saturating_sub(nanos);
    let (vm_tid, name) = (thread.thread_id.0, thread.name.clone());
    // zs s8/jf2: the stalling thread's own context, so the real backend's
    // event names its Java thread and carries its stack, as HotSpot's
    // (`CRATONVM_ZGC_JFR_STALL_STACK`, default ON, `=0` = no stack, the
    // thread found by name).
    let want_ctx =
        cratonvm_native_builtins_jfr::jfr_vm_events::any_recording();
    let mut ctx_store;
    let ctx: Option<&mut dyn cratonvm_native_api::NativeContext> = if want_ctx {
        ctx_store = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        Some(&mut ctx_store)
    } else {
        None
    };
    cratonvm_native_builtins_jfr::jfr_vm_more::zgc_allocation_stall(
        shared.vm_identity,
        vm_tid,
        &name,
        start_ns,
        nanos,
        page,
        ctx,
    );
    if cratonvm_jfr::is_enabled() {
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_zgc_allocation_stall_event(&mut jfr, page, start_ns, nanos);
    }
}

/// The process's `(user, system)` CPU seconds (`getrusage(RUSAGE_SELF)`),
/// for the `gc,cpu` line — HotSpot's `GCTraceCPUTime` reads the same
/// process-wide times (`os::getTimesSecs`). `None` on failure (Windows: the
/// `GetProcessTimes` arm below, gcg g4/yg). gen r5w3/obs7.
#[cfg(unix)]
fn process_cpu_secs() -> Option<(f64, f64)> {
    // SAFETY: `rusage` is plain old data; all-zero is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable `rusage` and `RUSAGE_SELF` a valid
    // `who`; the call writes only into `usage`.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    Some((secs(usage.ru_utime), secs(usage.ru_stime)))
}

/// gcg g4/yg (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 8): HotSpot
/// prints `gc,cpu` on Windows too (`os::getTimesSecs` is `GetProcessTimes`
/// there). The process's user and kernel times, in 100 ns units.
#[cfg(windows)]
fn process_cpu_secs() -> Option<(f64, f64)> {
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no close;
    // the four out-pointers are valid, writable `FILETIME`s (two `u32`s, the
    // Win32 layout) that the call only writes.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let secs = |t: FileTime| ((u64::from(t.high) << 32) | u64::from(t.low)) as f64 / 10_000_000.0;
    Some((secs(user), secs(kernel)))
}

#[cfg(not(any(unix, windows)))]
fn process_cpu_secs() -> Option<(f64, f64)> {
    None
}

/// Capture the "before" half of a collection's events. Always `Some` since
/// gc-common w6-f: the duration feeds `HeapRealm::gc_pause_ns_total` (ZGC's
/// `getCollectionTime()`), so the one unconditional cost is an `Instant` pair
/// per collection. JFR / `-Xlog:gc` stay gated.
///
/// gc-common w7-f: G1 (and ZGC, once its state is wired —
/// `HeapRealm::backend_gc_beans`) describe their JMX beans too, so the bean
/// snapshot below is taken on them as on Generational: one G1 region-table
/// walk under its read lock, or ZGC's arena lock once, per pause edge,
/// uncontended with the world stopped.
///
/// Called by each initiator arm once it holds the world stopped, immediately
/// before root collection, so the measured duration is the pause the collector
/// initiated (roots + collection + reference processing + remap).
///
/// gen r4w3/obs2 (2026-09-23): on the generational backend this ALWAYS takes
/// the bean snapshot -- HotSpot's `GCMemoryManager::gc_begin` records the pool
/// usage before every collection too, for `getLastGcInfo()` whether or not
/// anybody listens. Its cost is once per collection, never per allocation:
/// relaxed loads plus the young-from, young-to and old-generation locks once
/// each (`jmx_memory_pools`; the two young ones since gen r5w2/obs6, for the
/// committed granules), uncontended here because the world is stopped and
/// the collector is about to take the same locks.
///
/// gen r5w2/obs6: also publishes this VM's cumulative allocation total into
/// `gc_metrics` (`record_vm_allocation_totals`, the `[GC] cards/alloc:`
/// line's `allocated_bytes_total=`): one relaxed load and one `fetch_max` per
/// collection.
pub(super) fn gc_event_start(shared: &SharedVm) -> Option<GcEventStart> {
    // gci i45/sl: this pause's `-Xlog:safepoint` line opens here, inside the
    // pause; its VM operation is named by the door at the finish.
    // gci i47w/xl47: unless the pause is part of its G1 full-collection
    // door's one operation ([`g1_door_folds_this_pause`]): then no line and
    // no safepoint of the barrier's count, before anything reads its figures.
    if g1_door_folds_this_pause(shared) {
        shared.mem.gc_barrier.note_pause_not_a_safepoint();
    } else {
        safepoint_line_begin_for(shared, None);
    }
    cratonvm_gc::gc_metrics::record_vm_allocation_totals(
        shared
            .mem
            .bytes_allocated_total
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    cratonvm_jfr::set_real_backend_active(cratonvm_native_builtins_jfr::jfr_vm_events::any_recording());
    // gci i12/ge: a pause of a G1 full collection reported as one (opt-in
    // `CRATONVM_G1_FULL_GC_ONE_REPORT`) reports nothing of its own.
    let full_scope_id = g1_full_scope_fold(shared, false).map(|(id, _)| id);
    // gcj j1/yg: a `gc+tlab=debug`-only spec arms the events too, so its
    // `TLAB totals` line is computed (Generational; see
    // `xlog_tlab_totals_alone_wanted`).
    let events =
        full_scope_id.is_none() && (gc_events_wanted() || xlog_tlab_totals_alone_wanted(shared));
    // gcf f4/r: `-Xlog:gc+age` needs this young collection's age table; the
    // heap fills it only when asked (`GenerationalHeap::set_tenuring_xlog`).
    shared.mem.heap.gen_set_tenuring_xlog(events && xlog_age_on());
    let beans = if full_scope_id.is_some() {
        None
    } else {
        gc_bean_snapshot(shared)
    };
    // zs s3/ob2: the ZGC `Metaspace` pool's before-edge sample (one relaxed
    // load unless a program has been handed the pool beans).
    if let Some(b) = shared.mem.backend_gc_beans() {
        if b.metaspace_samples_wanted() {
            b.note_metaspace_before(zgc_metaspace_edge_sample(shared, false));
        }
        // zt t6/ob4: and the code heaps' (one relaxed load; ZGC with
        // HotSpot's non-heap pools, `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES`).
        if b.code_heap_edges_wanted() {
            b.note_code_heap_before(Some(crate::vm::vm_exec::jmx_code_cache_bytes().0));
        }
    }
    // gc-common w3-f: one relaxed load of the barrier's last TTSP and six of
    // the take-over counters, only under `--verbose:gc` (w4-a: and one of the
    // take-over's post-quota time; w5-a: and this pause's straggler, from the
    // same `ttsp_stats` read).
    let pause_line = shared
        .mem
        .verbose_gc
        .load(std::sync::atomic::Ordering::Relaxed)
        .then(|| pause_safepoint_facts(shared));
    if events {
        // A stale phase stash from a collection this door did not report (JFR
        // switched off and on between two) must not be emitted against this one.
        let _ = cratonvm_gc::gen_heap::take_last_pause_phases();
    }
    // gcg g3/x: the exit-summary figures (the pools, the arenas and one
    // class-store walk), only under `gc+heap=debug` / `gc+metaspace=debug`.
    let heap_debug_before = if events && xlog_heap_debug_on() {
        crate::runtime::unified_logging::serial_exit_heap(shared)
    } else {
        None
    };
    // gci i28/tl (gcg-g3x item 9): the `gc,tlab` debug `TLAB totals` line,
    // computed here with the world stopped (HotSpot's `ensure_parsability`).
    crate::runtime::unified_logging::stash_tlab_totals(shared, events);
    // gci i30/ob (tl review item 6): the line travels with this start.
    let tlab_totals = crate::runtime::unified_logging::take_tlab_totals(shared);
    let allocated_at_start = shared
        .mem
        .bytes_allocated_total
        .load(std::sync::atomic::Ordering::Relaxed);
    Some(GcEventStart {
        t0: std::time::Instant::now(),
        wall_start_ns: if events { wall_clock_ns() } else { 0 },
        heap_used_before: if events {
            gc_event_heap_used(&shared.mem.heap)
        } else {
            0
        },
        zgc_moved_before: if events {
            shared.mem.heap.zgc_slide_moved_bytes_total()
        } else {
            0
        },
        heap_committed_before: if events {
            shared.mem.heap.committed_bytes()
        } else {
            0
        },
        events,
        beans,
        pause_line,
        sealed: None,
        beans_after: None,
        backend_counted: false,
        young_only_cycles_before: shared.mem.heap.young_only_cycles(),
        // gci i5g/g1o: one relaxed load per G1 collection.
        g1_mixed_before: match &shared.mem.heap {
            _ if shared.mem.heap.is_g1() => shared.mem.heap.g1_mixed_collection_count(),
            _ => None,
        },
        g1_old_pauses_before: shared.mem.heap.g1_old_collecting_pauses(),
        // One `getrusage` per collection, only under `-Xlog:gc+cpu`. gci
        // i11/go: G1 too (HotSpot G1 prints the same `gc,cpu` line).
        cpu_start: if events
            && xlog_serial_or_g1_lines(&shared.mem.heap)
            && xlog_on(crate::runtime::unified_logging::LogTag::GcCpu)
        {
            process_cpu_secs()
        } else {
            None
        },
        // One class-store walk per collection, only under `-Xlog:gc+metaspace`.
        // gci i11/go: G1 too (`G1HeapTransition::print` ends with it).
        // gci i27/ob (gcg-g3x item 12): none of its own when the debug block
        // above already walked the class store at this edge.
        metaspace_before: if events
            && xlog_serial_or_g1_lines(&shared.mem.heap)
            && xlog_on(crate::runtime::unified_logging::LogTag::GcMetaspace)
        {
            Some(metaspace_edge_bytes(shared, heap_debug_before.as_ref()))
        } else {
            None
        },
        // gci i30/ob (g3x item 12): one relaxed load, with the figure only.
        classes_unloaded_before: if events
            && xlog_serial_or_g1_lines(&shared.mem.heap)
            && xlog_on(crate::runtime::unified_logging::LogTag::GcMetaspace)
        {
            classes_unloaded(shared)
        } else {
            0
        },
        tlab_totals,
        allocated_at_start,
        heap_debug_before,
        heap_debug_after: None,
        // gci i14/hd (item 9): G1's figures for the same debug blocks.
        g1_heap_debug_before: g1_heap_debug_sample(shared, events),
        g1_heap_debug_after: None,
        escalation: None,
        zgc_pause_split: None,
        // gci i11/go: one region-table walk, only under `-Xlog:gc+heap` on G1.
        g1_regions_before: if events
            && xlog_on(crate::runtime::unified_logging::LogTag::GcHeap)
        {
            G1RegionEdge::sample(&shared.mem.heap)
        } else {
            None
        },
        g1_regions_after: None,
        // gci i11/go: one relaxed load per G1 collection.
        g1_prepare_before: match &shared.mem.heap {
            _ if shared.mem.heap.is_g1() => shared.mem.heap.g1_prepare_mixed_pause_count(),
            _ => None,
        },
        g1_concurrent_start: false,
        g1_marking_idle: false,
        // gci i13/gp: one load of the G1 cycle record, only when the label
        // is printed or recorded.
        g1_cycle_seq_before: if events && shared.mem.heap.is_g1() {
            Some(cratonvm_gc::gc_metrics::last_g1_cycle().map_or(0, |f| f.sequence))
        } else {
            None
        },
        g1_evacuation_failed: false,
        full_scope_id,
        zgc_mark_continue: None,
        zgc_conc_mark_ns: 0,
        zgc_cycle_start: None,
        zgc_causes: shared.mem.heap.zgc_hotspot_gc_cause(false).zip(
            shared
                .mem
                .heap
                .zgc_forced_door_gc_cause(zgc_forced_door_occupancy()),
        ),
    })
}

/// gci i11/go: does this backend print the per-collection `gc,start`,
/// `gc,metaspace` and `gc,cpu` lines? HotSpot Serial's shape (Generational)
/// and HotSpot G1's (G1: the same three lines, its own labels and causes).
/// ZGC's HotSpot counterparts have a different grammar. Pure.
fn xlog_serial_or_g1_lines(heap: &crate::memory::vm_heap::VmHeap) -> bool {
    heap.is_generational() || heap.is_g1()
}

/// HotSpot Serial's `gc,age` block (`AgeTable::print_age_table` and
/// `DefNewGeneration`'s threshold line, measured on 25.0.3): one debug line
/// `GC(n) Desired survivor size <b> bytes, new threshold <t> (max threshold <m>)`,
/// then at trace `GC(n) Age table:` and one `GC(n) - age %3u: %10zu bytes,
/// %10zu total` line per non-empty age. gcf f4/r.
fn render_xlog_age_lines(
    gc_id: u64,
    desired_bytes: u64,
    new_threshold: u8,
    max_threshold: u8,
    ages: &[u64],
) -> Vec<(crate::runtime::unified_logging::LogLevel, String)> {
    use crate::runtime::unified_logging::LogLevel;
    let mut out = Vec::with_capacity(ages.len() + 2);
    out.push((
        LogLevel::Debug,
        format!(
            "GC({gc_id}) Desired survivor size {desired_bytes} bytes, new threshold \
             {new_threshold} (max threshold {max_threshold})"
        ),
    ));
    out.push((LogLevel::Trace, format!("GC({gc_id}) Age table:")));
    let mut total: u64 = 0;
    for (age, &bytes) in ages.iter().enumerate().skip(1) {
        if bytes == 0 {
            continue;
        }
        total = total.saturating_add(bytes);
        out.push((
            LogLevel::Trace,
            format!("GC({gc_id}) - age {age:>3}: {bytes:>10} bytes, {total:>10} total"),
        ));
    }
    out
}

/// zs s3/ob2: the class-metadata figure for the ZGC `Metaspace` pool's
/// `GcInfo` entry at one edge of a collection. Never blocks on the class
/// lock: it is taken with `try_read_untracked` (the before edge runs inside
/// the pause, where a stopped thread may hold it for write, and the
/// initiating thread may itself hold it). zt t13/x10 (`zgc-zt12x9-*` item 9):
/// under `CRATONVM_ZGC_METASPACE_EDGE_TALLY` (default ON) the after edge
/// takes the tally's own mutex.
///
/// The walk is memoised on the loaded / unloaded class counts
/// (`BackendGcBeans::metaspace_sample`), so a program that defines no class
/// between two collections pays two relaxed loads and a mutex, not a walk.
/// `walk = false` (the before edge, inside the pause) never walks: a memo for
/// another census, or a lock it cannot take, answers `None`, and the after
/// edge's figure then stands in for both (class metadata does not grow across
/// a pause; unloading can only lower it). The after edge runs with the world
/// released and walks when the census moved; when it cannot take the lock the
/// last walk's figure stands in.
///
/// `None` off ZGC, with `CRATONVM_ZGC_JMX_METASPACE_POOL=0`, or with no figure.
fn zgc_metaspace_edge_sample(shared: &SharedVm, walk: bool) -> Option<u64> {
    let b = shared.mem.backend_gc_beans()?;
    if !b.metaspace_pool_enabled() {
        return None;
    }
    let unloaded = shared
        .debug
        .diagnostic_counters
        .classes_unloaded
        .load(std::sync::atomic::Ordering::Relaxed);
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return if walk { b.metaspace_last() } else { None };
    };
    let key = (cm.loaded_count() as u64, unloaded);
    if !walk {
        return b.metaspace_memo_for(key);
    }
    b.set_metaspace_max(cm.max_metaspace_bytes());
    // zt t1/ob: the walk states HotSpot's committed and class-space figures
    // too, for the `GcInfo`'s `Metaspace` / `Compressed Class Space` entries.
    // zt t11/pl3: from the running arena tally under
    // `CRATONVM_ZGC_METASPACE_EDGE_TALLY` (default ON since zt t20/fl1;
    // `class_metadata_edge_figures`).
    Some(b.metaspace_sample_figures(key, || {
        crate::vm::vm_init::class_metadata_edge_figures(&cm)
    }))
}

/// gci i27/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 12): the
/// `gc,metaspace` figure at one edge of a collection -- the debug block's when
/// that edge took one (the same walk, `serial_exit_heap` -> `heap_summary`),
/// so `-Xlog:gc*=debug` walks the class store once per edge, not twice.
fn metaspace_edge_bytes(
    shared: &SharedVm,
    debug: Option<&crate::runtime::unified_logging::SerialExitHeap>,
) -> crate::vm::vm_init::MetaspaceStats {
    debug.map_or_else(
        || metaspace_stats_now(shared),
        |h| crate::vm::vm_init::MetaspaceStats::from_triples(h.metaspace, h.class_space),
    )
}

/// The class-metadata figures the heap summary, the non-heap `MemoryUsage` and
/// the `gc,exit` summary report (`VmDiagnosticState::heap_summary`). A walk of
/// the class store under its read lock: `-Xlog:gc+metaspace` and JFR only.
/// gci i33/ms: HotSpot's statistics (`vm_init::metaspace_stats`: used,
/// committed, reserved and the class space), one walk; was the used figure
/// alone (`metaspace_used_bytes`).
fn metaspace_stats_now(shared: &SharedVm) -> crate::vm::vm_init::MetaspaceStats {
    // gci i18/dk (orchestrator-applied): the metaspace walk alone, not the
    // whole heap summary for one field; same figure, same class-manager lock.
    crate::vm::vm_init::metaspace_stats(&shared.classes.class_manager.read())
}

/// gci i30/ob (g3x item 12): the VM's unloaded-class count (one relaxed load;
/// the unload transaction adds to it inside the collection that unloads).
fn classes_unloaded(shared: &SharedVm) -> u64 {
    shared
        .debug
        .diagnostic_counters
        .classes_unloaded
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 12's info-only
/// rest): the `gc,metaspace` after-figure of the collection `start` opened --
/// the debug `Metaspace After GC` block's walk when it took one
/// ([`metaspace_edge_bytes`]); otherwise `start`'s before-figure when the
/// collection unloaded no class ([`metaspace_after_is_the_before`]), the
/// class store being then what it was at the start (the collector neither
/// defines nor decodes classes); otherwise a walk. The info-only
/// `-Xlog:gc+metaspace` collection walked the class store twice; it now walks
/// once unless it unloaded classes. The figure is also nearer HotSpot's, which
/// is taken at the pause's end: the late walk counted what the released
/// mutators defined or decoded before the line was written.
fn metaspace_after_bytes(
    shared: &SharedVm,
    start: &GcEventStart,
) -> crate::vm::vm_init::MetaspaceStats {
    if let Some(h) = start.heap_debug_after.as_ref() {
        return crate::vm::vm_init::MetaspaceStats::from_triples(h.metaspace, h.class_space);
    }
    match start.metaspace_before {
        Some(before)
            if metaspace_after_is_the_before(
                start.classes_unloaded_before,
                classes_unloaded(shared),
            ) =>
        {
            before
        }
        _ => metaspace_stats_now(shared),
    }
}

/// gci i30/ob: [`metaspace_after_bytes`]'s rule, pure: no class unloaded
/// between the two counts.
fn metaspace_after_is_the_before(unloaded_before: u64, unloaded_now: u64) -> bool {
    unloaded_now == unloaded_before
}

/// HotSpot's per-collection `gc,metaspace` line
/// (`MetaspaceUtils::print_metaspace_change`). Without a compressed class
/// space -- `GC(n) Metaspace: usedK(committedK)->usedK(committedK)`, measured
/// on 25.0.3 with `-XX:-UseCompressedClassPointers` (gcf f4/r), this VM's
/// shape until gci i33/ms. gci i33/ms: with one (HotSpot 25's default, and
/// `MetaspaceStats::class_space` set), the same `HEAP_CHANGE_FORMAT` three
/// times -- `Metaspace`, `NonClass`, `Class` -- space separated. The shape
/// follows `after` (a `before` without a class space reads its class part 0).
fn render_xlog_metaspace_line(
    gc_id: u64,
    before: &crate::vm::vm_init::MetaspaceStats,
    after: &crate::vm::vm_init::MetaspaceStats,
) -> String {
    let change = |name: &str, b: (u64, u64, u64), a: (u64, u64, u64)| {
        format!(
            "{name}: {}K({}K)->{}K({}K)",
            b.0 / 1024,
            b.1 / 1024,
            a.0 / 1024,
            a.1 / 1024
        )
    };
    let total = change("Metaspace", before.triple(), after.triple());
    match after.class_space {
        None => format!("GC({gc_id}) {total}"),
        Some(class_after) => {
            let class_before = before.class_space.unwrap_or((0, 0, 0));
            format!(
                "GC({gc_id}) {total} {} {}",
                change("NonClass", before.non_class(), after.non_class()),
                change("Class", class_before, class_after)
            )
        }
    }
}

/// Sample this pause's safepoint facts (see [`PauseSafepointFacts`]). Must be
/// called INSIDE the pause, after `stw_take_over_and_wait` returned: that is
/// what makes every field this pause's.
fn pause_safepoint_facts(shared: &SharedVm) -> PauseSafepointFacts {
    let t = shared.mem.gc_barrier.ttsp_stats();
    PauseSafepointFacts {
        ttsp_ns: t.last_ns,
        straggler: t.last_straggler,
        xt_post_quota_ns: cratonvm_gc::gc_quiescence::xt_post_quota_ns(),
        xt_pass_ns: cratonvm_gc::gc_quiescence::xt_pass_ns(),
        xt: cratonvm_gc::gc_quiescence::xt_cycle_coverage(),
    }
}

/// Stamp the END of the collection while the world is still stopped — call it
/// immediately before `retire_skip_spans_and_resume` / `complete_gc` (the
/// resumed in-JIT peers and the released mutators can allocate from there on).
/// Every consumer (the `[GC] pause:` line, `-Xlog:gc`, JFR) then reads ONE
/// instant and ONE after-heap figure, none of them inflated by the released
/// mutators' allocation or by the pause line's own stderr write.
///
/// gc-common w5-a (`handoff-w4f-gc-event-end-sampled-in-the-pause`,
/// `common-w4f-gc-event-durations-sampled-after-release`). Printing stays after
/// the release: no I/O inside the pause. The heap reads happen only when a JFR
/// recording or `-Xlog:gc` is armed.
pub(super) fn gc_event_seal(shared: &SharedVm, start: &mut Option<GcEventStart>) {
    if let Some(s) = start.as_mut() {
        let (after, committed) = if s.events {
            (
                gc_event_heap_used(&shared.mem.heap),
                shared.mem.heap.committed_bytes(),
            )
        } else {
            (0, 0)
        };
        // gc-common w7-f: the end instant is taken BEFORE the bean snapshot, so
        // the backend's per-bean pause time (booked here, from this duration)
        // and every other consumer's duration exclude the snapshot's own cost.
        let end = std::time::Instant::now();
        if s.beans.is_some() {
            // The collection is booked on the backend's beans FIRST, so the
            // after-snapshot's counts include it (`gc_notifications_between`
            // decides by the count moving). Generational books its own inside
            // the collector and has no backend state.
            if let Some(b) = shared.mem.backend_gc_beans() {
                // zs s4/ob3: on ZGC the pause bean books the collection as
                // HotSpot's pauses, cut at its own phase boundaries
                // (`CRATONVM_ZGC_PAUSE_PHASES`); otherwise one pause, as
                // before. The boundaries are always taken, so none outlives
                // its pause.
                let split = shared.mem.heap.zgc_take_pause_split();
                // zt t31/zc11: a closed concurrent cycle's start, for its
                // cycle bean (and, below, its cycle record).
                s.zgc_cycle_start = zgc_concurrent_cycle_jfr(shared).and_then(|c| c.take_cycle_open_at());
                s.zgc_pause_split =
                    b.note_collection_split_from(s.zgc_cycle_start, s.t0, end, s.kind(shared), split);
                s.backend_counted = true;
            }
            // Generational: the old-generation lock once; G1: one region-table
            // walk; ZGC: the arena lock once — uncontended with the world
            // stopped (the same cost `gc_event_start` pays).
            s.beans_after = gc_bean_snapshot(shared);
            if let (Some(b), Some(a)) = (shared.mem.backend_gc_beans(), s.beans_after.as_ref()) {
                // `getCollectionUsage()`: the pools as this collection left them.
                // gci i5g/g1o / wave 9 lane G: a young-only G1 pause leaves
                // `G1 Old Gen`'s alone. gci i14/mp finding 3: only the
                // affected pools are stamped (used and committed); the others
                // keep their own figure.
                b.note_collection_usage_of(&a.pools, s.affected_pools(shared));
            }
        }
        // gcg g3/x: the `Heap After GC` block's figures, inside the pause.
        if s.heap_debug_before.is_some() {
            s.heap_debug_after = crate::runtime::unified_logging::serial_exit_heap(shared);
        }
        // gci i14/hd (item 9): G1's `Heap After GC` figures, inside the pause.
        if s.g1_heap_debug_before.is_some() {
            s.g1_heap_debug_after = g1_heap_debug_sample(shared, true);
        }
        // gci i11/go: the G1 `gc,heap` lines' after edge, inside the pause.
        if s.g1_regions_before.is_some() {
            s.g1_regions_after = G1RegionEdge::sample(&shared.mem.heap);
        }
        // gci i11/go (finding 4): HotSpot decides a concurrent start at the
        // pause; this VM's drive runs right after the release, so the same
        // question is asked here, side-effect free, while the world is
        // stopped. Only when the label is printed or recorded.
        if s.events {
            if shared.mem.heap.is_g1() {
                // gci i13/gp: the idle reading is kept for the metadata
                // opt-in's forced concurrent start (`g1_pause_type`).
                s.g1_marking_idle = !shared.mem.heap.g1_is_marking_active();
                s.g1_concurrent_start = s.g1_marking_idle && shared.mem.heap.g1_would_start_marking();
                // gci i13/gp (ef finding 2): the collector recorded this
                // pause (and a same-pause kept-region drain) last.
                s.g1_evacuation_failed = g1_pause_evacuation_failed(
                    s.g1_cycle_seq_before,
                    cratonvm_gc::gc_metrics::last_g1_cycle(),
                );
            }
        }
        // gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
        // HotSpot 25 starts a marking cycle only at a young-only pause; the
        // pause that ends a mixed phase only asks for one, and the next
        // young-only pause is the `Concurrent Start`. Recorded inside every G1
        // collection pause (one relaxed load and store), for the drives after it.
        if let Some(before) = s.g1_mixed_before.filter(|_| shared.mem.heap.is_g1()) {
            let wait = g1_mixed_pause_defers_concurrent_start(
                before,
                shared.mem.heap.g1_mixed_collection_count().unwrap_or(0),
                g1_concurrent_start_at_young_pause_enabled,
            );
            shared
                .mem
                .alloc_ladder
                .g1_conc_start_waits_for_young_pause
                .store(wait, std::sync::atomic::Ordering::Relaxed);
        }
        // gcg h1/ob: this pause's escalation split, taken on every Generational
        // collection (one leaf lock) so a split can never outlive its pause.
        if shared.mem.heap.is_generational() { s.escalation = shared.mem.heap.gen_take_escalation_split(); }
        // zt t22/hunt3 (`CRATONVM_ZGC_XLOG_SAFEPOINT_LINES`): a ZGC
        // collection's `-Xlog:safepoint` lines are HotSpot's operations, cut
        // where its pause rows are -- named here, inside the pause, before
        // the line's seal below. Nothing unless the tag is selected.
        if heap_is_zgc(shared)
            && crate::runtime::unified_logging::xlog_safepoint_lines_wanted()
            && zgc_xlog_safepoint_lines_enabled()
        {
            let major = matches!(
                s.kind(shared),
                cratonvm_gc::gc_metrics::CollectionKind::Major
            );
            let (names, cuts) = zgc_safepoint_ops(s.zgc_pause_split.as_ref(), major);
            crate::runtime::unified_logging::safepoint_line_name_ops(&names, &cuts);
        }
        // zt t31/zc11 (1f R3): this collection's closed cycle's failed Pause
        // Mark End attempts, inside the pause (empty with the switch off).
        if shared.mem.heap.is_zgc() {
            s.zgc_mark_continue = Some(shared.mem.heap.zgc_take_mark_continue_log());
            // zt t31/zc11 (1f R6): the closed cycle's concurrent phase.
            s.zgc_conc_mark_ns = shared.mem.heap.zgc_take_last_concurrent_mark_nanos();
        }
        s.sealed = Some((end, after, committed));
    }
    // gci i45/sl: the `-Xlog:safepoint` line's end, inside the pause (a no-op
    // without an open line).
    crate::runtime::unified_logging::safepoint_line_seal();
}

/// zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`, opt-in): the collection's
/// sweep ran after the release (`run_collection_pause`), so the after-figures
/// [`gc_event_seal`] took inside the pause describe the heap before its
/// reclaim. Re-sample them now, before [`gc_event_finish_by`] reports the
/// collection: the heap used and committed (the `-Xlog:gc` / JFR after edge)
/// and the backend beans' after-snapshot with the collection usage it stamps.
/// HotSpot reports a ZGC cycle at its end, after its concurrent phases. The
/// sealed END instant is kept: the collection's duration is its pause. A no-op
/// on an unsealed event.
pub(super) fn gc_event_reseal_after_concurrent_sweep(
    shared: &SharedVm,
    start: &mut Option<GcEventStart>,
) {
    let Some(s) = start.as_mut() else {
        return;
    };
    let Some((end, _, _)) = s.sealed else {
        return;
    };
    if s.events {
        s.sealed = Some((
            end,
            gc_event_heap_used(&shared.mem.heap),
            shared.mem.heap.committed_bytes(),
        ));
    }
    if s.beans.is_some() && s.beans_after.is_some() {
        s.beans_after = gc_bean_snapshot(shared);
        if let (Some(b), Some(a)) = (shared.mem.backend_gc_beans(), s.beans_after.as_ref()) {
            b.note_collection_usage_of(&a.pools, s.affected_pools(shared));
        }
    }
}

/// `end - start` in nanoseconds, saturating.
fn nanos_between(start: std::time::Instant, end: std::time::Instant) -> u64 {
    u64::try_from(end.saturating_duration_since(start).as_nanos()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Non-collection pauses on the `[GC] pause:` line (gc-common w5-a)
// ---------------------------------------------------------------------------

/// A stop-the-world pause that collects nothing, for its `--verbose:gc`
/// `[GC] pause: gc=- door=<token>` line
/// (`common-w4a-non-collection-pauses-have-no-pause-line`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NonCollectionPause {
    /// `stw_publish_frame_traces`: a cross-thread `getStackTrace()` / thread
    /// dump of a running thread.
    FrameTrace,
    /// `NonMovingPause`: the HPROF heap walk.
    HeapDump,
    /// `NonMovingPause::request_heap_inspection`: `jcmd GC.class_histogram`'s
    /// heap walk (gci i4/hd; it was labelled `heap-dump`).
    HeapInspection,
    /// `jvmti_events::request_compiled_loop_exits`: every mutator passes a
    /// poll so an OSR'd loop that must run interpreted leaves compiled code
    /// (`NonMovingPause::request_handshake`, interpreter round i1 wave 13).
    LoopExit,
    /// `obsolete_frames::after_redefinition`: every mutator passes a poll so
    /// a frame still running a body a class redefinition replaced moves onto
    /// its translated copy before it resolves another constant (interpreter
    /// round i1 wave 19, lane L3).
    Redefinition,
    /// Generational concurrent cycle, phase 1 (`maybe_concurrent_gc_at`).
    GenInitialMark,
    /// Generational concurrent cycle, phase 3 (`gen_concurrent_remark_pause`).
    GenRemark,
    /// ZGC mark start (`zgc_concurrent_mark_cycle`).
    ZgcMarkStart,
    /// zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`, opt-in): a ZGC Pause Mark
    /// End attempt that found mark work left and released the world without
    /// a collection (`run_collection_pause`'s `zgc_mark_end_attempt`); the
    /// pool continues marking (HotSpot's Concurrent Mark Continue).
    ZgcMarkEndContinue,
    /// G1 initial mark (`g1_concurrent_mark_cycle`).
    G1InitialMark,
    /// G1 final remark + cleanup (`g1_final_remark_cleanup`).
    G1Remark,
    /// `request_ic_grace_handshake`: every mutator passes a poll so the
    /// inline-cache retirement grace can advance while a thread spins in
    /// compiled code (JIT round 13 wave 11 lane mega9's patch, applied in
    /// round 14 wave 1 by lane codecache).
    IcGrace,
}

impl NonCollectionPause {
    /// The space-free `door=` token.
    fn token(self) -> &'static str {
        match self {
            NonCollectionPause::FrameTrace => "frame-trace",
            NonCollectionPause::HeapDump => "heap-dump",
            NonCollectionPause::HeapInspection => "heap-inspection",
            NonCollectionPause::LoopExit => "loop-exit",
            NonCollectionPause::Redefinition => "redefinition",
            NonCollectionPause::GenInitialMark => "gen-initial-mark",
            NonCollectionPause::GenRemark => "gen-remark",
            NonCollectionPause::ZgcMarkStart => "zgc-mark-start",
            NonCollectionPause::ZgcMarkEndContinue => "zgc-mark-end",
            NonCollectionPause::G1InitialMark => "g1-initial-mark",
            NonCollectionPause::G1Remark => "g1-remark",
            NonCollectionPause::IcGrace => "ic-grace",
        }
    }
}

impl NonCollectionPause {
    /// The backend whose JMX beans count this pause, and the notification
    /// cause (gc-common w7-f): G1's marking pauses are `G1 Concurrent GC`
    /// pauses (HotSpot counts remark and cleanup there; here the initial mark
    /// is its own pause too, and remark + cleanup are one), with the cause
    /// `No GC` that HotSpot's concurrent-cycle pauses carry and that Micrometer
    /// files as a concurrent phase rather than a pause; a ZGC mark start is a
    /// `ZGC Major Pauses` pause of the (allocation-triggered) cycle it opens. The
    /// other kinds belong to no bean: frame-trace and heap-dump pauses are not
    /// GC work, and Generational's Serial beans have no concurrent collector.
    fn bean_pause(self) -> Option<(cratonvm_gc::gc_metrics::BackendBeanShape, &'static str)> {
        use cratonvm_gc::gc_metrics::BackendBeanShape;
        match self {
            NonCollectionPause::G1InitialMark | NonCollectionPause::G1Remark => {
                Some((BackendBeanShape::G1, "No GC"))
            }
            // zr z1/tr: `Allocation Failure` is not a ZGC cause; the pause
            // opens an allocation-driven cycle, which HotSpot 25's ZGC names
            // `Allocation Rate` unless a rule says otherwise (the caller
            // substitutes the heap's own cause, `non_collection_pause_start`).
            NonCollectionPause::ZgcMarkStart => Some((BackendBeanShape::Zgc, "Allocation Rate")),
            // zt t24/zc8: a failed Pause Mark End is a `ZGC Major Pauses`
            // pause of the cycle on HotSpot 25 too (each ZGC pause operation
            // books its own); the caller substitutes the cycle's cause.
            NonCollectionPause::ZgcMarkEndContinue => {
                Some((BackendBeanShape::Zgc, "Allocation Rate"))
            }
            NonCollectionPause::FrameTrace
            | NonCollectionPause::HeapDump
            | NonCollectionPause::HeapInspection
            | NonCollectionPause::LoopExit
            | NonCollectionPause::Redefinition
            | NonCollectionPause::GenInitialMark
            | NonCollectionPause::GenRemark
            | NonCollectionPause::IcGrace => None,
        }
    }
}

/// The JMX half of a non-collection pause (gc-common w7-f). Owns its handles
/// (`Arc`s), because the seal and finish calls get only the timer.
struct NonCollectionPauseBeans {
    set: std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>,
    sampler: crate::memory::vm_heap::JmxPoolSampler,
    cause: &'static str,
    before: cratonvm_gc::gc_metrics::GcBeanSnapshot,
    /// Stamped by [`non_collection_pause_seal`] inside the pause.
    after: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    /// gci i14/mp finding 9 (opt-in `CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS`):
    /// the remark's work-to-cleanup cut (ns from the timer's `t0`) when the
    /// seal booked HotSpot's two `G1 Concurrent GC` collections (Remark,
    /// Cleanup); [`non_collection_pause_finish`] then records two
    /// notifications ([`split_concurrent_remark_records`]).
    split_ns: Option<u64>,
    /// gci i30/ob (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 9): this
    /// is the remark of G1's stop-the-world full mark cycle
    /// (`gc_and_alloc::g1_stw_full_mark_cycle`, `System.gc()`'s fresh cycle),
    /// part of HotSpot's ONE `Pause Full`, not a `G1 Concurrent GC` pause: it
    /// books no collection and queues no notification, and its finish stamps
    /// every pool's collection usage as a full collection does
    /// ([`stw_full_mark_stamps_every_pool`]).
    stw_full_mark: bool,
}

/// gci i14/mp finding 9: HotSpot 25 G1's concurrent start is a YOUNG pause
/// (`Pause Young (Concurrent Start)`, booked under `G1 Young Generation`),
/// and `G1 Concurrent GC` counts the Remark and the Cleanup pauses, one
/// collection and one notification each. This VM runs the young pause and the
/// initial mark as two pauses and booked the initial mark under `G1
/// Concurrent GC` with the remark + cleanup as one. With it the initial mark
/// books no bean (the young pause before it already counted as HotSpot's
/// concurrent start) and the remark books two.
///
/// gci i30/ob (`gch-h4g1-g1-review-findings-FIXED-20261017` item 6): default ON,
/// `=0` restores the initial mark's `G1 Concurrent GC` booking (cause `No GC`,
/// `end of concurrent GC pause`) and the one remark + cleanup collection.
/// Rows: `vi30-ob.list` block B.
pub(super) fn g1_jmx_hotspot_cycle_beans_enabled() -> bool {
    // WARNING gci-user-flip-20261015: default flipped ON (HotSpot parity, the
    // user's flip-pending-switches decision), pending apps triage.
    true
}

/// gci i30/ob (`gci-i20mr-merge-review-findings-FIXED-20261016` finding 9): is a
/// pause of `kind` with `g1_cycle` (its [`g1_cycle_edge`]) the remark of G1's
/// stop-the-world full mark cycle? A G1 remark with no open concurrent cycle:
/// every concurrent initial mark opens one ([`g1_cycle_edge`], on every run),
/// and a retried or aborted remark re-opens it; `g1_stw_full_mark_cycle` runs
/// its initial mark inside the remark pause and opens none. Pure.
fn stw_full_mark_remark(kind: NonCollectionPause, g1: bool, has_cycle_edge: bool) -> bool {
    kind == NonCollectionPause::G1Remark && g1 && !has_cycle_edge
}

/// gci i30/ob (finding 9): HotSpot's `System.gc()` on G1 is ONE `Pause Full`
/// (`G1 Old Generation`, which manages every pool): the stop-the-world full
/// mark's remark stamps every pool's collection usage and runs every armed
/// collection-usage threshold, books no `G1 Concurrent GC` collection and
/// queues no notification. It used to book one with cause `No GC`.
fn stw_full_mark_stamps_every_pool(
    set: &cratonvm_gc::gc_metrics::BackendGcBeans,
    after: &cratonvm_gc::gc_metrics::GcBeanSnapshot,
) {
    // gci i30/ob hand-back 1 (vi30-ob S1: `G1 Survivor Space`
    // `getCollectionUsage().getCommitted() > 0` here, `0` on HotSpot).
    let rows = g1_full_collection_usage_rows(&after.pools);
    set.note_collection_usage(&rows);
    let thresholds = set.notifications().pool_thresholds();
    if thresholds.is_armed() {
        let pools: Vec<&'static str> = rows.iter().map(|p| p.name).collect();
        thresholds.check(&rows, &pools);
    }
}

/// gci i30/ob hand-back 2 (vi34-ob S1: `G1 Survivor Space` still stamped
/// `committed > 0` after `System.gc()`): the default G1 `System.gc()` door runs
/// a young pause, the stop-the-world full mark AND the full-compaction mixed
/// pause (`gc_and_alloc::g1_full_compaction_pause`), and the LAST of them -- a
/// collection that affects every pool -- re-stamped the Survivor pool with the
/// survivors it made. HotSpot's door is ONE full collection, whose stamp leaves
/// the Survivor pool empty. So the door's end (`force_full_collection`, after
/// every pause it ran) re-stamps the Survivor pool alone as HotSpot's full
/// collection leaves it ([`g1_full_collection_usage_rows`]); every other pool
/// keeps the last pause's figure. Called only for a door whose cycle was the
/// stop-the-world one (HotSpot's `Pause Full`; under
/// `-XX:+ExplicitGCInvokesConcurrent` HotSpot runs a concurrent-start young
/// pause, which stamps the Survivor pool as ours does) and with no one-report
/// scope (`G1FullReport::finish` stamps itself). A no-op off G1.
pub(super) fn g1_full_door_leaves_the_survivor_empty(shared: &SharedVm) {
    if !shared.mem.heap.is_g1() {
        return;
    }
    let (Some(set), Some(sampler)) = (
        shared.mem.backend_gc_beans(),
        shared.mem.heap.jmx_pool_sampler(),
    ) else {
        return;
    };
    g1_stamp_the_survivor_as_a_full_collection_leaves_it(set, &set.pools(&sampler.sample()));}

/// The one stamp both Survivor re-stamps make: `pools` as a full collection
/// leaves them ([`g1_full_collection_usage_rows`]), the Survivor row alone.
fn g1_stamp_the_survivor_as_a_full_collection_leaves_it(
    set: &cratonvm_gc::gc_metrics::BackendGcBeans,
    pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
) {
    let rows = g1_full_collection_usage_rows(pools);
    let only: &[&str] = &[cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR];
    set.note_collection_usage_of(&rows, Some(only));
}

/// gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item 1):
/// [`g1_full_door_leaves_the_survivor_empty`]'s stamp taken INSIDE the door's
/// own full-compaction pause, right after [`gc_event_seal`] stamped the
/// pause's pools and before the release, from the sealed snapshot
/// (`beans_after`). The door-end call ran with the world released, so a young
/// pause another thread ran between the compaction and the stamp had its
/// Survivor figure overwritten by the door's late zero, and a compaction
/// request another thread's pause consumed was still followed by the door's
/// stamp. The caller (`gc_and_alloc::run_collection_pause`) decides that this
/// pause is such a pause (`g1_door_compaction_pause_stamps_survivor`). A
/// folded pause (`CRATONVM_G1_FULL_GC_ONE_REPORT`: `beans` is `None`) stamps
/// nothing, as the door-end call did not under the scope (`G1FullReport::finish`
/// stamps itself). A no-op off G1.
pub(super) fn g1_full_door_compaction_stamps_the_survivor(
    shared: &SharedVm,
    start: &Option<GcEventStart>,
) {
    let Some(after) = start
        .as_ref()
        .filter(|s| s.beans.is_some())
        .and_then(|s| s.beans_after.as_ref())
    else {
        return;
    };
    if let Some(set) = shared.mem.backend_gc_beans() {
        g1_stamp_the_survivor_as_a_full_collection_leaves_it(set, &after.pools);
    }
}

/// gci i30/ob hand-back 1: the collection-usage rows a G1 FULL collection
/// leaves, from the pools as this VM sampled them after it. HotSpot's
/// `G1FullCollector` compacts every young region into old ones and clears the
/// survivor list, so `G1 Survivor Space`'s collection usage after a full
/// collection is `0` used, `0` committed (measured: vi30-ob `ob30_s_g1`,
/// HotSpot `collection-usage-stamped false`). This VM's full collection (the
/// young pause + the stop-the-world full mark, or the one-report scope) moves
/// no survivor into old, so its sample still holds survivor regions; the row
/// is HotSpot's instead. Every other pool keeps its sample. Pure.
fn g1_full_collection_usage_rows(
    pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
) -> Vec<cratonvm_gc::gc_metrics::JmxPoolUsage> {
    pools
        .iter()
        .map(|p| {
            let mut row = *p;
            if row.name == cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR {
                row.used = 0;
                row.committed = 0;
            }
            row
        })
        .collect()
}

/// gci i14/mp finding 9: does `kind`'s pause book a bean? Every pause that
/// has one ([`NonCollectionPause::bean_pause`]) except, under the opt-in
/// (`hotspot_cycle_beans`, asked only for the initial mark), G1's initial
/// mark. Pure but for the closure.
fn non_collection_pause_books_a_bean(
    kind: NonCollectionPause,
    hotspot_cycle_beans: impl FnOnce() -> bool,
) -> bool {
    kind.bean_pause().is_some()
        && !(kind == NonCollectionPause::G1InitialMark && hotspot_cycle_beans())
}

/// gci i14/mp finding 9: a G1 remark booked as HotSpot's two `G1 Concurrent
/// GC` collections ends as two notifications: the remark (id one below the
/// bean's count, ending at `cut`) and the cleanup (the count, from `cut`).
/// Every other record passes through. Pure.
fn split_concurrent_remark_records(
    recs: Vec<cratonvm_gc::gc_metrics::GcNotificationRecord>,
    cut: std::time::Instant,
) -> Vec<cratonvm_gc::gc_metrics::GcNotificationRecord> {
    let mut out = Vec::with_capacity(recs.len() + 1);
    for rec in recs {
        if rec.collector == cratonvm_gc::gc_metrics::JMX_G1_CONCURRENT_COLLECTOR && rec.id >= 2 {
            let mut remark = rec.clone();
            remark.id = rec.id - 1;
            remark.end = cut.max(rec.start).min(rec.end);
            let mut cleanup = rec;
            cleanup.start = remark.end;
            out.push(remark);
            out.push(cleanup);
        } else {
            out.push(rec);
        }
    }
    out
}

impl NonCollectionPauseBeans {
    fn snapshot(&self) -> cratonvm_gc::gc_metrics::GcBeanSnapshot {
        cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
            self.set.collectors(),
            self.set.pools(&self.sampler.sample()),
        )
    }
}

/// One non-collection pause being timed. Exists under `--verbose:gc` (for the
/// pause line), and — gc-common w7-f — for a pause a backend's JMX beans count
/// ([`NonCollectionPause::bean_pause`]), and — gen r5w2/obs6 — for a
/// generational concurrent-cycle pause while a JFR recording runs.
pub(super) struct NonCollectionPauseTimer {
    kind: NonCollectionPause,
    /// `Some` iff `--verbose:gc`: print the pause line.
    facts: Option<PauseSafepointFacts>,
    t0: std::time::Instant,
    /// Sampled by [`non_collection_pause_seal`] inside the pause.
    work_ns: Option<u64>,
    beans: Option<NonCollectionPauseBeans>,
    /// gen r5w2/obs6: this pause's `jdk.GCPhasePause` row.
    jfr: Option<GenCycleJfrRow>,
    /// gci i12/ge: a G1 marking pause's edge of its concurrent cycle (the
    /// cycle's `GC(n)` and, under `-Xlog:gc`, the figures of its lines).
    /// `None` for every other pause.
    g1_cycle: Option<G1CycleEdge>,
    /// gci i12/ge: a G1 marking pause folded into a full collection's ONE
    /// report (`CRATONVM_G1_FULL_GC_ONE_REPORT`): the report's marking-pause
    /// time sum and longest pause, which this pause's work is added to.
    /// `None` otherwise.
    folded: Option<FoldSums>,
    /// zr z4/jf: a ZGC mark start's `jdk.GCPhasePause` row and cycle opening.
    zgc_jfr: Option<ZgcMarkStartJfr>,
}

/// gci i12/ge (`gci-i11go-g1-concurrent-cycle-xlog-and-jfr-gc-event-review-findings-FIXED-20261017`
/// item 1): one marking pause of a G1 concurrent cycle, as HotSpot G1's
/// `-Xlog:gc` reports the cycle:
///
/// ```text
/// GC(7) Pause Young (Concurrent Start) (G1 Evacuation Pause) 60M->52M(128M) 3.1ms
/// GC(8) Concurrent Mark Cycle
/// GC(8) Pause Remark 55M->40M(128M) 1.2ms
/// GC(8) Pause Cleanup 40M->40M(128M) 0.1ms
/// GC(8) Concurrent Mark Cycle 25.4ms
/// ```
///
/// The cycle has its own id from the ONE GC-id sequence (`gc_cycle_count`),
/// taken at its initial mark -- HotSpot's `GCIdMark` in
/// `G1ConcurrentMarkThread::run_service`, the id after the concurrent-start
/// pause -- on every run (it was taken only while a JFR recording ran, so a
/// later collection's `GC(n)` did not count the cycle). The cycle's state is
/// the G1 beans' [`cratonvm_gc::gc_metrics::ConcurrentCycleJfr`]: the initial
/// mark opens it, its end stamps the marking start, the remark takes and
/// re-opens it (an incomplete remark is retried under the same id).
struct G1CycleEdge {
    cycle: std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr>,
    cycle_id: u64,
    /// The remark only: the cycle's marking start (wall-clock ns, the initial
    /// mark's end), `0` when unknown (a retried remark).
    mark_start_ns: u64,
    /// The remark only: the cycle's marking start for its `-Xlog:gc` end line,
    /// kept across a retried remark (`ConcurrentCycleJfr::cycle_start`; review
    /// findings page item 4); `0` when unknown.
    cycle_start_ns: u64,
    /// Print the cycle's `-Xlog:gc` lines (an `-Xlog` rule for `gc`,
    /// `gc,start` or `gc,cpu` is on, and the pause is not folded into a full
    /// collection's report).
    xlog: bool,
    /// The remark under `xlog`: the heap's pools (`used`, `committed`) at the
    /// start and, stamped by [`non_collection_pause_seal`], at the end.
    sampler: Option<crate::memory::vm_heap::JmxPoolSampler>,
    heap_before: (usize, usize),
    heap_after: Option<(usize, usize)>,
    /// The remark under `xlog` and `gc,cpu`: the process CPU at the start.
    cpu_start: Option<(f64, f64)>,
    /// The remark: where its cleanup began, ns after the timer's `t0`
    /// ([`non_collection_pause_cleanup_point`]); `None` = no cleanup ran (an
    /// incomplete remark: one `Pause Remark` line, the cycle stays open).
    cleanup_after_ns: Option<u64>,
    /// gci i13/gp (opt-in `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): the
    /// remark of a cycle a full collection aborted
    /// ([`g1_full_gc_abort_open_cycle`]). It still runs (cleanup needs it) but
    /// reports what HotSpot's aborted cycle does: no `Pause Remark` / `Pause
    /// Cleanup` line, bean count or JFR pause row; `Concurrent Mark Abort`
    /// (`gc,marking`) and the cycle's `Concurrent Mark Cycle <ms>` (`gc`).
    /// `xlog` then means "either of those two tags is on".
    aborted: bool,
    /// gci i19/ob: the VM's `jdk.jfr` backend key, for the aborted cycle's
    /// `jdk.ConcurrentModeFailure` row ([`g1_cycle_edge_finish`]).
    vm_identity: usize,
    /// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 7): when
    /// the pause began (taken inside it, by [`g1_cycle_edge`]), so the
    /// remark's `Pause Remark` / `Pause Cleanup` `gc,start` lines, written
    /// after the release, are decorated with their pauses' starts.
    pause_start: std::time::Instant,
}

/// gci i12/ge: the heap's `(used, committed)` bytes as the G1 pools describe
/// them (their sum: Eden + Survivor + Old/humongous; the committed prefix).
/// Takes the region-table read lock: inside a pause, uncontended.
fn pooled_heap_figures(sampler: &crate::memory::vm_heap::JmxPoolSampler) -> (usize, usize) {
    let samples = sampler.sample();
    let used: u64 = samples.iter().map(|s| s.used).sum();
    let committed: u64 = samples.iter().map(|s| s.committed).sum();
    (
        usize::try_from(used).unwrap_or(usize::MAX),
        usize::try_from(committed).unwrap_or(usize::MAX),
    )
}

/// gci i12/ge: the cycle edge of a G1 marking pause (`None` for any other
/// pause, off G1, or for a remark with no open cycle). Call inside the pause,
/// from [`non_collection_pause_start`]. The initial mark takes the cycle's id
/// -- the next `gc_cycle_count`, or `folded` (the id of the full collection
/// the pause is folded into, which takes no other) -- and opens the cycle;
/// the remark takes the open cycle and re-opens it at once (marking start
/// dropped, as the JFR path always did, so a retry writes no second
/// `Concurrent Mark From Roots` row).
fn g1_cycle_edge(
    shared: &SharedVm,
    kind: NonCollectionPause,
    folded: Option<u64>,
) -> Option<G1CycleEdge> {
    use crate::runtime::unified_logging::LogTag;
    let initial = match kind {
        NonCollectionPause::G1InitialMark => true,
        NonCollectionPause::G1Remark => false,
        _ => return None,
    };
    if !shared.mem.heap.is_g1() {
        return None;
    }
    let cycle = std::sync::Arc::clone(
        shared
            .mem
            .backend_gc_beans()?
            .notifications()
            .concurrent_cycle_jfr(),
    );
    let (cycle_id, mark_start_ns, cycle_start_ns, aborted) = if initial {
        let id = folded.unwrap_or_else(|| {
            shared
                .mem
                .gc_cycle_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        });
        cycle.open(id);
        (id, 0, 0, false)
    } else {
        let (id, start) = cycle.take()?;
        cycle.open(id);
        // gci i13/gp: one uncontended lock; `false` unless a full collection
        // aborted this cycle under its opt-in.
        let aborted = cycle.take_aborted(id);
        (id, start, cycle.cycle_start(id).unwrap_or(start), aborted)
    };
    // gci i13/gp: an aborted cycle's two lines print whether or not its
    // remark is folded into a full collection's one report (HotSpot's
    // concurrent mark thread prints them beside the `Pause Full`).
    let xlog = if aborted {
        xlog_on(LogTag::Gc) || xlog_on(LogTag::GcMarking)
    } else {
        folded.is_none()
            && (xlog_on(LogTag::Gc) || xlog_on(LogTag::GcStart) || xlog_on(LogTag::GcCpu))
    };
    let sampler = if xlog && !initial && !aborted {
        shared.mem.heap.jmx_pool_sampler()
    } else {
        None
    };
    let heap_before = sampler.as_ref().map_or((0, 0), pooled_heap_figures);
    let cpu_start = if xlog && !initial && !aborted && xlog_on(LogTag::GcCpu) {
        process_cpu_secs()
    } else {
        None
    };
    Some(G1CycleEdge {
        cycle,
        cycle_id,
        mark_start_ns,
        cycle_start_ns,
        xlog,
        sampler,
        heap_before,
        heap_after: None,
        cpu_start,
        cleanup_after_ns: None,
        aborted,
        vm_identity: shared.vm_identity,
        pause_start: std::time::Instant::now(),
    })
}

/// gci i12/ge: HotSpot G1's `-Xlog` lines `(tag, line)` that close a remark
/// pause (all at info): `Pause Remark` (`gc,start`, `gc`, `gc,cpu`), and when
/// the pause reached its cleanup (`cleanup_ns`) `Pause Cleanup` the same way
/// and the cycle's end `Concurrent Mark Cycle <ms>` (`gc`; only with a known
/// cycle length). `VM_G1PauseConcurrent::doit`: `GCTraceTimeLogger(Info, gc)`
/// with the heap's usage (no cause), `GCTraceCPUTime`. HotSpot's remark
/// reclaims the empty regions, so the heap change is the remark's and the
/// cleanup's line repeats the after figure. The CPU delta of the one pause
/// is shared by wall time ([`split_cpu_delta`]). Pure.
#[allow(clippy::too_many_arguments)]
fn render_xlog_g1_remark_lines(
    cycle_id: u64,
    (before, after, committed): (usize, usize, usize),
    remark_ns: u64,
    cleanup_ns: Option<u64>,
    cycle_ns: Option<u64>,
    cpu: Option<(f64, f64)>,
) -> Vec<(crate::runtime::unified_logging::LogTag, String)> {
    use crate::runtime::unified_logging::LogTag;
    const M: usize = 1024 * 1024;
    let total_ns = remark_ns.saturating_add(cleanup_ns.unwrap_or(0));
    let (remark_cpu, cleanup_cpu) = match cpu {
        Some(delta) => {
            let (r, c) = split_cpu_delta(delta, remark_ns, total_ns);
            (Some(r), Some(c))
        }
        None => (None, None),
    };
    let pause = |out: &mut Vec<(LogTag, String)>,
                 name: &str,
                 from: usize,
                 ns: u64,
                 cpu: Option<(f64, f64)>| {
        out.push((LogTag::GcStart, format!("GC({cycle_id}) {name}")));
        out.push((
            LogTag::Gc,
            format!(
                "GC({cycle_id}) {name} {}M->{}M({}M) {:.3}ms",
                from / M,
                after / M,
                committed / M,
                ns as f64 / 1_000_000.0
            ),
        ));
        if let Some((user, sys)) = cpu {
            out.push((LogTag::GcCpu, render_xlog_cpu_line(cycle_id, user, sys, ns)));
        }
    };
    let mut out = Vec::with_capacity(7);
    pause(&mut out, "Pause Remark", before, remark_ns, remark_cpu);
    if let Some(cleanup_ns) = cleanup_ns {
        pause(&mut out, "Pause Cleanup", after, cleanup_ns, cleanup_cpu);
        if let Some(cycle_ns) = cycle_ns {
            out.push((
                LogTag::Gc,
                format!(
                    "GC({cycle_id}) Concurrent Mark Cycle {:.3}ms",
                    cycle_ns as f64 / 1_000_000.0
                ),
            ));
        }
    }
    out
}

/// gci i12/ge: the `-Xlog:gc` line that opens a G1 concurrent cycle
/// (`GCTraceConcTime(Info, gc)`'s start). Pure.
fn render_xlog_g1_cycle_open_line(cycle_id: u64) -> String {
    format!("GC({cycle_id}) Concurrent Mark Cycle")
}

/// gci i13/gp (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` item 3):
/// HotSpot G1's `-Xlog` lines `(tag, line)` for a concurrent cycle a full
/// collection aborted: `Concurrent Mark Abort` (`log_info(gc, marking)` in
/// `G1ConcurrentMark::concurrent_cycle_end`, `has_aborted()`) and then the
/// cycle's `GCTraceConcTime(Info, gc)` end, `Concurrent Mark Cycle <ms>`
/// (only with a known cycle length). No `Pause Remark` / `Pause Cleanup`:
/// HotSpot's aborted cycle never reaches them. Pure.
fn render_xlog_g1_aborted_cycle_lines(
    cycle_id: u64,
    cycle_ns: Option<u64>,
) -> Vec<(crate::runtime::unified_logging::LogTag, String)> {
    use crate::runtime::unified_logging::LogTag;
    let mut out = vec![(
        LogTag::GcMarking,
        format!("GC({cycle_id}) Concurrent Mark Abort"),
    )];
    if let Some(cycle_ns) = cycle_ns {
        out.push((
            LogTag::Gc,
            format!(
                "GC({cycle_id}) Concurrent Mark Cycle {:.3}ms",
                cycle_ns as f64 / 1_000_000.0
            ),
        ));
    }
    out
}

/// A generational concurrent-cycle pause's `jdk.GCPhasePause` row (`Pause
/// Init Mark` / `Pause Remark`), prepared by [`non_collection_pause_start`]
/// and emitted by [`non_collection_pause_finish`]. gen r5w2/obs6 (2026-09-26;
/// `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted` item 2).
///
/// Everything the finish needs is OWNED here, because the finish is called
/// with the timer alone: the event type is resolved at the start (the only
/// point that can reach the recorder) and the row is then pushed with
/// [`cratonvm_jfr::builtin::emit_gc_phase_pause_event_typed`], which needs no
/// recorder.
///
/// gci i9/jc (`gci-i8jf-jfr-cycle-rows-never-reach-the-jdk-jfr-backend`): the
/// row has two sinks. `pause_type` is the legacy recorder's (`Some` iff a
/// legacy recording runs and its recorder was free at the start); `backend_vm`
/// is the `jdk.jfr` backend's (`Some(vm_identity)` iff a `Recording` /
/// `RecordingStream` / `jcmd JFR.start` recording runs). At least one is
/// `Some`. Both get the same names, ids and spans.
struct GenCycleJfrRow {
    pause_type: Option<cratonvm_jfr::EventTypeId>,
    /// gci i9/jc: the VM whose `jdk.jfr` backend recording gets this row too.
    backend_vm: Option<usize>,
    /// gci i9/jc: the backend's copy of the cycle's concurrent-marking row
    /// `(name, start ns, duration ns)` under `gc_id`, measured inside the
    /// pause like the legacy one but written at the finish (the backend's
    /// lock is a blocking one, never taken inside a pause).
    backend_mark: Option<(&'static str, u64, u64)>,
    /// The concurrent cycle's `gcId` (see [`gen_cycle_jfr_row`]).
    gc_id: u64,
    /// The pause row's name (`Pause Initial Mark` / `Pause Remark`). A G1
    /// initial mark builds no row at all ([`g1_initial_mark_has_no_row`]).
    name: &'static str,
    /// Wall-clock ns at the pause's `t0`, JFR's time base.
    wall_start_ns: u64,
    /// The initial mark only: the heap's cycle state and the cycle's own
    /// `gcId`, to stamp where the concurrent marking starts (this pause's
    /// end). Generational only since gci i12/ge (G1's initial mark writes no
    /// row and stamps its start in [`g1_cycle_edge_finish`], see
    /// [`g1_initial_mark_has_no_row`]).
    opens: Option<(std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr>, u64)>,
    /// gci i8/jf: a G1 remark only -- where, in ns after the pause's `t0`,
    /// the remark's work ended and its cleanup began
    /// ([`non_collection_pause_cleanup_point`]). `Some` splits the pause into
    /// HotSpot G1's `Pause Remark` and `Pause Cleanup` rows; `None` (an
    /// incomplete remark, which cleaned nothing, or the Generational remark)
    /// leaves one `Pause Remark` row.
    cleanup_after_ns: Option<u64>,
    /// gci i13/gp: the remark of a G1 cycle a full collection aborted
    /// ([`G1CycleEdge::aborted`]): its concurrent-marking row is kept (the
    /// marking ran), its `Pause Remark` / `Pause Cleanup` rows are not written
    /// (HotSpot's aborted cycle has neither).
    aborted: bool,
    /// gci i23/pk: a G1 marking pause (its pause rows are HotSpot's
    /// `VM_G1PauseRemark` / `VM_G1PauseCleanup` operations,
    /// [`g1_cycle_pause_vm_operation`]); `false` on Generational.
    g1: bool,
}

/// The `jdk.GCPhasePause` name of a concurrent-cycle marking pause, `None` for
/// every other pause. gci i5g/g1o: G1's remark (HotSpot's Remark and Cleanup,
/// one pause here) is HotSpot G1's `Pause Remark` (gci i8/jf: followed by a
/// `Pause Cleanup` row when the pause reached its cleanup,
/// [`remark_cleanup_rows`]). gci i8/jf: G1's initial mark is HotSpot G1's
/// concurrent start, the young `Pause Young (Concurrent Start)` collection,
/// whose top-level `jdk.GCPhasePause` row HotSpot names `GC Pause`
/// (`STWGCTimer::register_gc_start`); it was `Pause Init Mark`, a name HotSpot
/// G1 never records. The Generational names are unchanged. Pure.
fn cycle_jfr_pause_name(kind: NonCollectionPause) -> Option<&'static str> {
    match kind {
        NonCollectionPause::GenInitialMark => Some("Pause Init Mark"),
        NonCollectionPause::G1InitialMark => Some("GC Pause"),
        NonCollectionPause::GenRemark | NonCollectionPause::G1Remark => Some("Pause Remark"),
        _ => None,
    }
}

/// gci i8/jf: the `jdk.GCPhaseConcurrent` name of a cycle's concurrent
/// marking (initial mark's end to remark's start). Generational keeps
/// `Concurrent Mark`; G1 takes HotSpot G1's name for the marking phase,
/// `Concurrent Mark From Roots` (`G1ConcurrentMarkThread::subphase_mark_from_roots`;
/// HotSpot G1 records no `jdk.GCPhaseConcurrent` row named `Concurrent Mark`).
/// Pure.
fn cycle_jfr_concurrent_mark_name(generational: bool) -> &'static str {
    if generational {
        "Concurrent Mark"
    } else {
        "Concurrent Mark From Roots"
    }
}

/// gci i8/jf, revised by gci i12/ge: does a cycle-opening pause write NO
/// `jdk.GCPhasePause` row of its own? HotSpot G1's concurrent start is a young
/// pause with its own id and its own `GC Pause` row, and the concurrent cycle
/// then takes the NEXT id (`GCIdMark` in `G1ConcurrentMarkThread`), under which
/// its concurrent phases and its `Pause Remark` / `Pause Cleanup` file. This
/// VM's G1 initial mark is the tail of that young pause, whose collection
/// already wrote the `GC Pause` row (`emit_jfr_collection`, the backend's
/// `gc_cycle`): a second `GC Pause` under that id would break HotSpot's one
/// row per collection (`GciI9JcJfrNestingCheck` `pause-per-gc`), and a row
/// under an id of its own (i8/jf took a second id for it) ran a recording one
/// id ahead of HotSpot per cycle. So G1's initial mark writes none; the cycle
/// takes ONE id ([`g1_cycle_edge`]). The Generational initial mark keeps its
/// `Pause Init Mark` row under the cycle's id. Pure.
fn g1_initial_mark_has_no_row(kind: NonCollectionPause) -> bool {
    kind == NonCollectionPause::G1InitialMark
}

/// gci i8/jf: the `jdk.GCPhasePause` rows `(name, start ns, duration ns)` of
/// a remark pause that started at `wall_start_ns` and worked `work_ns`. With a
/// cleanup point (`cleanup_after_ns`, ns after the pause's start) the pause is
/// HotSpot G1's two rows in HotSpot's order, `Pause Remark` then `Pause
/// Cleanup`, which partition the pause (the cut is clamped to the work);
/// without one, one `Pause Remark` row as before. Pure.
fn remark_cleanup_rows(
    wall_start_ns: u64,
    work_ns: u64,
    cleanup_after_ns: Option<u64>,
) -> Vec<(&'static str, u64, u64)> {
    match cleanup_after_ns {
        None => vec![("Pause Remark", wall_start_ns, work_ns)],
        Some(cut) => {
            let cut = cut.min(work_ns);
            vec![
                ("Pause Remark", wall_start_ns, cut),
                (
                    "Pause Cleanup",
                    wall_start_ns.saturating_add(cut),
                    work_ns - cut,
                ),
            ]
        }
    }
}

/// gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
/// the HotSpot 25 VM operation (`VMOp_Type` name, as `jdk.ExecuteVMOperation`
/// reports it) a concurrent-cycle pause row stands for: G1's `Pause Remark`
/// is `G1PauseRemark`, its `Pause Cleanup` `G1PauseCleanup`. `None` for
/// every other row -- G1's initial mark is its young pause's tail, which
/// `gc_cycle` already writes as a collection; the Generational cycle has no
/// HotSpot counterpart. Pure.
fn g1_cycle_pause_vm_operation(g1: bool, row_name: &str) -> Option<&'static str> {
    if !g1 {
        return None;
    }
    match row_name {
        "Pause Remark" => Some("G1PauseRemark"),
        "Pause Cleanup" => Some("G1PauseCleanup"),
        _ => None,
    }
}

/// Is `kind` a Generational marking pause (its cycle state is the
/// Generational heap's)? `false` for G1's (the backend beans' queue). Pure.
fn cycle_jfr_pause_is_generational(kind: NonCollectionPause) -> bool {
    matches!(
        kind,
        NonCollectionPause::GenInitialMark | NonCollectionPause::GenRemark
    )
}

/// Does `kind` open a concurrent cycle (take its `gcId`)? Pure.
fn cycle_jfr_pause_opens_the_cycle(kind: NonCollectionPause) -> bool {
    matches!(
        kind,
        NonCollectionPause::GenInitialMark | NonCollectionPause::G1InitialMark
    )
}

/// Prepare the JFR row of a concurrent-cycle marking pause (Generational, and
/// G1 since gci i5g/g1o), or `None` (one match, and one load when `kind` is a
/// marking pause) when `kind` is not one, no recording runs, the heap is not
/// the pause's backend, or the recorder is busy.
///
/// Merge of origin/dev (2026-10-15): dev's jr20 wave 9 lane gcH filed G1's
/// rows in a simpler form of the same function; the gci form (i5g/g1o,
/// i8/jf, i9/jc, i12/ge, i13/gp) is kept whole.
///
/// # The `gcId`
///
/// HotSpot gives a concurrent cycle its own id from the one GC-id sequence
/// (G1's `GC(7) Concurrent Mark Cycle`, its Remark and Cleanup under
/// `GC(7)`), and so does this: the initial mark takes the next
/// `gc_cycle_count` and the remark files under it. Reusing the last
/// collection's id would put the marking pauses under an unrelated young
/// collection in JMC. On Generational the id is taken ONLY while a recording
/// runs, so its `GC(n)` / `[GC] pause: gc=` numbering is unchanged without
/// one (the cycle's own `-Xlog:gc` line keeps its separate `GC(c<n>)`
/// counter). gci i12/ge: on G1 the cycle takes its id on EVERY run
/// ([`g1_cycle_edge`]; collection ids skip one per concurrent cycle, as on
/// HotSpot G1, and the cycle's `-Xlog:gc` lines carry it); its `Concurrent
/// Mark From Roots`, `Pause Remark` and `Pause Cleanup` rows file under it,
/// and the initial mark writes no row ([`g1_initial_mark_has_no_row`]; gci
/// i8/jf gave it a `GC Pause` row under a second id).
///
/// # Inside the pause
///
/// Called with the world stopped. The recorder is taken with `try_lock`, never
/// `lock`: a peer frozen by the take-over at an arbitrary instruction may hold
/// it (compiled code's helpers emit JFR events), and waiting for it would
/// never end. A busy recorder costs this pause its rows (and, at a remark,
/// the cycle's `Concurrent Mark` row); nothing else.
///
/// # The `jdk.jfr` backend (gci i9/jc)
///
/// A recording started through the JDK's own `jdk.jfr` (`Recording`,
/// `RecordingStream`, `jcmd JFR.start`) gets the same rows: the row is armed
/// when either recorder runs ([`cycle_jfr_sinks`]), takes the same ids (so the
/// id skip above also happens while only a backend recording runs, as on
/// HotSpot), and its backend copy is written at the finish, after the world
/// is released (the backend's lock blocks; it is never taken here). With only
/// the backend armed, a busy legacy recorder costs nothing.
///
/// # G1 (gci i12/ge)
///
/// A G1 marking pause's cycle state was already taken by [`g1_cycle_edge`]
/// (on every run, recording or not): `g1_cycle` is its `(cycle id, marking
/// start)`. The initial mark takes no id here and gets no row (its
/// concurrent-start collection wrote the `GC Pause`,
/// [`g1_initial_mark_has_no_row`]). `None` for a G1 kind (no open cycle at a
/// remark): no rows.
fn gen_cycle_jfr_row(
    shared: &SharedVm,
    kind: NonCollectionPause,
    g1_cycle: Option<(u64, u64)>,
) -> Option<GenCycleJfrRow> {
    let name = cycle_jfr_pause_name(kind)?;
    let (legacy, backend) = cycle_jfr_sinks(
        cratonvm_jfr::is_enabled(),
        cratonvm_native_builtins_jfr::jfr_vm_events::any_recording(),
    )?;
    // gci i5g/g1o (the G1 half of `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`,
    // hunk of `gch-h4g1-proposal-g1-concurrent-cycle-jfr-rows-DONE-20261015`): G1's marking pauses
    // file under the same mechanism; its cycle state lives in the backend's notification queue.
    let generational = cycle_jfr_pause_is_generational(kind);
    let cycle: &std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr> = match &shared.mem.heap
    {
        _ if generational && shared.mem.heap.is_generational() => {
            shared.mem.heap.gen_gc_notifications()?.concurrent_cycle_jfr()
        }
        _ if !generational && shared.mem.heap.is_g1() => {
            shared.mem.backend_gc_beans()?.notifications().concurrent_cycle_jfr()        }
        _ => return None,
    };
    let mut recorder = if legacy {
        shared.debug.flight_recorder.try_lock()
    } else {
        None
    };
    let pause_type = recorder
        .as_ref()
        .and_then(|r| cratonvm_jfr::builtin::gc_phase_pause_type(r));
    // Legacy only and its recorder busy (or the type missing): no rows and no
    // id, as before gci i9/jc.
    if pause_type.is_none() && !backend {
        return None;
    }
    let backend_vm = backend.then_some(shared.vm_identity);
    let mut backend_mark = None;
    let now_ns = wall_clock_ns();
    let (gc_id, opens, mark_start_ns) = if !generational {
        // gci i12/ge: G1's cycle state was taken by `g1_cycle_edge` (on every
        // run): the initial mark opened the cycle under its id, the remark
        // took and re-opened it. No open cycle at a remark: no rows.
        let (cycle_id, mark_start_ns) = g1_cycle?;
        if g1_initial_mark_has_no_row(kind) {
            return None;
        }
        (cycle_id, None, mark_start_ns)
    } else if cycle_jfr_pause_opens_the_cycle(kind) {
        // The Generational initial mark takes the cycle's id here, only while
        // a recording runs (its `-Xlog:gc` cycle line keeps its own
        // `GC(c<n>)` counter).
        let id = shared
            .mem
            .gc_cycle_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cycle.open(id);
        (id, Some((std::sync::Arc::clone(cycle), id)), 0)
    } else {
        // No open cycle: its initial mark ran before the recording started
        // (or could not take the recorder). No rows rather than rows under an
        // invented id.
        let (id, mark_start_ns) = cycle.take()?;
        // gen r5w3/unload7: the cycle is not over — its sweep follows, and its
        // `Concurrent Sweep` row files under the same id. Leave the id for the
        // driver (`gen_concurrent_sweep_jfr_open`), which takes it once the
        // remark returns. gci i5g/g1o: G1 re-opens it too (since gci i12/ge in
        // `g1_cycle_edge`): a G1 remark can end incomplete
        // (`g1_final_remark_and_cleanup` -> `false`) and be retried, and the
        // retry's `Pause Remark` files under the same cycle (re-opened with no
        // marking start, so no second `Concurrent Mark` row). The next initial
        // mark's `open` replaces the entry.
        cycle.open(id);
        (id, None, mark_start_ns)
    };
    {
        let id = gc_id;
        if mark_start_ns != 0 {
            // The concurrent marking: from the initial-mark pause's end to
            // this pause's start, mutators running throughout. JFR's `gcId`
            // is an `int`; wrap as `gc_event_finish` does.
            let mark_name = cycle_jfr_concurrent_mark_name(generational);
            let mark_ns = now_ns.saturating_sub(mark_start_ns);
            if let Some(r) = recorder.as_mut() {
                cratonvm_jfr::builtin::emit_gc_phase_concurrent_event(
                    r,
                    id as i32, // Truncation: deliberate wrap, see above.
                    mark_name,
                    mark_start_ns,
                    mark_ns,
                );
            }
            // gci i9/jc: the backend's copy, written at the finish.
            if backend {
                backend_mark = Some((mark_name, mark_start_ns, mark_ns));
            }
        }
    }
    drop(recorder);
    Some(GenCycleJfrRow {
        pause_type,
        backend_vm,
        backend_mark,
        gc_id,
        name,
        wall_start_ns: now_ns,
        opens,
        cleanup_after_ns: None,
        aborted: false,
        g1: !generational,
    })
}

/// gci i9/jc: which recorders a concurrent-cycle row goes to, from whether a
/// legacy recording runs (`cratonvm_jfr::is_enabled`) and whether a `jdk.jfr`
/// backend recording runs (`jfr_vm_events::any_recording`): `Some((legacy,
/// backend))` when at least one does, `None` (no row, no id taken) when
/// neither does. Pure.
fn cycle_jfr_sinks(legacy: bool, backend: bool) -> Option<(bool, bool)> {
    (legacy || backend).then_some((legacy, backend))
}

/// gen r5w3/unload7 — the `Concurrent Sweep` row's first half, for the
/// generational driver right after its remark pause (obs6's cross-lane
/// request 2, `docs/feature-designs/generative-gc-architecture.md`).
///
/// TAKES the heap's open cycle entry — the id the remark re-opened for the
/// sweep, or the initial mark's own entry when the remark could not reach the
/// recorder — so it cannot outlive this cycle, and returns `(gc_id, sweep
/// start in wall-clock ns)` when a recording runs. The caller holds the phase
/// at `ConcurrentMark`/`ConcurrentSweep` throughout, so no other cycle can
/// have opened in between. `None` on another backend. One uncontended lock
/// per cycle, whatever JFR says.
pub(super) fn gen_concurrent_sweep_jfr_open(shared: &SharedVm) -> Option<(u64, u64)> {
    let (gc_id, _) = shared.mem.heap.gen_gc_notifications()?.concurrent_cycle_jfr().take()?;
    // gci i9/jc: a `jdk.jfr` backend recording gets the row too.
    cycle_jfr_sinks(
        cratonvm_jfr::is_enabled(),
        cratonvm_native_builtins_jfr::jfr_vm_events::any_recording(),
    )
    .map(|_| (gc_id, wall_clock_ns()))
}

/// gen r5w3/unload7 — the `Concurrent Sweep` row's second half: emit
/// `jdk.GCPhaseConcurrent("Concurrent Sweep")` from the start
/// [`gen_concurrent_sweep_jfr_open`] recorded to now, under the cycle's id.
/// Called after the sweep, outside every pause. `try_lock`, never `lock`: a
/// thread holding the recorder may be parked at a safepoint of a pause that
/// is waiting for this one; a busy recorder costs the row, nothing else.
pub(super) fn gen_concurrent_sweep_jfr_close(shared: &SharedVm, open: Option<(u64, u64)>) {
    let Some((gc_id, start_ns)) = open else {
        return;
    };
    let duration_ns = wall_clock_ns().saturating_sub(start_ns);
    // gci i9/jc: the `jdk.jfr` backend's copy (its lock blocks; this runs
    // outside every pause, as `emit_jfr_collection`'s backend call does).
    if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
        cratonvm_native_builtins_jfr::jfr_vm_more::gc_phase_concurrent(
            shared.vm_identity,
            gc_id as i32, // Truncation: deliberate wrap, as `gc_event_finish`.
            "Concurrent Sweep",
            start_ns,
            duration_ns,
        );
    }
    if !cratonvm_jfr::is_enabled() {
        return;
    }
    let Some(mut recorder) = shared.debug.flight_recorder.try_lock() else {
        return;
    };
    cratonvm_jfr::builtin::emit_gc_phase_concurrent_event(
        &mut recorder,
        gc_id as i32, // Truncation: deliberate wrap, as `gc_event_finish`.
        "Concurrent Sweep",
        start_ns,
        duration_ns,
    );
}

/// Emit a [`GenCycleJfrRow`] for a pause whose work took `work_ns`, and, for
/// an initial mark, stamp the cycle's marking start at the pause's end. After
/// the world is released; no lock but the cycle state's (and, gci i9/jc, the
/// `jdk.jfr` backend's when a backend recording runs).
fn emit_gen_cycle_jfr_row(row: GenCycleJfrRow, work_ns: u64) {
    // gci i8/jf: a G1 remark that reached its cleanup is two rows (`Pause
    // Remark`, `Pause Cleanup`, HotSpot's order); every other row is one.
    let rows = if row.aborted {
        // gci i13/gp: an aborted G1 cycle writes no pause rows.
        Vec::new()
    } else if row.cleanup_after_ns.is_some() {
        remark_cleanup_rows(row.wall_start_ns, work_ns, row.cleanup_after_ns)
    } else {
        vec![(row.name, row.wall_start_ns, work_ns)]
    };
    let gc_id = row.gc_id as i32; // Truncation: deliberate wrap, as `gc_event_finish`.
    // gci i9/jc: the backend's concurrent-marking row first (it precedes the
    // remark in time), then the pause rows to both sinks.
    if let (Some(vm), Some((mark_name, mark_start_ns, mark_ns))) = (row.backend_vm, row.backend_mark) {
        cratonvm_native_builtins_jfr::jfr_vm_more::gc_phase_concurrent(
            vm,
            gc_id,
            mark_name,
            mark_start_ns,
            mark_ns,
        );
    }
    for (name, start_ns, dur_ns) in rows {
        if let Some(pause_type) = row.pause_type {
            cratonvm_jfr::builtin::emit_gc_phase_pause_event_typed(
                pause_type, gc_id, name, start_ns, dur_ns,
            );
        }
        if let Some(vm) = row.backend_vm {
            cratonvm_native_builtins_jfr::jfr_vm_more::gc_phase_pause(vm, gc_id, name, start_ns, dur_ns);
            // gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
            // HotSpot 25 runs G1's Remark and Cleanup as VM operations, so a
            // recording has their safepoint and `jdk.ExecuteVMOperation`
            // rows beside the `jdk.GCPhasePause` one. Backend only (the
            // legacy recorder never wrote VM operations).
            if let Some(op) = g1_cycle_pause_vm_operation(row.g1, name) {
                cratonvm_native_builtins_jfr::jfr_vm_more::vm_operation_pause(
                    vm, op, start_ns, dur_ns,
                );
            }
        }
    }
    if let Some((cycle, cycle_id)) = row.opens {
        cycle.mark_started(cycle_id, row.wall_start_ns.saturating_add(work_ns));
    }
}

/// gci i8/jf: stamp a G1 remark pause's cleanup point -- the instant `at` its
/// remark work (re-mark, drain, reference processing) ended and its cleanup
/// began -- so a recording gets HotSpot G1's `Pause Remark` and `Pause
/// Cleanup` rows ([`remark_cleanup_rows`]). Call inside the pause, before
/// [`non_collection_pause_seal`]. `at == None` (the cleanup never ran: an
/// incomplete remark), no timer, no JFR row, or any kind but `G1Remark`: no
/// effect, so the default path pays one `Option` test.
pub(super) fn non_collection_pause_cleanup_point(
    timer: &mut Option<NonCollectionPauseTimer>,
    at: Option<std::time::Instant>,
) {
    let (Some(t), Some(at)) = (timer.as_mut(), at) else {
        return;
    };
    if t.kind != NonCollectionPause::G1Remark {
        return;
    }
    let cut = nanos_between(t.t0, at);
    if let Some(row) = t.jfr.as_mut() {
        row.cleanup_after_ns = Some(cut);
    }
    // gci i12/ge: and the `-Xlog:gc` `Pause Remark` / `Pause Cleanup` split.
    if let Some(edge) = t.g1_cycle.as_mut() {
        edge.cleanup_after_ns = Some(cut);
    }
    // gci i47w/xl47 (`gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`
    // residual 1): and the `-Xlog:safepoint` split -- HotSpot 25 runs the
    // remark and the cleanup as two operations, two lines. Not for the
    // remark of a cycle a full collection aborted (opt-in
    // `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`: on HotSpot that abort is part
    // of the full collection's operation, no remark or cleanup of its own).
    // A pause whose line is not wanted, or that prints none (a remark folded
    // into a full collection), has nothing to split.
    if !t.g1_cycle.as_ref().is_some_and(|e| e.aborted) {
        crate::runtime::unified_logging::safepoint_line_split(at, G1_PAUSE_CLEANUP_OPERATION);
    }
}

/// gci i47w/xl47: HotSpot 25's `VM_G1PauseCleanup` operation name, the
/// second `Safepoint` line of a G1 remark pause
/// ([`non_collection_pause_cleanup_point`]).
const G1_PAUSE_CLEANUP_OPERATION: &str = "G1PauseCleanup";

/// Start timing a non-collection pause: `None` (one config load and one
/// match) unless `--verbose:gc`, the pause counts toward a backend's JMX
/// bean, or (gen r5w2/obs6) it is a generational concurrent-cycle pause
/// while a JFR recording runs ([`gen_cycle_jfr_row`]). Call right after
/// `stw_take_over_and_wait` returns, like [`gc_event_start`], so the facts
/// are this pause's.
///
/// gci i12/ge: a G1 marking pause also takes its cycle edge
/// ([`g1_cycle_edge`]: the cycle's id, on every run) and always gets a timer;
/// one the calling thread's G1 full collection folds into its ONE report
/// (`CRATONVM_G1_FULL_GC_ONE_REPORT`) books no bean, queues no notification
/// and writes no JFR row or `-Xlog` line -- only its pause line prints.
pub(super) fn non_collection_pause_start(
    shared: &SharedVm,
    kind: NonCollectionPause,
) -> Option<NonCollectionPauseTimer> {
    let fold = match kind {
        NonCollectionPause::G1InitialMark | NonCollectionPause::G1Remark => {
            g1_full_scope_fold(shared, true)
        }
        _ => None,
    };
    let folded_id = fold.as_ref().map(|(id, _)| *id);
    // gci i14/mp finding 9: under the opt-in the G1 initial mark books no bean.
    let books = non_collection_pause_books_a_bean(kind, g1_jmx_hotspot_cycle_beans_enabled);
    let beans = kind
        .bean_pause()
        .filter(|_| fold.is_none() && books)
        .and_then(|(shape, cause)| {
            let set = shared.mem.backend_gc_beans()?;
            if set.shape() != shape {
                return None;
            }
            // zr z1/tr: a ZGC mark start carries the cycle's own HotSpot cause
            // (`Warmup`, `Proactive`, ...) when the heap knows it; under
            // `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`, the pre-zr `Allocation Failure`.
            // zt t24/zc8: and a failed Pause Mark End of that cycle.
            let requested = (kind == NonCollectionPause::ZgcMarkStart)
                .then(|| zgc_requested_mark_start_cause(shared))
                .flatten();
            let cause = if let Some(requested) = requested {
                // zt t31/zc11 (1b): a requested cycle's mark start.
                requested
            } else if let Some(open) = (kind == NonCollectionPause::ZgcMarkEndContinue)
                .then(|| zgc_concurrent_cycle_jfr(shared).and_then(|c| c.open_cycle_cause()))
                .flatten()
            {
                // zt t32 (successor page section 10): a failed Pause Mark End is
                // a pause of the open cycle, booked under the cycle's cause. It
                // runs on an allocating thread's poll, whose trigger latch is
                // empty during a requested cycle, so a `System.gc()` cycle's
                // fourth pause read `Allocation Rate` on a slow host.
                open
            } else if matches!(
                kind,
                NonCollectionPause::ZgcMarkStart | NonCollectionPause::ZgcMarkEndContinue
            ) {
                shared
                    .mem
                    .heap
                    .zgc_hotspot_gc_cause(false)
                    .unwrap_or(GcDoor::AllocationThreshold.cause())
            } else {
                cause
            };
            let sampler = shared.mem.heap.jmx_pool_sampler()?;
            let before = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                set.collectors(),
                set.pools(&sampler.sample()),
            );
            Some(NonCollectionPauseBeans {
                set: std::sync::Arc::clone(set),
                sampler,
                cause,
                before,
                after: None,
                split_ns: None,
                stw_full_mark: false,
            })
        });
    let verbose = shared
        .mem
        .verbose_gc
        .load(std::sync::atomic::Ordering::Relaxed);
    let g1_cycle = g1_cycle_edge(shared, kind, folded_id);
    // gci i13/gp: the remark of a cycle a full collection aborted books no
    // `G1 Concurrent GC` collection and writes no pause row (its marking row
    // stays); see `G1CycleEdge::aborted`.
    let aborted = g1_cycle.as_ref().is_some_and(|e| e.aborted);
    let beans = if aborted { None } else { beans };
    // gci i30/ob (mr finding 9): the stop-the-world full mark's remark keeps
    // its snapshots but books nothing (`NonCollectionPauseBeans::stw_full_mark`).
    let stw_full_mark = stw_full_mark_remark(kind, shared.mem.heap.is_g1(), g1_cycle.is_some());
    // gci i45/sl: this pause's `-Xlog:safepoint` line. A marking pause folded
    // into a G1 full collection's report, and the stop-the-world full mark's
    // remark, are parts of HotSpot's one `G1CollectFull`: no line of their own.
    // gci i46/lg: nor a count of their own (`sun.rt.safepoints`).
    non_collection_safepoint_begin(shared, kind, fold.is_some() || stw_full_mark);
    let beans = beans.map(|mut b| {
        b.stw_full_mark = stw_full_mark;
        b
    });
    let jfr = if fold.is_some() {
        None
    } else {
        gen_cycle_jfr_row(
            shared,
            kind,
            g1_cycle.as_ref().map(|e| (e.cycle_id, e.mark_start_ns)),
        )
        .map(|mut row| {
            row.aborted = aborted;
            row
        })
    };
    let folded = fold.map(|(_, acc)| acc);
    let zgc_jfr = zgc_mark_start_jfr(shared, kind);
    if !verbose
        && beans.is_none()
        && jfr.is_none()
        && g1_cycle.is_none()
        && folded.is_none()
        && zgc_jfr.is_none()
    {
        return None;
    }
    Some(NonCollectionPauseTimer {
        kind,
        facts: verbose.then(|| pause_safepoint_facts(shared)),
        // After the before-snapshot: `work_us` and the bean's pause time are
        // the pause's work, not the sampling.
        t0: std::time::Instant::now(),
        work_ns: None,
        beans,
        jfr,
        g1_cycle,
        folded,
        zgc_jfr,
    })
}

/// zr z4/jf: a ZGC mark start says whether its concurrent mark opened, so its
/// JFR row opens the cycle's `Concurrent Mark` only when one runs. Call before
/// [`non_collection_pause_seal`]; a no-op for any other timer.
pub(super) fn non_collection_pause_note_opened(
    timer: &mut Option<NonCollectionPauseTimer>,
    opened: bool,
) {
    if let Some(z) = timer.as_mut().and_then(|t| t.zgc_jfr.as_mut()) {
        z.opened = opened;
    }
}

/// zt t24/zc8: a non-collection pause decided only part-way through its
/// stopped world (a ZGC Pause Mark End attempt that found mark work left)
/// measures its work from `t0`, the point its own work began, rather than
/// from the timer's start. A no-op without a timer.
pub(super) fn non_collection_pause_backdate(
    timer: &mut Option<NonCollectionPauseTimer>,
    t0: std::time::Instant,
) {
    if let Some(t) = timer.as_mut() {
        // zt t31/zc11 (1f R1): a JFR row's wall start moves back with it, or
        // the row (start + backdated work) ends after the pause did.
        let earlier = nanos_between(t0, t.t0);
        if let Some(row) = t.zgc_jfr.as_mut() {
            row.wall_start_ns = row.wall_start_ns.saturating_sub(earlier);
        }
        t.t0 = t.t0.min(t0);
    }
}

/// Stamp the end of the pause's work while the world is still stopped — call
/// it immediately before `retire_skip_spans_and_resume` (the counterpart of
/// [`gc_event_seal`]). gc-common w7-f: also books the pause on its bean and
/// samples the pools as the pause left them.
pub(super) fn non_collection_pause_seal(timer: &mut Option<NonCollectionPauseTimer>) {
    if let Some(t) = timer.as_mut() {
        let work_ns = nanos_between(t.t0, std::time::Instant::now());
        t.work_ns = Some(work_ns);
        // gci i14/mp finding 9 (opt-in): a remark whose cleanup ran is
        // HotSpot's two `G1 Concurrent GC` collections, split at the cut.
        let split = if t.beans.is_some()
            && t.kind == NonCollectionPause::G1Remark
            && g1_jmx_hotspot_cycle_beans_enabled()
        {
            t.g1_cycle
                .as_ref()
                .and_then(|e| e.cleanup_after_ns)
                .map(|cut| cut.min(work_ns))
        } else {
            None
        };
        if let Some(b) = t.beans.as_mut() {
            match split {
                // gci i30/ob (mr finding 9): no `G1 Concurrent GC` collection.
                _ if b.stw_full_mark => {}
                Some(cut) => {
                    b.set.note_non_collection_pause(cut);
                    b.set.note_non_collection_pause(work_ns - cut);
                }
                None => b.set.note_non_collection_pause(work_ns),
            }
            b.split_ns = split;
            b.after = Some(b.snapshot());
        }
        // gci i12/ge: the remark's `-Xlog:gc` after-figures, inside the pause.
        if let Some(edge) = t.g1_cycle.as_mut() {
            if let Some(sampler) = edge.sampler.as_ref() {
                edge.heap_after = Some(pooled_heap_figures(sampler));
            }
        }
    }
    // gci i45/sl: the `-Xlog:safepoint` line's end, inside the pause (a no-op
    // without an open line).
    crate::runtime::unified_logging::safepoint_line_seal();
}

/// Print the pause's line, and queue its GC notification (gc-common w7-f).
/// Call after `complete_gc` (no I/O inside the pause). An unsealed timer is
/// measured to now (and booked now).
///
/// The notification is delivered at the next drain point — the next
/// collection's door drain, or the reference-delivery thread's next batch —
/// not by this pause: no drain point runs after a pause that collected
/// nothing, and this function has no thread to run Java on. (G1 follows a
/// remark with mixed collections; a ZGC mark start opens a cycle that ends in
/// a collection.)
pub(super) fn non_collection_pause_finish(timer: Option<NonCollectionPauseTimer>) {
    // gci i45/sl: the pause's `-Xlog:safepoint` line, printed when this
    // returns (after the pause's own lines, on every exit).
    let _safepoint_line = SafepointLineOnDrop;
    let Some(t) = timer else {
        return;
    };
    let work_ns = t
        .work_ns
        .unwrap_or_else(|| nanos_between(t.t0, std::time::Instant::now()));
    if let Some(facts) = t.facts {
        eprintln!(
            "{}",
            render_safepoint_pause_line("-", t.kind.token(), facts, "work_us", work_ns)
        );
    }
    // gci i12/ge: a folded marking pause's work counts toward its full
    // collection's ONE report.
    if let Some(acc) = t.folded.as_ref() {
        acc.add(work_ns);
    }
    // The JFR row stamps an opened cycle's marking start itself.
    let jfr_stamps_the_start = t.jfr.as_ref().is_some_and(|r| r.opens.is_some());
    if let Some(row) = t.jfr {
        emit_gen_cycle_jfr_row(row, work_ns);
    }
    if let Some(edge) = t.g1_cycle {
        g1_cycle_edge_finish(t.kind, edge, work_ns, jfr_stamps_the_start);
    }
    if let Some(row) = t.zgc_jfr {
        emit_zgc_mark_start_jfr(row, work_ns);
    }
    if let Some(b) = t.beans {
        let after = match b.after.as_ref() {
            Some(a) => a.clone(),
            None => {
                if !b.stw_full_mark {
                    b.set.note_non_collection_pause(work_ns);
                }
                b.snapshot()
            }
        };
        // gci i30/ob (mr finding 9): part of HotSpot's one `Pause Full`.
        if b.stw_full_mark {
            stw_full_mark_stamps_every_pool(&b.set, &after);
            return;
        }
        let wall_ms = wall_clock_ns() / 1_000_000;
        let mut recs =
            cratonvm_gc::gc_metrics::gc_notifications_between(&b.before, &after, b.cause, wall_ms);
        // gci i14/mp finding 9 (opt-in): two collections booked, two
        // notifications (Remark, then Cleanup).
        if let Some(cut) = b.split_ns {
            recs = split_concurrent_remark_records(
                recs,
                t.t0 + std::time::Duration::from_nanos(cut),
            );
        }
        // zs s2/ob: a ZGC mark start's `ZGC Major Pauses` record carries no
        // pool usage, as HotSpot's pause tracer records none (a no-op on G1).
        // zs s3/ob2: including a zero `Metaspace` entry once the pool is out.
        b.set.append_metaspace_usage(&mut recs, None, None);
        b.set.unrecord_pause_usage(&mut recs);
        // gch h4/g1: a G1 remark + cleanup ends as HotSpot's Remark / Cleanup
        // pauses do (`gc_end` with countCollection on the concurrent manager).
        if t.kind == NonCollectionPause::G1Remark {
            g1_remark_note_collection_usage(&b.set, &after, &recs);
        }
        for rec in recs {
            b.set.notifications().record(rec);
        }
    }
}

/// gci i12/ge: the end of a G1 marking pause's cycle edge
/// ([`G1CycleEdge`]), after the release (no I/O inside the pause). The
/// initial mark stamps the cycle's marking start (unless its JFR row did) and
/// prints `GC(c) Concurrent Mark Cycle`; the remark prints its `Pause Remark`
/// / `Pause Cleanup` / `Concurrent Mark Cycle <ms>` lines
/// ([`render_xlog_g1_remark_lines`]). Lines only under `edge.xlog`.
fn g1_cycle_edge_finish(
    kind: NonCollectionPause,
    edge: G1CycleEdge,
    work_ns: u64,
    jfr_stamped_the_start: bool,
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel};
    if kind == NonCollectionPause::G1InitialMark {
        if !jfr_stamped_the_start {
            edge.cycle.mark_started(edge.cycle_id, wall_clock_ns());
        }
        if edge.xlog {
            crate::runtime::unified_logging::gc_info(&render_xlog_g1_cycle_open_line(
                edge.cycle_id,
            ));
        }
        return;
    }
    // gci i13/gp: an aborted cycle (opt-in). A remark that did not reach its
    // cleanup leaves the cycle open and is retried: the abort stays armed
    // for the retry, which reports it. One that did prints HotSpot's two
    // lines instead of `Pause Remark` / `Pause Cleanup`.
    if edge.aborted {
        if edge.cleanup_after_ns.is_none() {
            edge.cycle.mark_aborted(edge.cycle_id);
            return;
        }
        if edge.xlog {
            let cycle_ns = (edge.cycle_start_ns != 0)
                .then(|| wall_clock_ns().saturating_sub(edge.cycle_start_ns));
            for (tag, line) in render_xlog_g1_aborted_cycle_lines(edge.cycle_id, cycle_ns) {
                log_unified(&[tag], LogLevel::Info, &line);
            }
        }
        // gci i19/ob (`gci-i12ge-...-review-findings-20261013` residual r3):
        // HotSpot's `concurrent_cycle_end` commits `jdk.ConcurrentModeFailure`
        // beside the `Concurrent Mark Abort` line, under the cycle's id. Only
        // the `jdk.jfr` backend: the legacy recorder has no such type.
        if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
            cratonvm_native_builtins_jfr::jfr_vm_more::concurrent_mode_failure(
                edge.vm_identity,
                edge.cycle_id as i32, // Truncation: deliberate wrap, as the other GC rows.
            );
        }
        return;
    }
    if !edge.xlog {
        return;
    }
    let (before, _) = edge.heap_before;
    let (after, committed) = edge.heap_after.unwrap_or(edge.heap_before);
    let (remark_ns, cleanup_ns) = match edge.cleanup_after_ns {
        Some(cut) => {
            let cut = cut.min(work_ns);
            (cut, Some(work_ns - cut))
        }
        None => (work_ns, None),
    };
    // Findings page item 4: the cycle's start survives a retried remark.
    let cycle_ns = (edge.cycle_start_ns != 0)
        .then(|| wall_clock_ns().saturating_sub(edge.cycle_start_ns));
    let cpu = match (edge.cpu_start, process_cpu_secs()) {
        (Some((user0, sys0)), Some((user1, sys1))) => Some((user1 - user0, sys1 - sys0)),
        _ => None,
    };
    // gci i30/ob (g3x item 7): the first `gc,start` line is the remark's,
    // the second the cleanup's (`remark_ns` later); each decorated there.
    let mut starts = [
        edge.pause_start,
        edge.pause_start + std::time::Duration::from_nanos(remark_ns),
    ]
    .into_iter();
    // gci i47w/xl47 (safepoint residual 1): the remark's `Safepoint
    // "G1PauseRemark"` line (split off at the cleanup point) prints between
    // the two pauses' `gc` lines, as HotSpot's two operations order them.
    let mut gc_starts = 0u32;
    for (tag, line) in render_xlog_g1_remark_lines(
        edge.cycle_id,
        (before, after, committed),
        remark_ns,
        cleanup_ns,
        cycle_ns,
        cpu,
    ) {
        if tag == crate::runtime::unified_logging::LogTag::GcStart {
            gc_starts += 1;
            if gc_starts == 2 {
                crate::runtime::unified_logging::safepoint_line_flush_head();
            }
        }
        let at = if tag == crate::runtime::unified_logging::LogTag::GcStart {
            // gci i30/ob hand-back 3: `Pause Remark` / `Pause Cleanup`'s start
            // lines need the scope's `gc` set too (`xlog_start_line_on`).
            let at = starts.next();
            if !xlog_on(crate::runtime::unified_logging::LogTag::Gc) {
                continue;
            }
            at
        } else {
            None
        };
        crate::runtime::unified_logging::log_unified_at(&[tag], LogLevel::Info, &line, at);
    }
}

/// gci i12/ge: the `GC(n)` a G1 remark's deferred `gc+ref` block files under
/// -- its concurrent cycle's id (the open cycle [`g1_cycle_edge`] re-opened),
/// as HotSpot prints it inside `Pause Remark`. `None` off G1 or with no open
/// cycle (the caller keeps the collection ordinal).
pub(super) fn g1_open_cycle_id(shared: &SharedVm) -> Option<u64> {
    if !shared.mem.heap.is_g1() {
        return None;
    }
    shared
        .mem
        .backend_gc_beans()?
        .notifications()
        .concurrent_cycle_jfr()
        .open_id()
}

/// `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE` (default OFF, gci i13/gp,
/// `runtime_flag_on`): read once per G1 full collection that finds a cycle
/// open (no static). gci i15/ca extended it from the report to the work: the
/// marked cycle is aborted by a pause of its own
/// (`gc_and_alloc::g1_abort_open_cycle_pause`, see
/// [`g1_open_cycle_marked_aborted`]) instead of finished by a remark.
fn g1_full_gc_aborts_open_cycle_enabled() -> bool {
    false
}

/// gci i13/gp (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` item 3):
/// a G1 full collection found a concurrent cycle open. HotSpot aborts it
/// (`G1FullCollector::prepare_collection` -> `concurrent_cycle_abort`): no
/// remark or cleanup, `GC(c) Concurrent Mark Abort` (`gc,marking`) and the
/// cycle's `GC(c) Concurrent Mark Cycle <ms>` (`gc`), no `G1 Concurrent GC`
/// collection. This VM must still run that cycle's remark + cleanup before
/// it can start the full collection's own cycle (`g1_force_full_cycle`), so
/// under the opt-in the cycle is only MARKED aborted here: its remark (whoever
/// runs it) then reports it as HotSpot's aborted cycle ([`G1CycleEdge::aborted`]).
/// gci i15/ca: the mark now also changes the work -- when the cycle is still
/// open at `g1_force_full_cycle`, it is aborted there by a pause of its own
/// instead of finished ([`g1_open_cycle_marked_aborted`]; `force_full_collection`
/// skips its own cycle tick meanwhile); a remark another thread runs first
/// still reports as above. Only for a door HotSpot G1 answers with a
/// `Pause Full` ([`g1_door_is_a_pause_full`]): under
/// `-XX:+ExplicitGCInvokesConcurrent` HotSpot's `System.gc()` waits for the
/// running cycle instead. Returns the aborted id; `None` off G1, for any other
/// door, with the switch off, or with no cycle marking. Called by
/// `force_full_collection` at its start, outside every pause.
pub(super) fn g1_full_gc_abort_open_cycle(shared: &SharedVm, door: GcDoor) -> Option<u64> {
    if !shared.mem.heap.is_g1()
        || !shared.mem.heap.g1_is_marking_active()
        || !g1_full_door_is_a_pause_full(
            door,
            explicit_gc_invokes_concurrent(shared),
            door == GcDoor::MetadataThreshold && g1_metadata_concurrent_start_enabled(),
        )
        || !g1_full_gc_aborts_open_cycle_enabled()
    {
        return None;
    }
    g1_mark_open_cycle_aborted(shared)
}

/// gci i13/gp (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
/// item 7): the allocation-failure ladder's full cycle
/// (`gc_and_alloc::g1_last_ditch_full_cycle`) is HotSpot's allocation-failure
/// `Pause Full`, which aborts a running cycle as [`g1_full_gc_abort_open_cycle`]
/// does. Same switch. gci i15/ca: no longer report-only -- the marked cycle is
/// aborted by `g1_force_full_cycle_with` instead of finished, and only the
/// fresh cycle marks ([`g1_open_cycle_marked_aborted`]). Outside every pause.
pub(super) fn g1_last_ditch_abort_open_cycle(shared: &SharedVm) -> Option<u64> {
    if !shared.mem.heap.is_g1()
        || !shared.mem.heap.g1_is_marking_active()
        || !g1_full_gc_aborts_open_cycle_enabled()
    {
        return None;
    }
    g1_mark_open_cycle_aborted(shared)
}

/// gci i13/gp: mark the open G1 cycle aborted (see
/// [`g1_full_gc_abort_open_cycle`]); its id, `None` without beans or an open id.
/// gci i15/ca: also called inside the abort pause
/// (`gc_and_alloc::g1_abort_open_cycle_pause`) right before its timer starts,
/// so the pause's cycle edge reports the abort whichever cycle is open then.
pub(super) fn g1_mark_open_cycle_aborted(shared: &SharedVm) -> Option<u64> {
    let cycle = shared
        .mem
        .backend_gc_beans()?
        .notifications()
        .concurrent_cycle_jfr();
    let id = cycle.open_id()?;
    cycle.mark_aborted(id);
    Some(id)
}

/// gci i15/ca (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
/// item 7's cost half, same opt-in): the id of the open G1 cycle a full
/// collection has already marked aborted ([`g1_full_gc_abort_open_cycle`] /
/// [`g1_last_ditch_abort_open_cycle`]), leaving the mark in place for the
/// abort pause's report. This is how `g1_force_full_cycle_with` learns that
/// its caller is a full collection HotSpot answers with an abort (the door
/// rule stays in one place, the report half's): it then aborts the cycle
/// instead of finishing it with a remark. `None` off G1, with the switch off,
/// with no open cycle, or with the open cycle not marked (another door, or a
/// cycle that opened after the mark). Outside every pause: the take and the
/// re-mark cannot straddle a remark, which needs this thread to arrive.
pub(super) fn g1_open_cycle_marked_aborted(shared: &SharedVm) -> Option<u64> {
    if !shared.mem.heap.is_g1() || !g1_full_gc_aborts_open_cycle_enabled() {
        return None;
    }
    let cycle = shared
        .mem
        .backend_gc_beans()?
        .notifications()
        .concurrent_cycle_jfr();
    let id = cycle.open_id()?;
    if !cycle.take_aborted(id) {
        return None;
    }
    cycle.mark_aborted(id);
    Some(id)
}

// ---------------------------------------------------------------------------
// One report per G1 full collection (gci i12/ge, opt-in)
// ---------------------------------------------------------------------------
//
// `gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (b). This VM's G1 has
// no full collector: `force_full_collection` runs a young/mixed pause on the
// requester's door, a marking cycle (initial mark, remark + cleanup) and a
// compaction pause, and each reported itself -- `G1 Young Generation` and
// `G1 Concurrent GC` notifications, several `-Xlog:gc` lines and JFR
// `G1New` rows -- where HotSpot G1 reports ONE `Pause Full (<cause>)`: one
// `G1 Old Generation` / `end of major GC` notification, one `GC(n)` line, one
// JFR `G1Full` / `jdk.OldGarbageCollection`. Under
// `CRATONVM_G1_FULL_GC_ONE_REPORT` (default OFF) the full collection opens a
// per-VM scope (`AllocLadderState::g1_full_report`); every pause ITS thread
// initiates inside it is folded (`GcEventStart::full_scope_id`,
// `NonCollectionPauseTimer::folded`: no ordinal, bean, notification, `-Xlog`
// or JFR row of its own; its `--verbose:gc` pause line still prints, under
// the report's id), and the scope's end books and reports the one full
// collection: its duration is the folded pauses' sum, its heap edges the
// scope's two ends. Another thread's pause meanwhile is its own collection,
// reported as before; a second full collection while a scope is open
// (another thread's `System.gc()`) reports per pause, as before.

/// `CRATONVM_G1_FULL_GC_ONE_REPORT` (default OFF, gci i12/ge): read once per
/// G1 full collection (no static).
fn g1_full_gc_one_report_enabled() -> bool {
    false
}

/// HotSpot's `GCName::G1Full`, the JFR name (`jdk.GarbageCollection.name`)
/// of a G1 full collection (`G1FullGCTracer`). gci i30/ob (vi20 Batch 3,
/// `sg25_f_fulljfr_on`: HotSpot 25 `names [G1Full]`, ours `[G1Old]`): it was
/// `G1Old`, which is HotSpot's name for the concurrent cycle's tracer
/// (`G1OldTracer`). The `GarbageCollectorMXBean` stays `G1 Old Generation`
/// (the JMX manager, not the JFR name).
const G1_FULL_JFR_NAME: &str = "G1Full";

/// gci i12/ge: does the calling thread's open G1 full-collection scope fold
/// this pause in? `Some((the report's id, the marking-pause time sum))` when
/// the heap is G1, a scope is open, this OS thread owns it and -- for a
/// marking pause (`marking`) -- the full collection's own marking cycle is
/// still running. The report's id is taken by the first pause folded in. Called
/// inside the pause: `try_lock` (the only holders are the scope's own thread
/// outside its pauses and pause finishers after a release, so a failure is
/// not expected; it reports the pause on its own).
fn g1_full_scope_fold(shared: &SharedVm, marking: bool) -> Option<(u64, FoldSums)> {
    if !shared.mem.heap.is_g1() {
        return None;
    }
    let mut guard = shared.mem.alloc_ladder.g1_full_report.try_lock()?;
    let scope = match &mut *guard {
        Some(scope) => scope,
        None => return None,
    };
    if !g1_full_scope_folds(scope.owner == std::thread::current().id(), marking, scope.marking_folded) {
        return None;
    }
    let id = *scope.gc_id.get_or_insert_with(|| {
        shared
            .mem
            .gc_cycle_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    });
    Some((
        id,
        FoldSums {
            marking_ns: std::sync::Arc::clone(&scope.marking_ns),
            longest_ns: std::sync::Arc::clone(&scope.longest_ns),
        },
    ))
}

/// gci i12/ge: what a folded MARKING pause adds to its full collection's
/// report after its release (its timer's finish has no `SharedVm`): its work
/// to the marking sum, and to the longest pause.
struct FoldSums {
    marking_ns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    longest_ns: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl FoldSums {
    fn add(&self, work_ns: u64) {
        self.marking_ns
            .fetch_add(work_ns, std::sync::atomic::Ordering::Relaxed);
        self.longest_ns
            .fetch_max(work_ns, std::sync::atomic::Ordering::Relaxed);
    }
}

/// [`g1_full_scope_fold`]'s rule, pure: the owner's collection pauses always,
/// its marking pauses while the full collection's own cycle runs.
fn g1_full_scope_folds(owner: bool, marking: bool, marking_folded: bool) -> bool {
    owner && (!marking || marking_folded)
}

/// gci i24/rv (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017` item
/// 16): the id of the calling thread's open G1 full-collection scope while it
/// still folds marking pauses ([`g1_full_scope_folds`] with `marking`), once a
/// pause has taken it -- the `GC(n)` a remark inside that full collection files
/// its deferred `gc+ref` block under (`gc_and_alloc::remark_reference_xlog_id`).
/// The `System.gc()` stop-the-world cycle opens no concurrent cycle, so its
/// remark fell back to `gc_cycle_count`, the ordinal the NEXT, unrelated
/// collection takes, while every other line of the one report carried the
/// scope's id. `None` off G1, with no scope (every run without
/// `CRATONVM_G1_FULL_GC_ONE_REPORT`), for another thread's scope, once its
/// marking is done, or before any pause took the id. `try_lock`, as
/// [`g1_full_scope_fold`].
pub(super) fn g1_full_scope_marking_id(shared: &SharedVm) -> Option<u64> {
    if !shared.mem.heap.is_g1() {
        return None;
    }
    let guard = shared.mem.alloc_ladder.g1_full_report.try_lock()?;
    let scope = (*guard).as_ref()?;
    if !g1_full_scope_folds(scope.owner == std::thread::current().id(), true, scope.marking_folded) {
        return None;
    }
    scope.gc_id
}

/// gci i25/sg (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017` item
/// 16, `gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (b)): hand one
/// reference-processing round's `-Xlog:gc+ref` census to the calling thread's
/// open G1 full-collection scope instead of deferring it as a block of its
/// own. `true` when the scope took it (the caller then defers nothing);
/// `false` -- the caller defers as before -- off G1, with no scope (every run
/// without `CRATONVM_G1_FULL_GC_ONE_REPORT` / the OOME report), for another
/// thread's scope, for a marking round (`marking`: the remark) once the full
/// collection's own cycle is done, or before any pause took the report's id.
///
/// Why: inside the scope every folded pause printed its own block -- the
/// young pause's under `gc_cycle_count`, i.e. the report's id + 1 (the scope
/// took one ordinal and folded pauses take none), so the remark's flush
/// dropped it; the remark's under the report's id, printed BEFORE the
/// report's `gc,start` line; and the compaction pause's again under the
/// report's id + 1, never flushed by the report, so the NEXT, unrelated
/// collection printed it as its own second block. HotSpot's `Pause Full`
/// prints one block (`G1FullCollector::phase1_mark_live_objects`:
/// `pt.print_all_references()`, base indent 0) after its `gc,start` line;
/// [`G1FullReport::finish`] now prints the sum of the rounds that way.
/// Called inside the pause by the census producers; `try_lock`, as
/// [`g1_full_scope_fold`].
pub(super) fn g1_full_scope_absorb_ref_census(
    shared: &SharedVm,
    marking: bool,
    census: &crate::runtime::unified_logging::RefProcCensus,
) -> bool {
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let Some(mut guard) = shared.mem.alloc_ladder.g1_full_report.try_lock() else {
        return false;
    };
    let Some(scope) = (*guard).as_mut() else {
        return false;
    };
    if scope.gc_id.is_none()
        || !g1_full_scope_folds(scope.owner == std::thread::current().id(), marking, scope.marking_folded)
    {
        return false;
    }
    let sum = ref_census_sum(scope.ref_census.as_ref(), census);
    scope.ref_census = Some(sum);
    true
}

/// gci i25/sg: two reference-processing rounds as one HotSpot block: every
/// count and every time added (saturating). `None` + `b` is `b`. Pure.
fn ref_census_sum(
    a: Option<&crate::runtime::unified_logging::RefProcCensus>,
    b: &crate::runtime::unified_logging::RefProcCensus,
) -> crate::runtime::unified_logging::RefProcCensus {
    let Some(a) = a else {
        return *b;
    };
    let add4 = |x: [u64; 4], y: [u64; 4]| -> [u64; 4] {
        [
            x[0].saturating_add(y[0]),
            x[1].saturating_add(y[1]),
            x[2].saturating_add(y[2]),
            x[3].saturating_add(y[3]),
        ]
    };
    let add3 = |x: [u64; 3], y: [u64; 3]| -> [u64; 3] {
        [
            x[0].saturating_add(y[0]),
            x[1].saturating_add(y[1]),
            x[2].saturating_add(y[2]),
        ]
    };
    crate::runtime::unified_logging::RefProcCensus {
        discovered: add4(a.discovered, b.discovered),
        dropped: add4(a.dropped, b.dropped),
        processed: add4(a.processed, b.processed),
        total_ns: a.total_ns.saturating_add(b.total_ns),
        phase_ns: add3(a.phase_ns, b.phase_ns),
        phase1_kind_ns: add3(a.phase1_kind_ns, b.phase1_kind_ns),
    }
}

/// gci i12/ge: add a folded collection pause's duration to the scope (after
/// the pause's release).
fn g1_full_scope_add_collection(shared: &SharedVm, pause_ns: u64) {
    let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
    if let Some(scope) = &mut *guard {
        if scope.owner == std::thread::current().id() {
            scope.pause_ns = scope.pause_ns.saturating_add(pause_ns);
            scope.pauses = scope.pauses.saturating_add(1);
            scope
                .longest_ns
                .fetch_max(pause_ns, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// gci i12/ge: one G1 full collection being reported as one
/// ([`g1_full_report_begin`]). The facts its report needs from BEFORE the
/// collection; the scope (`AllocLadderState::g1_full_report`) collects what
/// its pauses add. Dropping it without [`Self::finish`] (an unwind) closes the
/// scope unreported, so no later pause of this thread stays folded.
pub(super) struct G1FullReport<'a> {
    shared: &'a SharedVm,
    door: GcDoor,
    wall_start_ns: u64,
    /// A JFR recording or an `-Xlog` collection rule was armed at the start.
    events: bool,
    beans_before: Option<cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    heap_before: usize,
    committed_before: usize,
    cpu_start: Option<(f64, f64)>,
    metaspace_before: Option<crate::vm::vm_init::MetaspaceStats>,
    regions_before: Option<G1RegionEdge>,
}

/// gci i12/ge: open the ONE-report scope of a G1 full collection on `door`
/// (`force_full_collection`), when the heap is G1, the door is one HotSpot G1
/// answers with a `Pause Full` ([`g1_door_is_a_pause_full`]; gci i13/gp: and
/// `Metadata GC Threshold` under `CRATONVM_G1_METADATA_CONCURRENT_START`,
/// [`g1_full_door_is_a_pause_full`]), the switch is on and no scope is open.
/// Outside every pause.
pub(super) fn g1_full_report_begin(shared: &SharedVm, door: GcDoor) -> Option<G1FullReport<'_>> {
    if !shared.mem.heap.is_g1()
        || !g1_full_door_is_a_pause_full(
            door,
            explicit_gc_invokes_concurrent(shared),
            door == GcDoor::MetadataThreshold && g1_metadata_concurrent_start_enabled(),
        )
        || !g1_full_gc_one_report_enabled()
    {
        return None;
    }
    g1_full_report_open(shared, door)
}

/// `CRATONVM_G1_OOME_PAUSE_FULL_REPORT` (default ON since 2026-10-17, `=0`
/// kills; gci i14/ox, `runtime_flag_default_on`): read twice per G1
/// allocation-failure ladder (`gc_and_alloc::last_ditch_reclaim`), never on a
/// successful allocation; no static.
pub(super) fn g1_oome_pause_full_report_enabled() -> bool {
    true
}

/// gci i14/ox (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
/// item 3): open the ONE-report scope of one of the two full collections
/// HotSpot G1 runs before a heap `OutOfMemoryError`
/// (`G1CollectedHeap::satisfy_failed_allocation`: `Pause Full (G1 Compaction
/// Pause)` with the LRU soft policy, then a second one clearing every soft
/// reference). The allocation ladder's first half (the marking cycle and the
/// recovery compaction) and its second half (the soft-reference rung) are each
/// reported as one such collection: one `G1 Old Generation` booking and `end
/// of major GC` notification with cause `G1 Compaction Pause`, one `GC(n)
/// Pause Full (G1 Compaction Pause)` line, one JFR `G1Full` row
/// ([`G1FullReport::finish`]). The door is [`GcDoor::G1CompactionPause`]
/// (HotSpot's `GCCause::_g1_compaction_pause`), which is not a `Pause Full` on
/// any other path ([`g1_door_is_a_pause_full`]), so this is its own entry and
/// does not read `CRATONVM_G1_FULL_GC_ONE_REPORT`. `None` off G1, with the
/// switch off, or while another scope is open (that collection then reports
/// per pause, as before). Outside every pause.
pub(super) fn g1_oome_full_report_begin(shared: &SharedVm) -> Option<G1FullReport<'_>> {
    if !g1_oome_full_report_applies(shared.mem.heap.is_g1(), g1_oome_pause_full_report_enabled()) {
        return None;
    }
    g1_full_report_open(shared, GcDoor::G1CompactionPause)
}

/// [`g1_oome_full_report_begin`]'s gate, pure: G1 and the switch.
fn g1_oome_full_report_applies(g1: bool, switch_on: bool) -> bool {
    g1 && switch_on
}

/// The scope-opening half of [`g1_full_report_begin`] and
/// [`g1_oome_full_report_begin`] (gci i14/ox split it out): `None` when a
/// scope is already open.
fn g1_full_report_open(shared: &SharedVm, door: GcDoor) -> Option<G1FullReport<'_>> {
    use crate::runtime::unified_logging::LogTag;
    {
        let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
        if guard.is_some() {
            return None;
        }
        *guard = Some(crate::vm::realms::heap_realm::G1FullReportScope {
            owner: std::thread::current().id(),
            gc_id: None,
            pause_ns: 0,
            pauses: 0,
            marking_folded: true,
            marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ref_census: None,
        });
    }
    let events = gc_events_wanted();
    Some(G1FullReport {
        shared,
        door,
        wall_start_ns: wall_clock_ns(),
        events,
        beans_before: gc_bean_snapshot(shared),
        heap_before: gc_event_heap_used(&shared.mem.heap),
        committed_before: shared.mem.heap.committed_bytes(),
        cpu_start: if events && xlog_on(LogTag::GcCpu) {
            process_cpu_secs()
        } else {
            None
        },
        metaspace_before: if events && xlog_on(LogTag::GcMetaspace) {
            Some(metaspace_stats_now(shared))
        } else {
            None
        },
        regions_before: if events && xlog_on(LogTag::GcHeap) {
            G1RegionEdge::sample(&shared.mem.heap)
        } else {
            None
        },
    })
}

/// gci i12/ge: the full collection's own marking cycle has finished
/// (`g1_force_full_cycle` returned): from here on a marking pause is an
/// ordinary one (a cycle the compaction pause's mark drive opens is reported
/// as a cycle). A no-op without a report.
pub(super) fn g1_full_report_marking_done(report: Option<&G1FullReport<'_>>) {
    let Some(report) = report else {
        return;
    };
    let mut guard = report.shared.mem.alloc_ladder.g1_full_report.lock();
    if let Some(scope) = &mut *guard {
        if scope.owner == std::thread::current().id() {
            scope.marking_folded = false;
        }
    }
}

impl G1FullReport<'_> {
    /// Close the scope and report the full collection as HotSpot G1's ONE
    /// `Pause Full (<cause>)`: one `G1 Old Generation` booking and `end of
    /// major GC` notification (every pool's collection usage stamped), and
    /// under `-Xlog` / JFR the `gc,start`, `gc,heap`, `gc,metaspace`, `gc`
    /// and `gc,cpu` lines and the `G1Full` / `jdk.OldGarbageCollection` rows.
    /// Nothing when no pause was folded in (every attempt lost or coalesced).
    /// After the collection, outside every pause.
    pub(super) fn finish(self) {
        let shared = self.shared;
        let scope = {
            let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
            let mine = (*guard)
                .as_ref()
                .is_some_and(|s| s.owner == std::thread::current().id());
            if mine {
                (*guard).take()
            } else {
                None
            }
        };
        let Some(scope) = scope else {
            return;
        };
        let Some(gc_id) = scope.gc_id else {
            return;
        };
        let marking_ns = scope
            .marking_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        let duration_ns = scope.pause_ns.saturating_add(marking_ns);
        // Findings page item 2: JFR `longestPause` is the longest folded pause.
        let scope_longest = Some(
            scope
                .longest_ns
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        if let Some(b) = shared.mem.backend_gc_beans() {
            b.note_full_collection(duration_ns);
        }
        let cause = self.door.cause_on(&shared.mem.heap);
        let after_beans = gc_bean_snapshot(shared);
        if let (Some(before), Some(after), Some(queue)) = (
            self.beans_before.as_ref(),
            after_beans.as_ref(),
            gc_notification_queue(shared),
        ) {
            let wall_ms = wall_clock_ns() / 1_000_000;
            let recs = g1_full_report_records(
                cratonvm_gc::gc_metrics::gc_notifications_between(before, after, cause, wall_ms),
                duration_ns,
            );
            // A full collection affects every pool (`G1 Old Generation`
            // manages all three).
            // gci i30/ob hand-back 1: HotSpot's full collection leaves the
            // survivor pool empty (`g1_full_collection_usage_rows`).
            let rows = g1_full_collection_usage_rows(&after.pools);
            if let Some(b) = shared.mem.backend_gc_beans() {
                b.note_collection_usage(&rows);
            }
            let thresholds = queue.pool_thresholds();
            if thresholds.is_armed() {
                let pools: Vec<&'static str> = rows.iter().map(|p| p.name).collect();
                thresholds.check(&rows, &pools);
            }
            for rec in recs {
                queue.record(rec);
            }
        }
        if !self.events {
            return;
        }
        let heap_after = gc_event_heap_used(&shared.mem.heap);
        let committed = shared.mem.heap.committed_bytes();
        emit_jfr_collection(
            shared,
            self.door,
            &JfrCollection {
                gc_id,
                cause,
                name: G1_FULL_JFR_NAME,
                old: true,
                start_ns: self.wall_start_ns,
                duration_ns,
                heap_before: self.heap_before,
                committed_before: self.committed_before,
                heap_after,
                committed_after: committed,
                phases: &[],
                g1_type: None,
                vm_operation: jfr_vm_operation(&shared.mem.heap, self.door),
                longest_ns: scope_longest,
                zgc: None,
                pauses: &[],
                // gci i20/ob: the folded report names no requester (its
                // pauses' doors may differ); `caller` stays 0.
                caller: None,
            },
        );
        self.xlog_lines(
            gc_id,
            cause,
            (heap_after, committed),
            duration_ns,
            scope.ref_census.as_ref(),
            g1_full_phase_ns(marking_ns, scope.pause_ns),
        );
    }

    /// The report's `-Xlog` lines, in HotSpot G1's order: `gc,start`,
    /// the reference-processing block (gci i25/sg: the folded rounds' summed
    /// census, [`g1_full_scope_absorb_ref_census`]), `gc,heap` (the four
    /// region lines over the scope's two ends), `gc,metaspace`, `gc`,
    /// `gc,cpu`. gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item
    /// 5, G1 half): HotSpot G1 Full GC's five `gc,phases,start` / `gc,phases`
    /// pairs ([`G1_FULL_GC_PHASES`], `phase_ns`), the reference block inside
    /// Phase 1, as HotSpot's `phase1_mark_live_objects` prints it.
    #[allow(clippy::too_many_arguments)]
    fn xlog_lines(
        &self,
        gc_id: u64,
        cause: &str,
        (heap_after, committed): (usize, usize),
        duration_ns: u64,
        refs: Option<&crate::runtime::unified_logging::RefProcCensus>,
        phase_ns: [u64; 5],
    ) {
        use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
        let shared = self.shared;
        let label = "Pause Full";
        // gci i30/ob hand-back 3: HotSpot's start line needs `gc` too.
        if xlog_start_line_on(LogTag::GcStart, LogTag::Gc) {
            log_unified(
                &[LogTag::GcStart],
                LogLevel::Info,
                &format!("GC({gc_id}) {label} ({cause})"),
            );
            // zt t31/x10: a before-dump of this collection, inside its scope.
            flush_deferred_full_gc_dump_lines();
        }
        // gci i30/ob (g3x item 5): the phase pairs, under the Serial switch
        // (`CRATONVM_XLOG_GC_FULL_PHASES`, default ON, `=0` kills both).
        // gci i30/ob hand-back 3: a phase scope is armed by `gc,phases`; its
        // start line goes to `gc,phases,start` only then.
        let phases = xlog_on(LogTag::GcPhases)
            && crate::runtime::unified_logging::xlog_full_gc_phases_enabled();
        let phase = |index: usize, open: bool, close: bool| {
            if !phases {
                return;
            }
            let (start_line, end_line) = g1_full_gc_phase_lines(gc_id, index, phase_ns[index]);
            if open {
                log_unified(&[LogTag::GcPhasesStart], LogLevel::Info, &start_line);
            }
            if close {
                log_unified(&[LogTag::GcPhases], LogLevel::Info, &end_line);
            }
        };
        phase(0, true, false);
        // gci i25/sg: HotSpot's G1 Full GC prints its one reference block
        // inside Phase 1, after `gc,start`, at base indent 0.
        for (tag, level, text) in g1_full_report_reference_lines(gc_id, refs) {
            log_unified(&[tag], level, &text);
        }
        phase(0, false, true);
        for index in 1..G1_FULL_GC_PHASES.len() {
            phase(index, true, true);
        }
        if let (Some(before), Some(after)) =
            (self.regions_before, G1RegionEdge::sample(&shared.mem.heap))
        {
            for line in render_xlog_g1_heap_lines(gc_id, before, after) {
                log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
            }
        }
        if let Some(before) = self.metaspace_before {
            log_unified(
                &[LogTag::GcMetaspace],
                LogLevel::Info,
                &render_xlog_metaspace_line(gc_id, &before, &metaspace_stats_now(shared)),
            );
        }
        // zt t31/x10: HotSpot G1's after-dump, after `gc,metaspace` and
        // before the `gc` line (`G1FullCollector::complete_collection`).
        run_owed_full_gc_dump_after(shared, gc_id);
        if xlog_on(LogTag::Gc) {
            crate::runtime::unified_logging::gc_info(&render_xlog_gc_line(
                gc_id,
                label,
                cause,
                self.heap_before,
                heap_after,
                committed,
                duration_ns,
            ));
        }
        if let (Some((user0, sys0)), Some((user1, sys1))) = (self.cpu_start, process_cpu_secs()) {
            log_unified(
                &[LogTag::GcCpu],
                LogLevel::Info,
                &render_xlog_cpu_line(gc_id, user1 - user0, sys1 - sys0, duration_ns),
            );
        }
    }
}

/// gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 5, G1 half):
/// HotSpot 25 G1 Full GC's info-level phases (`G1FullCollector::phase1..5`,
/// each a `GCTraceTime(Info, gc, phases)` scope: a `gc,phases,start` line when
/// it opens, `<name> <t>ms` on `gc,phases` when it closes).
const G1_FULL_GC_PHASES: [&str; 5] = [
    "Phase 1: Mark live objects",
    "Phase 2: Prepare compaction",
    "Phase 3: Adjust pointers",
    "Phase 4: Compact heap",
    "Phase 5: Reset Metadata",
];

/// gci i30/ob: one G1 Full GC phase's `(start_line, end_line)`, in the shape
/// of Serial's ([`crate::runtime::unified_logging::xlog_full_gc_phase_lines`]).
/// Pure.
fn g1_full_gc_phase_lines(gc_id: u64, index: usize, phase_ns: u64) -> (String, String) {
    let name = G1_FULL_GC_PHASES[index.min(G1_FULL_GC_PHASES.len() - 1)];
    (
        format!("GC({gc_id}) {name}"),
        format!("GC({gc_id}) {name} {:.3}ms", phase_ns as f64 / 1_000_000.0),
    )
}

/// gci i30/ob: the five phase times of a G1 full collection reported as one
/// (`G1FullReport`), from what this VM measured: its marking pauses' sum is
/// Phase 1 (mark live objects); its collection pauses' sum -- the evacuation
/// and, when one ran, the recovery compaction -- is Phase 4 (compact heap);
/// Phases 2, 3 and 5 have no separate step here and read `0.000ms` (shape
/// parity, as Serial's split had before gch h2/og). Their sum is the report's
/// duration. Pure.
fn g1_full_phase_ns(marking_ns: u64, collection_ns: u64) -> [u64; 5] {
    [marking_ns, 0, 0, collection_ns, 0]
}

impl Drop for G1FullReport<'_> {
    /// Close a scope [`G1FullReport::finish`] did not (an unwind out of the
    /// full collection): unreported, so this thread's later pauses report.
    fn drop(&mut self) {
        let mut guard = self.shared.mem.alloc_ladder.g1_full_report.lock();
        let mine = (*guard)
            .as_ref()
            .is_some_and(|s| s.owner == std::thread::current().id());
        if mine {
            *guard = None;
        }
    }
}

/// gci i25/sg: the ONE `gc,ref` / `gc,phases,ref` block of a G1 full
/// collection's report -- the folded rounds' summed census at HotSpot's
/// top-level indent, under the report's id; nothing when no round handed a
/// census in (`-Xlog` does not take the block, or no round ran). Pure.
fn g1_full_report_reference_lines(
    gc_id: u64,
    refs: Option<&crate::runtime::unified_logging::RefProcCensus>,
) -> Vec<(
    crate::runtime::unified_logging::LogTag,
    crate::runtime::unified_logging::LogLevel,
    String,
)> {
    refs.map(|c| crate::runtime::unified_logging::xlog_reference_lines(gc_id, c))
        .unwrap_or_default()
}

/// gci i12/ge: the notification records of a G1 full collection's ONE report
/// -- the `G1 Old Generation` record only (another thread's young pause
/// inside the scope moved `G1 Young Generation` too; that pause queued its
/// own), its `GcInfo` end at its start plus the folded pauses' sum (the
/// scope's wall span includes the concurrent marking between them). Pure.
fn g1_full_report_records(
    mut recs: Vec<cratonvm_gc::gc_metrics::GcNotificationRecord>,
    duration_ns: u64,
) -> Vec<cratonvm_gc::gc_metrics::GcNotificationRecord> {
    recs.retain(|r| r.collector == cratonvm_gc::gc_metrics::JMX_G1_OLD_COLLECTOR);
    for r in &mut recs {
        r.end = r
            .start
            .checked_add(std::time::Duration::from_nanos(duration_ns))
            .unwrap_or(r.end);
    }
    recs
}

/// gch h4/g1 (`gch-h3os-g1-concurrent-pauses-skip-the-collection-usage-update-and-threshold-check-FIXED-20261012`):
/// HotSpot 25 runs a G1 Remark and Cleanup pause under the concurrent memory
/// manager (`G1 Concurrent GC`, whose one pool is `G1 Old Gen`), and its
/// `GCMemoryManager::gc_end(.., countCollection = true, ..)` then, for that
/// manager's pools, sets the pool's last collection usage
/// (`getCollectionUsage()`) and runs `LowMemoryDetector::detect_after_gc_memory`
/// (the collection-usage threshold). This VM's remark + cleanup counted on the
/// bean and queued its notification but did neither, so after a concurrent
/// cycle `G1 Old Gen.getCollectionUsage()` still answered the last young /
/// mixed pause and a collection-usage threshold on it was not checked.
///
/// The managed pools are the ones the pause's own records name (the bean that
/// moved: `G1 Concurrent GC` -> `G1 Old Gen`), as `record_gc_notification`
/// derives them for a collection. The other pools keep their last
/// collection's figure: `note_collection_usage` stamps every slot, so they are
/// written back with their own `collection_used`
/// ([`affected_collection_usage_rows`]). gci i5g/g1o: before any collection
/// has stamped a figure, the others are written back as `0` (what an unstamped
/// pool reports, here and on HotSpot) and the remark still stamps `G1 Old Gen`;
/// gch h4/g1 skipped the stamp then, so a remark before the first young pause
/// left `getCollectionUsage()` at 0 where HotSpot's Remark sets it. G1 only:
/// the caller asks for `G1Remark`. (Dev's wave 9 lane G made the same fix.)
fn g1_remark_note_collection_usage(
    set: &cratonvm_gc::gc_metrics::BackendGcBeans,
    after: &cratonvm_gc::gc_metrics::GcBeanSnapshot,
    recs: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) {
    let managed = g1_remark_managed_pools(after, recs);
    if managed.is_empty() {
        return;
    }
    // gci i14/mp finding 3: only the managed pools are stamped (used and
    // committed); `affected_collection_usage_rows` is the pure model of it.
    set.note_collection_usage_of(&after.pools, Some(managed.as_slice()));
    let thresholds = set.notifications().pool_thresholds();
    if thresholds.is_armed() {
        thresholds.check(&after.pools, &managed);
    }
}

/// gch h4/g1: the pools of the beans `recs` name, from `after`'s collectors.
/// Pure.
fn g1_remark_managed_pools(
    after: &cratonvm_gc::gc_metrics::GcBeanSnapshot,
    recs: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) -> Vec<&'static str> {
    let mut managed: Vec<&'static str> = recs
        .iter()
        .filter_map(|r| after.collectors.iter().find(|c| c.name == r.collector))
        .flat_map(|c| c.pools.iter().copied())
        .collect();
    managed.dedup();
    managed
}

#[cfg(test)]
mod gch_h4g1_remark_usage_tests;

/// The JMX beans of a heap that describes them: every collector's count and
/// every pool's usage, stamped now. Generational since gen r4w3/obs2 (Serial's
/// beans, the heap's own); G1 since gc-common w7-f, and ZGC once its state is
/// wired (`HeapRealm::backend_gc_beans`). `None` where there is none.
fn gc_bean_snapshot(shared: &SharedVm) -> Option<cratonvm_gc::gc_metrics::GcBeanSnapshot> {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => {
            let snap = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                shared.mem.heap.gen_jmx_collectors()?,
                shared.mem.heap.gen_jmx_memory_pools()?,
            );
            shared
                .mem
                .heap
                .gen_gc_notifications()?
                .note_pool_peaks(&snap.pools);
            Some(snap)
        }
        heap => shared.mem.backend_gc_beans().map(|b| {
            let snap = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                b.collectors(),
                b.pools(&heap.backend_pool_samples()),
            );
            // gci i14/mp (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
            // finding 1): the same peak sample on G1 / ZGC. Without it their
            // peak was the query-time usage, so `G1 Eden Space` read ~0 after
            // every collection. One leaf mutex per collection edge.
            // zs s2/ob (merge of origin/dev, 2026-10-18: one sample, not two):
            // on ZGC it is `BackendGcBeans::note_pool_peaks`, the same sample
            // under the round's `CRATONVM_ZGC_POOL_PEAK_AT_GC` (`=0` kill: the
            // peak is a query-time sample), as HotSpot's ZGC cycle tracer
            // records peak usage at `gc_begin` / `gc_end`.
            if heap.is_g1() {
                b.notifications().note_pool_peaks(&snap.pools);
            } else {
                b.note_pool_peaks(&snap.pools);
            }
            snap
        }),
    }
}

/// The GC-notification queue of this VM's heap: the generational heap's own,
/// or a backend's (`HeapRealm::backend_gc_beans`). gc-common w7-f.
fn gc_notification_queue(
    shared: &SharedVm,
) -> Option<&cratonvm_gc::gc_metrics::GcNotificationQueue> {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => shared.mem.heap.gen_gc_notifications(),
        _ => shared.mem.backend_gc_beans().map(|b| b.notifications()),
    }
}

impl crate::vm::realms::HeapRealm {
    /// The JMX bean state of this VM's G1 or ZGC heap, booked by the common
    /// GC event plumbing (gc-common w7-f): G1's lives in the heap
    /// ([`crate::memory::vm_heap::VmHeap::backend_gc_beans`]).
    ///
    /// ZGC's cannot: `VmHeap::Zgc` holds an `Arc<ZgcRealHeap>`, so its beans
    /// live in this realm (`HeapRealm::zgc_gc_beans`, applied from
    /// `handoff-w7f-zgc-backend-gc-beans` at the wave-7 merge).
    ///
    /// Written here, not in `heap_realm.rs`, so every reader (this file's
    /// event plumbing, `vm_exec.rs`'s JMX natives) goes through ONE accessor.
    pub(crate) fn backend_gc_beans(
        &self,
    ) -> Option<&std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>> {
        match &self.heap {
            _ if self.heap.is_generational() => None,
            _ if self.heap.is_g1() => self.heap.g1_gc_beans(),
            // The ZGC arm, spelled `_` so this crate needs no `zgc` cfg.
            #[allow(unreachable_patterns)]
            _ => Some(&self.zgc_gc_beans),
        }
    }
}

/// Record the collection that ran since `before` as its bean's last GC and,
/// when that bean has a listener, queue its `GarbageCollectionNotificationInfo`
/// (gen r4w3/obs2). Nothing Java runs here: this is HotSpot's
/// `GCMemoryManager::gc_end` + `GCNotifier::pushNotification`. Delivery is
/// [`run_gc_notifications`], after the collection.
///
/// `sealed_after`: the snapshot [`gc_event_seal`] took inside the pause
/// (gc-common w5-a); `None` (an unsealed door) samples now, as before.
///
/// gc-common w7-f: on every heap that describes beans (Generational, G1, and
/// ZGC once wired), one record per bean the collection moved — a ZGC
/// collection is both a `ZGC Major Cycles` and a `ZGC Major Pauses`
/// notification (the Minor pair for a young cycle) -- the pause queued first
/// since zr z2/t2 (`CRATONVM_ZGC_HOTSPOT_BEAN_ORDER`).
///
/// zt t31/zc11: the cycle (non-pause) records of a collection that closes a
/// concurrent cycle start at the cycle's start (`cycle_start`, never later
/// than the record's own). The pause records are unchanged. Pure.
fn cycle_records_start_at(
    recs: &mut [cratonvm_gc::gc_metrics::GcNotificationRecord],
    cycle_start: Option<std::time::Instant>,
) {
    let Some(at) = cycle_start else {
        return;
    };
    for r in recs.iter_mut() {
        if r.action != cratonvm_gc::gc_metrics::GC_ACTION_PAUSE {
            r.start = r.start.min(at);
        }
    }
}

/// gcg h1/ob: `split` = `(mark, mid_pools)` of an escalated pause reported as
/// two collections ([`escalated_pause_split`]): the young bean's record ends at
/// the mark with the young half's pools, the full bean's starts there
/// ([`split_escalated_notifications`]).
///
/// gci i5g/g1o: `affected` = [`GcEventStart::affected_pools`]; `Some` limits
/// the collection-threshold check to those pools (a young-only G1 pause does
/// not check `G1 Old Gen`'s, as HotSpot's `gc_end` does not).
fn record_gc_notification(
    shared: &SharedVm,
    cause: &'static str,
    before: &cratonvm_gc::gc_metrics::GcBeanSnapshot,
    sealed_after: Option<&cratonvm_gc::gc_metrics::GcBeanSnapshot>,
    split: Option<(std::time::Instant, &[cratonvm_gc::gc_metrics::JmxPoolUsage])>,
    affected: Option<&[&'static str]>,
    pause_split: Option<&cratonvm_gc::gc_metrics::ZgcPauseSplit>,
    cycle_start: Option<std::time::Instant>,
) {
    let Some(queue) = gc_notification_queue(shared) else {
        return;
    };
    let late;
    let after = match sealed_after {
        Some(a) => a,
        None => {
            let Some(a) = gc_bean_snapshot(shared) else {
                return;
            };
            late = a;
            &late
        }
    };
    let wall_ms = wall_clock_ns() / 1_000_000;
    // gci i11/go: G1's allocation pauses notify `G1 Evacuation Pause` (the
    // caller's `GcEventStart::cause` spells it since the origin/dev merge).
    let mut recs = cratonvm_gc::gc_metrics::gc_notifications_between(before, after, cause, wall_ms);
    if let Some((mark, mid_pools)) = split {
        split_escalated_notifications(&mut recs, mark, mid_pools);
    }
    // zt t31/zc11: a closed concurrent cycle's record starts at its mark
    // start, so its own pauses lie inside it, as HotSpot's do.
    cycle_records_start_at(&mut recs, cycle_start);
    // zs s6/ob5: the `Metaspace` figure at the end edge (for the threshold
    // check below) and, while the pool is not handed out, the class-store
    // census its cycle records keep for the back-fill.
    let mut ms_after_used = None;
    let mut ms_census = None;
    // zr z2/t2: ZGC's pause notification before its cycle notification, as
    // HotSpot 25 delivers them (`CRATONVM_ZGC_HOTSPOT_BEAN_ORDER`; a no-op on
    // G1 and absent on Generational, which has no backend beans).
    // zs s2/ob: a ZGC pause record carries HotSpot's unrecorded (all-zero)
    // pool usage (`CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE`; a no-op on G1).
    if let Some(b) = shared.mem.backend_gc_beans() {
        // zs s4/ob3: one `end of GC pause` record per pause the collection
        // was booked as (`CRATONVM_ZGC_PAUSE_PHASES`), in time order, all
        // still before the cycle's record (the stable partition below).
        if let Some(sp) = pause_split {
            b.split_pause_records(&mut recs, sp);
        }
        b.order_notifications(&mut recs);
        // zs s3/ob2: every record carries the ZGC `Metaspace` pool too, as
        // HotSpot 25's `GcInfo` carries every pool (the pause records' entry
        // is zeroed just below). A no-op unless the pool was handed out.
        let ms_before = b.take_metaspace_before();
        let ms_after = if b.metaspace_samples_wanted() {
            zgc_metaspace_edge_sample(shared, true)
        } else {
            None
        };
        b.append_metaspace_usage(&mut recs, ms_before, ms_after);
        // zt t6/ob4 (`zgc-zt1ob-non-heap-pool-residuals` item 3): the code
        // heaps at both edges on the cycle records, as HotSpot samples every
        // pool at `gc_begin` / `gc_end` (they were rendered at delivery).
        let code_before = b.take_code_heap_before();
        if b.code_heap_edges_wanted() {
            let (code_after, cap) = crate::vm::vm_exec::jmx_code_cache_bytes();
            b.append_code_heap_usage(&mut recs, code_before, Some(code_after), cap);
        }
        b.unrecord_pause_usage(&mut recs);
        // zs s6/ob5: the edge samples are the pool's peaks too, as HotSpot
        // records every pool's peak at `gc_begin` / `gc_end`
        // (`CRATONVM_ZGC_METASPACE_PEAK_AT_GC`).
        b.note_metaspace_peaks(ms_before, ms_after);
        ms_after_used = ms_after
            .or(ms_before)
            .filter(|_| b.metaspace_threshold_enabled())
            .map(|u| b.metaspace_pool_usage(u));
        // zs s6/ob5: one class-store length read (no walk), only while the
        // pool is not handed out (`CRATONVM_ZGC_METASPACE_FIRST_CYCLES`).
        if b.metaspace_census_wanted() {
            ms_census = crate::vm::vm_init::class_store_cut(shared).map(|cut| (b, cut));
        }
    }
    // gen r5w3/obs7: HotSpot's `LowMemoryDetector` at `gc_end` — every pool
    // against its usage threshold, and the pools the collection's bean(s)
    // manage against their collection-usage thresholds (Serial: `Copy`'s
    // eden + survivor, `MarkSweepCompact`'s all three). One load unless a
    // program set a threshold; the requests are delivered by
    // `run_gc_notifications`.
    let thresholds = queue.pool_thresholds();
    if thresholds.is_armed() {
        let collected: Vec<&'static str> = recs
            .iter()
            .filter_map(|r| after.collectors.iter().find(|c| c.name == r.collector))
            .flat_map(|c| c.pools.iter().copied())
            // Wave 9 lane G / gci i5g/g1o: a G1 young-only pause checks eden
            // and survivor only (HotSpot 25 runs `detect_after_gc_memory` for
            // a pool the pause affected).
            .filter(|p| match affected {
                Some(only) => only.contains(p),
                None => true,
            })
            .collect();
        // zs s6/ob5: and the `Metaspace` pool's usage threshold
        // (`CRATONVM_ZGC_METASPACE_THRESHOLD`), checked at `gc_end` as
        // HotSpot's `MemoryService::track_memory_usage` checks every pool.
        match ms_after_used {
            Some(ms) => {
                let mut pools = after.pools.clone();
                pools.push(ms);
                thresholds.check(&pools, &collected);
            }
            None => thresholds.check(&after.pools, &collected),
        }
        // gci i16/nt: and the non-heap pools' usage thresholds (class
        // unloading can clear `Metaspace`'s gauge; the code heaps may have
        // crossed since the last check). Outside the pause, as the
        // `gc,metaspace` line's walk; the walk is on a cadence.
        crate::vm::vm_exec::jmx_check_non_heap_thresholds(
            shared,
            crate::vm::vm_exec::NonHeapThresholdCheck::CollectionEnd,
        );
    }
    // zt t13/x10 (`zgc-zt12x9-*` item 5,
    // `CRATONVM_ZGC_PERF_REFRESH_BEFORE_ENQUEUE`): the in-process perf
    // counters show this collection BEFORE a listener can run, as HotSpot
    // updates them at the pause; the beans are final here (`after`).
    if recs.iter().any(|r| queue.is_enabled(r.collector)) {
        zgc_perf_refresh_before_enqueue(shared, &recs);
    }
    match ms_census {
        // zs s6/ob5: queue first, then note the census, so a back-fill that
        // runs in between finds every record its entries name.
        Some((b, cut)) => {
            for rec in &recs {
                queue.record(rec.clone());
            }
            b.note_metaspace_deferred(&recs, Some(cut), Some(cut));
        }
        None => {
            for rec in recs {
                queue.record(rec);
            }
        }
    }
}

/// `CRATONVM_ZGC_PERF_REFRESH_BEFORE_ENQUEUE` (zt t13/x10,
/// `zgc-zt12x9-wave-t11-review-residuals-SUPERSEDED-20261019.md` item 5; default ON,
/// `=0` kill). zt t11/pl3's pause refresh
/// (`perf_data_file::refresh_after_pause`, `CRATONVM_ZGC_PERF_PAUSE_REFRESH`)
/// runs in `run_collection_pause` after `gc_event_finish_by` has queued this
/// collection's GC notifications, so a delivery thread an earlier hint had
/// already woken could run a listener that read the counters one collection
/// behind. With the switch the same refresh also runs in
/// [`record_gc_notification`], after the beans' final update and before the
/// first record is queued -- only when a record is about to be queued for a
/// listener, so a VM without GC listeners pays nothing more. The later refresh
/// stays (every pause, listener or not). Every collector since g1gen/c8, as
/// the refresh itself;
/// `=0`: the late refresh alone, as before. zt t14/x11 (review of t13): the
/// early refresh IS `refresh_after_pause`, so it is gated by
/// `CRATONVM_ZGC_PERF_PAUSE_REFRESH` (and `CRATONVM_PERF_EVENT_REFRESH`) as
/// well: with the pause refresh killed, neither runs.
///
/// zt t14/pl5: `recs` (this collection's records, not yet each bean's last)
/// give the pause collectors' `lastEntryTime` / `lastExitTime` and
/// `sun.gc.lastCause` this refresh writes, so a listener does not read the
/// previous collection's.
fn zgc_perf_refresh_before_enqueue(
    shared: &SharedVm,
    recs: &[cratonvm_gc::gc_metrics::GcNotificationRecord],
) {
    if zgc_perf_refresh_before_enqueue_applies(true) {
        crate::runtime::perf_data_file::refresh_after_pause_with(shared, recs);
    }
}

/// zt t13/x10: [`zgc_perf_refresh_before_enqueue`]'s gate (every collector
/// since g1gen/c8, as the pause refresh it repeats). Pure.
fn zgc_perf_refresh_before_enqueue_applies(enabled: bool) -> bool {
    enabled
}

/// Wave 9 lane G (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
/// the pools a G1 collection affected, from its old-collecting pause count on
/// both edges: eden and survivor when the count did not move (a young-only
/// pause; HotSpot 25's young manager lists `G1 Old Gen` as not always
/// affected), `None` (every pool) when it moved or off G1. Pure.
fn g1_young_only_pools(before: Option<u64>, after: Option<u64>) -> Option<&'static [&'static str]> {
    match (before, after) {
        (Some(b), Some(a)) if a == b => Some(cratonvm_gc::gc_metrics::G1_YOUNG_ONLY_POOLS),
        _ => None,
    }
}

#[cfg(test)]
mod w9g_young_only_pools_tests;

/// Whether GC notifications are queued for delivery (gc-common w4-a,
/// `handoff-w3d-gc-notifications-on-the-delivery-thread` edit 3): with the
/// reference-delivery thread on, a collection that queued ONLY notifications
/// must still wake (or start) that thread. One relaxed-ish load; `false` on a
/// heap that describes no GC beans.
pub(super) fn gc_notifications_pending(shared: &SharedVm) -> bool {
    gc_notification_queue(shared).is_some_and(|q| q.has_pending())
}

/// Deliver the heap's queued GC notifications (gen r4w3/obs2): HotSpot's
/// Notification Thread, run by the thread that just collected, after its
/// finalizer and Cleaner drains and for the same reason they run there --
/// the collecting thread is at a point where Java may run. With
/// `CRATONVM_FINALIZER_THREAD=1` the doors do not call this: the
/// reference-delivery thread does, at the end of every batch (gc-common w4-a,
/// `gc_notifications_go_to_delivery_thread` in `gc_and_alloc.rs`). One drainer at a time (a listener that
/// allocates can collect and reach this function again; the nested call
/// returns and the outer loop delivers what it queued).
///
/// `forced` follows the finalizer drain's rule: the allocation doors defer
/// while a JIT helper holds the thread borrow (counted, delivered at the next
/// drain point), and `System.gc()` -- a native call with a legitimate
/// `&mut JvmThread`, whose deferral would be forever for a caller in compiled
/// code -- re-enters under a nested JIT scope instead, exactly like
/// `run_finalizers_forced` / `run_cleaner_actions_forced`.
///
/// Costs one load when nothing is queued, which is every collection of a VM
/// where no `NotificationListener` was ever registered on a GC bean.
pub(super) fn run_gc_notifications(shared: &SharedVm, thread: &mut JvmThread, forced: bool) {
    let Some(queue) = gc_notification_queue(shared) else {
        return;
    };
    if !queue.has_pending() {
        return;
    }
    #[cfg(feature = "management")]
    {
        let jit_borrowed = crate::jit::helpers::is_jit_thread_set();
        if jit_borrowed && !forced {
            queue.note_deferred();
            return;
        }
        // zt t13/x10 (`CRATONVM_ZGC_NOTIFICATION_MISSED_REDRAIN`): on ZGC a
        // drain point refused by another thread's drain leaves a mark that
        // drainer reads after it lets go ([`zgc_notification_missed_redrain`]).
        let missed_rule = zgc_notification_missed_redrain(shared);
        let drain = if missed_rule {
            queue.begin_drain_noting_miss()
        } else {
            queue.begin_drain()
        };
        let Some(drain) = drain else {
            return;
        };
        // zt t15/x12 (`CRATONVM_ZGC_FORCED_NOTIFICATION_DRAIN_BOUNDED`): a
        // forced drain point delivers what was queued when it began; what
        // other threads queue meanwhile goes to the delivery thread
        // ([`zgc_forced_notification_drain_bounded`]).
        let mut budget = zgc_forced_notification_drain_bounded(shared, forced)
            .then(|| queue.pending_notifications());
        let mut bounded_out = false;
        // RAII so an unwinding listener cannot leave the outer JIT level
        // un-restored. Same shape as `run_cleaner_actions_forced`.
        struct NestedJitScope(Option<crate::jit::helpers::JitThreadScope>);
        impl Drop for NestedJitScope {
            fn drop(&mut self) {
                if let Some(scope) = self.0.take() {
                    crate::jit::helpers::restore_jit_thread(scope);
                }
            }
        }
        let _scope = NestedJitScope(if jit_borrowed {
            Some(crate::jit::helpers::set_jit_thread(thread))
        } else {
            None
        });
        // gen r5w3/obs7: GC notifications first, then the pool-threshold
        // sensor requests (`PoolThresholds`), and round again while either
        // grew — a listener that allocates can collect, and the nested drain
        // point returns at once (one drainer), leaving its work to this loop.
        // Bounded: a sensor listener that collects on every call (a counter
        // sensor re-triggers on every collection above its threshold) must
        // not keep this thread here; what is left stays pending for the next
        // drain point.
        const MAX_SENSOR_ROUNDS: u32 = 8;
        let mut sensor_rounds = 0u32;
        // zt t13/x10: a refused build ends this drain point for good (no
        // re-drain after a miss either: the heap is still short).
        let mut refused_any = false;
        loop {
            // gci i47w/oe47: a delivery the heap refused to build was put back
            // at the head of the queue; this drain point stops there.
            let mut refused = false;
            // zs s6/ob5: a record about to be rendered by a `GcInfoBuilder`
            // (which names every pool, `Metaspace` included) gets the
            // `Metaspace` key a collection before the pool's publication
            // could not record. One load when nothing waits.
            // zs s9/ob6: and when the class lock is busy, the cycle records
            // still waiting for that key are held back (with everything
            // behind them) for a later round or drain point rather than
            // delivered with an undefined `Metaspace` usage
            // (`CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL`).
            let hold = zgc_metaspace_backfill_for_drain(shared);
            let beans = shared.mem.backend_gc_beans();
            while let Some(rec) = if budget == Some(0) {
                bounded_out = true;
                None
            } else {
                match beans.filter(|_| hold) {
                    Some(b) => drain.pop_unless(|r| b.metaspace_record_waiting(r)),
                    None => drain.pop(),
                }
            } {
                if let Some(left) = budget.as_mut() {
                    *left = left.saturating_sub(1);
                }
                let info = crate::vm::vm_exec::gc_notification_info_of(&rec);
                // gci i16/oo: a heap that cannot hold the `GcInfo` fails this
                // delivery with an `OutOfMemoryError` instead of aborting the
                // process (`deliver_catching_native_alloc_oom`).
                let deliver = |t: &mut JvmThread| {
                    super::gc_and_alloc::deliver_catching_native_alloc_oom(t, |t| {
                        let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                        cratonvm_native_builtins_jmx::jmx::deliver_gc_notification(&mut ctx, &info)
                    })
                };
                // zwr: the previous notification's dead stack, zeroed on the
                // delivery thread (`scrub_delivery_dead_stack`).
                super::gc_and_alloc::scrub_delivery_dead_stack();
                let mut delivered = deliver(&mut *thread);
                // gci i48a/oe47c: HotSpot's Notification Thread allocates like
                // any Java thread, so a refused allocation there COLLECTS and
                // retries. The build's native doors cannot collect, but here
                // the native has unwound (nothing it held survives) and Java
                // may run, so the drainer runs the native ladder itself and
                // builds once more. Without it a refusal met by the delivery
                // thread just after the mutator's own recovery collection
                // waited for a collection that never came (i48a row 1: 116 of
                // 148 notifications still queued at exit).
                if let Err(err) = &delivered {
                    if gc_notification_build_refused(shared, err)
                        && gc_notification_reclaim_before_retry(shared, &mut *thread)
                    {
                        delivered = deliver(&mut *thread);
                    }
                }
                // gci i47w/oe47 (`gci-i47w-ob47-g1-oome-escapes-the-probes-catch-FIXED-20261016`):
                // a construction the heap REFUSED (the `GcInfo`, its
                // `CompositeData`, a reference array) is not this
                // notification's fault: HotSpot builds it on the Notification
                // Thread after the collection freed space. Keep it queued, at
                // the head, for the next drain point (after a further
                // collection), bounded by `GC_NOTIFICATION_REFUSED_RETRIES`.
                // `GciGo11G1NamesProbe oome` on G1 lost the `G1 Compaction
                // Pause` notification this way: drained while the probe's list
                // still filled the heap.
                if let Err(err) = &delivered {
                    if gc_notification_build_refused(shared, err) && drain.requeue_refused(rec) {
                        refused = true;
                        break;
                    }
                }
                let outcome =
                    match delivered {
                        Ok(true) => cratonvm_gc::gc_metrics::GcNotificationOutcome::Delivered,
                        Ok(false) => cratonvm_gc::gc_metrics::GcNotificationOutcome::Orphaned,
                        Err(err) => {
                            // gch h2/ob: say what failed (`--verbose:gc`, the
                            // first few only), before the census counts it.
                            note_gc_notification_failure(shared, queue, &info, &err);
                            cratonvm_gc::gc_metrics::GcNotificationOutcome::Failed
                        }
                    };
                drain.note_outcome(outcome);
            }
            if refused {
                // The heap is still short: what is left (the refused record,
                // anything behind it, any sensor request) waits for the next
                // drain point.
                refused_any = true;
                break;
            }
            if sensor_rounds == MAX_SENSOR_ROUNDS {
                break;
            }
            sensor_rounds += 1;
            let requests = queue.pool_thresholds().take_requests();
            if requests.is_empty() {
                break;
            }
            for req in requests {
                // `Sensor.trigger(count, usage)` / `Sensor.clear(count)` on
                // the pool bean `MemoryImpl` handed out; its Java side sends
                // the `MemoryNotificationInfo`. A throwing listener costs
                // this request only, as on HotSpot's Service Thread.
                // gci i16/oo: so does a heap that cannot hold the
                // `MemoryUsage` (`deliver_catching_native_alloc_oom`).
                let _ = super::gc_and_alloc::deliver_catching_native_alloc_oom(
                    &mut *thread,
                    |t| {
                        let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                        cratonvm_native_builtins_jmx::jmx::deliver_pool_sensor(
                            &mut ctx,
                            req.pool,
                            req.kind == cratonvm_gc::gc_metrics::PoolThresholdKind::Collection,
                            i32::try_from(req.count).unwrap_or(i32::MAX),
                            req.trigger_usage,
                        )
                    },
                );
            }
        }
        // zt t13/x10: let go, then drain again when a drain point on another
        // thread (or a listener's nested one) was refused meanwhile and
        // something is still queued: its records were queued after this
        // drain's last pop, and nothing else would deliver them before the
        // next drain point. Bounded ([`run_gc_notifications_after_miss`]).
        // zt t14/x11 (review of t13): the nested JIT scope this drain set is
        // restored BEFORE the re-drain, which sets its own; before, the
        // re-drain ran inside it and nested a second scope on the same
        // thread for no reason.
        let redrain = if missed_rule && !refused_any {
            queue.end_drain_take_missed(drain) && queue.has_pending()
        } else {
            drop(drain);
            false
        };
        drop(_scope);
        // zt t15/x12: a bounded drain point that left records hands them to
        // the delivery thread (after letting go, so its drain is not
        // refused) instead of re-draining them here; with no delivery thread
        // to take them, the re-drain above stands.
        if bounded_out
            && !refused_any
            && queue.has_pending()
            && super::gc_and_alloc::forced_drain_hands_rest_to_delivery_thread(shared, thread)
        {
            return;
        }
        if redrain {
            run_gc_notifications_after_miss(shared, thread, forced);
        }
    }
    #[cfg(not(feature = "management"))]
    {
        // No JMX beans to deliver to without the management natives, so no
        // bean can have been enabled and nothing was queued.
        let _ = (thread, forced);
    }
}

/// `CRATONVM_ZGC_NOTIFICATION_MISSED_REDRAIN` (zt t13/x10, the t12/gn4 review;
/// default ON, `=0` kill). One drainer at a time: a drain point that finds
/// another drain in progress returns, trusting that drain's loop to deliver
/// what it would have. That holds only while the loop still pops. A record
/// queued after the drainer's last pop -- a lock-free `--compatible` door
/// that collected while the delivery thread was finishing the batch a
/// t12/gn4 hand-off (`CRATONVM_ZGC_UNDELIVERABLE_NOTIFICATIONS_OFF_THREAD`)
/// started, or `System.gc()`'s forced drain at that moment -- then waited for
/// the next drain point, and a program that sleeps after it never got the
/// notification (HotSpot's Notification Thread delivers every one). With
/// the switch a refused drain point leaves a mark
/// (`GcNotificationQueue::begin_drain_noting_miss`), and the drainer,
/// having let go, drains again when the mark is set and anything is
/// pending. ZGC only: G1 and Generational keep the plain door. `=0`: the
/// next drain point, as before.
#[cfg(feature = "management")]
fn zgc_notification_missed_redrain(shared: &SharedVm) -> bool {
    shared.mem.heap.is_zgc()
}

/// `CRATONVM_ZGC_FORCED_NOTIFICATION_DRAIN_BOUNDED` (zt t15/x12, the x10
/// review page's missed-drain item; default ON, `=0` kill). A forced drain
/// point (`System.gc()`, the JVMTI and VM-internal full doors) on a
/// `--compatible` thread that holds no lock delivers inline, and its loop
/// popped until the queue was EMPTY: while another thread kept collecting
/// (each ZGC collection queues a pause and a cycle record), that could be
/// never, and `System.gc()` did not return while the peer's production
/// outran the listeners (`Zt13X10MissedDrain`: the main thread is the
/// one that stops the churn, after `System.gc()` returns). HotSpot's
/// `System.gc()` delivers nothing itself: its Notification Thread does,
/// after the call. With the switch the forced drain delivers at most the
/// records queued when it began (its own collection's included, so the
/// caller still sees them delivered before it returns, as before) and hands
/// whatever is left to the delivery thread, in order (one queue, one
/// drainer at a time). ZGC only, as the missed re-drain; `=0`: drain to
/// empty, as before. Non-forced drain points (the allocation doors, the
/// delivery thread itself) are not bounded.
#[cfg(feature = "management")]
fn zgc_forced_notification_drain_bounded(shared: &SharedVm, forced: bool) -> bool {
    forced
        && shared.mem.heap.is_zgc()
}

/// zt t13/x10: the most re-drains one drain point chains
/// ([`run_gc_notifications_after_miss`]); what a longer chain leaves waits
/// for the next drain point, as every record did before.
#[cfg(feature = "management")]
const GC_NOTIFICATION_MISSED_REDRAINS: u32 = 4;

#[cfg(feature = "management")]
thread_local! {
    /// zt t13/x10: how deep this thread is in re-drains
    /// ([`run_gc_notifications_after_miss`]). Per thread, transient.
    static GC_NOTIFICATION_REDRAIN_DEPTH: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
}

/// zt t13/x10: [`run_gc_notifications`] again after a missed drain point,
/// at most [`GC_NOTIFICATION_MISSED_REDRAINS`] deep on this thread (a
/// mutator that keeps collecting while this one delivers must not keep it
/// here forever). The depth is put back on unwind.
#[cfg(feature = "management")]
fn run_gc_notifications_after_miss(shared: &SharedVm, thread: &mut JvmThread, forced: bool) {
    struct Depth(u32);
    impl Drop for Depth {
        fn drop(&mut self) {
            GC_NOTIFICATION_REDRAIN_DEPTH.with(|d| d.set(self.0));
        }
    }
    let depth = GC_NOTIFICATION_REDRAIN_DEPTH.with(std::cell::Cell::get);
    if !gc_notification_redrain_allowed(depth) {
        return;
    }
    let _restore = Depth(depth);
    GC_NOTIFICATION_REDRAIN_DEPTH.with(|d| d.set(depth + 1));
    run_gc_notifications(shared, thread, forced);
}

/// zt t13/x10: the bound of [`run_gc_notifications_after_miss`]. Pure.
#[cfg(feature = "management")]
fn gc_notification_redrain_allowed(depth: u32) -> bool {
    depth < GC_NOTIFICATION_MISSED_REDRAINS
}

/// zs s9/ob6: the drain's `Metaspace` back-fill
/// (`vm_init.rs::zgc_metaspace_backfill`, non-blocking). Returns `true` when
/// census entries still wait because the class lock stayed busy -- after a
/// few yields, since a define holds it briefly -- so the caller must hold
/// the keyless cycle records back. `false` under
/// `CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL=0` (the s6/ob5 behaviour:
/// deliver them keyless). One load when nothing waits.
///
/// zt t6/rv3 (`zgc-zs10rv7-wave-s9-review-residuals-CLOSED-20261019.md` item 5):
/// once the retries failed, later drain points keep the hold after their one
/// non-blocking try, without the yields, until a class is defined or a
/// collection ends (the stamp moves): a long define held the lock while every
/// drain point paid 16 yields and 16 `try_read`s. `=0` kill
/// (`CRATONVM_ZGC_METASPACE_BACKFILL_HOLD_MEMO`): the retries at every point.
#[cfg(feature = "management")]
fn zgc_metaspace_backfill_for_drain(shared: &SharedVm) -> bool {
    const BUSY_LOCK_RETRIES: u32 = 16;
    // Nothing waits (every G1 run, and ZGC once the pool is published): one
    // load, as before. A hold left behind is harmless: new census entries
    // come only with a collection, which moves the stamp.
    if !crate::vm::vm_init::zgc_metaspace_backfill(shared, false) {
        return false;
    }
    let beans = shared.mem.backend_gc_beans();
    if !crate::vm::vm_init::zgc_metaspace_busy_lock_backfill_enabled() {
        return false;
    }
    let stamp = (
        cratonvm_classloading::store_epochs(shared.jit.class_layout_domain)
            .class_definition_epoch(),
        shared.mem.heap.collection_count(),
    );
    let memo =
        true;
    if memo && beans.is_some_and(|b| b.metaspace_backfill_held_at(stamp.0, stamp.1)) {
        return true;
    }
    if let Some(b) = beans {
        b.note_metaspace_backfill_retry_pass();
    }
    for _ in 0..BUSY_LOCK_RETRIES {
        std::thread::yield_now();
        if !crate::vm::vm_init::zgc_metaspace_backfill(shared, false) {
            if let Some(b) = beans {
                b.set_metaspace_backfill_hold(None);
            }
            return false;
        }
    }
    if let Some(b) = beans {
        b.set_metaspace_backfill_hold(Some(stamp));
    }
    true
}

/// gci i47w/oe47: did a GC notification's delivery fail because the HEAP
/// refused an allocation (its `GcInfo`, `CompositeData`, a reference array)?
/// A heap `OutOfMemoryError` the Java side threw, or the VM's own
/// `OutOfMemoryError: Java heap space ...` from a native door
/// (`deliver_catching_native_alloc_oom`). Anything else -- a listener's own
/// exception, the array-length refusal -- is a real failure, counted as
/// before. A thrown object is only read.
#[cfg(feature = "management")]
fn gc_notification_build_refused(
    shared: &SharedVm,
    err: &cratonvm_types::error::MethodCallFailed,
) -> bool {
    match err {
        cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::OutOfMemoryError { message },
            ),
        ) => heap_refusal_message(Some(message)),
        cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
            let class_id = shared.mem.heap.class_id_of(*obj);
            let is_oome = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .is_some_and(|c| &*c.name == "java/lang/OutOfMemoryError")
            };
            if !is_oome {
                return false;
            }
            let message = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
                shared,
                class_id,
                "detailMessage",
            )
            .and_then(|slot| match shared.mem.heap.get_field(*obj, slot) {
                crate::types::Value::Object(Some(s)) => {
                    crate::vm::read_java_string(&shared.mem.heap, s)
                }
                _ => None,
            });
            heap_refusal_message(message.as_deref())
        }
        _ => false,
    }
}

/// gci i48a/oe47c: the drainer's collection before it builds a refused
/// notification once more -- the native factories' ladder
/// (`NativeContextImpl::reclaim_before_alloc_retry`: young, then the full
/// rung; `false` when the overhead verdict says a retry is not worth it).
/// Called only after the delivery unwound to the drain loop, which holds no
/// heap reference across it (the record is plain Rust data), at a point
/// where the drain already runs Java that may collect.
#[cfg(feature = "management")]
fn gc_notification_reclaim_before_retry(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    use cratonvm_native_api::NativeHeapAccess as _;
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    ctx.reclaim_before_alloc_retry()
}

/// gci i47w/oe47: is an `OutOfMemoryError` with this detail message a heap
/// refusal? `Java heap space` (with or without the VM's parenthesised detail),
/// or no message at all (the preallocated singleton). Not the array-length
/// refusal (`Requested array size exceeds VM limit`), which no collection
/// cures. Pure.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
fn heap_refusal_message(message: Option<&str>) -> bool {
    match message {
        None => true,
        Some(m) => m.is_empty() || m.starts_with("Java heap space"),
    }
}

#[cfg(test)]
mod gci_i47w_oe47_notification_refusal_tests;

/// gch h2/ob: how many delivery failures [`note_gc_notification_failure`]
/// describes per VM (the census counts every one).
#[cfg(feature = "management")]
const GC_NOTIFICATION_FAILURE_LINES: u64 = 4;

/// gch h2/ob (`gch-h2ob-g1-gc-notifications-fail-after-the-first-round`):
/// under `--verbose:gc`, the first [`GC_NOTIFICATION_FAILURE_LINES`] failed
/// deliveries each print one `[GC] gc_notification_failed:` line naming the
/// bean, the action and what failed -- the thrown class, or the VM's internal
/// error. `notif_failed=` alone could not say why vh1-ob `ob1_notif_g1` lost
/// 12 of 16 deliveries. Called before the drain counts the failure, so `n`
/// is this failure's ordinal. A thrown object is only read (its class id and,
/// gch h3/os, its `detailMessage`), before anything on this thread can
/// allocate. Off `--verbose:gc`: one load.
#[cfg(feature = "management")]
fn note_gc_notification_failure(
    shared: &SharedVm,
    queue: &cratonvm_gc::gc_metrics::GcNotificationQueue,
    info: &cratonvm_native_api::GcNotificationInfo,
    err: &cratonvm_types::error::MethodCallFailed,
) {
    if !shared
        .mem
        .verbose_gc
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let n = queue.census().failed.saturating_add(1);
    if n > GC_NOTIFICATION_FAILURE_LINES {
        return;
    }
    let error = match err {
        cratonvm_types::error::MethodCallFailed::InternalError(e) => format!("internal: {e}"),
        cratonvm_types::error::MethodCallFailed::ExceptionThrown(obj) => {
            let class_id = shared.mem.heap.class_id_of(*obj);
            let class = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id)
                    .map(|c| c.name.replace('/', "."))
                    .unwrap_or_else(|| "<unknown class>".to_string())
            };
            // gch h3/os: and its message (`Throwable.detailMessage`, found up
            // the hierarchy; the manager's lock is released above, the slot
            // lookup takes it again). A `NoClassDefFoundError` names the
            // class, which is what the G1 page needs. Read only: nothing on
            // this thread allocates in Java between the throw and here.
            let message = crate::vm::vm_exec::resolve_field_slot_by_name_cached(
                shared,
                class_id,
                "detailMessage",
            )
            .and_then(|slot| match shared.mem.heap.get_field(*obj, slot) {
                crate::types::Value::Object(Some(s)) => {
                    crate::vm::read_java_string(&shared.mem.heap, s)
                }
                _ => None,
            });
            thrown_error_text(&class, message.as_deref())
        }
    };
    eprintln!(
        "{}",
        render_gc_notification_failure(n, info.gc_name, info.gc_action, info.gc_cause, &error)
    );
}

/// gch h3/os: the `error=` value of a delivery that threw: the class, then
/// `:` and its message when it has one, the message on one line and cut at
/// [`GC_NOTIFICATION_FAILURE_MESSAGE_CHARS`] characters. Pure.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
fn thrown_error_text(class: &str, message: Option<&str>) -> String {
    match message {
        Some(m) if !m.is_empty() => {
            let one_line: String = m
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(GC_NOTIFICATION_FAILURE_MESSAGE_CHARS)
                .collect();
            format!("{class}:{one_line}")
        }
        _ => class.to_string(),
    }
}

/// gch h3/os: the longest message [`thrown_error_text`] keeps.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
const GC_NOTIFICATION_FAILURE_MESSAGE_CHARS: usize = 160;

/// The `[GC] gc_notification_failed:` line of [`note_gc_notification_failure`]:
/// `key=value` pairs, each value with its spaces replaced by `_` so a scrape
/// splitting on spaces keeps every value whole. Pure.
#[cfg_attr(not(feature = "management"), allow(dead_code))]
fn render_gc_notification_failure(
    n: u64,
    collector: &str,
    action: &str,
    cause: &str,
    error: &str,
) -> String {
    let token = |s: &str| s.replace(' ', "_");
    format!(
        "[GC] gc_notification_failed: n={n} collector={} action={} cause={} error={}",
        token(collector),
        token(action),
        token(cause),
        token(error),
    )
}

/// The heap occupancy a GC event reports (`-Xlog:gc`'s `before->after`, JFR's
/// `jdk.GCHeapSummary.heapUsed`): bytes in objects, NOT the bump high-water.
///
/// gen r4w2/obs (2026-09-23). This was `VmHeap::allocated_bytes`, which on the
/// generational backend is `young_from.used() + old.used()` -- and `used()` is
/// the arena's bump cursor. A non-moving young sweep (the path every JIT-warm
/// workload takes) reclaims onto the from-space FREE LIST and never retracts
/// the cursor, so `allocated_bytes` is identical before and after it by
/// construction: `-XX:+UseGenerationalGC -Xlog:gc BinT 16` printed
/// `Pause Young (Allocation Failure) 8M->8M(64M)` on every pause of a run that
/// was reclaiming the whole time, while `--verbose:gc` on the same run showed
/// the reclamation in `young_free=a->b`. [`VmHeap::live_bytes_estimate`] is
/// `used - free_list` for young plus old used -- the figure that line's
/// `young_free` pair implies -- and it is what the GC-overhead productivity
/// metric already uses for the same reason. G1 and ZGC answer
/// `allocated_bytes` from it unchanged (their occupancy already retreats).
pub(super) fn gc_event_heap_used(heap: &crate::memory::vm_heap::VmHeap) -> usize {
    heap.live_bytes_estimate()
}

fn wall_clock_ns() -> u64 {
    // Truncation-checked: nanos since the epoch fit a u64 until ~2554.
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

/// The per-backend names a collection is reported under:
/// `(jfr_name, xlog_label, reclaimed_old_generation)`.
///
/// JFR names follow HotSpot's `GCName` for the nearest collector shape: the
/// generational backend is a copying young generation over a mark-sweep tenured
/// one, i.e. Serial's `DefNew` / `SerialOld`; G1's pause is `G1New`; ZGC is
/// HotSpot 25's `ZGC Major` / `ZGC Minor` (gcd d10/o retirement: it was the
/// non-generational `Z`, with the JDK 21 `-Xlog:gc` label `Garbage
/// Collection`; HotSpot 25 labels the same collection `Major Collection`).
/// `reclaimed_old_generation` is only known on the generational backend,
/// where the collecting thread (this one) carries it in
/// `gc_quiescence::old_gen_reclaimed_last_cycle`.
///
/// gci i11/go (`gci-i10rh-...-review-findings-20261013` finding 4): G1's
/// label is HotSpot 25 G1's `Pause Young (Normal)` / `Pause Young (Mixed)`
/// (`g1_type`, [`GcEventStart::g1_pause_type`]); it was `Pause Young` for
/// every G1 pause. `None` on G1 (an unsealed start without the count) keeps
/// `Normal`.
fn gc_event_shape(
    shared: &SharedVm,
    kind: cratonvm_gc::gc_metrics::CollectionKind,
    g1_type: Option<G1YoungPauseType>,
) -> (&'static str, &'static str, bool) {
    use cratonvm_gc::gc_metrics::CollectionKind;
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => {
            if cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() {
                ("SerialOld", "Pause Full", true)
            } else {
                ("DefNew", "Pause Young", false)
            }
        }
        _ if shared.mem.heap.is_g1() => (
            "G1New",
            g1_type.unwrap_or(G1YoungPauseType::Normal).xlog_label(),
            false,
        ),
        #[allow(unreachable_patterns)]
        _ => match kind {
            CollectionKind::Minor => ("ZGC Minor", ZGC_MINOR_COLLECTION, false),
            CollectionKind::Major => ("ZGC Major", ZGC_MAJOR_COLLECTION, false),
        },
    }
}

/// HotSpot 25 ZGC's `-Xlog:gc` label of a whole-heap collection.
const ZGC_MAJOR_COLLECTION: &str = "Major Collection";
/// HotSpot 25 ZGC's `-Xlog:gc` label of a young collection.
const ZGC_MINOR_COLLECTION: &str = "Minor Collection";

/// [`GcEventStart::kind`], pure: the young-only cycle count moved.
fn collection_kind(before: usize, after: usize) -> cratonvm_gc::gc_metrics::CollectionKind {
    if after > before {
        cratonvm_gc::gc_metrics::CollectionKind::Minor
    } else {
        cratonvm_gc::gc_metrics::CollectionKind::Major
    }
}

/// HotSpot 25 ZGC's two `-Xlog:gc` lines of one collection
/// (`ZStatPhaseCollection::register_start` / `register_end`, measured on
/// 25.0.4 under `-XX:+UseZGC -Xmx256m -Xlog:gc`):
///
/// ```text
/// GC(0) Major Collection (System.gc())
/// GC(0) Major Collection (System.gc()) 8M(3%)->2M(1%) 0.021s
/// ```
///
/// `ZSIZE_FMT` is `"%zuM(%.0f%%)"` of the size against the heap's max
/// capacity, the duration `%.3fs`. gcd d10/o retirement: this VM printed the
/// JDK 21 non-generational single line
/// `GC(0) Garbage Collection (System.gc()) 4M->0M(16M) 13.188ms`.
pub(super) fn render_xlog_zgc_lines(
    gc_id: u64,
    label: &str,
    cause: &str,
    heap_before: usize,
    heap_after: usize,
    max_capacity: usize,
    duration_ns: u64,
) -> [String; 2] {
    const M: usize = 1024 * 1024;
    let zsize = |bytes: usize| {
        let pct = if max_capacity == 0 {
            0.0
        } else {
            bytes as f64 * 100.0 / max_capacity as f64
        };
        format!("{}M({pct:.0}%)", bytes / M)
    };
    [
        format!("GC({gc_id}) {label} ({cause})"),
        format!(
            "GC({gc_id}) {label} ({cause}) {}->{} {:.3}s",
            zsize(heap_before),
            zsize(heap_after),
            duration_ns as f64 / 1_000_000_000.0,
        ),
    ]
}

/// `CRATONVM_ZGC_XLOG_PAUSE_PHASES` -- default ON (zt t5/jf, R9): a ZGC
/// collection prints its pause lines under `gc,phases` and its metaspace
/// line under `gc,metaspace` between its start and end `gc` lines, as HotSpot
/// 25.0.3 does (local `-Xlog:gc*` capture of `Zt1JfVmOperations`); zt
/// t6/ob4: and the three `gc,heap` capacity lines after the metaspace line.
/// `=0`: none of them (the zr shape). Read per collection, only under those
/// tags.
fn zgc_xlog_pause_phases_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_XLOG_HEAP_STATISTICS` -- default ON, `=0` kill (zt t7/x4b):
/// under [`zgc_xlog_pause_phases_enabled`], a ZGC collection's `gc,heap`
/// block is HotSpot 25.0.3's per generation -- the capacity triple and the
/// `Heap Statistics:` table ([`render_xlog_zgc_heap_block`]), under `Y:`
/// then `O:` on a major and `y:` on a minor. `=0`: t6/ob4's single capacity
/// triple. Read per collection, only under `gc,heap`.
fn zgc_xlog_heap_statistics_enabled() -> bool {
    true
}

/// zt t5/jf (R9): HotSpot 25 ZGC's `gc,phases` pause lines,
/// `GC(3) Y: Pause Mark Start (Major) 0.011ms`: the generation prefix, the
/// pause name, milliseconds to three decimals (`ZStatPhasePause`). A young
/// (`ZGC Minor`) collection's pauses are `y:`; a whole-heap collection's mark
/// start is the young generation's (`Y:`, as HotSpot's
/// `ZMarkStartYoungAndOld`) and its mark end and relocate start the old
/// generation's (`O:`), the same split the JFR VM operations use
/// (`jfr_vm_more::zgc_pause_vm_operation`). Pure.
fn render_xlog_zgc_pause_lines(
    gc_id: u64,
    pauses: &[(&'static str, u64, u64)],
    minor: bool,
) -> Vec<String> {
    pauses
        .iter()
        .map(|&(name, _, dur_ns)| {
            let generation = if minor {
                "y"
            } else if name == ZGC_PAUSE_MARK_START_MAJOR || name == ZGC_PAUSE_MARK_START {
                "Y"
            } else {
                "O"
            };
            let ms = dur_ns as f64 / 1_000_000.0;
            format!("GC({gc_id}) {generation}: {name} {ms:.3}ms")
        })
        .collect()
}

/// zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`): HotSpot 25 ZGC prints a
/// failed Pause Mark End and the Concurrent Mark Continue after it as
/// `gc,phases` lines of the cycle, before the Pause Mark End that succeeded:
/// `GC(n) O: Pause Mark End <ms>` then `GC(n) O: Concurrent Mark Continue
/// <ms>`, once per failed attempt (`attempts` holds `(pause_ns,
/// continue_ns)` in order). Inserted before this collection's own `O: Pause
/// Mark End` line, or appended when it has none. Pure; a no-op for no
/// attempt.
fn splice_xlog_zgc_mark_continue_lines(
    gc_id: u64,
    lines: &mut Vec<String>,
    attempts: &[(u64, u64)],
) {
    if attempts.is_empty() {
        return;
    }
    let own_mark_end = format!("GC({gc_id}) O: {ZGC_PAUSE_MARK_END} ");
    let at = lines
        .iter()
        .position(|l| l.starts_with(&own_mark_end))
        .unwrap_or(lines.len());
    let ms = |ns: u64| ns as f64 / 1_000_000.0;
    let failed: Vec<String> = attempts
        .iter()
        .flat_map(|&(pause_ns, continue_ns)| {
            [
                format!("GC({gc_id}) O: {ZGC_PAUSE_MARK_END} {:.3}ms", ms(pause_ns)),
                format!(
                    "GC({gc_id}) O: Concurrent Mark Continue {:.3}ms",
                    ms(continue_ns)
                ),
            ]
        })
        .collect();
    lines.splice(at..at, failed);
}

/// zt t5/jf (R9): HotSpot 25 ZGC's `gc,metaspace` line of a collection,
/// `GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved` (whole
/// mebibytes, truncated), from this VM's `(used, committed, reserved)`
/// metaspace figures; `O:` for a whole-heap collection, `y:` for a young one.
/// Pure. HotSpot prints one per generation collection: zt t9/x6's
/// [`zgc_xlog_collection_lines`] adds a major's `Y:` lines
/// ([`render_xlog_zgc_metaspace_line_as`]) under
/// `CRATONVM_ZGC_XLOG_GENERATION_ORDER`; this single `O:` / `y:` line is
/// the `=0` arm's and a minor's.
fn render_xlog_zgc_metaspace_line(gc_id: u64, minor: bool, triple: (u64, u64, u64)) -> String {
    let generation = if minor { "y" } else { "O" };
    render_xlog_zgc_metaspace_line_as(gc_id, generation, triple)
}

/// [`render_xlog_zgc_metaspace_line`] under an explicit generation prefix
/// (`Y`, `O`, `y`): zt t9/x6, so a major prints HotSpot's `Y:` line too.
/// Pure.
fn render_xlog_zgc_metaspace_line_as(
    gc_id: u64,
    generation: &str,
    (used, committed, reserved): (u64, u64, u64),
) -> String {
    const M: u64 = 1024 * 1024;
    format!(
        "GC({gc_id}) {generation}: Metaspace: {}M used, {}M committed, {}M reserved",
        used / M,
        committed / M,
        reserved / M
    )
}

/// zt t6/ob4 (R9 tail): HotSpot 25 ZGC's three `gc,heap` capacity lines of a
/// collection (`ZStatHeap::print`, `ZSIZE_FMT` = `%zuM(%.0f%%)`, the percent
/// of the max capacity at initialization):
///
/// ```text
/// GC(0) O: Min Capacity: 8M(3%)
/// GC(0) O: Max Capacity: 256M(100%)
/// GC(0) O: Soft Max Capacity: 256M(100%)
/// ```
///
/// `min` and `max` are the `gc,init` block's `Min Capacity` / `Max Capacity`
/// (`_at_initialize`), `soft_max` the soft maximum the collection ran under,
/// clamped to `max` as the `gc,init` line clamps it. The generation prefix
/// follows [`render_xlog_zgc_metaspace_line`]'s (`O:` for a whole-heap
/// collection, `y:` for a young one). Pure.
fn render_xlog_zgc_capacity_lines(
    gc_id: u64,
    minor: bool,
    min: usize,
    max: usize,
    soft_max: usize,
) -> [String; 3] {
    let generation = if minor { "y" } else { "O" };
    render_xlog_zgc_capacity_lines_as(gc_id, generation, min, max, soft_max)
}

/// [`render_xlog_zgc_capacity_lines`] under an explicit generation prefix
/// (`Y`, `O`, `y`): zt t7/x4b, so a major prints HotSpot's `Y:` triple too.
/// Pure.
fn render_xlog_zgc_capacity_lines_as(
    gc_id: u64,
    generation: &str,
    min: usize,
    max: usize,
    soft_max: usize,
) -> [String; 3] {
    const M: usize = 1024 * 1024;
    let zsize = |bytes: usize| {
        let pct = if max == 0 {
            0.0
        } else {
            bytes as f64 * 100.0 / max as f64
        };
        format!("{}M({pct:.0}%)", bytes / M)
    };
    [
        format!("GC({gc_id}) {generation}: Min Capacity: {}", zsize(min)),
        format!("GC({gc_id}) {generation}: Max Capacity: {}", zsize(max)),
        format!(
            "GC({gc_id}) {generation}: Soft Max Capacity: {}",
            zsize(soft_max.min(max))
        ),
    ]
}

/// zt t7/x4b: HotSpot 25.0.3 ZGC's `gc,heap` `Heap Statistics:` table
/// (`ZStatHeap::print`), the four lines after the capacity triple:
///
/// ```text
/// GC(0) Y: Heap Statistics:
/// GC(0) Y:                Mark Start          Mark End        Relocate Start  ...
/// GC(0) Y:  Capacity:      254M (99%)         254M (99%)  ...
/// GC(0) Y:      Free:      254M (99%)         252M (98%)  ...
/// GC(0) Y:      Used:        2M (1%)            4M (2%)   ...
/// ```
///
/// The layout is measured from a local `-Xlog:gc+heap` capture (every line
/// 125 columns after the prefix): a 10-column right-aligned label and a
/// space, then six 18-column cells each followed by a space; a header cell is
/// centred (the odd space on the right), a value cell is `%8zuM (%.0f%%)`
/// left-aligned; the percentages are of `max`.
///
/// `capacity` is the heap's capacity and `used` its occupancy at the six
/// points (`Mark Start`, `Mark End`, `Relocate Start`, `Relocate End`,
/// `High`, `Low`). Pure.
///
/// zt t8/x5: `Free` is `max - used`, as `ZStatHeap::free` computes it (two
/// local 25.0.3 captures, `-Xmx256m`: `Used: 26M` beside `Free: 230M` at a
/// capacity of 254M; `Used: 2M` beside `Free: 254M`), and its `High` / `Low`
/// cells are the free space's own extremes, `max - used Low` / `max - used
/// High` (`Used: High 32M, Low 18M` beside `Free: High 238M, Low 224M`). The
/// t7 text read the 254M as "page-granular" and printed `capacity - used`
/// against the used row's columns.
fn render_xlog_zgc_heap_statistics(
    gc_id: u64,
    generation: &str,
    capacity: usize,
    max: usize,
    used: [usize; 6],
) -> [String; 5] {
    const M: usize = 1024 * 1024;
    const HEADERS: [&str; 6] = [
        "Mark Start",
        "Mark End",
        "Relocate Start",
        "Relocate End",
        "High",
        "Low",
    ];
    let pct = |bytes: usize| {
        if max == 0 {
            0.0
        } else {
            bytes as f64 * 100.0 / max as f64
        }
    };
    let value = |bytes: usize| {
        let cell = format!("{:>8}M ({:.0}%)", bytes / M, pct(bytes));
        format!("{cell:<18} ")
    };
    let header = |text: &str| {
        let left = 18usize.saturating_sub(text.len()) / 2;
        let right = 18usize.saturating_sub(text.len() + left);
        format!("{}{text}{} ", " ".repeat(left), " ".repeat(right))
    };
    let prefix = format!("GC({gc_id}) {generation}:");
    let row = |label: &str, cells: String| format!("{prefix} {label:>10} {cells}");
    // zt t8/x5: HotSpot's free space per column (see the doc): `max - used`
    // at the four phase points, then the free space's own high and low.
    let free = |u: usize| max.saturating_sub(u);
    let free_cells = [
        free(used[0]),
        free(used[1]),
        free(used[2]),
        free(used[3]),
        free(used[5]),
        free(used[4]),
    ];
    [
        format!("{prefix} Heap Statistics:"),
        row("", HEADERS.iter().map(|&h| header(h)).collect()),
        row("Capacity:", used.iter().map(|_| value(capacity)).collect()),
        row("Free:", free_cells.iter().map(|f| value(*f)).collect()),
        row("Used:", used.iter().map(|u| value(*u)).collect()),
    ]
}

/// zt t7/x4b: one generation's `gc,heap` block -- the capacity triple
/// ([`render_xlog_zgc_capacity_lines_as`]) and the `Heap Statistics:` table
/// ([`render_xlog_zgc_heap_statistics`]). `caps` is `[min, max, soft max,
/// capacity]`, `used` the occupancy at the table's six points. Pure.
fn render_xlog_zgc_heap_block(
    gc_id: u64,
    generation: &str,
    caps: [usize; 4],
    used: [usize; 6],
) -> Vec<String> {
    let [min, max, soft_max, capacity] = caps;
    let mut lines = Vec::with_capacity(8);
    lines.extend(render_xlog_zgc_capacity_lines_as(gc_id, generation, min, max, soft_max));
    lines.extend(render_xlog_zgc_heap_statistics(gc_id, generation, capacity, max, used));
    lines
}

/// `CRATONVM_ZGC_XLOG_GENERATION_STATISTICS` -- default ON, `=0` kill (zt
/// t9/x6, `zgc-zt7x4-*` item 6, `zgc-zt8x5-*` item 5): under
/// [`zgc_xlog_heap_statistics_enabled`], each generation block of a ZGC
/// collection's `gc,heap` output carries HotSpot 25.0.3's `Young Generation
/// Statistics:` (9 lines) or `Old Generation Statistics:` (8 lines) table
/// after its `Heap Statistics:` table ([`zgc_xlog_heap_blocks`]); the `O:`
/// block's heap table reads the occupancy after the young half (HotSpot's
/// old collection starts after the young one promoted); and a major whose
/// cause makes HotSpot pre-clean the young generation
/// ([`zgc_major_precleans_young`], `System.gc()` among them) prints its two
/// young blocks (`Promote All`, then `Collect Roots`). `=0`: t7/x4b's one
/// `Y:` and one `O:` block, both over the whole-heap figures. Read per
/// collection, only under `gc,heap`.
fn zgc_xlog_generation_statistics_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_XLOG_GEN_YOUNG_FIGURES` -- default ON, `=0` kill (zt t10/gn2,
/// `zgc-zt10x7-*` item 8): a minor of ZGC's generational mode
/// (`CRATONVM_ZGC_GENERATIONAL=1`) fills its `y:` `Young Generation
/// Statistics:` table with the young generation's figures (the heap's old
/// generation after the minor and what the minor promoted,
/// `VmHeap::zgc_gen_last_young_xlog_figures`), as HotSpot 25's table counts
/// the young generation only. `=0`: t9/x6's whole-heap figures. `None` (no
/// change) on every other heap, where no minor is printed. Read per minor,
/// only under `gc,heap` with the generation tables on.
fn zgc_xlog_young_split_figures(heap: &crate::memory::vm_heap::VmHeap) -> Option<(usize, usize)> {
    if !true {
        return None;
    }
    heap.zgc_gen_last_young_xlog_figures()
}

/// `CRATONVM_ZGC_XLOG_GENERATION_ORDER` -- default ON, `=0` kill (zt t9/x6,
/// `zgc-zt7x4-*` item 6's last residual): a ZGC major prints its `-Xlog`
/// lines generation by generation, as HotSpot 25.0.3's `-Xlog:gc*` does --
/// the `Y:` pause lines, then per young collection a `Y: Metaspace:` line
/// and its `gc,heap` block, then the `O:` pause lines, the `O: Metaspace:`
/// line and the `O:` block. `=0`: every pause line, one `O:` metaspace line,
/// then every heap block (the t7 order). A minor is one generation either
/// way. Read per collection.
fn zgc_xlog_generation_order_enabled() -> bool {
    true
}

/// `CRATONVM_ZGC_XLOG_STALL_PRECLEAN=1` -- opt-in (zt t16/gn6, the
/// `zgc-zt12gn4-*` page's item 12): a ZGC major that began with an
/// allocation failure latched (`VmHeap::zgc_last_collection_after_alloc_failure`)
/// prints the two young blocks of a pre-cleaning major whatever its cause,
/// as HotSpot 25's `should_preclean_young` does when
/// `ZHeap::is_alloc_stalling_for_old()` (JDK 25 `zDriver.cpp`). Opt-in until
/// a probe reaches that state deterministically on both VMs; a failure-door
/// major is `Allocation Stall` and pre-cleans by its cause already. Read per
/// collection, only under `gc,heap`.
fn zgc_xlog_stall_preclean_enabled() -> bool {
    false
}

/// zt t9/x6: does HotSpot 25's `ZDriverMajor::collect_young` run TWO young
/// collections for a major of this cause (`major_full_preclean`, which
/// promotes every young survivor, then `major_full_roots`)? The causes of
/// `should_preclean_young`'s first switch that answer `true`, by their
/// `GCCause::to_string` text. Its other clause (`is_alloc_stalling_for_old`)
/// is the caller's opt-in `stalled_for_old` (zt t16/gn6,
/// `CRATONVM_ZGC_XLOG_STALL_PRECLEAN`). Pure.
fn zgc_major_precleans_young(cause: &str) -> bool {
    matches!(
        cause,
        "Heap Dump Initiated GC"
            | "Heap Inspection Initiated GC"
            | "WhiteBox Initiated Full GC"
            | "WhiteBox Initiated Run to Breakpoint"
            | "Diagnostic Command"
            | "System.gc()"
            | "FullGCAlot"
            | "JvmtiEnv ForceGarbageCollection"
            | "Metadata GC Clear Soft References"
            | "Allocation Stall"
    )
}

/// zt t9/x6: one generation's figures for HotSpot's `Young Generation
/// Statistics:` / `Old Generation Statistics:` table (`ZStatHeap::print`),
/// bytes. The columns are `Mark Start`, `Mark End`, `Relocate Start`,
/// `Relocate End`; HotSpot prints `-` where a figure does not exist yet, so
/// the arrays start at the first column that has one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ZgcXlogGenerationFigures {
    /// The generation's used bytes, all four columns.
    used: [usize; 4],
    /// Live bytes from `Mark End` on.
    live: [usize; 3],
    /// Garbage bytes from `Mark End` on.
    garbage: [usize; 3],
    /// Bytes allocated since mark start, from `Mark End` on.
    allocated: [usize; 3],
    /// Reclaimed bytes from `Relocate Start` on.
    reclaimed: [usize; 2],
    /// Promoted bytes from `Relocate Start` on (young only).
    promoted: [usize; 2],
    /// Compacted bytes at `Relocate End`.
    compacted: usize,
}

/// zt t9/x6: HotSpot 25.0.3's per-generation table, measured from a local
/// `-Xlog:gc*` capture of a `System.gc()` major (`-XX:+UseZGC -Xmx256m`):
/// the title line, a header of four centred column names, then `Used:`,
/// `Live:`, `Garbage:`, `Allocated:`, `Reclaimed:`, `Promoted:` (young only)
/// and `Compacted:`. Same grid as the `Heap Statistics:` table (a 10-column
/// right-aligned label and a space, 18-column cells each followed by a
/// space; every line 96 columns after a `GC(n) Y:` prefix); a missing figure
/// is HotSpot's `ZTABLE_ARGS_NA` (`%9s` of `-`), left-aligned like a value.
/// Percentages are of `max`. Pure.
fn render_xlog_zgc_generation_statistics(
    gc_id: u64,
    generation: &str,
    young: bool,
    max: usize,
    f: &ZgcXlogGenerationFigures,
) -> Vec<String> {
    const M: usize = 1024 * 1024;
    const HEADERS: [&str; 4] = ["Mark Start", "Mark End", "Relocate Start", "Relocate End"];
    let cell = |text: String| format!("{text:<18} ");
    let value = |bytes: usize| {
        let pct = if max == 0 {
            0.0
        } else {
            bytes as f64 * 100.0 / max as f64
        };
        cell(format!("{:>8}M ({pct:.0}%)", bytes / M))
    };
    let na = || cell(format!("{:>9}", "-"));
    let header = |text: &str| {
        let left = 18usize.saturating_sub(text.len()) / 2;
        let right = 18usize.saturating_sub(text.len() + left);
        format!("{}{text}{} ", " ".repeat(left), " ".repeat(right))
    };
    // `missing` leading `-` cells, then the figures.
    let cells = |missing: usize, figures: &[usize]| -> String {
        let mut out = String::new();
        for _ in 0..missing {
            out.push_str(&na());
        }
        for &bytes in figures {
            out.push_str(&value(bytes));
        }
        out
    };
    let prefix = format!("GC({gc_id}) {generation}:");
    let row = |label: &str, body: String| format!("{prefix} {label:>10} {body}");
    let title = if young { "Young" } else { "Old" };
    let mut lines = Vec::with_capacity(9);
    lines.push(format!("{prefix} {title} Generation Statistics:"));
    lines.push(row("", HEADERS.iter().map(|&h| header(h)).collect()));
    lines.push(row("Used:", cells(0, &f.used)));
    lines.push(row("Live:", cells(1, &f.live)));
    lines.push(row("Garbage:", cells(1, &f.garbage)));
    lines.push(row("Allocated:", cells(1, &f.allocated)));
    lines.push(row("Reclaimed:", cells(2, &f.reclaimed)));
    if young {
        lines.push(row("Promoted:", cells(2, &f.promoted)));
    }
    lines.push(row("Compacted:", cells(3, &[f.compacted])));
    lines
}

/// zt t9/x6: the `gc,heap` blocks of one ZGC collection, in print order, each
/// with its generation prefix. `caps` is `[min, max, soft max, capacity]`;
/// `before` / `after` the heap's occupancy around this stop-the-world
/// collection; `gen_tables` is [`zgc_xlog_generation_statistics_enabled`].
///
/// Off (`gen_tables == false`): t7/x4b's blocks -- `y:` for a minor, `Y:`
/// then `O:` for a major, each over the whole-heap points (mark start, mark
/// end and relocate start read `before`, relocate end `after`).
///
/// On, the figures follow HotSpot's split of a major into its young
/// collection(s) and its old one. Nothing allocates inside this pause and the
/// collection reclaims everything it finds dead at once, so:
/// * a major's (first) young collection holds the whole heap at mark start
///   and promotes every survivor (this mode has no young survivors left after
///   a major): `Used` `before` -> 0, `Live` `after` (0 once promoted),
///   `Garbage` `before - after` (0 at relocate end), `Reclaimed` and
///   `Promoted` at relocate end;
/// * a pre-cleaning major's second young collection (`Collect Roots`) finds
///   the young generation empty: every figure 0, heap table at `after`;
/// * the old collection starts after the promotion: heap table and `Used` at
///   `after`, `Live` `after`, no garbage, nothing reclaimed;
/// * a minor's young collection: with `young_split` (zt t10/gn2,
///   [`zgc_xlog_young_split_figures`]: the old generation after it and what
///   it promoted) the table counts the YOUNG generation only, as HotSpot's
///   does -- `Used` young at mark start (`before` less the old generation
///   before the minor) -> the young survivors it kept young, `Live` the
///   young survivors with the promoted bytes (the kept ones at relocate end),
///   `Garbage` / `Reclaimed` the rest, `Promoted` at relocate end. Without it
///   (`None`: `=0`, or no split known) t9/x6's whole-heap figures: `Used`
///   `before` -> `after`, nothing promoted. The `Heap Statistics` table above
///   it is whole-heap either way, as HotSpot's.
///
/// `stalled_for_old` (zt t16/gn6, item 12): the major began with an
/// allocation failure latched -- HotSpot 25's `is_alloc_stalling_for_old`
/// clause of `should_preclean_young`, which pre-cleans (two young blocks)
/// whatever the cause. `false` for a minor and off
/// `CRATONVM_ZGC_XLOG_STALL_PRECLEAN`.
///
/// `Allocated` and `Compacted` are 0 (no allocation inside the pause; the
/// slide's copy volume is not carried to the logger). Pure.
#[allow(clippy::too_many_arguments)]
fn zgc_xlog_heap_blocks(
    gc_id: u64,
    minor: bool,
    cause: &str,
    caps: [usize; 4],
    before: usize,
    after: usize,
    gen_tables: bool,
    young_split: Option<(usize, usize)>,
    stalled_for_old: bool,
) -> Vec<(&'static str, Vec<String>)> {
    zgc_xlog_heap_blocks_with(
        gc_id,
        minor,
        cause,
        caps,
        before,
        after,
        gen_tables,
        young_split,
        stalled_for_old,
        ZgcXlogEdges::default(),
    )
}

/// zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 1): the edges of
/// one ZGC collection the heap books for its `-Xlog` tables
/// (`CRATONVM_ZGC_XLOG_EDGE_BOOK`, default ON, `=0` kill: the tables as
/// before).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ZgcXlogEdges {
    /// The occupancy when the collection's mark started, when that was a
    /// concurrent mark start before its pause (`None`: the mark started
    /// inside this pause, at `before`).
    mark_start_used: Option<usize>,
    /// The bytes this collection's slide copied (`Compacted`; HotSpot's
    /// in-place relocation is this collector's only kind).
    compacted: usize,
}

/// `CRATONVM_ZGC_XLOG_EDGE_BOOK` (zt t31/x10, item 1; default ON, `=0`
/// kill): the ZGC `-Xlog` generation tables print the collection's booked
/// edges ([`ZgcXlogEdges`]). Read once per logged collection.
fn zgc_xlog_edge_book_enabled() -> bool {
    true
}

/// [`zgc_xlog_heap_blocks`] over the booked `edges` (zt t31/x10, item 1).
/// A concurrent mark's start moves the Mark Start column to the occupancy
/// then, and what the mutators allocated until the pause (`before` less
/// that: nothing is reclaimed before mark end here) is `Allocated` from
/// Mark End on -- young bytes, so in the young table; the slide's copy
/// volume is the relocating table's `Compacted` (the old collection's of a
/// major, the minor's own). Mark End and Relocate Start still read the
/// occupancy before the pause: inside this stop-the-world pause nothing is
/// allocated or freed between them (HotSpot frees empty pages there; this
/// collector frees every dead object at once, at relocate end). Pure.
#[allow(clippy::too_many_arguments)]
fn zgc_xlog_heap_blocks_with(
    gc_id: u64,
    minor: bool,
    cause: &str,
    caps: [usize; 4],
    before: usize,
    after: usize,
    gen_tables: bool,
    young_split: Option<(usize, usize)>,
    stalled_for_old: bool,
    edges: ZgcXlogEdges,
) -> Vec<(&'static str, Vec<String>)> {
    let max = caps[1];
    let mark_start = edges.mark_start_used.unwrap_or(before);
    let allocated = before.saturating_sub(mark_start);
    let high = before.max(after).max(mark_start);
    let low = before.min(after).min(mark_start);
    let whole = [mark_start, before, before, after, high, low];
    if !gen_tables {
        let generations: &[&'static str] = if minor { &["y"] } else { &["Y", "O"] };
        return generations
            .iter()
            .map(|&g| (g, render_xlog_zgc_heap_block(gc_id, g, caps, whole)))
            .collect();
    }
    let dead = before.saturating_sub(after);
    let block = |generation: &'static str,
                 used: [usize; 6],
                 young: bool,
                 f: ZgcXlogGenerationFigures| {
        let mut lines = render_xlog_zgc_heap_block(gc_id, generation, caps, used);
        lines.extend(render_xlog_zgc_generation_statistics(gc_id, generation, young, max, &f));
        (generation, lines)
    };
    if minor {
        let f = match young_split {
            Some((old_after, promoted)) => {
                let old_before = old_after.saturating_sub(promoted);
                let young_before = before.saturating_sub(old_before);
                let kept_young = after.saturating_sub(old_after);
                let live = kept_young.saturating_add(promoted).min(young_before);
                let garbage = young_before - live;
                ZgcXlogGenerationFigures {
                    used: [
                        young_before.saturating_sub(allocated),
                        young_before,
                        young_before,
                        kept_young,
                    ],
                    live: [live, live, kept_young],
                    garbage: [garbage, garbage, 0],
                    allocated: [allocated; 3],
                    reclaimed: [0, garbage],
                    promoted: [0, promoted],
                    compacted: edges.compacted,
                }
            }
            None => ZgcXlogGenerationFigures {
                used: [mark_start, before, before, after],
                live: [after; 3],
                garbage: [dead, dead, 0],
                allocated: [allocated; 3],
                reclaimed: [0, dead],
                compacted: edges.compacted,
                ..ZgcXlogGenerationFigures::default()
            },
        };
        return vec![block("y", whole, true, f)];
    }
    let promote_all = ZgcXlogGenerationFigures {
        used: [mark_start, before, before, 0],
        live: [after, after, 0],
        garbage: [dead, dead, 0],
        allocated: [allocated; 3],
        reclaimed: [0, dead],
        promoted: [0, after],
        ..ZgcXlogGenerationFigures::default()
    };
    let mut out = vec![block("Y", whole, true, promote_all)];
    // zt t16/gn6 (item 12): `should_preclean_young`'s second clause, a major
    // run while threads are stalled for old (`stalled_for_old`, opt-in at
    // the caller), pre-cleans whatever the cause.
    if zgc_major_precleans_young(cause) || stalled_for_old {
        out.push(block("Y", [after; 6], true, ZgcXlogGenerationFigures::default()));
    }
    let old = ZgcXlogGenerationFigures {
        used: [after; 4],
        live: [after; 3],
        compacted: edges.compacted,
        ..ZgcXlogGenerationFigures::default()
    };
    out.push(block("O", [after; 6], false, old));
    out
}

/// `CRATONVM_ZGC_XLOG_PHASE_FRAMES` -- default ON, `=0` kill (zt t31/zc11,
/// program page 1f residual R6): a ZGC collection's `gc,phases` block is
/// HotSpot 25's -- each generation collection framed by its `Y: Young
/// Generation` / `O: Old Generation` (`y:` for a minor) start and end lines,
/// with every phase line of HotSpot's cycle between them, the concurrent ones
/// included (`Concurrent Mark`, `Concurrent Mark Free`, `Concurrent Process
/// Non-Strong`, `Concurrent Reset Relocation Set`, `Concurrent Select
/// Relocation Set`, `Concurrent Remap Roots`, `Concurrent Relocate`); a major
/// whose cause pre-cleans the young generation runs HotSpot's two young
/// collections (`(Promote All)`, `(Collect Roots)`). `=0`: the pause lines
/// alone, as before. Read per collection, only under `gc,phases`.
fn zgc_xlog_phase_frames_enabled() -> bool {
    true
}

/// zt t31/zc11 (R6): one generation collection's `gc,phases` lines -- its
/// frame start and phase lines (`head`), then its frame end (`tail`), which
/// HotSpot prints after the generation's `gc,metaspace` / `gc,heap` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ZgcXlogFrame {
    /// `Y`, `O` or (a minor) `y`.
    generation: &'static str,
    head: Vec<String>,
    tail: String,
}

/// zt t31/zc11 (R6): what [`render_xlog_zgc_phase_frames`] reads of one
/// collection.
struct ZgcXlogPhaseFacts<'a> {
    gc_id: u64,
    minor: bool,
    /// HotSpot pre-cleans the young generation first (two young collections).
    precleans: bool,
    /// The pause rows ([`zgc_hotspot_pause_rows`]): `(name, start, dur ns)`.
    pauses: &'a [(&'static str, u64, u64)],
    /// The failed Pause Mark End attempts `(pause ns, continue ns)`.
    attempts: &'a [(u64, u64)],
    /// The closed cycle's concurrent phase (attempts included), `0` for a
    /// stop-the-world mark.
    conc_mark_ns: u64,
    heap_before: usize,
    heap_after: usize,
    capacity: usize,
    duration_ns: u64,
}

/// zt t31/zc11 (R6): HotSpot 25 ZGC's framed `gc,phases` block of one
/// collection ([`zgc_xlog_phase_frames_enabled`]). This collector runs a
/// cycle's pauses as one stop-the-world collection (the mark start of a
/// concurrent cycle apart), so every phase line is printed in HotSpot's order
/// with the time the phase took HERE: the pauses from the pause rows, the
/// concurrent mark (and each failed attempt's `Pause Mark End` / `Concurrent
/// Mark Continue` pair) from the closed cycle, and `0.000ms` for a phase this
/// collector does not run apart from a pause. The young collection that
/// starts the major carries its `Pause Mark Start (Major)`; the old
/// generation the mark end and relocate start. A frame end reads
/// `<before>M(<pct>%)-><after>M(<pct>%) <s>s` over the heap, the first young
/// collection taking the reclaim (as the `gc,heap` tables do), the old
/// generation the rest of the duration. Empty without pause rows (the caller
/// then prints the plain pause lines, as before). Pure.
fn render_xlog_zgc_phase_frames(f: &ZgcXlogPhaseFacts<'_>) -> Vec<ZgcXlogFrame> {
    if f.pauses.is_empty() {
        return Vec::new();
    }
    const M: usize = 1024 * 1024;
    let id = f.gc_id;
    let ms = |ns: u64| ns as f64 / 1_000_000.0;
    let pct = |b: usize| {
        if f.capacity == 0 {
            0.0
        } else {
            b as f64 * 100.0 / f.capacity as f64
        }
    };
    let edge = |a: usize, b: usize, ns: u64| {
        format!(
            "{}M({:.0}%)->{}M({:.0}%) {:.3}s",
            a / M,
            pct(a),
            b / M,
            pct(b),
            ns as f64 / 1_000_000_000.0
        )
    };
    let pause = |names: &[&str]| {
        f.pauses
            .iter()
            .find(|(n, _, _)| names.contains(n))
            .map_or(0, |&(_, _, d)| d)
    };
    let mark_start_ns = pause(&[ZGC_PAUSE_MARK_START_MAJOR, ZGC_PAUSE_MARK_START]);
    let mark_end_ns = pause(&[ZGC_PAUSE_MARK_END]);
    let relocate_ns = pause(&[ZGC_PAUSE_RELOCATE_START]);
    let attempts_ns: u64 = f
        .attempts
        .iter()
        .map(|&(p, c)| p.saturating_add(c))
        .sum();
    let conc_mark_ns = f.conc_mark_ns.saturating_sub(attempts_ns);
    // One young collection's phase lines, in HotSpot's order.
    let young = |g: &str, start: (&str, u64), mark_ns: u64, end_ns: u64, rel_ns: u64| {
        vec![
            format!("GC({id}) {g}: {} {:.3}ms", start.0, ms(start.1)),
            format!("GC({id}) {g}: Concurrent Mark {:.3}ms", ms(mark_ns)),
            format!("GC({id}) {g}: {ZGC_PAUSE_MARK_END} {:.3}ms", ms(end_ns)),
            format!("GC({id}) {g}: Concurrent Mark Free 0.000ms"),
            format!("GC({id}) {g}: Concurrent Reset Relocation Set 0.000ms"),
            format!("GC({id}) {g}: Concurrent Select Relocation Set 0.000ms"),
            format!("GC({id}) {g}: {ZGC_PAUSE_RELOCATE_START} {:.3}ms", ms(rel_ns)),
            format!("GC({id}) {g}: Concurrent Relocate 0.000ms"),
        ]
    };
    if f.minor {
        let label = "Young Generation";
        let mut head = vec![format!("GC({id}) y: {label}")];
        head.extend(young(
            "y",
            (ZGC_PAUSE_MARK_START, mark_start_ns),
            conc_mark_ns,
            mark_end_ns,
            relocate_ns,
        ));
        return vec![ZgcXlogFrame {
            generation: "y",
            head,
            tail: format!(
                "GC({id}) y: {label} {}",
                edge(f.heap_before, f.heap_after, f.duration_ns)
            ),
        }];
    }
    let mut out = Vec::new();
    // The major's young collection(s).
    let labels: &[&str] = if f.precleans {
        &["Young Generation (Promote All)", "Young Generation (Collect Roots)"]
    } else {
        &["Young Generation"]
    };
    for (i, label) in labels.iter().enumerate() {
        let roots = i + 1 == labels.len();
        let start = if roots {
            (ZGC_PAUSE_MARK_START_MAJOR, mark_start_ns)
        } else {
            (ZGC_PAUSE_MARK_START, 0)
        };
        let mut head = vec![format!("GC({id}) Y: {label}")];
        head.extend(young("Y", start, 0, 0, 0));
        let (a, b) = if i == 0 {
            (f.heap_before, f.heap_after)
        } else {
            (f.heap_after, f.heap_after)
        };
        out.push(ZgcXlogFrame {
            generation: "Y",
            head,
            tail: format!("GC({id}) Y: {label} {}", edge(a, b, start.1)),
        });
    }
    // The old generation.
    let mut head = vec![
        format!("GC({id}) O: Old Generation"),
        format!("GC({id}) O: Concurrent Mark {:.3}ms", ms(conc_mark_ns)),
    ];
    for &(p, c) in f.attempts {
        head.push(format!("GC({id}) O: {ZGC_PAUSE_MARK_END} {:.3}ms", ms(p)));
        head.push(format!(
            "GC({id}) O: Concurrent Mark Continue {:.3}ms",
            ms(c)
        ));
    }
    head.extend([
        format!("GC({id}) O: {ZGC_PAUSE_MARK_END} {:.3}ms", ms(mark_end_ns)),
        format!("GC({id}) O: Concurrent Mark Free 0.000ms"),
        format!("GC({id}) O: Concurrent Process Non-Strong 0.000ms"),
        format!("GC({id}) O: Concurrent Reset Relocation Set 0.000ms"),
        format!("GC({id}) O: Concurrent Select Relocation Set 0.000ms"),
        format!("GC({id}) O: Concurrent Remap Roots 0.000ms"),
        format!("GC({id}) O: {ZGC_PAUSE_RELOCATE_START} {:.3}ms", ms(relocate_ns)),
        format!("GC({id}) O: Concurrent Relocate 0.000ms"),
    ]);
    out.push(ZgcXlogFrame {
        generation: "O",
        head,
        tail: format!(
            "GC({id}) O: Old Generation {}",
            edge(
                f.heap_after,
                f.heap_after,
                f.duration_ns.saturating_sub(mark_start_ns)
            )
        ),
    });
    out
}

/// `CRATONVM_ZGC_XLOG_LOAD_MMU` (zt t31/x10, x10 page item 2; default ON,
/// `=0` kill): a ZGC collection's `-Xlog:gc*` block carries HotSpot 25's
/// per-generation `gc,load` and `gc,mmu` lines. Read once per logged
/// collection.
fn zgc_xlog_load_mmu_enabled() -> bool {
    true
}

/// zt t31/x10 (item 2): the `gc,load` / `gc,mmu` texts of a ZGC collection
/// whose pause began at `t0` and lasted `duration_ns`: `ZStatLoad::print`'s
/// one-, five- and fifteen-minute load averages with their share of the
/// CPUs, and `ZStatMMU::print`'s running minimum mutator utilisation over
/// 2 ms .. 100 ms windows, after this pause is registered with the VM's
/// pause history (`HeapRealm::zgc_mmu`; a stop-the-world collection is ONE
/// pause here, HotSpot's three are laps of it). Each `None` with its tag
/// off or the switch off.
fn zgc_xlog_stat_lines(shared: &SharedVm, t0: std::time::Instant, duration_ns: u64) -> ZgcXlogStatLines {
    use crate::runtime::unified_logging::LogTag;
    if !zgc_xlog_load_mmu_enabled() {
        return ZgcXlogStatLines::default();
    }
    let load = xlog_on(LogTag::GcLoad).then(|| {
        let cpus = crate::runtime::interpreter::gc_and_alloc::zgc_stat_cpu_count(shared);
        render_xlog_zgc_load(crate::runtime::interpreter::gc_and_alloc::load_averages(), cpus)
    });
    let mmu = xlog_on(LogTag::GcMmu).then(|| {
        let end = t0 + std::time::Duration::from_nanos(duration_ns);
        let mmu = shared.mem.zgc_mmu.lock().register_pause(t0, end);
        render_xlog_zgc_mmu(mmu)
    });
    ZgcXlogStatLines { load, mmu }
}

/// zt t31/x10: `ZStatLoad::print`'s text, `Load: %.2f (%.0f%%) / ...` (the
/// share of `cpus`). Pure.
fn render_xlog_zgc_load(loads: [f64; 3], cpus: usize) -> String {
    let pct = |l: f64| if cpus == 0 { 0.0 } else { l * 100.0 / cpus as f64 };
    format!(
        "Load: {:.2} ({:.0}%) / {:.2} ({:.0}%) / {:.2} ({:.0}%)",
        loads[0],
        pct(loads[0]),
        loads[1],
        pct(loads[1]),
        loads[2],
        pct(loads[2])
    )
}

/// zt t31/x10: `ZStatMMU::print`'s text over the six windows. Pure.
fn render_xlog_zgc_mmu(mmu: [f64; 6]) -> String {
    format!(
        "MMU: 2ms/{:.1}%, 5ms/{:.1}%, 10ms/{:.1}%, 20ms/{:.1}%, 50ms/{:.1}%, 100ms/{:.1}%",
        mmu[0], mmu[1], mmu[2], mmu[3], mmu[4], mmu[5]
    )
}

/// zt t9/x6: the `gc,phases` / `gc,metaspace` / `gc,heap` lines of one ZGC
/// collection between its two `gc` lines, in print order, each with its tag.
/// `pause_lines` are [`render_xlog_zgc_pause_lines`]'s (empty when
/// `gc,phases` is off), `metaspace` the metaspace triple (`None` when
/// `gc,metaspace` is off), `heap_blocks` [`zgc_xlog_heap_blocks`]' (empty
/// when `gc,heap` is off).
///
/// `grouped` ([`zgc_xlog_generation_order_enabled`]) and a major: HotSpot
/// 25.0.3's order -- the `Y:` pause lines, then per young block a `Y:
/// Metaspace:` line and the block (one metaspace line when there is no
/// block), then the `O:` pause lines, the `O: Metaspace:` line and the `O:`
/// blocks. Otherwise (a minor, or `=0`): every pause line, one metaspace line
/// (`O:` / `y:`), every block. Pure.
fn zgc_xlog_collection_lines(
    gc_id: u64,
    minor: bool,
    grouped: bool,
    pause_lines: Vec<String>,
    metaspace: Option<(u64, u64, u64)>,
    heap_blocks: Vec<(&'static str, Vec<String>)>,
) -> Vec<(crate::runtime::unified_logging::LogTag, String)> {
    zgc_xlog_collection_lines_framed(
        gc_id,
        minor,
        grouped,
        pause_lines,
        Vec::new(),
        metaspace,
        heap_blocks,
        &ZgcXlogStatLines::default(),
    )
}

/// zt t31/x10 (x10 page item 2): the per-generation `gc,load` / `gc,mmu`
/// lines' texts (`Load: ...`, `MMU: ...`, without the `GC(n) Y:` prefix),
/// `None` where the tag is off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ZgcXlogStatLines {
    load: Option<String>,
    mmu: Option<String>,
}

/// [`zgc_xlog_collection_lines`] with the `gc,load` / `gc,mmu` lines of
/// each generation block, before its `Metaspace:` line, in HotSpot 25's
/// order (`ZStatCycle`'s generation end: alloc, load, mmu, marking, nmethod,
/// metaspace). Pure.
#[allow(clippy::too_many_arguments)]
fn zgc_xlog_collection_lines_with(
    gc_id: u64,
    minor: bool,
    grouped: bool,
    pause_lines: Vec<String>,
    metaspace: Option<(u64, u64, u64)>,
    heap_blocks: Vec<(&'static str, Vec<String>)>,
    stats: &ZgcXlogStatLines,
) -> Vec<(crate::runtime::unified_logging::LogTag, String)> {
    zgc_xlog_collection_lines_framed(
        gc_id,
        minor,
        grouped,
        pause_lines,
        Vec::new(),
        metaspace,
        heap_blocks,
        stats,
    )
}

/// [`zgc_xlog_collection_lines`] with zt t31/zc11's frames (R6,
/// [`render_xlog_zgc_phase_frames`]): when `frames` is not empty (and the
/// generations are grouped, or a minor), each generation collection prints
/// its frame start and phase lines, its `gc,metaspace` line and its `gc,heap`
/// block, then its frame end -- HotSpot 25.0.4's `-Xlog:gc*` order; the old
/// generation takes every `O:` block, the last young collection any young
/// block beyond the frames. `pause_lines` is then unused. `stats` (zt t31/x10,
/// `gc,load` / `gc,mmu`) print before each generation's `Metaspace:` line,
/// framed or not. Pure.
#[allow(clippy::too_many_arguments)]
fn zgc_xlog_collection_lines_framed(
    gc_id: u64,
    minor: bool,
    grouped: bool,
    pause_lines: Vec<String>,
    frames: Vec<ZgcXlogFrame>,
    metaspace: Option<(u64, u64, u64)>,
    heap_blocks: Vec<(&'static str, Vec<String>)>,
    stats: &ZgcXlogStatLines,
) -> Vec<(crate::runtime::unified_logging::LogTag, String)> {
    use crate::runtime::unified_logging::LogTag;
    let mut out: Vec<(LogTag, String)> = Vec::new();
    let stat_lines = |out: &mut Vec<(LogTag, String)>, generation: &str| {
        if let Some(load) = &stats.load {
            out.push((LogTag::GcLoad, format!("GC({gc_id}) {generation}: {load}")));
        }
        if let Some(mmu) = &stats.mmu {
            out.push((LogTag::GcMmu, format!("GC({gc_id}) {generation}: {mmu}")));
        }
    };
    if !frames.is_empty() && (minor || grouped) {
        let (mut old_blocks, young_blocks): (Vec<_>, Vec<_>) =
            heap_blocks.into_iter().partition(|(g, _)| *g == "O");
        let young_frames = frames.iter().filter(|f| f.generation != "O").count();
        let mut young_blocks = young_blocks.into_iter();
        let mut seen_young = 0;
        for frame in frames {
            out.extend(frame.head.into_iter().map(|l| (LogTag::GcPhases, l)));
            stat_lines(&mut out, frame.generation);
            if let Some(m) = metaspace {
                let line = render_xlog_zgc_metaspace_line_as(gc_id, frame.generation, m);
                out.push((LogTag::GcMetaspace, line));
            }
            let blocks: Vec<(&'static str, Vec<String>)> = if frame.generation == "O" {
                std::mem::take(&mut old_blocks)
            } else {
                seen_young += 1;
                if seen_young == young_frames {
                    young_blocks.by_ref().collect()
                } else {
                    young_blocks.next().into_iter().collect()
                }
            };
            for (_, block) in blocks {
                out.extend(block.into_iter().map(|l| (LogTag::GcHeap, l)));
            }
            out.push((LogTag::GcPhases, frame.tail));
        }
        // A minor has no old frame: an old block (none today) prints last.
        for (_, block) in old_blocks {
            out.extend(block.into_iter().map(|l| (LogTag::GcHeap, l)));
        }
        return out;
    }
    if minor || !grouped {
        out.extend(pause_lines.into_iter().map(|l| (LogTag::GcPhases, l)));
        stat_lines(&mut out, if minor { "y" } else { "O" });
        if let Some(m) = metaspace {
            let line = render_xlog_zgc_metaspace_line(gc_id, minor, m);
            out.push((LogTag::GcMetaspace, line));
        }
        for (_, block) in heap_blocks {
            out.extend(block.into_iter().map(|l| (LogTag::GcHeap, l)));
        }
        return out;
    }
    let young_prefix = format!("GC({gc_id}) Y:");
    let (young_pauses, old_pauses): (Vec<String>, Vec<String>) =
        pause_lines.into_iter().partition(|l| l.starts_with(&young_prefix));
    let (young_blocks, old_blocks): (Vec<_>, Vec<_>) =
        heap_blocks.into_iter().partition(|(g, _)| *g == "Y");
    out.extend(young_pauses.into_iter().map(|l| (LogTag::GcPhases, l)));
    let young_collections = young_blocks.len().max(1);
    let mut young_blocks = young_blocks.into_iter();
    for _ in 0..young_collections {
        stat_lines(&mut out, "Y");
        if let Some(m) = metaspace {
            let line = render_xlog_zgc_metaspace_line_as(gc_id, "Y", m);
            out.push((LogTag::GcMetaspace, line));
        }
        if let Some((_, block)) = young_blocks.next() {
            out.extend(block.into_iter().map(|l| (LogTag::GcHeap, l)));
        }
    }
    out.extend(old_pauses.into_iter().map(|l| (LogTag::GcPhases, l)));
    stat_lines(&mut out, "O");
    if let Some(m) = metaspace {
        let line = render_xlog_zgc_metaspace_line_as(gc_id, "O", m);
        out.push((LogTag::GcMetaspace, line));
    }
    for (_, block) in old_blocks {
        out.extend(block.into_iter().map(|l| (LogTag::GcHeap, l)));
    }
    out
}

/// Render one HotSpot-shaped `-Xlog:gc` line (without the decorators the
/// unified logger adds): `GC(3) Pause Young (Allocation Failure) 24M->3M(96M)
/// 2.345ms`, the format HotSpot prints at `-Xlog:gc` (`gc,info`).
pub(super) fn render_xlog_gc_line(
    gc_id: u64,
    label: &str,
    cause: &str,
    heap_before: usize,
    heap_after: usize,
    heap_committed: usize,
    duration_ns: u64,
) -> String {
    const M: usize = 1024 * 1024;
    format!(
        "GC({gc_id}) {label} ({cause}) {}M->{}M({}M) {:.3}ms",
        heap_before / M,
        heap_after / M,
        heap_committed / M,
        duration_ns as f64 / 1_000_000.0,
    )
}

/// gci i13/gp (`gci-i13ef-g1-evacuation-failure-pinning-and-heap-sizing-review-findings-RETIRED-RULING-20261018`
/// finding 2): did the G1 pause whose start read the cycle record at
/// `seq_before` fail evacuation? The collector records every pause (and a
/// kept-region drain in the same pause, which overwrites it and always
/// carries the flag) through `gc_metrics::record_g1_cycle`; the flag is set
/// exactly when the forwarding map has an identity entry (a self-forwarded
/// object, `g1_pause_degraded_flags`). `false` when nothing was recorded
/// since the start. The record is the process's last one: a sibling VM's G1
/// pause in between would be read instead (multi-VM embedding only). Pure.
fn g1_pause_evacuation_failed(
    seq_before: Option<u64>,
    last: Option<cratonvm_gc::gc_metrics::G1CycleFacts>,
) -> bool {
    match (seq_before, last) {
        (Some(before), Some(facts)) => {
            facts.sequence != before
                && facts.degraded & cratonvm_gc::gc_metrics::g1_degraded::EVACUATION_FAILURE != 0
        }
        _ => false,
    }
}

/// gci i13/gp (ef finding 2): HotSpot 25 G1's end-line name of a pause whose
/// evacuation failed (`G1YoungGCTraceTime::update_young_gc_name`): `Pause
/// Young (<type>) (<cause>) (Evacuation Failure: Allocation)`, the suffix
/// right after the cause (`Pinned` never applies here: no JNI pin reaches G1,
/// and a JIT pin is not HotSpot's pinned region). The `gc,start` line keeps
/// the plain name. `line` is [`render_xlog_gc_line`]'s; unchanged unless
/// `failed`. Pure.
fn with_evacuation_failure_suffix(line: String, cause: &str, failed: bool) -> String {
    if !failed {
        return line;
    }
    line.replacen(
        &format!("({cause}) "),
        &format!("({cause}) (Evacuation Failure: Allocation) "),
        1,
    )
}

/// Render the backend-independent `--verbose:gc` per-pause line (gc-common
/// w3-f, 2026-09-23):
///
/// `[GC] pause: gc=3 door=system-gc ttsp_us=41 straggler=5 xt_pass_us=30
/// xt_post_quota_us=12 collect_us=2345 xt_passes=1 xt_frozen=0 xt_unclassified=0 xt_roots=0
/// hw_windows=0 hw_roots=0`
///
/// * `ttsp_us` — time-to-safepoint: request accepted -> every counted mutator
///   parked or taken over (HotSpot's "Reaching safepoint" of
///   `-Xlog:safepoint`). Before this, TTSP existed only as the exit
///   `[GC] ttsp:` aggregate, so a slow pause could not be attributed to a
///   slow safepoint.
/// * `straggler` — the registry `ThreadId` this pause waited for last, or
///   `none` when its quota was met with no participating arrival (gc-common
///   w5-a; same spelling as the exit line's `max_straggler=`).
/// * `xt_pass_us` — the take-over passes' own time (OS suspends, register /
///   stack copies, the word probe), summed over this pause's passes. They run
///   inside the barrier wait, so this is PART of `ttsp_us`, not in addition
///   to it: `ttsp_us - xt_pass_us` is the time spent waiting for polling
///   threads (gc-common w6-f). 0 exactly when `xt_passes=0`.
/// * `xt_post_quota_us` — the take-over's work AFTER the quota was met: the
///   frozen peers' interpreter-frame walk, the helper-window pass over blocked
///   peers with JIT frames, and the TLAB skip-span publish (gc-common w4-a;
///   the `<walk>/<hw>` half of
///   `docs/feature-designs/real-jdk-mode.md`). 0 when the
///   take-over did not run. `ttsp_us + xt_post_quota_us + collect_us` is the
///   pause from request to release (the initiator's own root snapshot falls
///   inside `ttsp_us`; only the `CRATONVM_DBG_*` pre-collection verifiers fall
///   outside all three).
/// * `collect_us` — roots, collection, reference processing and remap: from
///   the collection's start to its end, sealed INSIDE the pause immediately
///   before the frozen peers resume and the barrier releases (gc-common w5-a,
///   [`gc_event_seal`]); the same instant as the `-Xlog:gc` / JFR duration.
/// * `xt_*` / `hw_*` — this pause's cross-thread JIT take-over coverage
///   (`gc_quiescence::xt_cycle_coverage`): passes, peers frozen, peers whose
///   state could not be classified, conservative roots they yielded, and the
///   helper-window pass's windows and roots.
///
/// One line per collection on every backend, next to the collector's own
/// per-collection line (whose grammar differs per backend). A pause that
/// collects nothing (a G1 / Generational remark, an initial mark, a ZGC mark
/// start, a frame-trace or heap-dump pause) prints the same shape with
/// `gc=-`, its own `door=` token and `work_us=` in place of `collect_us=`
/// ([`non_collection_pause_finish`], gc-common w5-a).
fn render_pause_line(
    gc_id: u64,
    door: GcDoor,
    facts: PauseSafepointFacts,
    collect_ns: u64,
) -> String {
    // A space-free token per door (the HotSpot cause string "Allocation
    // Failure" would split a `key=value` scrape).
    let door = match door {
        GcDoor::AllocationThreshold => "alloc-threshold",
        GcDoor::AllocationFailure => "alloc-failure",
        GcDoor::SystemGc => "system-gc",
        GcDoor::MetadataThreshold => "metadata",
        GcDoor::DiagnosticCommand => "dcmd",
        // gci i9/dc: `-gc` so they never alias the non-collection pauses'
        // `heap-inspection` / `heap-dump` tokens (the walk's own pause).
        GcDoor::HeapInspection => "heap-inspection-gc",
        GcDoor::HeapDump => "heap-dump-gc",
        // gci i11/go: the G1 allocation-failure recovery compaction (it
        // printed `alloc-failure`).
        GcDoor::G1CompactionPause => "g1-compaction",
        GcDoor::G1HumongousAllocation => "g1-humongous",
        // gci i27/ob: JVMTI `ForceGarbageCollection`.
        GcDoor::JvmtiForceGc => "jvmti-force-gc",
        // zt t31/x10: G1's periodic collection.
        GcDoor::G1PeriodicCollection => "g1-periodic",
        GcDoor::G1PeriodicFull => "g1-periodic-full",
    };
    render_safepoint_pause_line(&gc_id.to_string(), door, facts, "collect_us", collect_ns)
}

/// The one `[GC] pause:` grammar, for collections ([`render_pause_line`]) and
/// non-collection pauses ([`non_collection_pause_finish`]) alike.
fn render_safepoint_pause_line(
    gc: &str,
    door: &str,
    facts: PauseSafepointFacts,
    work_key: &str,
    work_ns: u64,
) -> String {
    let (passes, frozen, unclassified, roots, hw_windows, hw_roots) = facts.xt;
    let straggler = facts
        .straggler
        .map_or_else(|| "none".to_string(), |tid| tid.to_string());
    format!(
        "[GC] pause: gc={gc} door={door} ttsp_us={} straggler={straggler} \
         xt_pass_us={} xt_post_quota_us={} {work_key}={} \
         xt_passes={passes} xt_frozen={frozen} xt_unclassified={unclassified} \
         xt_roots={roots} hw_windows={hw_windows} hw_roots={hw_roots}",
        facts.ttsp_ns / 1_000,
        facts.xt_pass_ns / 1_000,
        facts.xt_post_quota_ns / 1_000,
        work_ns / 1_000,
    )
}

/// Account one completed collection: advance the VM's collection ordinal
/// (`gc_cycle_count`, which is also the JFR `gcId` and the `GC(n)` of the
/// `-Xlog:gc` line), and — only when `start` is `Some` — emit the JFR GC events
/// and the `-Xlog:gc` line.
///
/// Called by every initiator arm, after the collection and before the
/// finalizer / cleaner drains, so a door cannot count a collection without
/// reporting it or report one without counting it.
///
/// gci i20/ob: a door that knows its requesting thread calls
/// [`gc_event_finish_by`] instead (`run_collection_pause` does once hunk X1 of
/// `i20-ob-report.md` lands; this requester-less entry then has no caller).
#[allow(dead_code)]
pub(super) fn gc_event_finish(shared: &SharedVm, door: GcDoor, start: Option<GcEventStart>) {
    if let Some(report) = gc_event_finish_with(shared, door, start, None) {
        // No requester to park it for: the prediction labels it, as before.
        emit_deferred_young_report(shared, &report, G1YoungPauseType::ConcurrentStart);
    }
    // gci i45/sl: the pause's `-Xlog:safepoint` line, after its `gc` lines.
    crate::runtime::unified_logging::safepoint_line_finish(collection_safepoint_name(shared, door));
}

/// [`gc_event_finish`]'s body; `who` (gci i20/ob, [`gc_event_finish_by`]) is
/// the requesting thread its JFR rows name.
///
/// gci i27/ob: a G1 young pause the IHOP prediction labelled `Concurrent
/// Start` is NOT reported here (under `CRATONVM_G1_CONCURRENT_START_FROM_DRIVE`):
/// its JFR rows and `-Xlog` lines come back as a [`G1DeferredYoungReport`]
/// for [`gc_event_finish_by`] to park until the requester's mark-cycle drive
/// says whether the cycle opened.
fn gc_event_finish_with(
    shared: &SharedVm,
    door: GcDoor,
    start: Option<GcEventStart>,
    who: Option<JfrRequester>,
) -> Option<G1DeferredYoungReport> {
    // gcg h1/ob: an escalated young pause reported as HotSpot Serial reports
    // it -- `Pause Young` with this id, `Pause Full` with the next -- takes two
    // ordinals (`CRATONVM_GC_ESCALATED_PAUSE_SPLIT`, default on since gch
    // h2/ob; `=0` keeps one).
    let split = start
        .as_ref()
        .and_then(|s| escalated_pause_split(s, s.end_facts().0));
    // gci i12/ge: a pause folded into a G1 full collection's ONE report
    // (`CRATONVM_G1_FULL_GC_ONE_REPORT`) takes no ordinal: it runs under the
    // report's, taken by the scope's first pause.
    let folded_id = start.as_ref().and_then(|s| s.full_scope_id);
    let gc_id = match folded_id {
        Some(id) => id,
        None => shared.mem.gc_cycle_count.fetch_add(
            if split.is_some() { 2 } else { 1 },
            std::sync::atomic::Ordering::Relaxed,
        ),
    };
    let Some(mut start) = start else {
        return None;
    };
    // gc-common w5-a: ONE end instant for every consumer below, sealed inside
    // the pause by `gc_event_seal` (before, the pause line took one `elapsed`
    // after the bean record and the `-Xlog:gc` / JFR duration a second one
    // after the pause line's own stderr write, both with the world released).
    let (duration_ns, sealed_after) = start.end_facts();
    // gc-common w6-f: the backend-independent pause sum ZGC's
    // `getCollectionTime()` reads (`vm_exec.rs::gc_collection_time_ms`).
    shared
        .mem
        .gc_pause_ns_total
        .fetch_add(duration_ns, std::sync::atomic::Ordering::Relaxed);
    // zs s7/jf: the phase boundaries the collection's pause was booked with,
    // for its JFR pause rows too.
    let mut pause_split = start.zgc_pause_split;
    if folded_id.is_some() {
        g1_full_scope_add_collection(shared, duration_ns);
    }
    // gen r4w3/obs2: the per-bean record (last GC, notification queue) first,
    // then the JFR / `-Xlog:gc` events only if one of them is armed.
    if let Some(before) = start.beans.as_ref() {
        // gc-common w7-f: a door that did not seal books the backend's beans
        // here, before the late snapshot `record_gc_notification` takes.
        if !start.backend_counted {
            if let Some(b) = shared.mem.backend_gc_beans() {
                // zs s4/ob3: the same split as the seal's, over the pause
                // this finish measured.
                let end = start.t0 + std::time::Duration::from_nanos(duration_ns);
                let split = shared.mem.heap.zgc_take_pause_split();
                // zt t31/zc11: as the seal's.
                start.zgc_cycle_start =
                    zgc_concurrent_cycle_jfr(shared).and_then(|c| c.take_cycle_open_at());
                pause_split = b.note_collection_split_from(
                    start.zgc_cycle_start,
                    start.t0,
                    end,
                    start.kind(shared),
                    split,
                );
            }
        }
        // gcg h1/ob: under the split, the `Copy` record ends at the mark with
        // the young half's pools, and the `MarkSweepCompact` record starts there.
        let mid = split.and_then(|sp| {
            start
                .beans_after
                .as_ref()
                .map(|a| (sp.at, escalation_mid_pools(&a.pools, sp.old_used_at_mark)))
        });
        record_gc_notification(
            shared,
            start.cause(door),
            before,
            start.beans_after.as_ref(),
            mid.as_ref().map(|(at, pools)| (*at, pools.as_slice())),
            start.affected_pools(shared),
            pause_split.as_ref(),
            start.zgc_cycle_start,
        );
    }
    if let Some(facts) = start.pause_line {
        eprintln!("{}", render_pause_line(gc_id, door, facts, duration_ns));
    }
    if !start.events {
        return None;
    }
    let (heap_after, committed) = sealed_after.unwrap_or_else(|| {
        (
            gc_event_heap_used(&shared.mem.heap),
            shared.mem.heap.committed_bytes(),
        )
    });
    // gci i11/go: the G1 pause type, for the `-Xlog:gc` label and the
    // `jdk.G1GarbageCollection` row.
    let g1_type = start.g1_pause_type(shared, door);
    // The JFR name of an escalated pause's full half; one collection takes
    // its whole shape in `emit_collection_events` (gci i27/ob).
    let jfr_name = gc_event_shape(shared, start.kind(shared), g1_type).0;
    // SBR5 lane jfr: ONE take of the collector's phase marks feeds both the
    // legacy recorder below and the real JFR backend's GC / safepoint events.
    let want_phases =
        cratonvm_jfr::is_enabled() || cratonvm_native_builtins_jfr::jfr_vm_events::any_recording();
    let mut phase_rows: Vec<(&'static str, u64, u64)> = if want_phases {
        cratonvm_gc::gen_heap::take_last_pause_phases()
            .map(|(pause_us, marks)| phase_pause_rows(start.wall_start_ns, pause_us, &marks))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    // zr z4/jf: a ZGC collection's own facts (its laps as the level-1 rows,
    // relocation set, uncommit) and its cycle's `Concurrent Mark`. ZGC only,
    // and only while a recording runs.
    let zgc_facts = if want_phases {
        zgc_collection_jfr(shared, start.wall_start_ns)
    } else {
        None
    };
    if let Some(f) = zgc_facts.as_ref().filter(|f| !f.marks.is_empty()) {
        phase_rows = phase_pause_rows(
            start.wall_start_ns,
            u128::from(duration_ns / 1_000),
            &f.marks,
        );
    }
    // zr z9/hp: HotSpot 25 ZGC writes no `jdk.GCPhasePauseLevel1` row (its
    // pauses have no sub-phases; its mark, reference processing, selection
    // and relocation are `GCPhaseConcurrent[Level1]` rows of phases that run
    // concurrently there). This collector's laps run INSIDE its one pause, so
    // they cannot honestly be written as concurrent rows either; the HotSpot
    // event set is kept by writing none. Host-measured on 25.0.3/25.0.4:
    // `Zr4JfJfrEvents sysgc` -> `type jdk.GCPhasePauseLevel1 present=false`.
    if !zgc_jfr_pause_level1_rows_for(zgc_facts.is_some()) {
        phase_rows.clear();
    }
    let mut zgc_jfr = zgc_facts.as_ref().map(|f| ZgcCollectionJfr {
        major: matches!(
            start.kind(shared),
            cratonvm_gc::gc_metrics::CollectionKind::Major
        ),
        facts: f,
        cycle_span: None,
        cycle_heap_before: None,
    });
    // zs s7/jf: HotSpot's pause rows for a ZGC collection (empty: one
    // `GC Pause` row, the z4/jf shape).
    let mut zgc_pauses = match zgc_jfr.as_ref() {
        Some(z) if true => zgc_hotspot_pause_rows(
            pause_split.as_ref(),
            start.t0,
            start.wall_start_ns,
            duration_ns,
            z.major,
        ),
        _ => Vec::new(),
    };
    // zs s10/jf3 (ported by zt t1/jf): a concurrent cycle's mark-start pause
    // joins its closing collection's rows and span (ZGC only: the facts are
    // ZGC's, `None` on every other collector).
    if let Some(z) = zgc_jfr.as_mut() {
        if let Some(initial) = z.facts.initial_pause {
            z.cycle_span = zgc_cycle_shape(
                shared,
                gc_id,
                Some(initial),
                &mut zgc_pauses,
                z.major,
                start.wall_start_ns.saturating_add(duration_ns),
            );
            // zt t2/jf (R2): the cycle's `Before GC` figures go with its span.
            z.cycle_heap_before = z.cycle_span.and(z.facts.cycle_heap_before);
        }
    }
    // gcg h1/ob: the split's young half ends at the mark with the old
    // generation as the decision saw it; the full half starts there.
    let split_view = split.and_then(|sp| {
        let after = start.beans_after.as_ref()?;
        let heap_mid = escalation_mid_heap_used(heap_after, &after.pools, sp.old_used_at_mark);
        Some((sp, heap_mid))
    });
    let Some((sp, heap_mid)) = split_view else {
        // gci i27/ob (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
        // the label of a pause the IHOP prediction calls a concurrent start
        // waits for the drive's outcome.
        // gci cs48: and the forced start of a user-requested concurrent cycle
        // (only that rule labels a `System.gc()` / `GC.run` pause so) waits
        // for the cycle `force_full_collection` opens.
        if g1_type == Some(G1YoungPauseType::ConcurrentStart)
            && (g1_concurrent_start_is_the_ihop_prediction(&start, door)
                || g1_door_is_user_requested(door))
        {
            return Some(G1DeferredYoungReport {
                gc_id,
                door,
                start,
                heap_after,
                committed,
                duration_ns,
                phase_rows,
                who,
            });
        }
        // zt t5/jf (R9): the ZGC pause rows `-Xlog:gc+phases` prints -- the
        // JFR rows when a recording made them (a concurrent cycle's joined
        // mark start included), else the same split cut for the log alone.
        let xlog_pauses = if !zgc_pauses.is_empty() {
            zgc_pauses.clone()
        } else if heap_is_zgc(shared)
            && zgc_xlog_pause_phases_enabled()
            && crate::runtime::unified_logging::is_unified_logging_enabled(
                &[crate::runtime::unified_logging::LogTag::GcPhases],
                crate::runtime::unified_logging::LogLevel::Info,
            )
        {
            let major = matches!(
                start.kind(shared),
                cratonvm_gc::gc_metrics::CollectionKind::Major
            );
            let mut rows = zgc_hotspot_pause_rows(
                pause_split.as_ref(),
                start.t0,
                start.wall_start_ns,
                duration_ns,
                major,
            );
            {
                let initial = zgc_jfr
                    .as_ref()
                    .and_then(|z| z.facts.initial_pause)
                    .or_else(|| zgc_take_xlog_initial_pause(shared));
                let _ = zgc_join_initial_pause(
                    gc_id,
                    initial,
                    &mut rows,
                    major,
                    start.wall_start_ns.saturating_add(duration_ns),
                );
            }
            rows
        } else {
            if zgc_jfr.is_none() && heap_is_zgc(shared) {
                let _ = zgc_take_xlog_initial_pause(shared);
            }
            Vec::new()
        };
        emit_collection_events(
            shared,
            door,
            gc_id,
            &start,
            (heap_after, committed),
            duration_ns,
            &phase_rows,
            zgc_jfr.as_ref(),
            &zgc_pauses,
            &xlog_pauses,
            g1_type,
            who.as_ref(),
        );
        return None;
    };
    let caller = who.as_ref().map(JfrRequester::as_caller);
    let full_ns = duration_ns.saturating_sub(sp.young_ns);
    // The phase marks start at the pause start, i.e. with the young half.
    // gci i9/jc: both halves get the marks; `emit_jfr_collection` keeps the
    // part of each that falls in its half (`phase_rows_within`), so the young
    // half's level-1 rows no longer run past its `GC Pause` row and the full
    // half gets the phases of its own window (it had none).
    emit_jfr_collection(
        shared,
        door,
        &JfrCollection {
            gc_id,
            cause: start.cause(door),
            name: SERIAL_YOUNG_JFR_NAME,
            old: false,
            start_ns: start.wall_start_ns,
            duration_ns: sp.young_ns,
            heap_before: start.heap_used_before,
            committed_before: start.heap_committed_before,
            heap_after: heap_mid,
            committed_after: committed,
            phases: &phase_rows,
            g1_type: None,
            vm_operation: jfr_vm_operation(&shared.mem.heap, door),
            longest_ns: None,
            zgc: None,
            pauses: &[],
            caller,
        },
    );
    emit_jfr_collection(
        shared,
        door,
        &JfrCollection {
            gc_id: gc_id.wrapping_add(1),
            cause: start.cause(door),
            name: jfr_name,
            old: true,
            start_ns: start.wall_start_ns.saturating_add(sp.young_ns),
            duration_ns: full_ns,
            heap_before: heap_mid,
            committed_before: committed,
            heap_after,
            committed_after: committed,
            phases: &phase_rows,
            g1_type: None,
            vm_operation: jfr_vm_operation(&shared.mem.heap, door),
            longest_ns: None,
            zgc: None,
            pauses: &[],
            caller,
        },
    );
    xlog_escalated_collection_lines(
        shared,
        gc_id,
        door,
        &start,
        sp,
        (heap_mid, heap_after, committed),
        (sp.young_ns, full_ns),
    );
    None
}

/// The JFR rows and `-Xlog` lines of ONE collection (not an escalated
/// pause's two halves), with `g1_type` as its G1 label. gci i27/ob: split out
/// of [`gc_event_finish_with`] so a deferred G1 young report
/// ([`G1DeferredYoungReport`]) is emitted by the same code, later.
#[allow(clippy::too_many_arguments)]
fn emit_collection_events(
    shared: &SharedVm,
    door: GcDoor,
    gc_id: u64,
    start: &GcEventStart,
    (heap_after, committed): (usize, usize),
    duration_ns: u64,
    phase_rows: &[(&'static str, u64, u64)],
    zgc: Option<&ZgcCollectionJfr<'_>>,
    pauses: &[(&'static str, u64, u64)],
    xlog_pauses: &[(&'static str, u64, u64)],
    g1_type: Option<G1YoungPauseType>,
    who: Option<&JfrRequester>,
) {
    let (jfr_name, xlog_label, old) = gc_event_shape(shared, start.kind(shared), g1_type);
    emit_jfr_collection(
        shared,
        door,
        &JfrCollection {
            gc_id,
            cause: start.cause(door),
            name: jfr_name,
            old,
            start_ns: start.wall_start_ns,
            duration_ns,
            heap_before: start.heap_used_before,
            committed_before: start.heap_committed_before,
            heap_after,
            committed_after: committed,
            phases: phase_rows,
            g1_type: g1_type.map(G1YoungPauseType::jfr_type),
            vm_operation: jfr_vm_operation(&shared.mem.heap, door),
            longest_ns: None,
            zgc,
            pauses,
            caller: who.map(JfrRequester::as_caller),
        },
    );
    xlog_collection_lines(
        shared,
        gc_id,
        xlog_label,
        old,
        door,
        start,
        (heap_after, committed),
        duration_ns,
        xlog_pauses,
    );
}

/// gci i27/ob (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
/// a G1 young pause predicted `Pause Young (Concurrent Start)` and not yet
/// reported. HotSpot decides the concurrent start before the pause and the
/// pause IS the initial mark; here the initial mark is a separate pause the
/// requester's drive (`gc_and_alloc::g1_drive_mark_cycle`) runs right after,
/// and only when the IHOP policy, the STW race and the other threads' drives
/// let it. So the report is parked per VM, keyed by its requester
/// ([`gc_event_finish_by`]), and emitted by [`g1_emit_deferred_young_report`]:
/// `Concurrent Start` just before the initial mark prints its
/// `GC(c) Concurrent Mark Cycle` line, `Normal` when the drive opens nothing
/// (or the door never drives). Everything but the label was sealed in the
/// pause, so the lines are the ones the pause would have printed.
pub(super) struct G1DeferredYoungReport {
    gc_id: u64,
    door: GcDoor,
    start: GcEventStart,
    heap_after: usize,
    committed: usize,
    duration_ns: u64,
    phase_rows: Vec<(&'static str, u64, u64)>,
    who: Option<JfrRequester>,
}

/// gci i27/ob: is this pause's `Concurrent Start` the IHOP prediction
/// [`gc_event_seal`] took (`G1Collector::would_start_marking` on a door that
/// drives the cycle), and not the metadata opt-in's (whose cycle
/// `force_gc_for_class_metadata` opens unconditionally)? Pure but for the
/// flag snapshot read.
fn g1_concurrent_start_is_the_ihop_prediction(start: &GcEventStart, door: GcDoor) -> bool {
    start.g1_concurrent_start
        && g1_door_drives_mark_cycle(door, true)
}



/// gci cs48: `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE` (default ON, `=0`
/// kills). A G1 marking cycle opens only after a young-only pause (never right
/// after a `Mixed` one), and a user-requested concurrent cycle (`System.gc()` /
/// `jcmd GC.run` under `-XX:+ExplicitGCInvokesConcurrent`, no cycle open)
/// takes its door's young pause as the forced `Concurrent Start` and runs no
/// compaction pause after its cleanup -- HotSpot 25's
/// `decide_on_concurrent_start_pause`. `=0`: the cycle opens after any pause
/// the IHOP drive follows, and the user-requested door ticks IHOP and compacts.
/// Read inside a mixed G1 pause and once per user-requested door (no static).
pub(super) fn g1_concurrent_start_at_young_pause_enabled() -> bool {
    true
}

/// gci i27/ob: emit the deferred G1 young reports `requester` parked
/// ([`gc_event_finish_by`]), labelled `Concurrent Start` when `started` (the
/// drive opened the cycle) and `Normal` otherwise; a no-op when it parked
/// none (the common case: one uncontended lock, an empty `Vec`). Called with
/// the world running, on the requester's thread: by `g1_concurrent_mark_cycle`
/// just before its initial mark prints `GC(c) Concurrent Mark Cycle`
/// (`true`), and at every exit of `g1_drive_mark_cycle` and of the forced
/// allocation door after its drive decision (`false`; a report already
/// emitted is gone, so those calls find nothing after a started cycle).
pub(super) fn g1_emit_deferred_young_report(
    shared: &SharedVm,
    requester: crate::threading::jvm_thread::ThreadId,
    started: bool,
) {
    let mine: Vec<G1DeferredYoungReport> = {
        let mut slots = shared.mem.alloc_ladder.g1_deferred_young.lock();
        if slots.is_empty() {
            return;
        }
        let mut mine = Vec::new();
        let mut i = 0;
        while i < slots.len() {
            if slots[i].requester == requester.0 {
                let slot = slots.remove(i);
                if let Ok(report) = slot.report.downcast::<G1DeferredYoungReport>() {
                    mine.push(*report);
                }
            } else {
                i += 1;
            }
        }
        mine
    };
    for r in &mine {
        emit_deferred_young_report(shared, r, g1_deferred_young_label(started));
    }
    // gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual 3):
    // the `-Xlog:safepoint` lines `gc_event_finish_by` held for these
    // reports, right after their `gc` lines.
    crate::runtime::unified_logging::safepoint_line_release_held(requester.0);
}

/// gci i27/ob: [`emit_collection_events`] for a deferred report, labelled
/// `g1_type`.
fn emit_deferred_young_report(
    shared: &SharedVm,
    r: &G1DeferredYoungReport,
    g1_type: G1YoungPauseType,
) {
    emit_collection_events(
        shared,
        r.door,
        r.gc_id,
        &r.start,
        (r.heap_after, r.committed),
        r.duration_ns,
        &r.phase_rows,
        None,
        &[],
        &[],
        Some(g1_type),
        r.who.as_ref(),
    );
}

/// gci i27/ob: park `report` for `requester` (see [`G1DeferredYoungReport`]).
/// A report the same requester parked earlier and never resolved (a path
/// that did not drive) is emitted first, as `Normal`, so the log keeps the
/// pauses in order.
fn g1_park_deferred_young_report(
    shared: &SharedVm,
    requester: crate::threading::jvm_thread::ThreadId,
    report: G1DeferredYoungReport,
) {
    g1_emit_deferred_young_report(shared, requester, false);
    shared
        .mem
        .alloc_ladder
        .g1_deferred_young
        .lock()
        .push(crate::vm::realms::heap_realm::G1DeferredYoungSlot {
            requester: requester.0,
            report: Box::new(report),
        });
}

/// gci i27/ob: the label of a deferred young report, from the drive's
/// outcome. Pure.
fn g1_deferred_young_label(started: bool) -> G1YoungPauseType {
    if started {
        G1YoungPauseType::ConcurrentStart
    } else {
        G1YoungPauseType::Normal
    }
}

/// JFR's name of a Serial young collection (`GCName::DefNew`).
const SERIAL_YOUNG_JFR_NAME: &str = "DefNew";

/// One collection as the JFR GC events describe it. gcg h1/ob: factored out
/// of [`gc_event_finish`] so an escalated pause can be reported as two (one
/// call per half); the single-collection call passes what it always did.
struct JfrCollection<'a> {
    gc_id: u64,
    /// zr z1/tr: the collection's `GCCause` ([`GcEventStart::cause`]).
    cause: &'static str,
    name: &'static str,
    old: bool,
    start_ns: u64,
    duration_ns: u64,
    heap_before: usize,
    committed_before: usize,
    heap_after: usize,
    committed_after: usize,
    phases: &'a [(&'static str, u64, u64)],
    /// gci i11/go: the `jdk.G1GarbageCollection.type` of a G1 pause
    /// ([`G1YoungPauseType::jfr_type`]); `None` off G1.
    g1_type: Option<&'static str>,
    /// gci i11/go: the `jdk.ExecuteVMOperation` name ([`jfr_vm_operation`]);
    /// `None` keeps the backend's historical spelling (ZGC).
    vm_operation: Option<&'static str>,
    /// gci i12/ge: the longest single pause of a collection made of several
    /// (a G1 full collection's ONE report); `None` = one pause.
    longest_ns: Option<u64>,
    /// zr z4/jf: the ZGC collection's own facts; `None` off ZGC and under
    /// `CRATONVM_ZGC_JFR_EVENTS=0`.
    zgc: Option<&'a ZgcCollectionJfr<'a>>,
    /// zs s7/jf: the ZGC collection's pause as HotSpot's named pause rows
    /// ([`zgc_hotspot_pause_rows`]); empty = one `GC Pause` row.
    pauses: &'a [(&'static str, u64, u64)],
    /// gci i20/ob: the Java thread that asked for the collection
    /// (`jdk.ExecuteVMOperation.caller`), see [`JfrRequester`]; `None` for
    /// a concurrent-cycle pause and for a door that named nobody.
    caller: Option<cratonvm_native_builtins_jfr::jfr_vm_more::JfrCallerThread<'a>>,
}

/// gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1): the
/// Java thread that asked for a collection, as the JFR backend keys it --
/// `Thread.tid`, the Java `Thread.name` and the OS thread id (0 = unknown).
/// HotSpot's `jdk.ExecuteVMOperation.caller` is the VM operation's
/// `calling_thread()`: `main` for its `System.gc()`, the allocating thread
/// for an allocation pause.
pub(super) struct JfrRequester {
    java_tid: i64,
    name: String,
    os_tid: i64,
}

impl JfrRequester {
    pub(super) fn as_caller(&self) -> cratonvm_native_builtins_jfr::jfr_vm_more::JfrCallerThread<'_> {
        cratonvm_native_builtins_jfr::jfr_vm_more::JfrCallerThread {
            java_tid: self.java_tid,
            name: &self.name,
            os_tid: self.os_tid,
        }
    }
}

/// gci i20/ob: [`JfrRequester`] for the registry thread `thread_id`, by the
/// rule `gc_and_alloc::full_gc_door`'s `jdk.SystemGC` writer uses (gci
/// i19/ob): the name from the Java `Thread` object (the registry's name is
/// empty for the primordial `main`), else the registry's; the tid from the
/// registry, else off the `Thread` object. `None` when no Java tid is known
/// (the row keeps `caller` 0). Reads the heap: call with the world running,
/// and only while a backend recording runs.
pub(super) fn jfr_requester_of(
    shared: &SharedVm,
    thread_id: crate::threading::jvm_thread::ThreadId,
) -> Option<JfrRequester> {
    let registry = &shared.threads.thread_registry;
    let thread_obj = registry.java_thread_obj(thread_id);
    let java_tid = registry.java_tid_of(thread_id).or_else(|| {
        thread_obj.and_then(|obj| crate::vm::vm_exec::read_java_thread_tid(shared, obj))
    })?;
    let name = requester_thread_name(shared, thread_id);
    Some(JfrRequester {
        java_tid: i64::try_from(java_tid).unwrap_or(0),
        name,
        os_tid: registry.os_tid_of(thread_id).map_or(0, i64::from),
    })
}

/// The Java name of the registry thread `thread_id` (gci i20/ob's rule, split
/// out by gci i27/ob for the `gc,jni` line): the Java `Thread` object's
/// `name` (the registry's is empty for the primordial `main`), else the
/// registry's, else empty. Reads the heap: call with the world running.
fn requester_thread_name(
    shared: &SharedVm,
    thread_id: crate::threading::jvm_thread::ThreadId,
) -> String {
    let registry = &shared.threads.thread_registry;
    let heap = &shared.mem.heap;
    registry
        .java_thread_obj(thread_id)
        .and_then(|obj| {
            crate::vm::vm_exec::thread_field_slot(shared, heap.class_id_of(obj), "name")
                .and_then(|slot| match heap.get_field(obj, slot) {
                    crate::types::Value::Object(Some(s)) => crate::vm::read_java_string(heap, s),
                    _ => None,
                })
        })
        .filter(|n| !n.is_empty())
        .or_else(|| registry.thread_name(thread_id))
        .unwrap_or_default()
}

/// gci i27/ob (gcg-g3x item 9): HotSpot 25's `gc,jni` debug line, written by
/// `GCLockerTimingDebugLogger` when `GCLocker::block()` returns in the
/// prologue of every collecting VM operation (`VM_GC_Operation`; not G1's
/// remark / cleanup, which are not one), on the requesting thread, before the
/// collection's own lines and without a `GC(n)` prefix. The wait it times is
/// for threads inside a JNI critical region; this VM's critical regions are
/// copies (`jni.rs` `JNI_CRITICAL_COPIES`) that never hold a collection off,
/// so the block is empty and the figure is HotSpot's usual `0ms`. Pure.
fn render_xlog_gc_jni_line(thread_name: &str) -> String {
    format!("Thread blocked to start GC. Resumed after 0ms. Thread \"{thread_name}\".")
}

/// gci i27/ob: is `-Xlog` taking `gc,jni` at debug?
fn xlog_gc_jni_on() -> bool {
    crate::runtime::unified_logging::is_unified_logging_enabled(
        &[crate::runtime::unified_logging::LogTag::GcJni],
        crate::runtime::unified_logging::LogLevel::Debug,
    )
}

/// gci i20/ob: [`gc_event_finish`] for a collection `requester` asked for:
/// its JFR rows name that thread as `jdk.ExecuteVMOperation.caller`. The
/// requester is resolved only when the collection reports events and a
/// backend recording runs (one registry read and one `Thread.name` read, the
/// world already released).
pub(super) fn gc_event_finish_by(
    shared: &SharedVm,
    door: GcDoor,
    start: Option<GcEventStart>,
    requester: Option<crate::threading::jvm_thread::ThreadId>,
) {
    // gci i27/ob: a G1 young report this requester parked and no drive
    // resolved (a path that did not drive) is printed before this pause's
    // lines, as `Normal`. One uncontended lock per G1 collection.
    if let Some(id) = requester.filter(|_| shared.mem.heap.is_g1()) {
        g1_emit_deferred_young_report(shared, id, false);
    }
    // gci i27/ob: the `gc,jni` line first -- HotSpot writes it in the VM
    // operation's prologue, before the pause's `gc,start`. Serial and G1 (the
    // backends whose lines are HotSpot's); not for a pause folded into a G1
    // full collection's one report (one VM operation there, one line, which
    // the folded pauses cannot attribute).
    // gci i47w/xl47: nor for a pause folded into its G1 full-collection
    // door's one operation (the door's own pause printed it).
    if let (Some(s), Some(id)) = (start.as_ref(), requester) {
        if s.full_scope_id.is_none()
            && xlog_serial_or_g1_lines(&shared.mem.heap)
            && xlog_gc_jni_on()
            && !g1_door_folds_this_pause(shared)
        {
            crate::runtime::unified_logging::log_unified(
                &[crate::runtime::unified_logging::LogTag::GcJni],
                crate::runtime::unified_logging::LogLevel::Debug,
                &render_xlog_gc_jni_line(&requester_thread_name(shared, id)),
            );
        }
    }
    let wanted = start.as_ref().is_some_and(|s| s.events)
        && cratonvm_native_builtins_jfr::jfr_vm_events::any_recording();
    let who = if wanted {
        requester.and_then(|id| jfr_requester_of(shared, id))
    } else {
        None
    };
    if let Some(report) = gc_event_finish_with(shared, door, start, who) {
        match requester {
            Some(id) => g1_park_deferred_young_report(shared, id, report),
            None => {
                emit_deferred_young_report(shared, &report, G1YoungPauseType::ConcurrentStart)
            }
        }
    }
    // gci i45/sl: the pause's `-Xlog:safepoint` line, after its `gc` lines.
    // gci i46/lg: held while a G1 young report parked above waits for its
    // label, and printed right after it.
    safepoint_line_finish_by(shared, door, requester);
}

/// zr z4/jf: [`JfrCollection`]'s ZGC half.
struct ZgcCollectionJfr<'a> {
    /// A whole-heap (`ZGC Major`) collection.
    major: bool,
    facts: &'a ZgcCycleFacts,
    /// zs s10/jf3: a concurrent cycle's span `(wall start ns, duration ns)`
    /// from its mark-start pause ([`zgc_cycle_shape`]); `None` = the
    /// collection's.
    cycle_span: Option<(u64, u64)>,
    /// zt t2/jf (R2): with [`Self::cycle_span`], the heap's `(used,
    /// committed)` at the cycle's mark-start pause: the `Before GC` heap
    /// summary is written at the span's start with these figures.
    cycle_heap_before: Option<(u64, u64)>,
}

/// gci i11/go (`gci-i11go-g1-concurrent-cycle-xlog-and-jfr-gc-event-review-findings-FIXED-20261017`
/// item 2): the HotSpot 25 VM operation a collection on `door` runs under --
/// `jdk.ExecuteVMOperation.operation` (`jdk.types.VMOperationType`). G1:
/// `G1CollectForAllocation` for every allocation pause (the recovery
/// compaction and a humongous request included), `G1CollectFull` for
/// `System.gc()` and `jcmd GC.run`. Serial (Generational):
/// `SerialCollectForAllocation` / `SerialGCCollect` (JDK 23+ names of the
/// former `GenCollectForAllocation` / `GenCollectFull`). Both:
/// `CollectForMetadataAllocation`, `GC_HeapInspection`, `HeapDumper`. ZGC:
/// `None` (unchanged). Pure apart from the backend match.
fn jfr_vm_operation(heap: &crate::memory::vm_heap::VmHeap, door: GcDoor) -> Option<&'static str> {
    let g1 = match heap {
        _ if heap.is_g1() => true,
        _ if heap.is_generational() => false,
        #[allow(unreachable_patterns)]
        _ => return None,
    };
    Some(vm_operation_name(door, g1))}

/// gci i12/ge (`gci-i11go-g1-concurrent-cycle-xlog-and-jfr-gc-event-review-findings-FIXED-20261017`
/// item 3): is a collection on `door` recorded as `jdk.OldGarbageCollection`
/// (else `jdk.YoungGarbageCollection`)? See [`jfr_old_event_rule`].
fn jfr_old_event(shared: &SharedVm, door: GcDoor, old: bool) -> bool {
    let (generational, g1) = match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => (true, false),
        _ if shared.mem.heap.is_g1() => (false, true),
        #[allow(unreachable_patterns)]
        _ => (false, false),
    };
    jfr_old_event_rule(
        door,
        old,
        (generational, g1),
        explicit_gc_invokes_concurrent(shared),
    )
}

/// `-XX:+ExplicitGCInvokesConcurrent` on this VM (`VmConfig::explicit_gc`):
/// HotSpot G1 then answers `System.gc()` and `jcmd GC.run` with a
/// concurrent-start young pause and a marking cycle, not a `Pause Full`.
fn explicit_gc_invokes_concurrent(shared: &SharedVm) -> bool {
    shared.config.explicit_gc == crate::config::ExplicitGcPolicy::Concurrent
}

/// [`jfr_old_event`], pure. A collection that reclaimed the old generation
/// (`old`: Serial's `SerialOld`, a G1 full collection's one report) is old.
/// Otherwise HotSpot 25's operation decides: the full-collection operations
/// -- `System.gc()` / `jcmd GC.run` (`G1CollectFull`, `SerialGCCollect`;
/// on G1 not under `-XX:+ExplicitGCInvokesConcurrent`, `explicit_concurrent`),
/// the live heap inspection and heap dump (`GC_HeapInspection`, `HeapDumper`)
/// and G1's pre-OOME `G1 Compaction Pause` -- are old; `Metadata GC
/// Threshold` is a full collection on Serial and a concurrent-start YOUNG
/// pause on G1; the allocation pauses are young. ZGC (out of scope) keeps the
/// recorder's historical rule, `cause.contains("System.gc")`. Pure.
fn jfr_old_event_rule(
    door: GcDoor,
    old: bool,
    (generational, g1): (bool, bool),
    explicit_concurrent: bool,
) -> bool {
    if old {
        return true;
    }
    if !generational && !g1 {
        return door == GcDoor::SystemGc;
    }
    match door {
        GcDoor::SystemGc | GcDoor::DiagnosticCommand => generational || !explicit_concurrent,
        // gci i27/ob: `_jvmti_force_gc` is never a user-requested concurrent
        // cause on G1: a full collection on both backends.
        GcDoor::HeapInspection | GcDoor::HeapDump | GcDoor::JvmtiForceGc => true,
        GcDoor::MetadataThreshold => generational,
        GcDoor::G1CompactionPause => g1,
        // zt t31/x10: `G1CollectFull` under `-XX:-G1PeriodicGCInvokesConcurrent`.
        GcDoor::G1PeriodicFull => g1,
        GcDoor::AllocationThreshold
        | GcDoor::AllocationFailure
        | GcDoor::G1HumongousAllocation
        | GcDoor::G1PeriodicCollection => false,
    }
}

/// gci i12/ge: is a G1 full collection on `door` (`force_full_collection`)
/// HotSpot G1's ONE `Pause Full (<cause>)`? `System.gc()` and `jcmd GC.run`
/// unless `-XX:+ExplicitGCInvokesConcurrent` (`explicit_concurrent`: then a
/// concurrent-start pause and a cycle), the live heap inspection and heap
/// dump. NOT `Metadata GC Threshold`: HotSpot G1 answers it with a
/// concurrent-start young pause (`start_concurrent_gc_for_metadata_allocation`),
/// so its pauses keep their own reports. The allocation doors never reach a
/// full collection. Pure.
fn g1_door_is_a_pause_full(door: GcDoor, explicit_concurrent: bool) -> bool {
    match door {
        GcDoor::SystemGc | GcDoor::DiagnosticCommand => !explicit_concurrent,
        GcDoor::HeapInspection | GcDoor::HeapDump | GcDoor::JvmtiForceGc => true,
        // zt t31/x10: `Pause Full (G1 Periodic Collection)`.
        GcDoor::G1PeriodicFull => true,
        GcDoor::MetadataThreshold
        | GcDoor::AllocationThreshold
        | GcDoor::AllocationFailure
        | GcDoor::G1CompactionPause
        | GcDoor::G1HumongousAllocation
        | GcDoor::G1PeriodicCollection => false,
    }
}

/// gci i13/gp: [`g1_door_is_a_pause_full`] with the metadata opt-in
/// (`metadata_cs` = `CRATONVM_G1_METADATA_CONCURRENT_START` is on). Under it
/// a `Metadata GC Threshold` FULL collection runs only where HotSpot's
/// `VM_CollectForMetadataAllocation` runs one -- the concurrent start was not
/// taken (a cycle was already running) or did not make the define fit
/// (`gc_and_alloc::force_gc_for_class_metadata`) -- and that is
/// HotSpot's `Pause Full (Metadata GC Threshold)` ("G1 full GC for
/// Metaspace"). Pure.
fn g1_full_door_is_a_pause_full(door: GcDoor, explicit_concurrent: bool, metadata_cs: bool) -> bool {
    g1_door_is_a_pause_full(door, explicit_concurrent)
        || (metadata_cs && door == GcDoor::MetadataThreshold)
}

/// zt t31/x10: does HotSpot 25 answer `force_full_collection`'s `door` with a
/// FULL collection (the one `CollectedHeap::full_gc_dump` brackets) on this
/// heap? Serial: every such door (`SerialHeap::collect` runs a young
/// collection only for the GC locker and WhiteBox causes); G1:
/// [`g1_full_door_is_a_pause_full`]; ZGC: never.
pub(super) fn door_runs_hotspot_full_collection(shared: &SharedVm, door: GcDoor) -> bool {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => true,
        _ if shared.mem.heap.is_g1() => g1_full_door_is_a_pause_full(
            door,
            explicit_gc_invokes_concurrent(shared),
            door == GcDoor::MetadataThreshold && g1_metadata_concurrent_start_enabled(),
        ),
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

/// `CRATONVM_G1_METADATA_CONCURRENT_START` (default OFF, gci i13/gp,
/// `runtime_flag_on`; `gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018`
/// item 5): G1 answers a `-XX:MaxMetaspaceSize` refusal with HotSpot's
/// `start_concurrent_gc_for_metadata_allocation` -- one `Pause Young
/// (Concurrent Start) (Metadata GC Threshold)` and a concurrent cycle it does
/// not wait for -- and runs the full collection only when a cycle was already
/// running or the define still does not fit. Read once per metadata
/// collection (rare) and once per G1 pause report on the metadata door; no
/// static.
pub(super) fn g1_metadata_concurrent_start_enabled() -> bool {
    false
}

/// [`jfr_vm_operation`]'s table, pure.
fn vm_operation_name(door: GcDoor, g1: bool) -> &'static str {
    match door {
        GcDoor::AllocationThreshold
        | GcDoor::AllocationFailure
        | GcDoor::G1CompactionPause
        | GcDoor::G1HumongousAllocation => {
            if g1 {
                "G1CollectForAllocation"
            } else {
                "SerialCollectForAllocation"
            }
        }
        // gci i27/ob: `collect(_jvmti_force_gc)` runs the same operation as
        // `System.gc()` (`VM_G1CollectFull` / `VM_SerialGCCollect`).
        GcDoor::SystemGc
        | GcDoor::DiagnosticCommand
        | GcDoor::JvmtiForceGc
        | GcDoor::G1PeriodicFull => {
            if g1 {
                "G1CollectFull"
            } else {
                "SerialGCCollect"
            }
        }
        // zt t31/x10: `try_collect`'s concurrent start for the periodic cause.
        GcDoor::G1PeriodicCollection => "G1TryInitiateConcMark",
        GcDoor::MetadataThreshold => "CollectForMetadataAllocation",
        GcDoor::HeapInspection => "GC_HeapInspection",
        GcDoor::HeapDump => "HeapDumper",
    }
}

// ---------------------------------------------------------------------------
// `-Xlog:safepoint` (gci i45/sl)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/real-jdk-mode.md`.
// Every pause of the VM's barrier that HotSpot 25 runs a safepoint for prints
// HotSpot's one `safepoint` info line (`unified_logging::safepoint_line`):
// opened inside the pause by its initiator (`gc_event_start`,
// `non_collection_pause_start`), stamped at the seal (`gc_event_seal`,
// `non_collection_pause_seal`) and printed after the release, after the
// pause's own `gc` lines (`gc_event_finish_by`, `non_collection_pause_finish`).
// Nothing runs unless `-Xlog` takes `safepoint` at info
// (`CRATONVM_XLOG_SAFEPOINT_LINES=0` kills the line).

/// gci i45/sl: open this pause's `safepoint` line with the barrier's figures
/// (`GcBarrier::safepoint_stats`, `GcBarrier::ttsp_stats`, the take-over's
/// post-quota time). Call INSIDE the pause, on its initiator, after the
/// take-over returned. `name`: the HotSpot VM operation when known here (a
/// non-collection pause), else `None`. One check when the tag is off.
fn safepoint_line_begin_for(shared: &SharedVm, name: Option<&'static str>) {
    let print_line = crate::runtime::unified_logging::xlog_safepoint_lines_wanted();
    // gcj j1/ob (residual 8 of
    // `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`):
    // and, on Generational only, HotSpot 25's `safepoint,stats` row
    // (`CRATONVM_GEN_XLOG_SAFEPOINT_STATS`). G1 / ZGC: the line alone, as before.
    let stats = shared.mem.heap.is_generational()
        && crate::runtime::unified_logging::xlog_safepoint_stats_wanted();
    if !print_line && !stats {
        return;
    }
    let sp = shared.mem.gc_barrier.safepoint_stats();
    let ttsp = shared.mem.gc_barrier.ttsp_stats();
    crate::runtime::unified_logging::safepoint_line_begin_with_stats(
        crate::runtime::unified_logging::SafepointBegin {
            vm: shared.vm_identity,
            application_ns: sp.application_ns,
            ttsp_pauses: ttsp.pauses,
            ttsp_last_ns: ttsp.last_ns,
            post_quota_ns: cratonvm_gc::gc_quiescence::xt_post_quota_ns(),
            // gci i47w/j45: HotSpot 25's `Threads: .. total` (one registry
            // read lock, only when the line is wanted).
            threads_total: u32::try_from(shared.threads.thread_registry.alive_count())
                .unwrap_or(u32::MAX),
            name,
        },
        print_line,
        stats,
    );
}

/// gci i47w/xl47 (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`,
/// residual 7 of `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`):
/// while held, the COLLECTION pauses the calling OS thread initiates are part
/// of its G1 full-collection door's ONE HotSpot VM operation. HotSpot 25 runs
/// `System.gc()` / `jcmd GC.run` / JVMTI `ForceGarbageCollection` /
/// `Metadata GC Threshold` as one operation (`VM_G1CollectFull`,
/// `VM_G1TryInitiateConcMark` under `-XX:+ExplicitGCInvokesConcurrent`,
/// `VM_CollectForMetadataAllocation`): one `Safepoint` line, one `gc,jni`
/// line, one `sun.rt.safepoints`. This VM's door runs its own collection
/// pause (which prints and counts as that operation), then a marking cycle
/// and the full-compaction pause (`gc_and_alloc::g1_full_compaction_pause`),
/// which printed a second line under the door's name (`sl45_s_g1`:
/// `{G1CollectFull=10}` for 5 calls; `sl45_i_g1_conc`: two
/// `G1TryInitiateConcMark` per call; `sp44_s_g1`: count 0 -> 10). Held by
/// `force_full_collection` around its G1 cycle and compaction only; the
/// marking pauses (initial mark, remark) are named by their own rule
/// ([`non_collection_safepoint_begin`]). Off G1: a no-op. A field of the VM
/// (`AllocLadderState::g1_door_safepoint_folds`), keyed by the OS thread,
/// as the one-report scope's owner is; nested holds stack.
pub(super) struct G1DoorSafepointFold<'a> {
    shared: &'a SharedVm,
    thread: Option<std::thread::ThreadId>,
}

/// gci i47w/xl47: enter the calling thread's [`G1DoorSafepointFold`]. One
/// uncontended lock on G1; nothing elsewhere.
pub(super) fn g1_door_safepoint_fold(shared: &SharedVm) -> G1DoorSafepointFold<'_> {
    let thread = shared.mem.heap.is_g1().then(|| std::thread::current().id());
    if let Some(t) = thread {
        shared.mem.alloc_ladder.g1_door_safepoint_folds.lock().push(t);
    }
    G1DoorSafepointFold { shared, thread }
}

impl Drop for G1DoorSafepointFold<'_> {
    fn drop(&mut self) {
        if let Some(t) = self.thread {
            let mut folds = self.shared.mem.alloc_ladder.g1_door_safepoint_folds.lock();
            if let Some(at) = folds.iter().rposition(|x| *x == t) {
                folds.remove(at);
            }
        }
    }
}

/// gci i47w/xl47: is the calling thread inside a [`G1DoorSafepointFold`] of
/// this VM? `false` off G1 without a lock; one uncontended lock on G1 (once
/// per collection pause, inside it, and once at its finish).
pub(super) fn g1_door_folds_this_pause(shared: &SharedVm) -> bool {
    if !shared.mem.heap.is_g1() {
        return false;
    }
    let folds = shared.mem.alloc_ladder.g1_door_safepoint_folds.lock();
    !folds.is_empty() && folds.contains(&std::thread::current().id())
}

/// gci i45/sl: the HotSpot 25 VM operation a collection on `door` is a
/// safepoint of, on this VM's backend ([`safepoint_vm_operation`]).
fn collection_safepoint_name(shared: &SharedVm, door: GcDoor) -> Option<&'static str> {
    let shape = match &shared.mem.heap {
        _ if shared.mem.heap.is_generational() => (true, false),
        _ if shared.mem.heap.is_g1() => (false, true),
        #[allow(unreachable_patterns)]
        _ => (false, false),
    };
    safepoint_vm_operation(door, shape, explicit_gc_invokes_concurrent(shared))}

/// gci i45/sl: the `Safepoint "<op>"` name of a collection on `door`, pure.
/// The JFR table ([`vm_operation_name`]: `SerialCollectForAllocation` /
/// `SerialGCCollect` for Serial-shaped Generational, `G1CollectForAllocation`
/// / `G1CollectFull` for G1, `CollectForMetadataAllocation` on both), except:
/// * G1 `System.gc()` / `jcmd GC.run` under `-XX:+ExplicitGCInvokesConcurrent`
///   is HotSpot's `VM_G1TryInitiateConcMark` (`try_collect_concurrently`),
///   `G1TryInitiateConcMark`;
/// * the live heap inspection's and heap dump's collection print nothing: on
///   HotSpot it runs inside the ONE `GC_HeapInspection` / `HeapDumper`
///   operation whose line the following heap walk's pause prints
///   ([`non_collection_safepoint_name`]);
/// * ZGC: `None` here; since zt t22/hunt3 its collection names its own
///   lines inside the pause, at the seal ([`zgc_safepoint_ops`]).
fn safepoint_vm_operation(
    door: GcDoor,
    (generational, g1): (bool, bool),
    explicit_concurrent: bool,
) -> Option<&'static str> {
    if !generational && !g1 {
        return None;
    }
    match door {
        GcDoor::HeapInspection | GcDoor::HeapDump => None,
        GcDoor::SystemGc | GcDoor::DiagnosticCommand if g1 && explicit_concurrent => {
            Some("G1TryInitiateConcMark")
        }
        _ => Some(vm_operation_name(door, g1)),
    }
}

/// gci i45/sl: the `Safepoint "<op>"` name of a non-collection pause, pure.
/// `None` for a pause HotSpot 25 runs no safepoint of its own for: the
/// compiled-loop exit and inline-cache grace handshakes (HotSpot's handshakes
/// print no safepoint line), the Generational concurrent cycle's two pauses
/// (Serial has no concurrent cycle), G1's initial mark (the young pause's
/// concurrent start on HotSpot, whose `G1CollectForAllocation` line this
/// VM's young pause already prints) and ZGC's mark start (out of scope).
fn non_collection_safepoint_name(kind: NonCollectionPause) -> Option<&'static str> {
    match kind {
        // A thread dump / cross-thread stack walk: `VM_ThreadDump`.
        NonCollectionPause::FrameTrace => Some("ThreadDump"),
        NonCollectionPause::HeapDump => Some("HeapDumper"),
        NonCollectionPause::HeapInspection => Some("GC_HeapInspection"),
        NonCollectionPause::Redefinition => Some("RedefineClasses"),
        // Remark and cleanup run as one pause here (HotSpot: two lines,
        // `G1PauseRemark` then `G1PauseCleanup`).
        NonCollectionPause::G1Remark => Some("G1PauseRemark"),
        NonCollectionPause::LoopExit
        | NonCollectionPause::IcGrace
        | NonCollectionPause::GenInitialMark
        | NonCollectionPause::GenRemark
        | NonCollectionPause::G1InitialMark
        | NonCollectionPause::ZgcMarkStart
        | NonCollectionPause::ZgcMarkEndContinue => None,
    }
}

/// gci i45/sl: prints the calling thread's open `safepoint` line when dropped
/// -- at every exit of [`non_collection_pause_finish`], after the pause's own
/// lines. Not while unwinding.
struct SafepointLineOnDrop;

impl Drop for SafepointLineOnDrop {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::runtime::unified_logging::safepoint_line_finish(None);
        }
    }
}

/// gci i46/lg: open a non-collection pause's `safepoint` line
/// ([`safepoint_line_begin_for`]) under its HotSpot 25 operation
/// ([`non_collection_safepoint_name_on`]); `part_of_another` (a marking
/// pause folded into a G1 full collection's report, the stop-the-world full
/// mark's remark) names none. A pause with no operation is no HotSpot
/// safepoint (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual 6):
/// the barrier counts nothing for it (`GcBarrier::note_pause_not_a_safepoint`,
/// `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0` counts it), before the line reads
/// the barrier's figures. ZGC's mark start (out of scope, and a safepoint on
/// HotSpot) keeps its count. Call INSIDE the pause, on its initiator.
fn non_collection_safepoint_begin(
    shared: &SharedVm,
    kind: NonCollectionPause,
    part_of_another: bool,
) {
    let name = if part_of_another {
        None
    } else {
        non_collection_safepoint_name_on(shared, kind)    };
    if non_collection_pause_is_no_safepoint(kind, name) {
        shared.mem.gc_barrier.note_pause_not_a_safepoint();
    }
    safepoint_line_begin_for(shared, name);
}

/// gci i46/lg (residual 6): a non-collection pause named `name` is no
/// HotSpot safepoint -- it has no operation -- unless it is ZGC's mark start.
/// Pure.
fn non_collection_pause_is_no_safepoint(
    kind: NonCollectionPause,
    name: Option<&'static str>,
) -> bool {
    name.is_none()
        && !matches!(
            kind,
            NonCollectionPause::ZgcMarkStart | NonCollectionPause::ZgcMarkEndContinue
        )
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 2): `CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS`, default ON (`=0`: every
/// frame-trace pause is `ThreadDump`, as gci i45/sl had it). Read only for a
/// frame-trace pause inside a `GcBarrier::frame_trace_op_scope`.
fn xlog_safepoint_frame_trace_ops_enabled() -> bool {
    true
}

/// gci i46/lg: [`non_collection_safepoint_name`], except that a frame-trace
/// pause inside the initiator's `GcBarrier::frame_trace_op_scope` is the
/// operation the scope names -- `None` for a handshake (a single thread's
/// `Thread.getStackTrace`: HotSpot 25's `GetStackTraceHandshakeClosure`, no
/// safepoint), `PrintThreads` for `jcmd Thread.print` (`VM_PrintThreads`).
fn non_collection_safepoint_name_on(
    shared: &SharedVm,
    kind: NonCollectionPause,
) -> Option<&'static str> {
    if kind == NonCollectionPause::FrameTrace {
        if let Some(op) = shared.mem.gc_barrier.frame_trace_op() {
            if xlog_safepoint_frame_trace_ops_enabled() {
                return op;
            }
        }
    }
    // zt t22/hunt3 (`CRATONVM_ZGC_XLOG_SAFEPOINT_LINES`): a concurrent
    // cycle's own mark-start pause is HotSpot's `ZMarkStartYoungAndOld`, the
    // operation its JFR row carries. Only under the tag.
    if kind == NonCollectionPause::ZgcMarkStart
        && crate::runtime::unified_logging::xlog_safepoint_lines_wanted()
        && zgc_xlog_safepoint_lines_enabled()
    {
        return Some(ZGC_MARK_START_MAJOR_OPERATION);
    }
    // zt t24/zc8: a failed Pause Mark End is HotSpot 25's `ZMarkEndOld`
    // operation (the name a whole-heap collection's mark end carries).
    if kind == NonCollectionPause::ZgcMarkEndContinue
        && crate::runtime::unified_logging::xlog_safepoint_lines_wanted()
        && zgc_xlog_safepoint_lines_enabled()
    {
        return Some(ZGC_MARK_END_OLD_OPERATION);
    }
    non_collection_safepoint_name(kind)
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 3): `CRATONVM_XLOG_SAFEPOINT_AFTER_PARKED_REPORT`, default ON (`=0`: the
/// line prints at the pause's finish, before a parked young report's `gc`
/// lines, as gci i45/sl had it). Read once per G1 collection a thread
/// requested, only under `-Xlog:safepoint`.
fn xlog_safepoint_after_parked_report_enabled() -> bool {
    true
}

/// gci i46/lg: has `requester` a G1 young report parked for its mark-cycle
/// drive ([`G1DeferredYoungReport`])? One uncontended lock.
fn g1_young_report_parked_for(
    shared: &SharedVm,
    requester: crate::threading::jvm_thread::ThreadId,
) -> bool {
    shared
        .mem
        .alloc_ladder
        .g1_deferred_young
        .lock()
        .iter()
        .any(|slot| slot.requester == requester.0)
}

/// gci i46/lg (residual 3): [`gc_event_finish_by`]'s `safepoint` line. HotSpot
/// writes a young pause's `gc` lines inside its VM operation and the
/// `safepoint` line after it; a G1 young report this VM parked for the
/// requester's mark-cycle drive prints after the pause's finish, so its line
/// is HELD (rendered now, decorated with the seal instant) and printed right
/// after the report, by the release in [`g1_emit_deferred_young_report`]. A
/// line of this requester still held from an earlier pause is printed first.
/// Elsewhere (no requester, not G1, no `-Xlog:safepoint`, nothing parked, the
/// kill switch) the line prints now, as before.
fn safepoint_line_finish_by(
    shared: &SharedVm,
    door: GcDoor,
    requester: Option<crate::threading::jvm_thread::ThreadId>,
) {
    let name = collection_safepoint_name(shared, door);
    match requester {
        Some(id)
            if shared.mem.heap.is_g1()
                && crate::runtime::unified_logging::xlog_safepoint_lines_wanted()
                && xlog_safepoint_after_parked_report_enabled() =>
        {
            crate::runtime::unified_logging::safepoint_line_release_held(id.0);
            if g1_young_report_parked_for(shared, id) {
                crate::runtime::unified_logging::safepoint_line_hold(name, id.0);
            } else {
                crate::runtime::unified_logging::safepoint_line_finish(name);
            }
        }
        _ => crate::runtime::unified_logging::safepoint_line_finish(name),
    }
}

/// Emit one collection's JFR events: the real backend's GC cycle and the
/// legacy recorder's heap summaries, GC, phase, old / young events.
fn emit_jfr_collection(shared: &SharedVm, door: GcDoor, c: &JfrCollection<'_>) {
    // gci i9/jc: the phase rows as nested rows: only their part inside this
    // collection's window, so a level-1 row never runs past its `GC Pause`.
    let clipped = phase_rows_within(
        c.phases,
        c.start_ns,
        c.start_ns.saturating_add(c.duration_ns),
    );
    let c = &JfrCollection {
        phases: &clipped,
        ..*c
    };
    // gci i12/ge (`gci-i11go-...-review-findings-20261013` item 3): the
    // young / old event kind, from the door and the backend (it was
    // `cause.contains("System.gc")` in the backend recorder and `old` alone in
    // the legacy one, so the two disagreed on a G1 `System.gc()`).
    let old_event = jfr_old_event(shared, door, c.old);
    // g1gen/c1b: G1's / Generational's reference counts of this collection,
    // taken whether or not a recording runs, so a later collection never
    // reports them.
    let gc_reference_counts = if heap_is_zgc(shared) {
        None
    } else {
        shared.debug.jfr_gc_reference_counts.lock().take()    };
    let zgc_real = c
        .zgc
        .map(|z| cratonvm_native_builtins_jfr::jfr_vm_more::ZgcCycleJfr {
            major: z.major,
            relocation_set: z.facts.relocation_set,
            relocation_groups: &z.facts.relocation_groups,
            uncommit: z.facts.uncommit,
            young_tenuring_threshold: zgc_young_tenuring_threshold(shared, z.major, c.cause),
            // zs s8/jf2: majors only, as HotSpot 25.
            reference_counts: z.facts.reference_counts.filter(|_| z.major),
            cycle_span: z.cycle_span,
            cycle_heap_before: z.cycle_heap_before.map(|(used, committed)| {
                let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
                (i(used), i(committed))
            }),
            pause_vm_operations: zgc_jfr_pause_vm_operations_enabled(),
        });
    if cratonvm_native_builtins_jfr::jfr_vm_events::any_recording() {
        let i = |b: usize| i64::try_from(b).unwrap_or(i64::MAX);
        // gci i33/ms: `jdk.MetaspaceSummary`'s committed / reserved and its
        // `classSpace` (HotSpot's statistics, one walk).
        let metaspace = metaspace_stats_now(shared);
        let w = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
        cratonvm_native_builtins_jfr::jfr_vm_more::gc_cycle(
            shared.vm_identity,
            &cratonvm_native_builtins_jfr::jfr_vm_more::GcCycle {
                gc_id: c.gc_id as i32,
                name: c.name,
                cause: c.cause,
                start_nanos: c.start_ns,
                duration_nanos: c.duration_ns,
                heap_before: i(c.heap_before),
                committed_before: i(c.committed_before),
                heap_after: i(c.heap_after),
                committed_after: i(c.committed_after),
                max: i(shared.config.max_heap_size),
                old: c.old,
                old_event,
                tenuring_threshold: shared
                    .mem
                    .heap
                    .tenuring_threshold()
                    .map(|t| i32::try_from(t).unwrap_or(i32::MAX)),
                phases: c.phases,
                metaspace_used: w(metaspace.used),
                metaspace_spaces: metaspace.class_space.map(|(cu, cc, cr)| {
                    [w(metaspace.committed), w(metaspace.reserved), w(cu), w(cc), w(cr)]
                }),
                g1_type: c.g1_type,
                vm_operation: c.vm_operation,
                longest_pause_nanos: c.longest_ns,
                zgc: zgc_real.as_ref(),
                pauses: c.pauses,
                gc_thread: c.zgc.and_then(|z| zgc_driver_thread(z.major)),
                reference_counts: gc_reference_counts
                    .map(|counts| counts.map(|n| i64::try_from(n).unwrap_or(i64::MAX))),
                caller: c.caller,
                metaspace_chunk_free_lists: jfr_metaspace_chunk_free_lists(
                    heap_is_zgc(shared),
                    zgc_real.is_some(),
                ),
            },
        );
    }
    if cratonvm_jfr::is_enabled() {
        // JFR's `gcId` is an `int`; wrap rather than saturate so ids stay
        // distinct for longer than `i32::MAX` collections would ever last.
        let id = c.gc_id as i32; // Truncation: deliberate wrap, see above.
        let to_i64 = |b: usize| i64::try_from(b).unwrap_or(i64::MAX);
        let max = to_i64(shared.config.max_heap_size);
        let end_ns = c.start_ns.saturating_add(c.duration_ns);
        let mut jfr = shared.debug.flight_recorder.lock();
        // zt t2/jf (R2): a ZGC concurrent cycle's `Before GC` is the heap at
        // its mark-start pause, at the cycle span's start.
        let (before_used, before_committed, before_at) =
            match c.zgc.and_then(|z| z.cycle_span.zip(z.cycle_heap_before)) {
                Some(((at, _), (used, committed))) => (
                    i64::try_from(used).unwrap_or(i64::MAX),
                    i64::try_from(committed).unwrap_or(i64::MAX),
                    at,
                ),
                None => (
                    to_i64(c.heap_before),
                    to_i64(c.committed_before),
                    c.start_ns,
                ),
            };
        cratonvm_jfr::builtin::emit_gc_heap_summary_event(
            &mut jfr,
            id,
            "Before GC",
            "Java Heap",
            before_used,
            // gen r5w2/obs6: the committed heap as it was BEFORE (it was the
            // after figure on both edges).
            before_committed,
            max,
            before_at,
        );
        if c.pauses.is_empty() {
            cratonvm_jfr::builtin::emit_gc_event(
                &mut jfr,
                id,
                c.name,
                c.cause,
                c.start_ns,
                c.duration_ns,
            );
        } else {
            // zs s7/jf: `sumOfPauses` / `longestPause` over the named pauses.
            let sum = c.pauses.iter().map(|p| p.2).fold(0u64, u64::saturating_add);
            let longest = c.pauses.iter().map(|p| p.2).max().unwrap_or(0);
            // zs s10/jf3: a ZGC concurrent cycle spans the cycle.
            let (span_start, span_ns) = c
                .zgc
                .and_then(|z| z.cycle_span)
                .unwrap_or((c.start_ns, c.duration_ns));
            cratonvm_jfr::builtin::emit_gc_event_with_pauses(
                &mut jfr,
                id,
                c.name,
                c.cause,
                span_start,
                span_ns,
                sum,
                longest,
            );
        }
        // `jdk.GCPhasePause` (gen r4w2/obs): the generational collector's phase
        // marks, stashed by `collect_garbage_inner`'s pause timer while a
        // recording runs. Consecutive from the pause start, plus the residual
        // as its own `other` phase, so the rows partition the collector's
        // pause exactly as the `[gcpause]` line does.
        //
        // gci i8/jf: HotSpot G1 records a top-level `GC Pause` row for every
        // stop-the-world collection (`STWGCTimer::register_gc_start`); this
        // recorder recorded none on G1 (its collections carry no phase marks),
        // so a G1 recording had `jdk.GarbageCollection` rows with no pause
        // phase at all.
        //
        // gci i9/jc: HotSpot Serial does the same, and nests the collection's
        // phases one level down (`jdk.GCPhasePauseLevel1`). This recorder
        // wrote the Generational phase marks as level-0 rows with no `GC
        // Pause` parent, while the `jdk.jfr` backend (`jfr_vm_more::gc_cycle`)
        // already wrote HotSpot's shape: the two recorders disagreed on the
        // same collection. Now Generational and G1 both get the `GC Pause`
        // row and level-1 phases (clipped to the collection's window by the
        // caller's `phase_rows_within`); ZGC keeps its level-0 rows.
        let nests = jfr_collection_nests_phases(&shared.mem.heap);
        if c.zgc.is_some() {
            // zr z4/jf (merge of origin/dev, 2026-10-15): the ZGC
            // collection's one pause at level 0 and the collector's laps
            // under it (below), as the real backend writes them.
            // zs s7/jf: HotSpot's named pauses in place of the one row.
            if c.pauses.is_empty() {
                cratonvm_jfr::builtin::emit_gc_phase_pause_event(
                    &mut jfr,
                    id,
                    ZGC_COLLECTION_PAUSE,
                    c.start_ns,
                    c.duration_ns,
                );
            }
            for &(name, start_ns, dur_ns) in c.pauses {
                cratonvm_jfr::builtin::emit_gc_phase_pause_event(
                    &mut jfr, id, name, start_ns, dur_ns,
                );
            }
        } else if nests {
            cratonvm_jfr::builtin::emit_gc_phase_pause_event(
                &mut jfr,
                id,
                "GC Pause",
                c.start_ns,
                c.duration_ns,
            );
        }
        for &(name, start_ns, dur_ns) in c.phases {
            if nests || c.zgc.is_some() {
                cratonvm_jfr::builtin::emit_gc_phase_pause_level1_event(
                    &mut jfr, id, name, start_ns, dur_ns,
                );
            } else {
                cratonvm_jfr::builtin::emit_gc_phase_pause_event(&mut jfr, id, name, start_ns, dur_ns);
            }
        }
        if let Some(z) = c.zgc {
            emit_zgc_legacy_rows(&mut jfr, id, c, z);
        } else if let Some(counts) = gc_reference_counts {
            // g1gen/c1b: G1's / Generational's `jdk.GCReferenceStatistics`.
            let at = c.start_ns.saturating_add(c.duration_ns);
            for (name, count) in cratonvm_native_builtins_jfr::jfr_vm_more::REFERENCE_TYPE_NAMES
                .iter()
                .copied()
                .zip(counts)
            {
                let count = i64::try_from(count).unwrap_or(i64::MAX);
                cratonvm_jfr::builtin::emit_gc_reference_statistics_event(&mut jfr, id, name, count, at);
            }
        }
        // gci i12/ge: the same kind as the backend recorder on Generational
        // and G1 (`old_event`); ZGC keeps `old` (out of scope, unchanged).
        let legacy_old = if nests { old_event } else { c.old };
        if legacy_old {
            cratonvm_jfr::builtin::emit_old_gc_event(&mut jfr, id, c.start_ns, c.duration_ns);
        } else if let Some(threshold) = shared.mem.heap.tenuring_threshold() {
            // `VmHeap::tenuring_threshold` per backend (gc-common w3-f,
            // 2026-09-23; `handoff-w2e-comment-followups-outside-lane-e.md`
            // item 8): Generational's `PROMOTION_AGE - 1` (HotSpot's unit since
            // gen r5w2/obs6; it was `PROMOTION_AGE`, one too high), G1's ADAPTIVE threshold
            // (this arm passed the literal `15`, G1's configured ceiling,
            // whatever the policy had lowered it to), and `None` on ZGC --
            // its collection is not a young-generation collection, and
            // HotSpot's non-generational ZGC emits no young event either.
            cratonvm_jfr::builtin::emit_young_gc_event(
                &mut jfr,
                id,
                i32::try_from(threshold).unwrap_or(i32::MAX),
                c.start_ns,
                c.duration_ns,
            );
        }
        cratonvm_jfr::builtin::emit_gc_heap_summary_event(
            &mut jfr,
            id,
            "After GC",
            "Java Heap",
            to_i64(c.heap_after),
            to_i64(c.committed_after),
            max,
            end_ns,
        );
    }
}

/// zr z4/jf: the legacy recorder's ZGC rows of one collection
/// (`ZOldGarbageCollection` for a whole-heap one, the relocation set and its
/// groups, the uncommit). `jdk.ZYoungGarbageCollection` is not written: the
/// VM's ZGC states no tenuring threshold.
fn emit_zgc_legacy_rows(
    jfr: &mut cratonvm_jfr::FlightRecorder,
    id: i32,
    c: &JfrCollection<'_>,
    z: &ZgcCollectionJfr<'_>,
) {
    if z.major {
        // zs s10/jf3: a concurrent cycle's old event spans the cycle.
        let (cs, cd) = z.cycle_span.unwrap_or((c.start_ns, c.duration_ns));
        cratonvm_jfr::builtin::emit_zgc_old_gc_event(jfr, id, cs, cd);
    }
    if let Some((total, empty, relocate)) = z.facts.relocation_set {
        cratonvm_jfr::builtin::emit_zgc_relocation_set_event(
            jfr, total, empty, relocate, c.start_ns,
        );
        for &(ty, cand, total, empty, sel, reloc) in &z.facts.relocation_groups {
            cratonvm_jfr::builtin::emit_zgc_relocation_set_group_event(
                jfr, ty, cand, total, empty, sel, reloc, c.start_ns,
            );
        }
    }
    if let Some((bytes, nanos)) = z.facts.uncommit {
        cratonvm_jfr::builtin::emit_zgc_uncommit_event(
            jfr,
            bytes,
            c.start_ns,
            u64::try_from(nanos).unwrap_or(0),
        );
    }
    // zs s7/jf: `jdk.GCReferenceStatistics`, at the end of the collection;
    // zs s8/jf2: majors only (HotSpot 25: 4 rows per major, none per minor).
    if let Some(counts) = z.facts.reference_counts.filter(|_| z.major) {
        let at = c.start_ns.saturating_add(c.duration_ns);
        for (name, count) in cratonvm_native_builtins_jfr::jfr_vm_more::REFERENCE_TYPE_NAMES
            .iter()
            .copied()
            .zip(counts)
        {
            cratonvm_jfr::builtin::emit_gc_reference_statistics_event(jfr, id, name, count, at);
        }
    }
}

/// zs s7/jf: the tenuring threshold a ZGC collection's
/// `jdk.ZYoungGarbageCollection` carries, with
/// `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS` on: a young (`ZGC Minor`)
/// collection of the generational mode, its promotion age; zt t1/jf
/// (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`): a whole-heap collection,
/// [`zgc_major_young_threshold`]. `None` writes no event.
fn zgc_young_tenuring_threshold(shared: &SharedVm, major: bool, cause: &str) -> Option<i32> {
    if !zgc_jfr_ref_and_young_enabled() {
        return None;
    }
    let generational = shared.mem.heap.zgc_jfr_tenuring_threshold();
    if major {
        return true
            .then(|| zgc_major_young_threshold(generational, cause));
    }
    generational.map(|t| i32::try_from(t).unwrap_or(i32::MAX))
}

/// zt t1/jf: the `tenuringThreshold` of the young collection inside a ZGC
/// major. HotSpot 25 ZGC is generational only, and its major always starts
/// with a young collection (`ZDriverMajor`: `collect_young` before
/// `collect_old`), which `jdk.ZYoungGarbageCollection` reports under the
/// major's `gcId`. This collector's whole-heap collection marks and keeps
/// the young objects with the old in one pass, so:
///
/// * the single-generation mode (the default, `generational == None`) has
///   no young generation left after a major: every survivor is in the one
///   generation, HotSpot's "promote all" (threshold 0);
/// * the generational mode keeps its promotion age, except on the
///   `System.gc()` door, whose major tenures every survivor (gci i48/zl3's
///   `CRATONVM_GC_SYSTEM_GC_TENURES_ALL`; HotSpot's explicit major promotes
///   all too): 0.
///
/// Pure.
fn zgc_major_young_threshold(generational: Option<u32>, cause: &str) -> i32 {
    match generational {
        Some(t) if cause != "System.gc()" => i32::try_from(t).unwrap_or(i32::MAX),
        _ => 0,
    }
}



/// gcg h1/ob: one escalated pause's split, as [`gc_event_finish`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EscalatedPauseSplit {
    /// The young half's end (the major's start).
    at: std::time::Instant,
    /// The young half's share of the collection's duration.
    young_ns: u64,
    /// The old generation's used bytes at the mark.
    old_used_at_mark: u64,
}

/// gcg h1/ob: whether [`gc_event_finish`] reports this collection as two, and
/// how: only a sealed Generational collection whose heap booked a split
/// (`GenerationalHeap::take_escalation_split`: an UNREQUESTED major after a
/// completed young half, booked under `Copy` too), that reclaimed the old
/// generation, with both bean snapshots, under the switch.
fn escalated_pause_split(start: &GcEventStart, duration_ns: u64) -> Option<EscalatedPauseSplit> {
    let esc = start.escalation?;
    if start.sealed.is_none()
        || start.beans.is_none()
        || start.beans_after.is_none()
        || !cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle()
    {
        return None;
    }
    Some(EscalatedPauseSplit {
        at: esc.at,
        young_ns: nanos_between(start.t0, esc.at).min(duration_ns),
        // Widening: usize to u64 on every supported target.
        old_used_at_mark: esc.old_used_at_mark as u64,
    })
}

/// gcg h1/ob: the pools at the end of an escalated pause's young half. The
/// major that followed touched only the old generation, so the young pools are
/// the pause's end figures; `Tenured Gen` is what the major's decision saw.
/// Pure.
fn escalation_mid_pools(
    after: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
    old_used_at_mark: u64,
) -> Vec<cratonvm_gc::gc_metrics::JmxPoolUsage> {
    after
        .iter()
        .map(|p| {
            if p.name == cratonvm_gc::gc_metrics::JMX_POOL_TENURED {
                cratonvm_gc::gc_metrics::JmxPoolUsage {
                    used: old_used_at_mark,
                    committed: p.committed.max(old_used_at_mark),
                    ..*p
                }
            } else {
                *p
            }
        })
        .collect()
}

/// gcg h1/ob: the heap occupancy at the end of an escalated pause's young
/// half: the end figure with the old generation's share replaced by the
/// mark's. Pure.
fn escalation_mid_heap_used(
    heap_after: usize,
    after: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
    old_used_at_mark: u64,
) -> usize {
    let old_after = after
        .iter()
        .find(|p| p.name == cratonvm_gc::gc_metrics::JMX_POOL_TENURED)
        .map_or(0, |p| usize::try_from(p.used).unwrap_or(usize::MAX));
    heap_after
        .saturating_sub(old_after)
        .saturating_add(usize::try_from(old_used_at_mark).unwrap_or(usize::MAX))
}

/// gcg h1/ob: the `Heap After GC` figures of an escalated pause's young half
/// (and its full half's `Heap Before GC`): the end figures with the tenured
/// space at the mark's occupancy and the metaspace as the pause found it (the
/// young half unloads no class). Pure.
fn escalation_mid_exit_heap(
    after: &crate::runtime::unified_logging::SerialExitHeap,
    old_used_at_mark: u64,
    metaspace_before: Option<(u64, u64, u64)>,
) -> crate::runtime::unified_logging::SerialExitHeap {
    let mut mid = *after;
    mid.old.used = old_used_at_mark;
    mid.the.used = old_used_at_mark;
    mid.the.bounds[1] = mid.the.bounds[0].saturating_add(old_used_at_mark);
    if let Some(m) = metaspace_before {
        mid.metaspace = m;
    }
    mid
}

/// gcg h1/ob: `invocations=<i> (full <f>)` at the end of an escalated pause's
/// young half: one young collection more than the pause found, no full one.
fn escalation_mid_invocations((invocations, full): (u64, u64)) -> (u64, u64) {
    (invocations.saturating_add(1), full)
}

/// gcg h1/ob: an escalated pause's GC notifications, split at the mark: the
/// young bean's record ends there with the young half's pools, the full
/// bean's starts there with them as its before-usage. Pure.
fn split_escalated_notifications(
    recs: &mut [cratonvm_gc::gc_metrics::GcNotificationRecord],
    mark: std::time::Instant,
    mid_pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
) {
    use cratonvm_gc::gc_metrics::{JMX_SERIAL_OLD_COLLECTOR, JMX_SERIAL_YOUNG_COLLECTOR};
    for rec in recs.iter_mut() {
        if rec.collector == JMX_SERIAL_YOUNG_COLLECTOR {
            // The notification's wall-clock stamp moves back with its end.
            let early_ms = rec.end.saturating_duration_since(mark).as_millis();
            rec.timestamp_ms = rec
                .timestamp_ms
                .saturating_sub(u64::try_from(early_ms).unwrap_or(u64::MAX));
            rec.end = mark.max(rec.start);
            rec.after = mid_pools.to_vec();
        } else if rec.collector == JMX_SERIAL_OLD_COLLECTOR {
            rec.start = mark.min(rec.end);
            rec.before = mid_pools.to_vec();
        }
    }
}

/// gcg h1/ob: an escalated pause's `(user, sys)` CPU delta shared between its
/// two halves in proportion to their wall time (one `getrusage` pair brackets
/// the whole pause; there is no sample at the mark). Pure.
fn split_cpu_delta(
    (user, sys): (f64, f64),
    young_ns: u64,
    total_ns: u64,
) -> ((f64, f64), (f64, f64)) {
    let share = if total_ns == 0 {
        0.0
    } else {
        young_ns as f64 / total_ns as f64
    };
    (
        (user * share, sys * share),
        (user * (1.0 - share), sys * (1.0 - share)),
    )
}

/// gcg h1/ob: the `-Xlog` lines of an escalated pause reported as HotSpot
/// Serial reports a promotion failure (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full`,
/// measured on 25.0.3): the young half's `gc,start`, `Heap Before GC`,
/// `gc,age`, `gc,heap`, `gc,metaspace`, `Heap After GC`, `gc` and `gc,cpu`
/// lines with `GC(young_id)` (gch h2/ob: and the deferred reference block,
/// rendered with that id), then the full half's with `GC(young_id + 1)`:
/// `gc,start`, `Heap Before GC`, the `Phase 1..4` pairs, `gc,heap`,
/// `gc,metaspace`, `Heap After GC`, `gc`, `gc,cpu`. No `Promotion failed` line: this collector's young half never
/// fails a promotion (an object old gen cannot take stays young), so the
/// escalation is the occupancy major, HotSpot's next-pause `Pause Full`.
/// Called for a sealed Generational collection only
/// ([`escalated_pause_split`]).
#[allow(clippy::too_many_arguments)]
fn xlog_escalated_collection_lines(
    shared: &SharedVm,
    young_id: u64,
    door: GcDoor,
    start: &GcEventStart,
    sp: EscalatedPauseSplit,
    (heap_mid, heap_after, committed): (usize, usize, usize),
    (young_ns, full_ns): (u64, u64),
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag, SerialExitHeap};
    let full_id = young_id.wrapping_add(1);
    let cause = door.cause();
    let (Some(beans_before), Some(beans_after)) =
        (start.beans.as_ref(), start.beans_after.as_ref())
    else {
        return;
    };
    let mid_pools = escalation_mid_pools(&beans_after.pools, sp.old_used_at_mark);
    let inv_before = serial_invocations(beans_before);
    let inv_mid = escalation_mid_invocations(inv_before);
    let inv_after = serial_invocations(beans_after);
    let debug_after = start.heap_debug_after.as_ref();
    let debug_mid = debug_after.map(|a| {
        let mut mid = escalation_mid_exit_heap(
            a,
            sp.old_used_at_mark,
            start.heap_debug_before.as_ref().map(|b| b.metaspace),
        );
        // gci i33/ms: its `class space` line as the pause found it too.
        if let Some(b) = start.heap_debug_before.as_ref() {
            mid.class_space = b.class_space;
        }
        mid
    });
    let heap_debug = |gc_id: u64, when: &str, heap: Option<&SerialExitHeap>, inv: (u64, u64)| {
        let Some(heap) = heap else {
            return;
        };
        let (heap_lines, meta_lines) = render_xlog_heap_debug_blocks(gc_id, when, inv, heap);
        for line in heap_lines {
            log_unified(&[LogTag::GcHeap], LogLevel::Debug, &line);
        }
        for line in meta_lines {
            log_unified(&[LogTag::GcMetaspace], LogLevel::Debug, &line);
        }
    };
    let heap_info = xlog_on(LogTag::GcHeap);
    let heap_lines = |gc_id: u64,
                      before: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
                      after: &[cratonvm_gc::gc_metrics::JmxPoolUsage]| {
        if !heap_info {
            return;
        }
        if let Some(lines) = render_xlog_heap_lines(gc_id, before, after) {
            for line in lines {
                log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
            }
        }
    };
    let metaspace_after = start
        .metaspace_before
        .map(|_| metaspace_after_bytes(shared, start));
    let cpu = if xlog_on(LogTag::GcCpu) {
        match (start.cpu_start, process_cpu_secs()) {
            (Some((u0, s0)), Some((u1, s1))) => Some(split_cpu_delta(
                (u1 - u0, s1 - s0),
                young_ns,
                young_ns.saturating_add(full_ns),
            )),
            _ => None,
        }
    } else {
        None
    };

    // The young half. gci i30/ob (g3x item 7): `gc,start` decorated with the
    // pause's start, where HotSpot writes it. Hand-back 3: and only with `gc`.
    if xlog_start_line_on(LogTag::GcStart, LogTag::Gc) {
        crate::runtime::unified_logging::log_unified_at(
            &[LogTag::GcStart],
            LogLevel::Info,
            &format!("GC({young_id}) Pause Young ({cause})"),
            Some(start.t0),
        );
    }
    heap_debug(
        young_id,
        "Before",
        start.heap_debug_before.as_ref(),
        inv_before,
    );
    // gci i28/tl: `TLAB totals` with the young half only -- HotSpot's full
    // half finds every TLAB already retired and prints none.
    crate::runtime::unified_logging::log_tlab_totals(start.tlab_totals.as_deref(), young_id);
    // gch h2/ob: the deferred reference-processing block (`gc,ref` /
    // `gc,phases,ref` at debug) was rendered INSIDE the pause with the id the
    // collection's lines carry before `gc_event_finish` advances the count --
    // the young id. `flush_deferred_gc_lines` prints only the lines of the id
    // it is given and drops the rest, so the h1 call with `full_id` dropped
    // the whole block under the split. The pause processed references once;
    // the block is printed with the young half, before `gc,age`, as the
    // unsplit path prints it.
    crate::runtime::unified_logging::flush_deferred_gc_lines(young_id);
    if shared.mem.heap.is_generational() {
        if let Some((desired, next, max, ages)) = shared.mem.heap.gen_take_tenuring_xlog() {
            for (level, line) in render_xlog_age_lines(young_id, desired, next, max, &ages) {
                log_unified(&[LogTag::GcAge], level, &line);
            }
        }
    }
    heap_lines(
        young_id,
        beans_before.pools.as_slice(),
        mid_pools.as_slice(),
    );
    if let Some(before) = start.metaspace_before {
        log_unified(
            &[LogTag::GcMetaspace],
            LogLevel::Info,
            &render_xlog_metaspace_line(young_id, &before, &before),
        );
    }
    heap_debug(young_id, "After", debug_mid.as_ref(), inv_mid);
    if xlog_on(LogTag::Gc) {
        crate::runtime::unified_logging::gc_info(&render_xlog_gc_line(
            young_id,
            "Pause Young",
            cause,
            start.heap_used_before,
            heap_mid,
            committed,
            young_ns,
        ));
    }
    if let Some(((user, sys), _)) = cpu {
        log_unified(
            &[LogTag::GcCpu],
            LogLevel::Info,
            &render_xlog_cpu_line(young_id, user, sys, young_ns),
        );
    }

    // The full half (gci i30/ob: decorated with the young half's end).
    if xlog_start_line_on(LogTag::GcStart, LogTag::Gc) {
        crate::runtime::unified_logging::log_unified_at(
            &[LogTag::GcStart],
            LogLevel::Info,
            &format!("GC({full_id}) Pause Full ({cause})"),
            Some(start.t0 + std::time::Duration::from_nanos(young_ns)),
        );
    }
    heap_debug(full_id, "Before", debug_mid.as_ref(), inv_mid);
    // gci i30/ob hand-back 3: the scope's own set arms the pair.
    let full_phases = xlog_on(LogTag::GcPhases)
        && crate::runtime::unified_logging::xlog_full_gc_phases_enabled();
    let phase = |index: usize, ns: u64, open: bool, close: bool| {
        let (start_line, end_line) =
            crate::runtime::unified_logging::xlog_full_gc_phase_lines(full_id, index, ns);
        if open {
            log_unified(&[LogTag::GcPhasesStart], LogLevel::Info, &start_line);
        }
        if close {
            log_unified(&[LogTag::GcPhases], LogLevel::Info, &end_line);
        }
    };
    if full_phases {
        // gch h2/og (X3): the old collection's real split, when THIS pause
        // stamped one (a split left by an earlier collection ended before `t0`).
        let phase_ns: [u64; 4] = cratonvm_gc::gen_heap::take_last_old_gc_phase_split()
            .filter(|s| s.ended_at >= start.t0)
            .map_or([full_ns, 0, 0, 0], |s| s.serial_phases_ns(full_ns));
        phase(0, phase_ns[0], true, false);
        // (The deferred reference block went with the young half; see there.)
        phase(0, phase_ns[0], false, true);
        for (index, &ns) in phase_ns
            .iter()
            .enumerate()
            .take(crate::runtime::unified_logging::SERIAL_FULL_GC_PHASES.len())
            .skip(1)
        {
            phase(index, ns, true, true);
        }
    }
    heap_lines(full_id, mid_pools.as_slice(), beans_after.pools.as_slice());
    if let (Some(before), Some(after)) = (start.metaspace_before, metaspace_after) {
        log_unified(
            &[LogTag::GcMetaspace],
            LogLevel::Info,
            &render_xlog_metaspace_line(full_id, &before, &after),
        );
    }
    if start.heap_debug_before.is_some() {
        heap_debug(full_id, "After", debug_after, inv_after);
    }
    if xlog_on(LogTag::Gc) {
        crate::runtime::unified_logging::gc_info(&render_xlog_gc_line(
            full_id,
            "Pause Full",
            cause,
            heap_mid,
            heap_after,
            committed,
            full_ns,
        ));
    }
    if let Some((_, (user, sys))) = cpu {
        log_unified(
            &[LogTag::GcCpu],
            LogLevel::Info,
            &render_xlog_cpu_line(full_id, user, sys, full_ns),
        );
    }
}

/// The `-Xlog` lines of one collection, in HotSpot Serial's order
/// (gen r5w3/obs7, 2026-09-26;
/// `gengc-r5w2-obs6-proposal-xlog-gc-hotspot-compatible-output`):
///
/// ```text
/// [gc,start    ] GC(3) Pause Young (Allocation Failure)
/// [gc,heap     ] GC(3) DefNew: 17472K(19648K)->2176K(19648K) Eden: 17472K(17472K)->0K(17472K) From: 0K(2176K)->2176K(2176K)
/// [gc,heap     ] GC(3) Tenured: 0K(43712K)->3000K(43712K)
/// [gc          ] GC(3) Pause Young (Allocation Failure) 17M->5M(61M) 4.567ms
/// [gc,cpu      ] GC(3) User=0.00s Sys=0.00s Real=0.00s
/// ```
///
/// The `gc` line is every backend's (unchanged); the `gc,start`, `gc,heap`
/// and `gc,cpu` lines are printed on the Generational backend only, whose
/// pools are Serial's (`DefNew` = eden + survivor, `From` = the survivor
/// pool, `Tenured` = the old generation, all from the JMX pool snapshots the
/// collection already takes at both edges). gci i11/go: G1 prints
/// `gc,start`, `gc,metaspace` and `gc,cpu` in the same grammar and its own
/// four `gc,heap` region lines ([`render_xlog_g1_heap_lines`]), with G1's
/// label (`Pause Young (Normal|Mixed)`) and cause (`G1 Evacuation Pause` for
/// an allocation pause, [`GcDoor::cause_on`]). ZGC's HotSpot counterparts
/// have a different grammar and are not attempted.
///
/// The `gc,start` line is printed with the others after the pause (no I/O
/// inside it), so its uptime decoration is the collection's end, not its
/// start. Also printed: `gc,metaspace` (gcf f4/r), `gc,age` (gcf f4/r), the
/// debug `Heap|Metaspace Before|After GC` blocks and the deferred
/// `gc,ref` / `gc,phases,ref` block (gcg g3/x). `gc,phases`: a FULL
/// collection's four `Phase 1..4` pairs at info under
/// `CRATONVM_XLOG_GC_FULL_PHASES` (gcg g4/yg): this collector's default full
/// collection is an in-place mark-sweep, so Phase 1 carries the whole
/// pause and Phases 2-4 print `0.000ms` (shape parity, not a phase
/// breakdown; `gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 5). The exit
/// summary is `gc,exit`, printed once at exit
/// (`unified_logging::log_gc_exit_summary`, gcf f2/r).
#[allow(clippy::too_many_arguments)]
fn xlog_collection_lines(
    shared: &SharedVm,
    gc_id: u64,
    label: &str,
    full: bool,
    door: GcDoor,
    start: &GcEventStart,
    (heap_after, committed): (usize, usize),
    duration_ns: u64,
    zgc_pauses: &[(&'static str, u64, u64)],
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let generational = shared.mem.heap.is_generational();
    // gci i11/go: G1 prints `gc,start`, `gc,heap`, `gc,metaspace` and
    // `gc,cpu` too, with its own label and cause (`G1 Evacuation Pause`).
    let serial_or_g1 = xlog_serial_or_g1_lines(&shared.mem.heap);
    // Merge of origin/dev (2026-10-15): `GcEventStart::cause` is
    // `GcDoor::cause_on` plus zr z1/tr's ZGC causes.
    let cause = start.cause(door);
    // gci i30/ob (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016` item 7): still
    // written after the pause (no I/O inside it; the label is known only at
    // its end), but decorated with the pause's start (`start.t0`, taken once
    // the world stopped), where HotSpot's `GCTraceTime` writes it.
    // gci i30/ob hand-back 3: HotSpot writes it only when `gc` is selected too
    // (`xlog_start_line_on`).
    if serial_or_g1 && xlog_start_line_on(LogTag::GcStart, LogTag::Gc) {
        crate::runtime::unified_logging::log_unified_at(
            &[LogTag::GcStart],
            LogLevel::Info,
            &format!("GC({gc_id}) {label} ({cause})"),
            Some(start.t0),
        );
        // zt t31/x10: a before-dump of this collection, inside its scope.
        flush_deferred_full_gc_dump_lines();
    }
    // gcg g3/x: the debug `Heap Before GC` and `Metaspace Before GC` blocks,
    // right after `gc,start` as on HotSpot Serial (young and full alike).
    let heap_debug =
        |when: &str,
         heap: Option<&crate::runtime::unified_logging::SerialExitHeap>,
         beans: Option<&cratonvm_gc::gc_metrics::GcBeanSnapshot>| {
            let (Some(heap), Some(beans)) = (heap, beans) else {
                return;
            };
            let (heap_lines, meta_lines) =
                render_xlog_heap_debug_blocks(gc_id, when, serial_invocations(beans), heap);
            for line in heap_lines {
                log_unified(&[LogTag::GcHeap], LogLevel::Debug, &line);
            }
            for line in meta_lines {
                log_unified(&[LogTag::GcMetaspace], LogLevel::Debug, &line);
            }
        };
    heap_debug(
        "Before",
        start.heap_debug_before.as_ref(),
        start.beans.as_ref(),
    );
    // gci i14/hd (item 9): G1's before blocks, at the same place.
    log_g1_heap_debug(
        gc_id,
        "Before",
        start.g1_heap_debug_before.as_ref(),
        start.beans.as_ref(),
    );
    // gci i28/tl (gcg-g3x item 9): `TLAB totals`, after the before blocks.
    crate::runtime::unified_logging::log_tlab_totals(start.tlab_totals.as_deref(), gc_id);
    // gci i30/ob hand-back 2: the refill census's witness (off unless
    // `RUST_LOG` selects the target; after the pause, no I/O inside it).
    tracing::info!(
        target: "cratonvm::tlab_census",
        "[tlab-census] GC({}) line={} vm_allocated_at_start={}",
        gc_id,
        start.tlab_totals.is_some(),
        start.allocated_at_start
    );
    // gcg g4/yg (obs7 item 3; default on since g5, `=0` kill): Serial's full-collection phases. The
    // reference-processing block belongs INSIDE Phase 1 (HotSpot processes
    // references while marking), so the deferred lines flush between its
    // start and end lines.
    let full_phases = generational
        && full
        // gci i30/ob hand-back 3: armed by `gc,phases` alone, as HotSpot's scope.
        && xlog_on(LogTag::GcPhases)
        && crate::runtime::unified_logging::xlog_full_gc_phases_enabled();
    let phase = |index: usize, ns: u64, open: bool, close: bool| {
        let (start_line, end_line) =
            crate::runtime::unified_logging::xlog_full_gc_phase_lines(gc_id, index, ns);
        if open {
            log_unified(&[LogTag::GcPhasesStart], LogLevel::Info, &start_line);
        }
        if close {
            log_unified(&[LogTag::GcPhases], LogLevel::Info, &end_line);
        }
    };
    // gch h2/og (X3): the old collection's real split, when THIS pause stamped
    // one (a split left by an earlier collection ended before `start.t0`).
    let phase_ns: [u64; 4] = if full_phases {
        cratonvm_gc::gen_heap::take_last_old_gc_phase_split()
            .filter(|s| s.ended_at >= start.t0)
            .map_or([duration_ns, 0, 0, 0], |s| s.serial_phases_ns(duration_ns))
    } else {
        [duration_ns, 0, 0, 0]
    };
    if full_phases {
        phase(0, phase_ns[0], true, false);
    }
    // gcg g3/x: the reference-processing block the pause deferred
    // (`gc,ref` / `gc,phases,ref` at debug), before `gc,age` as on HotSpot.
    crate::runtime::unified_logging::flush_deferred_gc_lines(gc_id);
    if full_phases {
        phase(0, phase_ns[0], false, true);
        for (index, &ns) in phase_ns
            .iter()
            .enumerate()
            .take(crate::runtime::unified_logging::SERIAL_FULL_GC_PHASES.len())
            .skip(1)
        {
            phase(index, ns, true, true);
        }
    }
    // zt t31/x10: HotSpot Serial's after-dump, after the phases and before
    // `gc,heap` (`SerialFullGC::invoke_at_safepoint`'s order).
    if generational && full {
        run_owed_full_gc_dump_after(shared, gc_id);
    }
    // gcf f4/r: `gc,age` (debug: the desired survivor size and threshold;
    // trace: the age table), between `gc,start` and `gc,heap` as on HotSpot
    // Serial, for a young collection that filled its age table.
    // gcg g5/yg (vg3 `x3_heapdbg_full`): HotSpot Serial prints `gc,age` from
    // the young collection only (`DefNewGeneration` adjusts the threshold);
    // a `Pause Full` prints none. This VM's full collection runs a young
    // cycle first and stashes its table, so the stash is TAKEN (it must not
    // leak into the next collection) and printed only on a young pause.
    if shared.mem.heap.is_generational() {
        if let Some((desired, next, max, ages)) = shared.mem.heap.gen_take_tenuring_xlog() {
            if xlog_age_lines_printed_for(full) {
                for (level, line) in render_xlog_age_lines(gc_id, desired, next, max, &ages) {
                    log_unified(&[LogTag::GcAge], level, &line);
                }
            }
        }
    }
    // gci i22/sz: HotSpot G1's `gc,age` lines (`update_survivors_policy`'s
    // desired size and threshold at debug, `G1Policy::print_age_table` at
    // trace) for a young or mixed pause: the pair the pause USED and the
    // table it FILLED (`G1Collector::take_tenuring_xlog`, `None` after a
    // remark / cleanup pause; taken either way so it cannot leak).
    if shared.mem.heap.is_g1() {
        if let Some((desired, threshold, max, ages)) = shared.mem.heap.g1_take_tenuring_xlog() {
            if xlog_age_lines_printed_for(full) {
                for (level, line) in render_xlog_age_lines(gc_id, desired, threshold, max, &ages) {
                    log_unified(&[LogTag::GcAge], level, &line);
                }
            }
        }
    }
    let heap_info = generational && xlog_on(LogTag::GcHeap);
    // The after-collection beans: sealed inside the pause, or sampled late
    // (once) for a door that did not seal.
    let late_beans =
        if start.beans_after.is_none() && (heap_info || start.heap_debug_before.is_some()) {
            gc_bean_snapshot(shared)
        } else {
            None
        };
    let after_beans = start.beans_after.as_ref().or(late_beans.as_ref());
    if heap_info {
        if let (Some(before), Some(after)) = (start.beans.as_ref(), after_beans) {
            if let Some(lines) = render_xlog_heap_lines(gc_id, &before.pools, &after.pools) {
                for line in lines {
                    log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
                }
            }
        }
    }
    // gci i11/go (`gci-i10rh-...-review-findings-20261013` finding 5): HotSpot
    // G1's four `gc,heap` region lines (`G1HeapTransition::print`), from the
    // region census taken at the pause's two edges. Nothing for a door that
    // did not seal (no after edge).
    if let (Some(before), Some(after)) = (start.g1_regions_before, start.g1_regions_after) {
        if xlog_on(LogTag::GcHeap) {
            for line in render_xlog_g1_heap_lines(gc_id, before, after) {
                log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
            }
        }
    }
    // gcf f4/r: HotSpot Serial's order is `gc,heap`, `gc,metaspace`, `gc`
    // (young and full collections alike: re-measured by gcg g3/x).
    if let Some(before) = start.metaspace_before {
        log_unified(
            &[LogTag::GcMetaspace],
            LogLevel::Info,
            &render_xlog_metaspace_line(gc_id, &before, &metaspace_after_bytes(shared, start)),
        );
    }
    // gcg g3/x: the debug `Heap After GC` / `Metaspace After GC` blocks,
    // before the `gc` line as on HotSpot Serial.
    if start.heap_debug_before.is_some() {
        let late_heap;
        let after_heap = match start.heap_debug_after.as_ref() {
            Some(h) => Some(h),
            None => {
                late_heap = crate::runtime::unified_logging::serial_exit_heap(shared);
                late_heap.as_ref()
            }
        };
        heap_debug("After", after_heap, after_beans);
    }
    // gci i14/hd (item 9): G1's after blocks (sealed doors only).
    log_g1_heap_debug(gc_id, "After", start.g1_heap_debug_after.as_ref(), after_beans);
    let zgc_label = matches!(label, ZGC_MAJOR_COLLECTION | ZGC_MINOR_COLLECTION);
    if zgc_label {
        // gcd d10/o retirement: HotSpot 25 ZGC's start and end lines.
        // zt t5/jf (R9): between them, as HotSpot 25.0.3 prints them inside
        // the collection (`-Xlog:gc*` capture), the pause lines (`gc,phases`)
        // and the metaspace line (`gc,metaspace`) of this collection
        // (`CRATONVM_ZGC_XLOG_PAUSE_PHASES`).
        let [start_line, end_line] = render_xlog_zgc_lines(
            gc_id,
            label,
            cause,
            start.heap_used_before,
            heap_after,
            shared.mem.heap.heap_capacity(),
            duration_ns,
        );
        let gc = xlog_on(LogTag::Gc);
        if gc {
            crate::runtime::unified_logging::gc_info(&start_line);
        }
        if zgc_xlog_pause_phases_enabled() {
            let minor = label == ZGC_MINOR_COLLECTION;
            // zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`): the failed Pause
            // Mark End attempts of the cycle this collection closed, taken
            // whether or not they print (empty with the switch off). zt
            // t31/zc11 (1f R3): the seal took them inside the pause; only an
            // unsealed door takes them here.
            let mark_continue = match (&start.zgc_mark_continue, &shared.mem.heap) {
                (Some(sealed), _) => sealed.clone(),
                (None, h) if h.is_zgc() => h.zgc_take_mark_continue_log(),
                _ => Vec::new(),
            };
            let phases = xlog_on(LogTag::GcPhases);
            // zt t31/zc11 (1f R6): HotSpot's framed block, every phase line.
            let frames = if phases && zgc_xlog_phase_frames_enabled() {
                render_xlog_zgc_phase_frames(&ZgcXlogPhaseFacts {
                    gc_id,
                    minor,
                    precleans: zgc_major_precleans_young(cause),
                    pauses: zgc_pauses,
                    attempts: &mark_continue,
                    conc_mark_ns: start.zgc_conc_mark_ns,
                    heap_before: start.heap_used_before,
                    heap_after,
                    capacity: shared.mem.heap.heap_capacity(),
                    duration_ns,
                })
            } else {
                Vec::new()
            };
            let pause_lines = if phases && frames.is_empty() {
                let mut lines = render_xlog_zgc_pause_lines(gc_id, zgc_pauses, minor);
                splice_xlog_zgc_mark_continue_lines(gc_id, &mut lines, &mark_continue);
                lines
            } else {
                Vec::new()
            };
            let metaspace =
                xlog_on(LogTag::GcMetaspace).then(|| metaspace_stats_now(shared).triple());
            // zt t6/ob4 (R9 tail): the `gc,heap` capacity lines, after the
            // metaspace line as in HotSpot 25.0.3's `-Xlog:gc*` block, with
            // the `gc,init` block's figures.
            let heap_blocks: Vec<(&'static str, Vec<String>)> = if xlog_on(LogTag::GcHeap) {
                let cfg = &shared.config;
                let max = cfg.max_heap_size;
                let min = crate::runtime::unified_logging::hotspot_min_heap_size(
                    cfg.min_heap_size,
                    cfg.initial_heap_size.min(max),
                );
                let soft_max = shared.mem.heap.soft_max_capacity().unwrap_or(max);
                if zgc_xlog_heap_statistics_enabled() {
                    // zt t7/x4b: HotSpot's block per generation -- a major
                    // prints the `Y:` triple and table, then the `O:` ones; a
                    // minor its `y:` ones. The six table points of this
                    // stop-the-world collection: nothing allocates inside the
                    // pause, so mark start, mark end and relocate start read
                    // the occupancy before it, relocate end the one after.
                    // zt t9/x6: with the generation tables
                    // (`zgc_xlog_heap_blocks`).
                    let gen_tables = zgc_xlog_generation_statistics_enabled();
                    // zt t10/gn2: a minor's young table over the young
                    // generation (`CRATONVM_ZGC_XLOG_GEN_YOUNG_FIGURES`).
                    let young_split = if minor && gen_tables {
                        zgc_xlog_young_split_figures(&shared.mem.heap)
                    } else {
                        None
                    };
                    // zt t16/gn6 (item 12): the stall clause of HotSpot's
                    // pre-clean rule (`CRATONVM_ZGC_XLOG_STALL_PRECLEAN=1`).
                    let stalled_for_old = !minor
                        && gen_tables
                        && zgc_xlog_stall_preclean_enabled()
                        && shared.mem.heap.zgc_last_collection_after_alloc_failure();
                    // zt t31/x10 (item 1, `CRATONVM_ZGC_XLOG_EDGE_BOOK`):
                    // the collection's own edges -- its mark start when the
                    // mark ran concurrently, and the slide's copy volume.
                    let edges = if zgc_xlog_edge_book_enabled() {
                        ZgcXlogEdges {
                            mark_start_used: shared.mem.heap.zgc_last_mark_start_used(),
                            compacted: shared
                                .mem
                                .heap
                                .zgc_slide_moved_bytes_total()
                                .saturating_sub(start.zgc_moved_before),
                        }
                    } else {
                        ZgcXlogEdges::default()
                    };
                    zgc_xlog_heap_blocks_with(
                        gc_id,
                        minor,
                        cause,
                        [min, max, soft_max, committed],
                        start.heap_used_before,
                        heap_after,
                        gen_tables,
                        young_split,
                        stalled_for_old,
                        edges,
                    )
                } else {
                    let generation = if minor { "y" } else { "O" };
                    let lines = render_xlog_zgc_capacity_lines(gc_id, minor, min, max, soft_max);
                    vec![(generation, Vec::from(lines))]
                }
            } else {
                Vec::new()
            };
            // zt t9/x6: in HotSpot's order (`CRATONVM_ZGC_XLOG_GENERATION_ORDER`).
            // zt t31/x10 (item 2, `CRATONVM_ZGC_XLOG_LOAD_MMU`): the
            // per-generation load and MMU lines, from this pause.
            let stats = zgc_xlog_stat_lines(shared, start.t0, duration_ns);
            for (tag, line) in zgc_xlog_collection_lines_framed(
                gc_id,
                minor,
                zgc_xlog_generation_order_enabled(),
                pause_lines,
                frames,
                metaspace,
                heap_blocks,
                &stats,
            ) {
                log_unified(&[tag], LogLevel::Info, &line);
            }
        }
        if gc {
            crate::runtime::unified_logging::gc_info(&end_line);
        }
    } else if xlog_on(LogTag::Gc) {
        // gci i13/gp (ef finding 2): a G1 pause whose evacuation failed
        // carries HotSpot's suffix on this end line only.
        crate::runtime::unified_logging::gc_info(&with_evacuation_failure_suffix(
            render_xlog_gc_line(
                gc_id,
                label,
                cause,
                start.heap_used_before,
                heap_after,
                committed,
                duration_ns,
            ),
            cause,
            start.g1_evacuation_failed,
        ));
    }
    if serial_or_g1 && xlog_on(LogTag::GcCpu) {
        if let (Some((user0, sys0)), Some((user1, sys1))) = (start.cpu_start, process_cpu_secs()) {
            log_unified(
                &[LogTag::GcCpu],
                LogLevel::Info,
                &render_xlog_cpu_line(gc_id, user1 - user0, sys1 - sys0, duration_ns),
            );
        }
    }
}

/// HotSpot 25 G1's `gc,heap` lines (`G1HeapTransition::print`, info level):
///
/// ```text
/// GC(0) Eden regions: 25->0(22)
/// GC(0) Survivor regions: 0->3(4)
/// GC(0) Old regions: 0->2
/// GC(0) Humongous regions: 0->0
/// ```
///
/// Region counts at the pause's two edges. The two capacities are HotSpot's:
/// Eden's is the young target after the pause minus the survivors after it
/// (`young_list_target_length() - after._survivor_length`), Survivor's the
/// maximum survivor regions, which HotSpot derives from the young target as
/// `ceil(target / SurvivorRatio)` (default 8) and which is taken here from the
/// target at the START (HotSpot's figure was fixed before the pause). The
/// counts are this VM's; the capacities are the same formulas over this VM's
/// adaptive target, so they agree with HotSpot's shape, not its numbers.
/// gci i11/go (finding 5). Pure.
fn render_xlog_g1_heap_lines(
    gc_id: u64,
    before: G1RegionEdge,
    after: G1RegionEdge,
) -> [String; 4] {
    let (b, a) = (before.census, after.census);
    let eden_capacity = after.young_target_regions.saturating_sub(a.survivor.1);
    let survivor_capacity = before.young_target_regions.div_ceil(before.survivor_ratio.max(1));
    [
        format!("GC({gc_id}) Eden regions: {}->{}({eden_capacity})", b.eden.1, a.eden.1),
        format!(
            "GC({gc_id}) Survivor regions: {}->{}({survivor_capacity})",
            b.survivor.1, a.survivor.1
        ),
        format!("GC({gc_id}) Old regions: {}->{}", b.old.1, a.old.1),
        format!("GC({gc_id}) Humongous regions: {}->{}", b.humongous.1, a.humongous.1),
    ]
}

/// HotSpot's default `-XX:SurvivorRatio` (G1's maximum survivor regions are
/// `ceil(young target / SurvivorRatio)`). gci i11/go.
const G1_SURVIVOR_RATIO: usize = 8;

/// HotSpot Serial's two `gc,heap` lines (`SerialHeap::print_heap_change`,
/// `HEAP_CHANGE_FORMAT`: `name: usedK(capacityK)->usedK(capacityK)`, bytes
/// divided by 1024 and truncated) from the pool snapshots at the two edges
/// of the collection. `None` unless both snapshots carry Serial's three
/// pools. gen r5w3/obs7.
fn render_xlog_heap_lines(
    gc_id: u64,
    before: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
    after: &[cratonvm_gc::gc_metrics::JmxPoolUsage],
) -> Option<[String; 2]> {
    use cratonvm_gc::gc_metrics::{JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED};
    let find = |pools: &[cratonvm_gc::gc_metrics::JmxPoolUsage], name: &str| {
        pools
            .iter()
            .find(|p| p.name == name)
            .map(|p| (p.used, p.committed))
    };
    let edges = |name: &str| Some((find(before, name)?, find(after, name)?));
    let (eden0, eden1) = edges(JMX_POOL_EDEN)?;
    let (from0, from1) = edges(JMX_POOL_SURVIVOR)?;
    let (old0, old1) = edges(JMX_POOL_TENURED)?;
    let change = |name: &str, (u0, c0): (u64, u64), (u1, c1): (u64, u64)| {
        format!(
            "{name}: {}K({}K)->{}K({}K)",
            u0 / 1024,
            c0 / 1024,
            u1 / 1024,
            c1 / 1024
        )
    };
    // gcg g1/c: split each edge's young usage as the exit summary does, so
    // `DefNew` is `Eden + From` in the printed KiB (the pools carve the
    // survivors byte for byte, and two truncations printed `DefNew` one KiB
    // above the sum).
    let split = crate::runtime::unified_logging::serial_young_kib_split;
    let [young0, eden0, from0] = split(eden0, from0);
    let [young1, eden1, from1] = split(eden1, from1);
    Some([
        format!(
            "GC({gc_id}) {} {} {}",
            change("DefNew", young0, young1),
            change("Eden", eden0, eden1),
            change("From", from0, from1),
        ),
        format!("GC({gc_id}) {}", change("Tenured", old0, old1)),
    ])
}

/// HotSpot's `gc,cpu` line (`GCTraceCPUTime`: `User=%3.2fs Sys=%3.2fs
/// Real=%3.2fs`). A negative delta (a clock that does not move per process
/// on this platform) prints as 0. gen r5w3/obs7.
fn render_xlog_cpu_line(gc_id: u64, user_s: f64, sys_s: f64, duration_ns: u64) -> String {
    format!(
        "GC({gc_id}) User={:.2}s Sys={:.2}s Real={:.2}s",
        user_s.max(0.0),
        sys_s.max(0.0),
        duration_ns as f64 / 1_000_000_000.0,
    )
}

/// Lay a collection's phase marks out as consecutive `jdk.GCPhasePause` rows:
/// `(phase, start_ns, duration_ns)`, starting at `start_ns`, followed by an
/// `other` row holding the part of `pause_us` no mark covered (always emitted,
/// like `[gcpause]`'s `other=`, so a future uncovered phase is visible rather
/// than silently missing from the sum). gen r4w2/obs.
fn phase_pause_rows(
    start_ns: u64,
    pause_us: u128,
    marks: &[(&'static str, u128)],
) -> Vec<(&'static str, u64, u64)> {
    let to_ns = |us: u128| u64::try_from(us.saturating_mul(1000)).unwrap_or(u64::MAX);
    let mut rows = Vec::with_capacity(marks.len() + 1);
    let mut at = start_ns;
    let mut covered: u128 = 0;
    for (name, us) in marks {
        let d = to_ns(*us);
        rows.push((*name, at, d));
        at = at.saturating_add(d);
        covered = covered.saturating_add(*us);
    }
    rows.push(("other", at, to_ns(pause_us.saturating_sub(covered))));
    rows
}

/// gci i9/jc: the part of each phase row `(name, start ns, duration ns)` that
/// lies in the collection window `[lo, hi]`, in order: a row is cut to the
/// window, a row wholly outside it is dropped, and a zero-length row is kept
/// when its instant is inside the window. The rows nest as HotSpot's
/// `jdk.GCPhasePauseLevel1` rows do inside their `GC Pause`; for an escalated
/// pause reported as two collections, each half keeps the phases of its own
/// window. Rows already inside the window are returned unchanged. Pure.
fn phase_rows_within(
    rows: &[(&'static str, u64, u64)],
    lo: u64,
    hi: u64,
) -> Vec<(&'static str, u64, u64)> {
    rows.iter()
        .filter_map(|&(name, start, dur)| {
            let end = start.saturating_add(dur);
            if dur == 0 {
                return (lo <= start && start <= hi).then_some((name, start, 0));
            }
            let (s, e) = (start.max(lo), end.min(hi));
            // `then`, not `then_some`: the argument of `then_some` is evaluated
            // first, and `e - s` underflows for a row outside the window.
            (s < e).then(|| (name, s, e - s))
        })
        .collect()
}

/// gci i9/jc: does the legacy recorder nest this heap's collection phases
/// under a top-level `GC Pause` row (HotSpot Serial's and G1's shape)?
/// Generational and G1: yes. ZGC: no (its rows are unchanged, out of scope).
fn jfr_collection_nests_phases(heap: &crate::memory::vm_heap::VmHeap) -> bool {
    heap.is_generational() || heap.is_g1()
}

#[cfg(test)]
mod gc_event_tests;

#[cfg(test)]
mod gci_i11_go_g1_observability_tests;

#[cfg(test)]
mod gci_i12_ge_g1_event_model_tests;







/// gci i14/mp finding 9 (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`,
/// opt-in `CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS`): HotSpot G1's cycle beans.
#[cfg(test)]
mod gci_i14_mp_cycle_bean_tests;

#[cfg(test)]
mod gci_i14hd_g1_heap_debug_tests;

#[cfg(test)]
mod gci_i20_ob_requester_tests;

#[cfg(test)]
mod gci_i23pk_g1_vm_operation_rows_tests;

#[cfg(test)]
mod gci_i27_ob_observability_tests;

/// gci i30/ob: observability residuals -- the stop-the-world full mark's
/// remark (mr finding 9), the G1 cycle beans flip (h4g1 item 6), G1 Full GC's
/// phases (g3x item 5), the metaspace after-figure (g3x item 12), the
/// `gc,start` decoration (g3x item 7) and the TLAB line carrier (tl item 6).
#[cfg(test)]
mod gci_i30_ob_tests;

/// gci i45/sl: `-Xlog:safepoint` (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016`):
/// the door / pause -> HotSpot 25 VM-operation name map and the emitter's
/// call sites (open inside the pause, end at the seal, print after the
/// release and after the pause's own lines).
#[cfg(test)]
mod gci_i45_sl_safepoint_line_tests;

/// gci i46/lg: the safepoint residuals of
/// `gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` -- 2 (a frame-trace
/// pause's operation from its initiator's scope), 3 (a parked G1 young
/// report's line held) and 6 (a pause with no HotSpot operation is no
/// safepoint of the barrier's count).
#[cfg(test)]
mod gci_i46_lg_safepoint_tests;

/// gci i47w/xl47: the G1 full-collection door's one operation
/// (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`) and the
/// remark / cleanup split (residual 1 of
/// `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`).
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i47w_xl47_`.
#[cfg(test)]
mod gci_i47w_xl47_safepoint_tests;

/// gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`
/// CS1, `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` vi28-dg
/// E4): a G1 cycle opens only after a young-only pause, and a user-requested
/// concurrent cycle's young pause is its forced `Concurrent Start`.


/// zt t13/x10 (`CRATONVM_ZGC_NOTIFICATION_MISSED_REDRAIN`): the re-drain after
/// a missed drain point is ZGC-only, default ON with a `=0` kill, bounded,
/// and runs only after the drain lets go. The mark's protocol itself is
/// tested in `cratonvm_gc::gc_metrics` (`zt13x10_missed_drain_tests`).
#[cfg(all(test, feature = "management"))]
mod zt13x10_missed_redrain_tests;

/// zt t15/x12 (`CRATONVM_ZGC_FORCED_NOTIFICATION_DRAIN_BOUNDED`): a forced
/// ZGC drain point delivers at most what was queued when it began and hands
/// the rest to the delivery thread after letting go; ZGC and forced only,
/// default ON with a `=0` kill.
#[cfg(all(test, feature = "management"))]
mod zt15x12_forced_drain_bound_tests;

/// zt t13/x10 (`zgc-zt12x9-*` item 5,
/// `CRATONVM_ZGC_PERF_REFRESH_BEFORE_ENQUEUE`): the ZGC perf counters are
/// refreshed in `record_gc_notification` after the beans' update and before
/// the first record is queued, only when a listener waits; every collector
/// since g1gen/c8, `=0` kill.
#[cfg(test)]
mod zt13x10_perf_refresh_before_enqueue_tests;

#[cfg(test)]
mod gcj_j1yg_tlab_totals_alone_tests;
