// SPDX-License-Identifier: Apache-2.0
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
//   (`docs/internal/gc/gengc-plumbing-jfr-gc-events-are-constants-FIXED-20260923.md`);
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
            matches!(heap, crate::memory::vm_heap::VmHeap::G1(_)),
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
        let crate::memory::vm_heap::VmHeap::G1(h) = heap else {
            return None;
        };
        Some(Self {
            census: h.region_census(),
            young_target_regions: h.young_target_regions(),
            survivor_ratio: h.survivor_ratio(),
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
        let crate::memory::vm_heap::VmHeap::G1(h) = &shared.mem.heap else {
            return None;
        };
        g1_young_only_pause(before, h.mixed_collection_count())
            .then_some(G1_YOUNG_ONLY_AFFECTED_POOLS)
    }

    /// gci i11/go: this collection's G1 pause type, `None` off G1
    /// ([`g1_young_pause_type`] over the two edges' counters and the
    /// concurrent-start decision [`gc_event_seal`] took inside the pause).
    /// Read after the collection, like [`Self::affected_pools`].
    fn g1_pause_type(&self, shared: &SharedVm, door: GcDoor) -> Option<G1YoungPauseType> {
        let before = self.g1_mixed_before?;
        let crate::memory::vm_heap::VmHeap::G1(h) = &shared.mem.heap else {
            return None;
        };
        let prepare_moved = self
            .g1_prepare_before
            .is_some_and(|b| h.prepare_mixed_pause_count() != b);
        let concurrent_start = (self.g1_concurrent_start
            && g1_door_drives_mark_cycle(door, cratonvm_types::flags().gc.g1_alloc_mark_drive))
            || g1_metadata_pause_is_concurrent_start(door, self.g1_marking_idle, || {
                g1_metadata_concurrent_start_enabled()
            })
            // gci cs48: the user-requested concurrent cycle's forced start.
            || g1_user_requested_concurrent_start(
                door,
                explicit_gc_invokes_concurrent(shared),
                self.g1_marking_idle,
                g1_concurrent_start_at_young_pause_enabled,
            );
        Some(g1_young_pause_type(
            before,
            h.mixed_collection_count(),
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
        | GcDoor::JvmtiForceGc => false,
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
/// half of `docs/internal/gc-common-round-20260923/common-c-proposal-takeover-cost-in-the-pause-line-FIXED-20260923.md`).
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
        || cratonvm_native_builtins::jfr_vm_events::any_recording()
        || crate::runtime::unified_logging::is_unified_logging_enabled(
            XLOG_COLLECTION_TAGS,
            crate::runtime::unified_logging::LogLevel::Info,
        )
        || xlog_age_on()
        // gcg g3/x: the deferred reference-processing block is flushed by the
        // collection's `-Xlog` lines, so a `gc+ref=debug`-only spec arms them.
        || crate::runtime::unified_logging::xlog_reference_lines_wanted()
}

/// gcj j1/yg (`docs/internal/gc/gci-i47w-j43-tlab-totals-line-absent-in-single-thread-churn-RETIRED-UNFIXED-20261018.md`):
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
    let generational = matches!(shared.mem.heap, crate::memory::vm_heap::VmHeap::Generational(_));
    let tlab_debug_on = generational
        && crate::runtime::unified_logging::is_unified_logging_enabled(
            &[crate::runtime::unified_logging::LogTag::GcTlab],
            crate::runtime::unified_logging::LogLevel::Debug,
        );
    let armed = tlab_debug_on
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XLOG_GEN_TLAB_TOTALS_ALONE");
    tlab_totals_alone_arms_events(generational, tlab_debug_on, armed)
}

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
    /// zr z3/t2: the classification of the forced (allocation-failure) door
    /// whose pause this thread is about to run -- `true` for an
    /// occupancy-verdict site (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
    /// Set only around `run_collection_pause` by `maybe_gc_forced_collected`
    /// and read by `gc_event_start` on the same thread, so the failure door's
    /// ZGC cause follows the site. A per-thread, per-call scratch value, not
    /// compatibility state.
    static ZGC_FORCED_DOOR_OCCUPANCY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
/// With `CRATONVM_ZGC_XLOG_ALLOCATION_STALL=1` (default OFF: the HotSpot line
/// is from reading `ZStatCriticalPhase`, not host-observed on 25) and
/// `-Xlog:gc`, also prints HotSpot ZGC's
/// `Allocation Stall (<thread>) <ms>ms` line. A no-op off ZGC.
pub(super) fn zgc_allocation_stall_finish(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    t0: std::time::Instant,
    page_at_entry: Option<(&'static str, i64)>,
) {
    use crate::runtime::unified_logging::{log_unified, LogLevel, LogTag};
    let heap = &shared.mem.heap;
    if matches!(heap, crate::memory::vm_heap::VmHeap::Generational(_)) || heap.is_g1() {
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

/// HotSpot ZGC's stall line (`ZStatCriticalPhase::register_end`, verbose
/// phase `Allocation Stall`): `Allocation Stall (main) 0.204ms`.
fn zgc_allocation_stall_line(thread_name: &str, nanos: u64) -> String {
    // Lossless for any stall shorter than 2^53 ns (104 days).
    let ms = nanos as f64 / 1_000_000.0;
    format!("Allocation Stall ({thread_name}) {ms:.3}ms")
}

/// `CRATONVM_ZGC_XLOG_ALLOCATION_STALL` -- default OFF, `=1` opts in. A
/// logging spelling, not compatibility state. gci i20/ob: read from the flag
/// snapshot per call (once per reported stall, after `xlog_on`), not through
/// a process `OnceLock` latch (`per_vm_state_statics_ratchet`), so a
/// per-thread test override reaches it.
fn zgc_xlog_allocation_stall_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_XLOG_ALLOCATION_STALL")
}

// ---------------------------------------------------------------------------
// zr z4/jf: ZGC's JFR events
// ---------------------------------------------------------------------------
//
// `docs/internal/gc/zgc-zr1tr-proposal-zgc-jfr-phase-and-stall-events-DONE-20261012.md`.
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_EVENTS")
}

/// `CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET` -- default ON (zr z9/hp): a ZGC
/// collection writes no `jdk.GCPhasePauseLevel1` rows, as HotSpot 25 ZGC
/// (both JFR backends). `=0` writes the collector's laps as level-1 rows
/// again (the z4/jf shape). An observability spelling, not compatibility
/// state. gci i20/ob: read from the flag snapshot per call (only for a ZGC
/// collection, see [`zgc_jfr_pause_level1_rows_for`]) instead of a process
/// `OnceLock` latch.
fn zgc_jfr_hotspot_phase_set_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET")
}

/// gci i20/ob: [`zgc_jfr_pause_level1_rows`] reading the switch only when the
/// collection has ZGC facts, so a Generational or G1 collection reads no flag.
fn zgc_jfr_pause_level1_rows_for(zgc_facts: bool) -> bool {
    !zgc_facts || zgc_jfr_pause_level1_rows(zgc_facts, zgc_jfr_hotspot_phase_set_enabled())
}

/// `CRATONVM_ZGC_JFR_HOTSPOT_PAUSES` -- default ON (zs s7/jf): a ZGC
/// collection's `jdk.GCPhasePause` rows are HotSpot 25's pause names over the
/// pause's phase boundaries ([`zgc_hotspot_pause_rows`]), and
/// `jdk.GarbageCollection.longestPause` the longest of them (both JFR
/// backends). `=0` writes the one `GC Pause` row again. An observability
/// spelling, not compatibility state; read from the flag snapshot per call
/// (gci i20/ob's rule for this file's switches, applied at the 2026-10-18 dev
/// merge to the six zs latches below too).
fn zgc_jfr_hotspot_pauses_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_HOTSPOT_PAUSES")
}

/// `CRATONVM_ZGC_JFR_STALL_REQUEST` -- default ON (zs s7/jf):
/// `jdk.ZAllocationStall` names the page HotSpot 25 ZGC would allocate for the
/// request the arena refused before the stall (`VmHeap::zgc_jfr_stall_page`).
/// `=0` writes `type = null`, `size = 0` (the z4/jf shape). Read per call.
fn zgc_jfr_stall_request_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_STALL_REQUEST")
}

/// `CRATONVM_ZGC_JFR_STALL_STACK` -- default ON (zs s8/jf2): the real
/// backend's `jdk.ZAllocationStall` is written from the stalling thread's own
/// context -- its Java thread entry and its stack, as HotSpot's. `=0` writes
/// no stack and finds the thread by name (the s7 shape). Read per call.
fn zgc_jfr_stall_stack_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_STALL_STACK")
}

/// `CRATONVM_ZGC_JFR_DRIVER_THREADS` -- default ON (zs s8/jf2): a ZGC
/// collection's own JFR events (real backend) are committed on HotSpot's
/// collector thread names, `ZDriverMajor` for a whole-heap collection and its
/// mark-start pause / concurrent mark, `ZDriverMinor` for a young one; the
/// safepoint rows stay on `VM Thread`. `=0` = everything on `VM Thread`.
/// Read per call.
fn zgc_jfr_driver_threads_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_DRIVER_THREADS")
}

/// zs s8/jf2: the collector thread a ZGC collection's events name
/// ([`zgc_jfr_driver_threads_enabled`]); `None` = `VM Thread`.
fn zgc_driver_thread(major: bool) -> Option<&'static str> {
    use cratonvm_native_builtins::jfr_vm_more::{ZGC_DRIVER_MAJOR, ZGC_DRIVER_MINOR};
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS")
}

/// `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER` -- default ON (zs s7/jf): on ZGC
/// `jdk.SystemGC` is written by the `Runtime.gc()` native, on the calling
/// thread with its stack and spanning the call
/// (`jfr_vm_more::force_gc_as_system_gc`), and not by the collection on the VM
/// thread. `=0`: the `System.gc()` door's own per-request row
/// (`gc_and_alloc::full_gc_door`, gci i12/ge + i19/ob; the collection's
/// per-pause row it replaced in s7 is gone since the 2026-10-18 dev merge).
/// Read per call.
fn zgc_jfr_system_gc_on_caller_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER")
}

/// `CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR` -- default ON (zt t3/jf, residual
/// R1 and its proposal; host-free evidence: local HotSpot 25.0.3
/// `Zt1JfSystemGcSpan` -> `ends_with_collection=true`, this VM's caller-side
/// row `false`): on ZGC `jdk.SystemGC` is the `System.gc()` door's
/// per-request row (`gc_and_alloc::full_gc_door` -> `jfr_vm_more::system_gc_by`:
/// the calling thread, the request's stack under the `Runtime.gc()` frame,
/// spanning the collection only, as HotSpot's `JVM_GC`), and the native
/// registers the calling thread's pool record first so the row names it as
/// `register_thread` would (`isVirtual`, OS name). Wins over
/// `CRATONVM_ZGC_JFR_SYSTEM_GC_ON_CALLER`. `=0`: the caller-side writer
/// (spanning the door's finalizer / cleaner / notification drains). Read per
/// call.
fn zgc_jfr_system_gc_from_door_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR")
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
        || !zgc_jfr_system_gc_on_caller_enabled()
        || zgc_jfr_system_gc_from_door_enabled()
    {
        return None;
    }
    match shared.config.explicit_gc {
        ExplicitGcPolicy::Disabled => None,
        policy => Some(policy == ExplicitGcPolicy::Concurrent),
    }
}

/// `CRATONVM_ZGC_JFR_METASPACE_CHUNK_FREE_LISTS` -- default ON (zs s10/jf3,
/// ported by zt t1/jf): the real backend writes HotSpot 25's four all-zero
/// `jdk.MetaspaceChunkFreeListSummary` rows per ZGC collection (Before /
/// After GC x `Metadata` / `Class`). `=0`: none. Read per call (this file's
/// rule for its switches since gci i20/ob).
fn zgc_jfr_metaspace_chunk_free_lists_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_METASPACE_CHUNK_FREE_LISTS")
}

/// `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN` -- default ON (zs s10/jf3,
/// ported by zt t1/jf): a ZGC concurrent cycle (`CRATONVM_ZGC_CONC_START`)
/// is written as HotSpot 25 writes one: its `jdk.GarbageCollection` (and
/// `ZOld` / `ZYoungGarbageCollection`) span from the mark-start pause to the
/// closing collection's end, and its `sumOfPauses` / `longestPause` count
/// the mark-start pause, which is the cycle's first pause row (`Pause Mark
/// Start (Major)`, `Pause Mark Start` for a minor) inside that span. `=0`:
/// the s7 shape (the collection's pause only; the mark-start row on its own,
/// `Pause Mark Start`). zt t2/jf (R2): with the span, the `Before GC` heap
/// summary is the heap as the mark-start pause found it, at the span's
/// start. Read per call.
fn zgc_jfr_concurrent_cycle_span_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN")
}

/// `CRATONVM_ZGC_JFR_STANDALONE_MARK_START_JOINS` -- default ON (zt t5/jf):
/// the mark-start pause of a concurrent cycle that the closing collection did
/// not close concurrently (abandoned by a `System.gc()`, refused
/// certification) joins that collection's pause rows and
/// `jdk.GarbageCollection` span when the ids match, instead of a row of the
/// same `gcId` before the collection started. Needs
/// `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN` (which defers the row). `=0`: the
/// row on its own (t3 shape). Read per call.
fn zgc_jfr_standalone_mark_start_joins_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_STANDALONE_MARK_START_JOINS")
}

/// `CRATONVM_ZGC_JFR_MAJOR_YOUNG` -- default ON (zt t1/jf, s7a host finding
/// JF1): a whole-heap (`ZGC Major`) collection writes the
/// `jdk.ZYoungGarbageCollection` of the young collection HotSpot 25's major
/// always runs (HotSpot 25 ZGC is generational only), under the major's
/// `gcId`, spanning the collection, with the threshold of
/// [`zgc_major_young_threshold`]. Also needs
/// `CRATONVM_ZGC_JFR_REF_AND_YOUNG_EVENTS`. `=0`: none for a major (the zs
/// shape). Read per call.
fn zgc_jfr_major_young_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_MAJOR_YOUNG")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS")
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

/// zr z9/hp: whether a collection's level-1 pause rows are written. Off ZGC
/// (no ZGC facts: Generational, G1, or `CRATONVM_ZGC_JFR_EVENTS=0`) always;
/// on ZGC only with the HotSpot phase set killed.
fn zgc_jfr_pause_level1_rows(zgc_facts: bool, hotspot_phase_set: bool) -> bool {
    !zgc_facts || !hotspot_phase_set
}

/// Whether either JFR backend records.
fn any_jfr_recording() -> bool {
    cratonvm_jfr::is_enabled() || cratonvm_native_builtins::jfr_vm_events::any_recording()
}

/// The heap is ZGC (neither Generational nor G1), the test
/// [`zgc_allocation_stall_finish`] uses.
fn heap_is_zgc(shared: &SharedVm) -> bool {
    let heap = &shared.mem.heap;
    !matches!(heap, crate::memory::vm_heap::VmHeap::Generational(_)) && !heap.is_g1()
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
fn zgc_concurrent_cycle_jfr(
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
}

fn zgc_mark_start_jfr(shared: &SharedVm, kind: NonCollectionPause) -> Option<ZgcMarkStartJfr> {
    if kind != NonCollectionPause::ZgcMarkStart || !zgc_jfr_events_enabled() || !any_jfr_recording()
    {
        return None;
    }
    let legacy_pause_type = if cratonvm_jfr::is_enabled() {
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
    // zs s10/jf3 (ported by zt t1/jf): a mark start that opened a concurrent
    // cycle is written by the collection that closes the cycle, as the first
    // of the cycle's pauses inside its `jdk.GarbageCollection` span
    // (HotSpot's shape, `CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN`); a refused
    // one (no cycle) is written here as before.
    if row.opened && zgc_jfr_concurrent_cycle_span_enabled() {
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
    zgc_write_mark_start_rows(
        row.vm,
        row.legacy_pause_type,
        row.gc_id,
        row.wall_start_ns,
        work_ns,
    );
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
    let hotspot_shape = zgc_jfr_concurrent_cycle_span_enabled();
    let name = if hotspot_shape {
        ZGC_PAUSE_MARK_START_MAJOR
    } else {
        ZGC_PAUSE_MARK_START
    };
    let id = gc_id as i32; // Truncation: deliberate wrap, as `gc_event_finish`.
    cratonvm_native_builtins::jfr_vm_more::gc_phase(
        vm,
        "jdk.GCPhasePause",
        id,
        name,
        start_ns,
        dur_ns,
        zgc_driver_thread(true),
    );
    if hotspot_shape && zgc_jfr_pause_vm_operations_enabled() {
        cratonvm_native_builtins::jfr_vm_more::zgc_vm_operation_pause(
            vm,
            cratonvm_native_builtins::jfr_vm_more::zgc_pause_vm_operation(name, true),
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
fn zgc_cycle_shape(
    shared: &SharedVm,
    gc_id: u64,
    initial: Option<(u64, u64, u64)>,
    pauses: &mut Vec<(&'static str, u64, u64)>,
    major: bool,
    collection_end_ns: u64,
) -> Option<(u64, u64)> {
    let (cycle_id, start, dur) = initial?;
    if cycle_id != gc_id || pauses.is_empty() || start > collection_end_ns {
        zgc_emit_mark_start_row(shared, cycle_id, start, dur);
        return None;
    }
    pauses.insert(0, (zgc_mark_start_pause_name(major), start, dur));
    Some((start, collection_end_ns - start))
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
                if start <= pause_start_ns && zgc_jfr_standalone_mark_start_joins_enabled() {
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
    cratonvm_native_builtins::jfr_vm_more::gc_phase(
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
    if !zgc_jfr_stall_request_enabled() {
        return None;
    }
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
        zgc_jfr_stall_stack_enabled() && cratonvm_native_builtins::jfr_vm_events::any_recording();
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
    cratonvm_native_builtins::jfr_vm_more::zgc_allocation_stall(
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
    cratonvm_jfr::set_real_backend_active(cratonvm_native_builtins::jfr_vm_events::any_recording());
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
    if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
        h.set_tenuring_xlog(events && xlog_age_on());
    }
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
            crate::memory::vm_heap::VmHeap::G1(h) => Some(h.mixed_collection_count()),
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
            crate::memory::vm_heap::VmHeap::G1(h) => Some(h.prepare_mixed_pause_count()),
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
    matches!(
        heap,
        crate::memory::vm_heap::VmHeap::Generational(_) | crate::memory::vm_heap::VmHeap::G1(_)
    )
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
/// `GcInfo` entry at one edge of a collection. Never blocks: the class lock
/// is taken with `try_read_untracked` (the before edge runs inside the pause,
/// where a stopped thread may hold it for write, and the initiating thread may
/// itself hold it).
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
    Some(b.metaspace_sample_figures(key, || {
        crate::vm::vm_init::class_metadata_walk_figures(&cm)
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
                s.zgc_pause_split = b.note_collection_split(s.t0, end, s.kind(shared), split);
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
            if let crate::memory::vm_heap::VmHeap::G1(h) = &shared.mem.heap {
                // gci i13/gp: the idle reading is kept for the metadata
                // opt-in's forced concurrent start (`g1_pause_type`).
                s.g1_marking_idle = !shared.mem.heap.g1_is_marking_active();
                s.g1_concurrent_start = s.g1_marking_idle && h.would_start_marking();
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
        if let (Some(before), crate::memory::vm_heap::VmHeap::G1(h)) =
            (s.g1_mixed_before, &shared.mem.heap)
        {
            let wait = g1_mixed_pause_defers_concurrent_start(
                before,
                h.mixed_collection_count(),
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
        if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
            s.escalation = h.take_escalation_split();
        }
        s.sealed = Some((end, after, committed));
    }
    // gci i45/sl: the `-Xlog:safepoint` line's end, inside the pause (a no-op
    // without an open line).
    crate::runtime::unified_logging::safepoint_line_seal();
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS")
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
    g1_stamp_the_survivor_as_a_full_collection_leaves_it(set, &set.pools(&sampler.sample()));
}

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
        cratonvm_native_builtins::jfr_vm_events::any_recording(),
    )?;
    // gci i5g/g1o (the G1 half of `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`,
    // hunk of `gch-h4g1-proposal-g1-concurrent-cycle-jfr-rows-DONE-20261015`): G1's marking pauses
    // file under the same mechanism; its cycle state lives in the backend's notification queue.
    let generational = cycle_jfr_pause_is_generational(kind);
    let cycle: &std::sync::Arc<cratonvm_gc::gc_metrics::ConcurrentCycleJfr> = match &shared.mem.heap
    {
        crate::memory::vm_heap::VmHeap::Generational(h) if generational => {
            h.gc_notifications().concurrent_cycle_jfr()
        }
        crate::memory::vm_heap::VmHeap::G1(_) if !generational => {
            shared.mem.backend_gc_beans()?.notifications().concurrent_cycle_jfr()
        }
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
/// request 2, `docs/internal/reviews/gengc-round5-w2-obs6-20260926.md`).
///
/// TAKES the heap's open cycle entry — the id the remark re-opened for the
/// sweep, or the initial mark's own entry when the remark could not reach the
/// recorder — so it cannot outlive this cycle, and returns `(gc_id, sweep
/// start in wall-clock ns)` when a recording runs. The caller holds the phase
/// at `ConcurrentMark`/`ConcurrentSweep` throughout, so no other cycle can
/// have opened in between. `None` on another backend. One uncontended lock
/// per cycle, whatever JFR says.
pub(super) fn gen_concurrent_sweep_jfr_open(shared: &SharedVm) -> Option<(u64, u64)> {
    let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap else {
        return None;
    };
    let (gc_id, _) = h.gc_notifications().concurrent_cycle_jfr().take()?;
    // gci i9/jc: a `jdk.jfr` backend recording gets the row too.
    cycle_jfr_sinks(
        cratonvm_jfr::is_enabled(),
        cratonvm_native_builtins::jfr_vm_events::any_recording(),
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
    if cratonvm_native_builtins::jfr_vm_events::any_recording() {
        cratonvm_native_builtins::jfr_vm_more::gc_phase_concurrent(
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
        cratonvm_native_builtins::jfr_vm_more::gc_phase_concurrent(
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
            cratonvm_native_builtins::jfr_vm_more::gc_phase_pause(vm, gc_id, name, start_ns, dur_ns);
            // gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
            // HotSpot 25 runs G1's Remark and Cleanup as VM operations, so a
            // recording has their safepoint and `jdk.ExecuteVMOperation`
            // rows beside the `jdk.GCPhasePause` one. Backend only (the
            // legacy recorder never wrote VM operations).
            if let Some(op) = g1_cycle_pause_vm_operation(row.g1, name) {
                cratonvm_native_builtins::jfr_vm_more::vm_operation_pause(
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
    let beans = kind.bean_pause().filter(|_| fold.is_none() && books).and_then(|(shape, cause)| {
        let set = shared.mem.backend_gc_beans()?;
        if set.shape() != shape {
            return None;
        }
        // zr z1/tr: a ZGC mark start carries the cycle's own HotSpot cause
        // (`Warmup`, `Proactive`, ...) when the heap knows it; under
        // `CRATONVM_ZGC_HOTSPOT_GC_CAUSE=0`, the pre-zr `Allocation Failure`.
        let cause = if kind == NonCollectionPause::ZgcMarkStart {
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
        if cratonvm_native_builtins::jfr_vm_events::any_recording() {
            cratonvm_native_builtins::jfr_vm_more::concurrent_mode_failure(
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE")
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_FULL_GC_ONE_REPORT")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_OOME_PAUSE_FULL_REPORT")
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
mod gch_h4g1_remark_usage_tests {
    use super::*;
    use cratonvm_gc::gc_metrics::{
        gc_notifications_between, GcBeanSnapshot, JmxCollector, JmxPoolUsage,
    };

    const OLD: &[&str] = &["G1 Old Gen"];
    const ALL: &[&str] = &["G1 Eden Space", "G1 Survivor Space", "G1 Old Gen"];

    fn pool(name: &'static str, used: u64, collection_used: Option<u64>) -> JmxPoolUsage {
        JmxPoolUsage {
            name,
            init: None,
            used,
            committed: used,
            max: None,
            collection_used,
            managers: &[],
        }
    }

    fn collector(name: &'static str, count: u64, pools: &'static [&'static str]) -> JmxCollector {
        JmxCollector {
            name,
            count,
            time_ms: 0,
            pools,
            action: "end of concurrent GC pause",
        }
    }

    /// A remark + cleanup moves `G1 Concurrent GC` only: `G1 Old Gen` takes
    /// the pause's usage as its collection usage, eden and survivor keep their
    /// last young pause's. Before any collection the others read 0 (gci
    /// i5g/g1o; was: no stamp).
    #[test]
    fn gch_h4g1_a_remark_stamps_only_the_old_pools_collection_usage() {
        let before = GcBeanSnapshot::now(
            vec![
                collector("G1 Young Generation", 3, ALL),
                collector("G1 Concurrent GC", 1, OLD),
            ],
            vec![
                pool("G1 Eden Space", 0, Some(0)),
                pool("G1 Survivor Space", 256, Some(256)),
                pool("G1 Old Gen", 9500, Some(7000)),
            ],
        );
        let after = GcBeanSnapshot::now(
            vec![
                collector("G1 Young Generation", 3, ALL),
                collector("G1 Concurrent GC", 2, OLD),
            ],
            vec![
                pool("G1 Eden Space", 4096, Some(0)),
                pool("G1 Survivor Space", 256, Some(256)),
                pool("G1 Old Gen", 9000, Some(7000)),
            ],
        );
        let recs = gc_notifications_between(&before, &after, "No GC", 0);
        assert_eq!(recs.len(), 1, "only the concurrent bean moved");
        let managed = g1_remark_managed_pools(&after, &recs);
        assert_eq!(managed, vec!["G1 Old Gen"]);
        let rows = affected_collection_usage_rows(&after.pools, &managed);
        let used: Vec<(&str, u64)> = rows.iter().map(|p| (p.name, p.used)).collect();
        assert_eq!(
            used,
            vec![
                ("G1 Eden Space", 0),
                ("G1 Survivor Space", 256),
                ("G1 Old Gen", 9000)
            ]
        );
        // gci i5g/g1o: before any collection the remark still stamps the old
        // pool; the others read 0, as an unstamped pool reports.
        let fresh = [pool("G1 Eden Space", 1, None), pool("G1 Old Gen", 2, None)];
        let rows = affected_collection_usage_rows(&fresh, &managed);
        let used: Vec<u64> = rows.iter().map(|p| p.used).collect();
        assert_eq!(used, vec![0, 2]);
        // A pause that moved no bean manages nothing.
        assert!(g1_remark_managed_pools(&after, &[]).is_empty());
    }

    /// gci i5g/g1o (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
    /// a young-only G1 pause stamps eden and survivor and writes `G1 Old Gen`
    /// back with its own last figure (0 before any); a mixed pause (the count
    /// moved) affects every pool.
    #[test]
    fn gci_i5g_g1o_a_young_only_pause_leaves_the_old_pools_collection_usage() {
        assert!(g1_young_only_pause(4, 4), "no Mixed record: young-only");
        assert!(!g1_young_only_pause(4, 5), "a Mixed record: all pools");
        assert_eq!(
            G1_YOUNG_ONLY_AFFECTED_POOLS,
            &["G1 Eden Space", "G1 Survivor Space"][..]
        );
        let after = [
            pool("G1 Eden Space", 0, Some(0)),
            pool("G1 Survivor Space", 512, Some(256)),
            pool("G1 Old Gen", 9000, Some(7000)),
        ];
        let rows = affected_collection_usage_rows(&after, G1_YOUNG_ONLY_AFFECTED_POOLS);
        let used: Vec<(&str, u64)> = rows.iter().map(|p| (p.name, p.used)).collect();
        assert_eq!(
            used,
            vec![
                ("G1 Eden Space", 0),
                ("G1 Survivor Space", 512),
                ("G1 Old Gen", 7000)
            ]
        );
        // Before any stamp the old pool keeps HotSpot's initial 0.
        let fresh = [
            pool("G1 Eden Space", 0, None),
            pool("G1 Survivor Space", 128, None),
            pool("G1 Old Gen", 4096, None),
        ];
        let rows = affected_collection_usage_rows(&fresh, G1_YOUNG_ONLY_AFFECTED_POOLS);
        let used: Vec<u64> = rows.iter().map(|p| p.used).collect();
        assert_eq!(used, vec![0, 128, 0]);
        // Every pool affected: the pools as they are.
        assert_eq!(affected_collection_usage_rows(&after, ALL), after.to_vec());
    }

    /// gci i5g/g1o (the G1 half of
    /// `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`): G1's
    /// marking pauses get `jdk.GCPhasePause` rows, the initial mark opens the
    /// cycle, and each kind finds its cycle state on its own backend.
    #[test]
    fn gci_i5g_g1o_g1_marking_pauses_have_cycle_jfr_rows() {
        use super::NonCollectionPause as P;
        // gci i8/jf: G1's initial mark is HotSpot G1's `GC Pause`.
        assert_eq!(cycle_jfr_pause_name(P::G1InitialMark), Some("GC Pause"));
        assert_eq!(cycle_jfr_pause_name(P::G1Remark), Some("Pause Remark"));
        assert_eq!(cycle_jfr_pause_name(P::GenInitialMark), Some("Pause Init Mark"));
        assert_eq!(cycle_jfr_pause_name(P::GenRemark), Some("Pause Remark"));
        for other in [
            P::FrameTrace,
            P::HeapDump,
            P::HeapInspection,
            P::LoopExit,
            P::Redefinition,
            P::ZgcMarkStart,
            P::IcGrace,
        ] {
            assert_eq!(cycle_jfr_pause_name(other), None, "{other:?}");
        }
        assert!(cycle_jfr_pause_opens_the_cycle(P::G1InitialMark));
        assert!(cycle_jfr_pause_opens_the_cycle(P::GenInitialMark));
        assert!(!cycle_jfr_pause_opens_the_cycle(P::G1Remark));
        assert!(!cycle_jfr_pause_opens_the_cycle(P::GenRemark));
        assert!(cycle_jfr_pause_is_generational(P::GenRemark));
        assert!(!cycle_jfr_pause_is_generational(P::G1Remark));
        assert!(!cycle_jfr_pause_is_generational(P::G1InitialMark));
    }

    /// gci i8/jf (`gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted`,
    /// G1 half): HotSpot G1's shape. The initial mark is `GC Pause` under its
    /// own id and the cycle takes the next one; the concurrent marking is
    /// `Concurrent Mark From Roots`; the remark that reached its cleanup is
    /// `Pause Remark` then `Pause Cleanup`, partitioning the pause. The
    /// Generational names and id count are unchanged.
    #[test]
    fn gci_i8_jf_g1_cycle_rows_take_hotspot_g1_names_and_ids() {
        use super::NonCollectionPause as P;
        assert_eq!(cycle_jfr_pause_name(P::GenInitialMark), Some("Pause Init Mark"));
        assert_ne!(cycle_jfr_pause_name(P::G1InitialMark), Some("Pause Init Mark"));
        assert_eq!(cycle_jfr_concurrent_mark_name(true), "Concurrent Mark");
        assert_eq!(cycle_jfr_concurrent_mark_name(false), "Concurrent Mark From Roots");
        // Ids (gci i12/ge): the cycle takes one; G1's initial mark writes no
        // row (its concurrent-start collection wrote the `GC Pause`), the
        // Generational initial mark keeps its `Pause Init Mark`.
        assert!(g1_initial_mark_has_no_row(P::G1InitialMark));
        assert!(!g1_initial_mark_has_no_row(P::GenInitialMark));
        assert!(!g1_initial_mark_has_no_row(P::G1Remark));
        // Remark rows: no cleanup point -> one row, as before.
        assert_eq!(
            remark_cleanup_rows(1_000, 900, None),
            vec![("Pause Remark", 1_000, 900)]
        );
        // A cleanup point -> Remark then Cleanup, partitioning the pause.
        let rows = remark_cleanup_rows(1_000, 900, Some(600));
        assert_eq!(
            rows,
            vec![("Pause Remark", 1_000, 600), ("Pause Cleanup", 1_600, 300)]
        );
        assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 900);
        // A cut past the work is clamped: an empty cleanup row, never an
        // underflow.
        assert_eq!(
            remark_cleanup_rows(1_000, 900, Some(5_000)),
            vec![("Pause Remark", 1_000, 900), ("Pause Cleanup", 1_900, 0)]
        );
    }
}

/// The JMX beans of a heap that describes them: every collector's count and
/// every pool's usage, stamped now. Generational since gen r4w3/obs2 (Serial's
/// beans, the heap's own); G1 since gc-common w7-f, and ZGC once its state is
/// wired (`HeapRealm::backend_gc_beans`). `None` where there is none.
fn gc_bean_snapshot(shared: &SharedVm) -> Option<cratonvm_gc::gc_metrics::GcBeanSnapshot> {
    match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(h) => {
            let snap = cratonvm_gc::gc_metrics::GcBeanSnapshot::now(
                h.jmx_collectors(),
                h.jmx_memory_pools(),
            );
            // gen r5w2/obs6: every collection edge is a peak sample, as
            // HotSpot's `GCMemoryManager::gc_begin` / `gc_end` record each
            // pool's peak — the moment eden is full, which a query-time sample
            // almost never sees (`MemoryPoolMXBean.getPeakUsage()`).
            h.gc_notifications().note_pool_peaks(&snap.pools);
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
        crate::memory::vm_heap::VmHeap::Generational(h) => Some(h.gc_notifications()),
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
            crate::memory::VmHeap::Generational(_) => None,
            crate::memory::VmHeap::G1(h) => Some(h.gc_beans()),
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
mod w9g_young_only_pools_tests {
    use super::g1_young_only_pools;

    #[test]
    fn w9g_only_an_unmoved_g1_old_pause_count_is_young_only() {
        assert_eq!(
            g1_young_only_pools(Some(3), Some(3)),
            Some(cratonvm_gc::gc_metrics::G1_YOUNG_ONLY_POOLS)
        );
        assert_eq!(g1_young_only_pools(Some(3), Some(4)), None);
        assert_eq!(g1_young_only_pools(None, None), None);
    }
}

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
        let Some(drain) = queue.begin_drain() else {
            return;
        };
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
            while let Some(rec) = match beans.filter(|_| hold) {
                Some(b) => drain.pop_unless(|r| b.metaspace_record_waiting(r)),
                None => drain.pop(),
            } {
                let info = crate::vm::vm_exec::gc_notification_info_of(&rec);
                // gci i16/oo: a heap that cannot hold the `GcInfo` fails this
                // delivery with an `OutOfMemoryError` instead of aborting the
                // process (`deliver_catching_native_alloc_oom`).
                let deliver = |t: &mut JvmThread| {
                    super::gc_and_alloc::deliver_catching_native_alloc_oom(t, |t| {
                        let mut ctx = crate::vm::NativeContextImpl { shared, thread: t };
                        cratonvm_native_builtins::jmx::deliver_gc_notification(&mut ctx, &info)
                    })
                };
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
                        cratonvm_native_builtins::jmx::deliver_pool_sensor(
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
    }
    #[cfg(not(feature = "management"))]
    {
        // No JMX beans to deliver to without the management natives, so no
        // bean can have been enabled and nothing was queued.
        let _ = (thread, forced);
    }
}

/// zs s9/ob6: the drain's `Metaspace` back-fill
/// (`vm_init.rs::zgc_metaspace_backfill`, non-blocking). Returns `true` when
/// census entries still wait because the class lock stayed busy -- after a
/// few yields, since a define holds it briefly -- so the caller must hold
/// the keyless cycle records back. `false` under
/// `CRATONVM_ZGC_METASPACE_BUSY_LOCK_BACKFILL=0` (the s6/ob5 behaviour:
/// deliver them keyless). One load when nothing waits.
///
/// zt t6/rv3 (`zgc-zs10rv7-wave-s9-review-residuals-20261016.md` item 5):
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
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_METASPACE_BACKFILL_HOLD_MEMO");
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
mod gci_i47w_oe47_notification_refusal_tests {
    use super::heap_refusal_message;

    /// The rows' two failures are heap refusals; the array-length refusal
    /// and an unrelated message are not.
    #[test]
    fn gci_i47w_oe47_heap_refusals_are_recognised() {
        assert!(heap_refusal_message(Some("Java heap space")));
        assert!(heap_refusal_message(Some(
            "Java heap space (native reference array of length 8)"
        )));
        assert!(heap_refusal_message(None));
        assert!(heap_refusal_message(Some("")));
        assert!(!heap_refusal_message(Some(
            "Requested array size exceeds VM limit"
        )));
        assert!(!heap_refusal_message(Some("Metaspace")));
    }

    /// gci i48a/oe47c: a refused build first collects on the drainer and
    /// builds once more; only a second refusal is put back on the queue.
    #[test]
    fn gci_i47w_oe47_a_refused_build_collects_and_retries_before_requeue() {
        let src = include_str!("gc_events.rs");
        let start = src
            .find("pub(super) fn run_gc_notifications(")
            .expect("the drain");
        let drain = &src[start..];
        let first = drain.find("let mut delivered = deliver(&mut *thread);").expect("first build");
        let reclaim = drain
            .find("gc_notification_reclaim_before_retry(shared, &mut *thread)")
            .expect("the drainer's collection");
        let requeue = drain.find("drain.requeue_refused(rec)").expect("requeue");
        assert!(first < reclaim && reclaim < requeue);
        assert!(drain[reclaim..requeue].contains("delivered = deliver(&mut *thread);"));
    }
}

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
        crate::memory::vm_heap::VmHeap::Generational(_) => {
            if cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle() {
                ("SerialOld", "Pause Full", true)
            } else {
                ("DefNew", "Pause Young", false)
            }
        }
        crate::memory::vm_heap::VmHeap::G1(_) => (
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_XLOG_PAUSE_PHASES")
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

/// zt t5/jf (R9): HotSpot 25 ZGC's `gc,metaspace` line of a collection,
/// `GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved` (whole
/// mebibytes, truncated), from this VM's `(used, committed, reserved)`
/// metaspace figures; `O:` for a whole-heap collection (HotSpot prints one per
/// generation collection; this collection has no young one of its own),
/// `y:` for a young one. Pure.
fn render_xlog_zgc_metaspace_line(
    gc_id: u64,
    minor: bool,
    (used, committed, reserved): (u64, u64, u64),
) -> String {
    const M: u64 = 1024 * 1024;
    let generation = if minor { "y" } else { "O" };
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
    const M: usize = 1024 * 1024;
    let generation = if minor { "y" } else { "O" };
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
///   `docs/internal/gc-common-round-20260923/common-c-proposal-takeover-cost-in-the-pause-line-FIXED-20260923.md`). 0 when the
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
    let Some(start) = start else {
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
                pause_split = b.note_collection_split(start.t0, end, start.kind(shared), split);
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
        cratonvm_jfr::is_enabled() || cratonvm_native_builtins::jfr_vm_events::any_recording();
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
        Some(z) if zgc_jfr_hotspot_pauses_enabled() => zgc_hotspot_pause_rows(
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
            && g1_concurrent_start_from_drive_enabled()
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
            zgc_hotspot_pause_rows(
                pause_split.as_ref(),
                start.t0,
                start.wall_start_ns,
                duration_ns,
                matches!(
                    start.kind(shared),
                    cratonvm_gc::gc_metrics::CollectionKind::Major
                ),
            )
        } else {
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
        && g1_door_drives_mark_cycle(door, cratonvm_types::flags().gc.g1_alloc_mark_drive)
}

/// gci i27/ob: `CRATONVM_G1_CONCURRENT_START_FROM_DRIVE` (default ON, `=0`
/// restores the prediction as the label). Read once per predicted
/// concurrent-start pause, only under `-Xlog` / JFR.
fn g1_concurrent_start_from_drive_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_CONCURRENT_START_FROM_DRIVE")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE")
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
    caller: Option<cratonvm_native_builtins::jfr_vm_more::JfrCallerThread<'a>>,
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
    pub(super) fn as_caller(&self) -> cratonvm_native_builtins::jfr_vm_more::JfrCallerThread<'_> {
        cratonvm_native_builtins::jfr_vm_more::JfrCallerThread {
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
        && cratonvm_native_builtins::jfr_vm_events::any_recording();
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
        crate::memory::vm_heap::VmHeap::G1(_) => true,
        crate::memory::vm_heap::VmHeap::Generational(_) => false,
        #[allow(unreachable_patterns)]
        _ => return None,
    };
    Some(vm_operation_name(door, g1))
}

/// gci i12/ge (`gci-i11go-g1-concurrent-cycle-xlog-and-jfr-gc-event-review-findings-FIXED-20261017`
/// item 3): is a collection on `door` recorded as `jdk.OldGarbageCollection`
/// (else `jdk.YoungGarbageCollection`)? See [`jfr_old_event_rule`].
fn jfr_old_event(shared: &SharedVm, door: GcDoor, old: bool) -> bool {
    let (generational, g1) = match &shared.mem.heap {
        crate::memory::vm_heap::VmHeap::Generational(_) => (true, false),
        crate::memory::vm_heap::VmHeap::G1(_) => (false, true),
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
        GcDoor::AllocationThreshold
        | GcDoor::AllocationFailure
        | GcDoor::G1HumongousAllocation => false,
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
        GcDoor::MetadataThreshold
        | GcDoor::AllocationThreshold
        | GcDoor::AllocationFailure
        | GcDoor::G1CompactionPause
        | GcDoor::G1HumongousAllocation => false,
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
    cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_METADATA_CONCURRENT_START")
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
        GcDoor::SystemGc | GcDoor::DiagnosticCommand | GcDoor::JvmtiForceGc => {
            if g1 {
                "G1CollectFull"
            } else {
                "SerialGCCollect"
            }
        }
        GcDoor::MetadataThreshold => "CollectForMetadataAllocation",
        GcDoor::HeapInspection => "GC_HeapInspection",
        GcDoor::HeapDump => "HeapDumper",
    }
}

// ---------------------------------------------------------------------------
// `-Xlog:safepoint` (gci i45/sl)
// ---------------------------------------------------------------------------
//
// `docs/internal/gc/gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016.md`.
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
        crate::memory::vm_heap::VmHeap::Generational(_) => (true, false),
        crate::memory::vm_heap::VmHeap::G1(_) => (false, true),
        #[allow(unreachable_patterns)]
        _ => (false, false),
    };
    safepoint_vm_operation(door, shape, explicit_gc_invokes_concurrent(shared))
}

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
/// * ZGC (out of scope): `None`.
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
        | NonCollectionPause::ZgcMarkStart => None,
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
        non_collection_safepoint_name_on(shared, kind)
    };
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
    name.is_none() && kind != NonCollectionPause::ZgcMarkStart
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 2): `CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS`, default ON (`=0`: every
/// frame-trace pause is `ThreadDump`, as gci i45/sl had it). Read only for a
/// frame-trace pause inside a `GcBarrier::frame_trace_op_scope`.
fn xlog_safepoint_frame_trace_ops_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS")
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
    non_collection_safepoint_name(kind)
}

/// gci i46/lg (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` residual
/// 3): `CRATONVM_XLOG_SAFEPOINT_AFTER_PARKED_REPORT`, default ON (`=0`: the
/// line prints at the pause's finish, before a parked young report's `gc`
/// lines, as gci i45/sl had it). Read once per G1 collection a thread
/// requested, only under `-Xlog:safepoint`.
fn xlog_safepoint_after_parked_report_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_XLOG_SAFEPOINT_AFTER_PARKED_REPORT")
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
    let zgc_real = c
        .zgc
        .map(|z| cratonvm_native_builtins::jfr_vm_more::ZgcCycleJfr {
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
            metaspace_chunk_free_lists: zgc_jfr_metaspace_chunk_free_lists_enabled(),
            pause_vm_operations: zgc_jfr_pause_vm_operations_enabled(),
        });
    if cratonvm_native_builtins::jfr_vm_events::any_recording() {
        let i = |b: usize| i64::try_from(b).unwrap_or(i64::MAX);
        // gci i33/ms: `jdk.MetaspaceSummary`'s committed / reserved and its
        // `classSpace` (HotSpot's statistics, one walk).
        let metaspace = metaspace_stats_now(shared);
        let w = |b: u64| i64::try_from(b).unwrap_or(i64::MAX);
        cratonvm_native_builtins::jfr_vm_more::gc_cycle(
            shared.vm_identity,
            &cratonvm_native_builtins::jfr_vm_more::GcCycle {
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
                caller: c.caller,
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
        for (name, count) in cratonvm_native_builtins::jfr_vm_more::REFERENCE_TYPE_NAMES
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
        return zgc_jfr_major_young_enabled()
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

/// gcg h1/ob: `CRATONVM_GC_ESCALATED_PAUSE_SPLIT`: report a Generational
/// young pause that escalated into an unrequested major as HotSpot Serial
/// reports a young collection followed by a full one -- two `GC(n)` ids,
/// `Pause Young` then `Pause Full` on `-Xlog:gc*`, two JFR
/// `jdk.GarbageCollection` events, and `Copy` / `MarkSweepCompact`
/// notifications that end / start at the mark.
///
/// gch h2/ob: default ON (`runtime_flag_default_on`; `=0` is the kill switch,
/// one `Pause Full` and one id per such pause, the pre-h1 shape). vh1-ob
/// `ob1_split_*`: stdout SAME 8/8 on all three collectors; the young-then-full
/// pairs with consecutive ids where the `=0` twin printed one `Pause Full`. On,
/// the `Pause Young` count equals the `Copy` bean's (the bean has booked the
/// young half since gcg g6, `CRATONVM_GC_ESCALATED_YOUNG_BOOKS_COPY`), which
/// is HotSpot's own invariant; off, the log and the bean disagree by one per
/// escalation (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full`).
/// Read once per escalated pause only.
fn escalated_pause_split_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_ESCALATED_PAUSE_SPLIT")
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
        || !escalated_pause_split_enabled()
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
    if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
        if let Some((desired, next, max, ages)) = h.take_tenuring_xlog() {
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
    let generational = matches!(
        shared.mem.heap,
        crate::memory::vm_heap::VmHeap::Generational(_)
    );
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
    // gcf f4/r: `gc,age` (debug: the desired survivor size and threshold;
    // trace: the age table), between `gc,start` and `gc,heap` as on HotSpot
    // Serial, for a young collection that filled its age table.
    // gcg g5/yg (vg3 `x3_heapdbg_full`): HotSpot Serial prints `gc,age` from
    // the young collection only (`DefNewGeneration` adjusts the threshold);
    // a `Pause Full` prints none. This VM's full collection runs a young
    // cycle first and stashes its table, so the stash is TAKEN (it must not
    // leak into the next collection) and printed only on a young pause.
    if let crate::memory::vm_heap::VmHeap::Generational(h) = &shared.mem.heap {
        if let Some((desired, next, max, ages)) = h.take_tenuring_xlog() {
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
    if let crate::memory::vm_heap::VmHeap::G1(h) = &shared.mem.heap {
        if let Some((desired, threshold, max, ages)) = h.take_tenuring_xlog() {
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
            if xlog_on(LogTag::GcPhases) {
                for line in render_xlog_zgc_pause_lines(gc_id, zgc_pauses, minor) {
                    log_unified(&[LogTag::GcPhases], LogLevel::Info, &line);
                }
            }
            if xlog_on(LogTag::GcMetaspace) {
                let m = metaspace_stats_now(shared);
                log_unified(
                    &[LogTag::GcMetaspace],
                    LogLevel::Info,
                    &render_xlog_zgc_metaspace_line(gc_id, minor, m.triple()),
                );
            }
            // zt t6/ob4 (R9 tail): the `gc,heap` capacity lines, after the
            // metaspace line as in HotSpot 25.0.3's `-Xlog:gc*` block, with
            // the `gc,init` block's figures.
            if xlog_on(LogTag::GcHeap) {
                let cfg = &shared.config;
                let max = cfg.max_heap_size;
                let min = crate::runtime::unified_logging::hotspot_min_heap_size(
                    cfg.min_heap_size,
                    cfg.initial_heap_size.min(max),
                );
                let soft_max = shared.mem.heap.soft_max_capacity().unwrap_or(max);
                for line in render_xlog_zgc_capacity_lines(gc_id, minor, min, max, soft_max) {
                    log_unified(&[LogTag::GcHeap], LogLevel::Info, &line);
                }
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
    matches!(
        heap,
        crate::memory::vm_heap::VmHeap::Generational(_) | crate::memory::vm_heap::VmHeap::G1(_)
    )
}

#[cfg(test)]
mod gc_event_tests {
    use super::*;

    /// gen r4w2/obs: the JFR phase rows are consecutive and, with `other`,
    /// sum to the collector's pause.
    #[test]
    fn phase_pause_rows_partition_the_pause() {
        let rows = phase_pause_rows(1_000, 900, &[("roots", 100), ("cheney_drain", 650)]);
        assert_eq!(
            rows,
            vec![
                ("roots", 1_000, 100_000),
                ("cheney_drain", 101_000, 650_000),
                ("other", 751_000, 150_000),
            ]
        );
        let total: u64 = rows.iter().map(|r| r.2).sum();
        assert_eq!(total, 900_000);
        // No marks: one `other` row carrying the whole pause.
        assert_eq!(phase_pause_rows(5, 3, &[]), vec![("other", 5, 3_000)]);
    }

    /// gci i9/jc: the level-1 phase rows nest inside their collection's
    /// window: rows inside are unchanged, a straddling row is cut, rows
    /// outside are dropped, and an escalated pause's two halves partition the
    /// rows at the mark.
    #[test]
    fn gci_i9_jc_phase_rows_nest_inside_the_collection_window() {
        let rows = phase_pause_rows(1_000, 900, &[("roots", 100), ("cheney_drain", 650)]);
        // The whole pause's window: unchanged.
        assert_eq!(phase_rows_within(&rows, 1_000, 901_000), rows);
        // A shorter door window: the `other` row is cut at its end.
        assert_eq!(
            phase_rows_within(&rows, 1_000, 800_000),
            vec![
                ("roots", 1_000, 100_000),
                ("cheney_drain", 101_000, 650_000),
                ("other", 751_000, 49_000),
            ]
        );
        // An escalated pause split at 400_000: the halves partition the rows.
        let young = phase_rows_within(&rows, 1_000, 400_000);
        let full = phase_rows_within(&rows, 400_000, 901_000);
        assert_eq!(
            young,
            vec![("roots", 1_000, 100_000), ("cheney_drain", 101_000, 299_000)]
        );
        assert_eq!(
            full,
            vec![("cheney_drain", 400_000, 351_000), ("other", 751_000, 150_000)]
        );
        let total: u64 = young.iter().chain(full.iter()).map(|r| r.2).sum();
        assert_eq!(total, 900_000);
        // A zero-length row is kept only when its instant is in the window.
        let zero = [("other", 500, 0)];
        assert_eq!(phase_rows_within(&zero, 100, 500), vec![("other", 500, 0)]);
        assert!(phase_rows_within(&zero, 501, 900).is_empty());
        // No rows, no rows.
        assert!(phase_rows_within(&[], 0, 10).is_empty());
    }

    /// gci i9/jc (`gci-i8jf-jfr-cycle-rows-never-reach-the-jdk-jfr-backend`):
    /// a concurrent-cycle row is armed when EITHER recorder runs (the legacy
    /// one or the `jdk.jfr` backend), and not at all (no id taken) when
    /// neither runs.
    #[test]
    fn gci_i9_jc_cycle_rows_go_to_either_recorder() {
        assert_eq!(cycle_jfr_sinks(false, false), None);
        assert_eq!(cycle_jfr_sinks(true, false), Some((true, false)));
        assert_eq!(cycle_jfr_sinks(false, true), Some((false, true)));
        assert_eq!(cycle_jfr_sinks(true, true), Some((true, true)));
    }

    // gen r4w2/obs: `gc_event_heap_used` is deliberately NOT tested here with a
    // real heap. Constructing a `GenerationalHeap` publishes into process-global
    // bounds tables (`gc/tests/published_bounds_isolation.rs` explains the
    // flake that caused), and this binary runs whole VMs in parallel. The
    // computation -- a non-moving young sweep moves `VmHeap::live_bytes_estimate`
    // and leaves `allocated_bytes` where it was, which is the `8M->8M` bug --
    // is pinned in its own process by
    // `gc/tests/gengc_r4w2_obs.rs::xlog_occupancy_sees_a_non_moving_young_sweep`.

    /// gcd d10/o retirement: HotSpot 25 ZGC's start and end lines
    /// (25.0.4, `-XX:+UseZGC -Xmx256m -Xlog:gc`, `Gcd1MxNamesProbe`):
    /// `GC(0) Major Collection (System.gc()) 8M(3%)->2M(1%) 0.021s`.
    #[test]
    fn xlog_zgc_lines_have_hotspot_25s_shape() {
        const M: usize = 1024 * 1024;
        let [start, end] = render_xlog_zgc_lines(
            0,
            ZGC_MAJOR_COLLECTION,
            GcDoor::SystemGc.cause(),
            8 * M + 5,
            2 * M,
            256 * M,
            21_400_000,
        );
        assert_eq!(start, "GC(0) Major Collection (System.gc())");
        assert_eq!(
            end,
            "GC(0) Major Collection (System.gc()) 8M(3%)->2M(1%) 0.021s"
        );
        let [_, end] =
            render_xlog_zgc_lines(7, ZGC_MINOR_COLLECTION, "Allocation Rate", 0, 0, 0, 0);
        assert_eq!(
            end,
            "GC(7) Minor Collection (Allocation Rate) 0M(0%)->0M(0%) 0.000s"
        );
    }

    /// gcd d10/o retirement: a collection is Minor only when the young-only
    /// cycle count moved across it.
    #[test]
    fn collection_kind_follows_the_young_only_cycle_count() {
        use cratonvm_gc::gc_metrics::CollectionKind;
        assert_eq!(collection_kind(0, 0), CollectionKind::Major);
        assert_eq!(collection_kind(3, 3), CollectionKind::Major);
        assert_eq!(collection_kind(3, 4), CollectionKind::Minor);
    }

    #[test]
    fn xlog_gc_line_has_hotspots_shape() {
        let line = render_xlog_gc_line(
            3,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            24 * 1024 * 1024 + 5,
            3 * 1024 * 1024,
            96 * 1024 * 1024,
            2_345_000,
        );
        assert_eq!(
            line,
            "GC(3) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms"
        );
    }

    /// gc-common w3-f: the per-pause `--verbose:gc` line carries this pause's
    /// TTSP and take-over coverage as space-free `key=value` pairs.
    #[test]
    fn w3f_pause_line_carries_ttsp_and_takeover_coverage() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 41_999,
            straggler: Some(3),
            xt_post_quota_ns: 12_500,
            xt_pass_ns: 30_100,
            xt: (2, 1, 0, 17, 3, 5),
        };
        let line = render_pause_line(7, GcDoor::AllocationFailure, facts, 2_345_678);
        assert_eq!(
            line,
            "[GC] pause: gc=7 door=alloc-failure ttsp_us=41 straggler=3 xt_pass_us=30 \
             xt_post_quota_us=12 collect_us=2345 xt_passes=2 xt_frozen=1 xt_unclassified=0 xt_roots=17 \
             hw_windows=3 hw_roots=5"
        );
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            let l = render_pause_line(0, door, facts, 0);
            assert!(
                l.split(' ').skip(2).all(|kv| kv.split_once('=').is_some()),
                "every field after `[GC] pause:` is key=value: {l}"
            );
        }
        // gc-common w5-a: no participating arrival spells `none`, like the
        // exit line's `max_straggler=`.
        let none = PauseSafepointFacts {
            straggler: None,
            ..facts
        };
        assert!(render_pause_line(1, GcDoor::SystemGc, none, 0).contains(" straggler=none "));
    }

    /// gc-common w5-a (`common-w4f-gc-event-durations-sampled-after-release`):
    /// a sealed collection hands the pause line and the `-Xlog:gc` line the
    /// SAME duration, and its after-heap figures, however late they render.
    #[test]
    fn w5a_a_sealed_collection_reports_one_duration_everywhere() {
        let t0 = std::time::Instant::now();
        let end = t0 + std::time::Duration::from_micros(2_345);
        let start = GcEventStart {
            t0,
            wall_start_ns: 0,
            heap_used_before: 24 * 1024 * 1024,
            heap_committed_before: 96 * 1024 * 1024,
            events: true,
            beans: None,
            pause_line: None,
            sealed: Some((end, 3 * 1024 * 1024, 96 * 1024 * 1024)),
            beans_after: None,
            backend_counted: false,
            young_only_cycles_before: 0,
            g1_mixed_before: None,
            g1_old_pauses_before: None,
            cpu_start: None,
            metaspace_before: None,
            classes_unloaded_before: 0,
            tlab_totals: None,
            allocated_at_start: 0,
            heap_debug_before: None,
            heap_debug_after: None,
            g1_heap_debug_before: None,
            g1_heap_debug_after: None,
            escalation: None,
            zgc_pause_split: None,
            g1_regions_before: None,
            g1_regions_after: None,
            g1_prepare_before: None,
            g1_concurrent_start: false,
            g1_marking_idle: false,
            g1_cycle_seq_before: None,
            g1_evacuation_failed: false,
            full_scope_id: None,
            zgc_causes: None,
        };
        // Rendering happens "later" -- the sealed end does not move.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (duration_ns, after) = start.end_facts();
        assert_eq!(duration_ns, 2_345_000);
        assert_eq!(after, Some((3 * 1024 * 1024, 96 * 1024 * 1024)));
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let pause = render_pause_line(4, GcDoor::AllocationThreshold, facts, duration_ns);
        let (heap_after, committed) = after.unwrap();
        let xlog = render_xlog_gc_line(
            4,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            start.heap_used_before,
            heap_after,
            committed,
            duration_ns,
        );
        assert!(pause.contains(" collect_us=2345 "), "{pause}");
        assert_eq!(
            xlog,
            "GC(4) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms"
        );
        // Unsealed: measured to "now", and no heap figures (the caller reads
        // them live, the pre-w5-a behaviour).
        let unsealed = GcEventStart {
            sealed: None,
            ..start
        };
        let (late_ns, late_after) = unsealed.end_facts();
        assert!(late_ns >= 2_000_000);
        assert_eq!(late_after, None);
        // zr z1/tr: off ZGC (no causes captured) every door keeps its own.
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
        ] {
            assert_eq!(unsealed.cause(door), door.cause());
        }
    }

    /// zr z3/t2: the occupancy sites are exactly the doors an occupancy
    /// verdict initiates; the generic failure retries are not among them; the
    /// per-thread classification is scoped to the pause and restored.
    #[test]
    fn zr3t2_forced_sites_classify_exactly_and_the_scope_restores() {
        use super::super::gc_and_alloc::zgc_forced_site_is_occupancy as occ;
        for site in [
            "tlab-alloc-shaped",
            "jit-newarray-zgc-headroom",
            "vm-exec-native-array",
            "vm-exec-pressure",
            "jit-box-pressure",
            "jit-object-young-probe",
            "jit-anewarray-young-probe",
            "tlab-refill-wedge-gate",
        ] {
            assert!(occ(site), "{site}");
        }
        for site in [
            "jit-helpers",
            "vm-exec",
            "unlabelled",
            "jit-ldc-string",
            "tlab-refill-wedge",
        ] {
            assert!(!occ(site), "{site} is a failure retry");
        }
        assert!(!zgc_forced_door_occupancy(), "default: a failed allocation");
        let inner = with_zgc_forced_door_occupancy(true, || {
            let nested = with_zgc_forced_door_occupancy(false, zgc_forced_door_occupancy);
            (nested, zgc_forced_door_occupancy())
        });
        assert_eq!(
            inner,
            (false, true),
            "nested scope restores the outer value"
        );
        assert!(!zgc_forced_door_occupancy(), "restored after the pause");
    }

    /// zr z9/hp: a ZGC collection writes no level-1 pause rows unless the
    /// HotSpot phase set is killed; every other collector keeps its rows.
    #[test]
    fn zr9hp_zgc_writes_no_pause_level1_rows_by_default() {
        // ZGC facts present (ZGC with its JFR events on), switch on: none.
        assert!(!zgc_jfr_pause_level1_rows(true, true));
        // Kill control: the z4/jf laps come back.
        assert!(zgc_jfr_pause_level1_rows(true, false));
        // Generational / G1 / ZGC with CRATONVM_ZGC_JFR_EVENTS=0: unchanged.
        assert!(zgc_jfr_pause_level1_rows(false, true));
        assert!(zgc_jfr_pause_level1_rows(false, false));
    }

    /// zs s7/jf: a ZGC collection's pause is written as HotSpot 25's named
    /// pauses, cut at the split's boundaries, contiguous and summing to the
    /// pause; a cycle whose mark start was its own pause has two; no split
    /// (or one that does not fit) leaves the caller's one `GC Pause` row.
    #[test]
    fn zs7jf_zgc_pause_rows_are_hotspots_named_pauses() {
        use cratonvm_gc::gc_metrics::ZgcPauseSplit;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |us: u64| t0 + Duration::from_micros(us);
        let wall = 1_000_000_000u64;
        // A stop-the-world major collection of 1 ms: 100 us to the end of
        // the snapshot, 700 us to the end of reference processing.
        let split = ZgcPauseSplit::new(Some(at(100)), at(700));
        let rows = zgc_hotspot_pause_rows(Some(&split), t0, wall, 1_000_000, true);
        assert_eq!(
            rows,
            vec![
                ("Pause Mark Start (Major)", wall, 100_000),
                ("Pause Mark End", wall + 100_000, 600_000),
                ("Pause Relocate Start", wall + 700_000, 300_000),
            ]
        );
        assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 1_000_000);
        // A young collection's mark start has no `(Major)`.
        let rows = zgc_hotspot_pause_rows(Some(&split), t0, wall, 1_000_000, false);
        assert_eq!(rows[0].0, "Pause Mark Start");
        // A concurrently marked cycle: its mark start was its own pause.
        let conc = ZgcPauseSplit::new(None, at(400));
        let rows = zgc_hotspot_pause_rows(Some(&conc), t0, wall, 1_000_000, true);
        assert_eq!(
            rows,
            vec![
                ("Pause Mark End", wall, 400_000),
                ("Pause Relocate Start", wall + 400_000, 600_000),
            ]
        );
        // No split, or one past the pause's end: the caller's one row.
        assert!(zgc_hotspot_pause_rows(None, t0, wall, 1_000_000, true).is_empty());
        let late = ZgcPauseSplit::new(Some(at(100)), at(2_000));
        assert!(zgc_hotspot_pause_rows(Some(&late), t0, wall, 1_000_000, true).is_empty());
    }

    /// zs s10/jf3 (ported by zt t1/jf): a concurrent cycle's mark-start pause
    /// becomes the first row of its closing collection and the cycle span
    /// starts there; another cycle's pause, a collection with no named rows,
    /// or a pause after the collection's end leaves rows and span as they
    /// were (positive control: the matching case changes both).
    #[test]
    fn zs10jf3_concurrent_cycle_shape_joins_the_mark_start_pause() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let rows = || {
            vec![
                (ZGC_PAUSE_MARK_END, 5_000u64, 300u64),
                (ZGC_PAUSE_RELOCATE_START, 5_300, 200),
            ]
        };
        let mut pauses = rows();
        let span = zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut pauses, true, 5_500);
        assert_eq!(span, Some((1_000, 4_500)));
        assert_eq!(pauses[0], ("Pause Mark Start (Major)", 1_000, 100));
        assert_eq!(pauses.len(), 3);
        let mut minor = rows();
        let _ = zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut minor, false, 5_500);
        assert_eq!(minor[0].0, "Pause Mark Start");
        // Another cycle's pause: unchanged.
        let mut other = rows();
        assert_eq!(
            zgc_cycle_shape(&shared, 5, Some((4, 1_000, 100)), &mut other, true, 5_500),
            None
        );
        assert_eq!(other, rows());
        // No named rows (the s7 `GC Pause` shape): unchanged.
        let mut none: Vec<(&'static str, u64, u64)> = Vec::new();
        assert_eq!(
            zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut none, true, 5_500),
            None
        );
        assert!(none.is_empty());
        // A pause that started after the collection ended: unchanged.
        let mut late = rows();
        assert_eq!(
            zgc_cycle_shape(&shared, 4, Some((4, 9_000, 100)), &mut late, true, 5_500),
            None
        );
        assert_eq!(late, rows());
        assert_eq!(
            zgc_cycle_shape(&shared, 4, None, &mut rows(), true, 5_500),
            None
        );
    }

    /// zt t5/jf (R9): HotSpot 25.0.3's ZGC `gc,phases` / `gc,metaspace`
    /// line shapes (local `-Xlog:gc*` capture): `GC(3) Y: Pause Mark Start
    /// (Major) 0.011ms`, `GC(3) O: Pause Mark End 0.016ms`,
    /// `GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved`; a minor's
    /// lines are `y:`.
    #[test]
    fn zt5jf_zgc_pause_and_metaspace_lines_have_hotspots_shape() {
        let rows = [
            (ZGC_PAUSE_MARK_START_MAJOR, 0u64, 11_000u64),
            (ZGC_PAUSE_MARK_END, 0, 16_000),
            (ZGC_PAUSE_RELOCATE_START, 0, 10_400),
        ];
        assert_eq!(
            render_xlog_zgc_pause_lines(3, &rows, false),
            vec![
                "GC(3) Y: Pause Mark Start (Major) 0.011ms".to_string(),
                "GC(3) O: Pause Mark End 0.016ms".to_string(),
                "GC(3) O: Pause Relocate Start 0.010ms".to_string(),
            ]
        );
        let minor = [(ZGC_PAUSE_MARK_START, 0u64, 12_000u64)];
        assert_eq!(
            render_xlog_zgc_pause_lines(7, &minor, true),
            vec!["GC(7) y: Pause Mark Start 0.012ms".to_string()]
        );
        assert!(render_xlog_zgc_pause_lines(1, &[], false).is_empty());
        let m = 1024 * 1024;
        assert_eq!(
            render_xlog_zgc_metaspace_line(3, false, (3 * m + 5, 3 * m + 9, 1088 * m)),
            "GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved"
        );
        assert_eq!(
            render_xlog_zgc_metaspace_line(4, true, (0, 0, 0)),
            "GC(4) y: Metaspace: 0M used, 0M committed, 0M reserved"
        );
    }

    /// zt t6/ob4 (R9 tail): HotSpot 25.0.3's ZGC `gc,heap` capacity lines
    /// (local `-Xlog:gc*` capture, `-Xmx256m`): `Min Capacity: 8M(3%)`,
    /// `Max Capacity: 256M(100%)`, `Soft Max Capacity: 256M(100%)`; a soft
    /// max above the max prints the max.
    #[test]
    fn zt6ob4_zgc_capacity_lines_have_hotspots_shape() {
        let m = 1024 * 1024;
        assert_eq!(
            render_xlog_zgc_capacity_lines(0, false, 8 * m, 256 * m, 256 * m),
            [
                "GC(0) O: Min Capacity: 8M(3%)".to_string(),
                "GC(0) O: Max Capacity: 256M(100%)".to_string(),
                "GC(0) O: Soft Max Capacity: 256M(100%)".to_string(),
            ]
        );
        assert_eq!(
            render_xlog_zgc_capacity_lines(5, true, 64 * m, 256 * m, 128 * m),
            [
                "GC(5) y: Min Capacity: 64M(25%)".to_string(),
                "GC(5) y: Max Capacity: 256M(100%)".to_string(),
                "GC(5) y: Soft Max Capacity: 128M(50%)".to_string(),
            ]
        );
        assert_eq!(
            render_xlog_zgc_capacity_lines(1, false, 8 * m, 64 * m, usize::MAX)[2],
            "GC(1) O: Soft Max Capacity: 64M(100%)"
        );
    }

    /// zt t1/jf: the young collection inside a ZGC major promotes every
    /// survivor in the single-generation mode and on the `System.gc()` door;
    /// the generational mode keeps its promotion age otherwise.
    #[test]
    fn zt1jf_major_young_threshold_is_promote_all_unless_generational() {
        assert_eq!(zgc_major_young_threshold(None, "System.gc()"), 0);
        assert_eq!(zgc_major_young_threshold(None, "Allocation Rate"), 0);
        assert_eq!(zgc_major_young_threshold(Some(3), "Warmup"), 3);
        assert_eq!(zgc_major_young_threshold(Some(3), "System.gc()"), 0);
        assert_eq!(zgc_major_young_threshold(Some(u32::MAX), "Proactive"), i32::MAX);
    }

    /// zt t1/jf: the four JFR switches of this lane are read per call (the
    /// file's rule since gci i20/ob), default ON, `=0` kills each.
    #[test]
    fn zt1jf_jfr_switches_follow_a_thread_override() {
        use cratonvm_types::flags::with_thread_overrides;
        let switches: [(&str, fn() -> bool); 5] = [
            (
                "CRATONVM_ZGC_JFR_METASPACE_CHUNK_FREE_LISTS",
                zgc_jfr_metaspace_chunk_free_lists_enabled,
            ),
            (
                "CRATONVM_ZGC_JFR_CONCURRENT_CYCLE_SPAN",
                zgc_jfr_concurrent_cycle_span_enabled,
            ),
            ("CRATONVM_ZGC_JFR_MAJOR_YOUNG", zgc_jfr_major_young_enabled),
            (
                "CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS",
                zgc_jfr_pause_vm_operations_enabled,
            ),
            // zt t3/jf.
            (
                "CRATONVM_ZGC_JFR_SYSTEM_GC_FROM_DOOR",
                zgc_jfr_system_gc_from_door_enabled,
            ),
        ];
        for (key, read) in switches {
            with_thread_overrides(&[(key, Some("0"))], || assert!(!read(), "{key}=0"));
            with_thread_overrides(&[(key, None)], || assert!(read(), "{key} default"));
        }
    }

    /// gci i20/ob (`gci-i12sx-vm-statics-ratchet-red-on-dev-by-27-FIXED-20261016`):
    /// the ZGC observability switches of this file are read per call, not
    /// latched by the first reader in the process: a per-thread override
    /// reaches each of them, in both directions, in one test.
    #[test]
    fn gci_i20_ob_zgc_observability_switches_follow_a_thread_override() {
        use cratonvm_types::flags::with_thread_overrides;
        with_thread_overrides(&[("CRATONVM_ZGC_JFR_EVENTS", Some("0"))], || {
            assert!(!zgc_jfr_events_enabled())
        });
        with_thread_overrides(&[("CRATONVM_ZGC_JFR_EVENTS", None)], || {
            assert!(zgc_jfr_events_enabled())
        });
        with_thread_overrides(&[("CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET", Some("0"))], || {
            assert!(!zgc_jfr_hotspot_phase_set_enabled());
            assert!(zgc_jfr_pause_level1_rows_for(true), "killed: ZGC laps written");
        });
        with_thread_overrides(&[("CRATONVM_ZGC_JFR_HOTSPOT_PHASE_SET", None)], || {
            assert!(zgc_jfr_hotspot_phase_set_enabled());
            assert!(!zgc_jfr_pause_level1_rows_for(true), "default: no ZGC level-1 rows");
            assert!(zgc_jfr_pause_level1_rows_for(false), "other collectors keep theirs");
        });
        with_thread_overrides(&[("CRATONVM_ZGC_XLOG_ALLOCATION_STALL", Some("1"))], || {
            assert!(zgc_xlog_allocation_stall_enabled())
        });
        with_thread_overrides(&[("CRATONVM_ZGC_XLOG_ALLOCATION_STALL", None)], || {
            assert!(!zgc_xlog_allocation_stall_enabled())
        });
        // No latch is left in this file for these three switches.
        let src = include_str!("gc_events.rs");
        for f in [
            "\nfn zgc_jfr_events_enabled() -> bool \u{7b}",
            "\nfn zgc_jfr_hotspot_phase_set_enabled() -> bool \u{7b}",
            "\nfn zgc_xlog_allocation_stall_enabled() -> bool \u{7b}",
        ] {
            let at = src.find(f).expect(f);
            let body = &src[at..at + 160];
            assert!(!body.contains("OnceLock"), "{f} latches again");
        }
    }

    /// zr z2/t2: HotSpot ZGC's stall line, three decimals of milliseconds.
    #[test]
    fn zr2t2_allocation_stall_line_has_hotspots_shape() {
        assert_eq!(
            zgc_allocation_stall_line("main", 204_400),
            "Allocation Stall (main) 0.204ms"
        );
        assert_eq!(
            zgc_allocation_stall_line("worker-1", 12_345_678_901),
            "Allocation Stall (worker-1) 12345.679ms"
        );
    }

    /// zr z1/tr: an allocation-driven ZGC collection is reported under HotSpot
    /// 25 ZGC's cause; explicit doors keep theirs.
    #[test]
    fn zr1_zgc_allocation_doors_take_hotspots_zgc_causes() {
        let t0 = std::time::Instant::now();
        let start = GcEventStart {
            t0,
            wall_start_ns: 0,
            heap_used_before: 0,
            heap_committed_before: 0,
            events: false,
            beans: None,
            pause_line: None,
            sealed: None,
            beans_after: None,
            backend_counted: false,
            young_only_cycles_before: 0,
            g1_mixed_before: None,
            g1_old_pauses_before: None,
            cpu_start: None,
            metaspace_before: None,
            classes_unloaded_before: 0,
            tlab_totals: None,
            allocated_at_start: 0,
            heap_debug_before: None,
            heap_debug_after: None,
            g1_heap_debug_before: None,
            g1_heap_debug_after: None,
            escalation: None,
            zgc_pause_split: None,
            g1_regions_before: None,
            g1_regions_after: None,
            g1_prepare_before: None,
            g1_concurrent_start: false,
            g1_marking_idle: false,
            g1_cycle_seq_before: None,
            g1_evacuation_failed: false,
            full_scope_id: None,
            zgc_causes: Some(("Warmup", "Allocation Stall")),
        };
        assert_eq!(start.cause(GcDoor::AllocationThreshold), "Warmup");
        assert_eq!(start.cause(GcDoor::AllocationFailure), "Allocation Stall");
        assert_eq!(start.cause(GcDoor::SystemGc), "System.gc()");
        assert_eq!(
            start.cause(GcDoor::MetadataThreshold),
            "Metadata GC Threshold"
        );
        // zs s8/jf2 (on its own door, `GcDoor::JvmtiForceGc`, since the
        // 2026-10-18 dev merge): JVMTI ForceGarbageCollection reports
        // HotSpot's JVMTI cause on ZGC too; the `System.gc()` door keeps its own.
        assert_eq!(
            start.cause(GcDoor::JvmtiForceGc),
            "JvmtiEnv ForceGarbageCollection"
        );
        assert_eq!(start.cause(GcDoor::SystemGc), "System.gc()");
        let [first, end] = render_xlog_zgc_lines(
            2,
            ZGC_MAJOR_COLLECTION,
            start.cause(GcDoor::AllocationThreshold),
            0,
            0,
            0,
            0,
        );
        assert_eq!(first, "GC(2) Major Collection (Warmup)");
        assert!(end.starts_with("GC(2) Major Collection (Warmup) "), "{end}");
    }

    /// gc-common w5-a (`common-w4a-non-collection-pauses-have-no-pause-line`):
    /// a non-collection pause prints the collection line's grammar with
    /// `gc=-`, its own door token and `work_us=`.
    #[test]
    fn w5a_non_collection_pause_line_shape() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 7_000,
            straggler: Some(9),
            xt_post_quota_ns: 1_000,
            xt_pass_ns: 2_000,
            xt: (1, 0, 0, 0, 0, 0),
        };
        let line = render_safepoint_pause_line(
            "-",
            NonCollectionPause::G1Remark.token(),
            facts,
            "work_us",
            40_000_000,
        );
        assert_eq!(
            line,
            "[GC] pause: gc=- door=g1-remark ttsp_us=7 straggler=9 xt_pass_us=2 \
             xt_post_quota_us=1 work_us=40000 xt_passes=1 xt_frozen=0 xt_unclassified=0 xt_roots=0 \
             hw_windows=0 hw_roots=0"
        );
        let kinds = [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::HeapInspection,
            NonCollectionPause::LoopExit,
            NonCollectionPause::Redefinition,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
            NonCollectionPause::ZgcMarkStart,
            NonCollectionPause::G1InitialMark,
            NonCollectionPause::G1Remark,
            NonCollectionPause::IcGrace,
        ];
        let mut tokens: Vec<&str> = kinds.iter().map(|k| k.token()).collect();
        for t in &tokens {
            assert!(!t.contains(' ') && !t.contains('='), "scrapable token: {t}");
            for door in [
                "alloc-threshold",
                "alloc-failure",
                "system-gc",
                "heap-inspection-gc",
                "heap-dump-gc",
            ] {
                assert_ne!(*t, door, "a non-collection token must not alias a GC door");
            }
        }
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), kinds.len(), "tokens are distinct");
        // A sealed timer reports its sealed work time, not the time to print.
        let mut timer = Some(NonCollectionPauseTimer {
            kind: NonCollectionPause::FrameTrace,
            facts: Some(facts),
            t0: std::time::Instant::now(),
            work_ns: None,
            beans: None,
            jfr: None,
            g1_cycle: None,
            folded: None,
            zgc_jfr: None,
        });
        // zr z4/jf: a no-op on a timer with no ZGC JFR row.
        non_collection_pause_note_opened(&mut timer, true);
        non_collection_pause_seal(&mut timer);
        assert!(timer.as_ref().is_some_and(|t| t.work_ns.is_some()));
        non_collection_pause_seal(&mut None);
    }

    /// gc-common w7-f: exactly the marking pauses of the backend a bean set
    /// describes count toward it; frame-trace, heap-dump and Generational's
    /// marking pauses count toward none. G1's carry `No GC` (a concurrent
    /// phase to Micrometer), ZGC's mark start an allocation cause (a pause).
    #[test]
    fn w7f_only_backend_marking_pauses_count_toward_a_bean() {
        use cratonvm_gc::gc_metrics::BackendBeanShape;
        assert_eq!(
            NonCollectionPause::G1Remark.bean_pause(),
            Some((BackendBeanShape::G1, "No GC"))
        );
        assert_eq!(
            NonCollectionPause::G1InitialMark.bean_pause().map(|p| p.0),
            Some(BackendBeanShape::G1)
        );
        assert_eq!(
            NonCollectionPause::ZgcMarkStart.bean_pause(),
            // zr z1/tr: HotSpot 25 ZGC's spelling (it never says
            // `Allocation Failure`); the start substitutes the heap's rule.
            Some((BackendBeanShape::Zgc, "Allocation Rate"))
        );
        for k in [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::HeapInspection,
            NonCollectionPause::LoopExit,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
        ] {
            assert_eq!(k.bean_pause(), None, "{k:?}");
        }
    }

    /// gen r5w3/obs7: HotSpot Serial's `gc,heap` lines from the pool
    /// snapshots at the two edges of a collection: `DefNew` is eden +
    /// survivor, `From` the survivor pool, all in truncated KiB.
    #[test]
    fn xlog_heap_lines_have_serials_shape() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let before = [
            pool(JMX_POOL_EDEN, 17_472 * K + 5, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 2_176 * K, 2_176 * K),
            pool(JMX_POOL_TENURED, 3_000 * K, 43_712 * K),
        ];
        let lines = render_xlog_heap_lines(3, &before, &after).expect("Serial's pools");
        assert_eq!(
            lines[0],
            "GC(3) DefNew: 17472K(17472K)->2176K(19648K) Eden: 17472K(17472K)->0K(17472K) \
             From: 0K(0K)->2176K(2176K)"
        );
        assert_eq!(lines[1], "GC(3) Tenured: 0K(43712K)->3000K(43712K)");
        // Not Serial's pools (G1's): no lines.
        assert!(render_xlog_heap_lines(3, &before[..1], &after).is_none());
    }

    /// gcg g1/c: survivors that are not a KiB multiple no longer print
    /// `DefNew` one KiB above `Eden + From` (the vf2 r2-2 sanity check).
    #[test]
    fn gcg_g1c_xlog_heap_line_defnew_is_eden_plus_from_in_kib() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let surv = 57_000;
        let before = [
            pool(JMX_POOL_EDEN, 10 * K * K, 12 * K * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 12 * K * K - surv),
            pool(JMX_POOL_SURVIVOR, surv, surv),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let lines = render_xlog_heap_lines(3, &before, &after).expect("Serial's pools");
        assert_eq!(
            lines[0],
            "GC(3) DefNew: 10240K(12288K)->55K(12288K) Eden: 10240K(12288K)->0K(12232K) \
             From: 0K(0K)->55K(56K)"
        );
    }

    #[test]
    fn xlog_cpu_line_has_hotspots_shape() {
        assert_eq!(
            render_xlog_cpu_line(7, 0.013, 0.0049, 9_876_543),
            "GC(7) User=0.01s Sys=0.00s Real=0.01s"
        );
        assert_eq!(
            render_xlog_cpu_line(0, -0.5, 1.5, 1_234_000_000),
            "GC(0) User=0.00s Sys=1.50s Real=1.23s"
        );
    }

    #[test]
    fn system_gc_is_reported_with_hotspots_cause_string() {
        assert_eq!(GcDoor::SystemGc.cause(), "System.gc()");
        let line = render_xlog_gc_line(0, "Pause Full", GcDoor::SystemGc.cause(), 0, 0, 0, 999);
        assert_eq!(line, "GC(0) Pause Full (System.gc()) 0M->0M(0M) 0.001ms");
    }

    /// gci i2/oo: `jcmd GC.run` is HotSpot's `_dcmd_gc_run`
    /// (`GCCause::to_string`: `Diagnostic Command`), a full collection, with a
    /// space-free token of its own on the `[GC] pause:` line.
    #[test]
    fn gci_i2oo_diagnostic_command_has_hotspots_cause_string() {
        assert_eq!(GcDoor::DiagnosticCommand.cause(), "Diagnostic Command");
        let line = render_xlog_gc_line(
            2,
            "Pause Full",
            GcDoor::DiagnosticCommand.cause(),
            0,
            0,
            0,
            999,
        );
        assert_eq!(line, "GC(2) Pause Full (Diagnostic Command) 0M->0M(0M) 0.001ms");
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        assert!(render_pause_line(5, GcDoor::DiagnosticCommand, facts, 0).contains(" door=dcmd "));
    }

    /// gci i9/dc (`gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017` fix 2):
    /// the live-only walks' collections carry HotSpot's own causes
    /// (`GCCause::to_string`: `_heap_inspection`, `_heap_dump`), not `GC.run`'s,
    /// and door tokens of their own that alias no non-collection pause.
    #[test]
    fn gci_i9dc_live_walk_collections_have_hotspots_cause_strings() {
        assert_eq!(GcDoor::HeapInspection.cause(), "Heap Inspection Initiated GC");
        assert_eq!(GcDoor::HeapDump.cause(), "Heap Dump Initiated GC");
        let line = render_xlog_gc_line(
            3,
            "Pause Full",
            GcDoor::HeapInspection.cause(),
            0,
            0,
            0,
            999,
        );
        assert_eq!(line, "GC(3) Pause Full (Heap Inspection Initiated GC) 0M->0M(0M) 0.001ms");
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let inspect = render_pause_line(6, GcDoor::HeapInspection, facts, 0);
        assert!(inspect.contains(" door=heap-inspection-gc "), "{inspect}");
        let dump = render_pause_line(7, GcDoor::HeapDump, facts, 0);
        assert!(dump.contains(" door=heap-dump-gc "), "{dump}");
        for k in [NonCollectionPause::HeapInspection, NonCollectionPause::HeapDump] {
            assert_ne!(k.token(), "heap-inspection-gc");
            assert_ne!(k.token(), "heap-dump-gc");
        }
    }

    /// gcf f4/r: HotSpot 25.0.3's `gc,metaspace` line without a class space
    /// (`GC(0) Metaspace: 4137K(4224K)->4137K(4224K)` there), committed = used
    /// here, truncating K.
    #[test]
    fn gcf_f4r_metaspace_line_is_hotspots_no_class_space_shape() {
        use crate::vm::vm_init::MetaspaceStats;
        assert_eq!(
            render_xlog_metaspace_line(
                3,
                &MetaspaceStats::flat(96 * 1024 + 1023),
                &MetaspaceStats::flat(97 * 1024)
            ),
            "GC(3) Metaspace: 96K(96K)->97K(97K)"
        );
        assert_eq!(
            render_xlog_metaspace_line(0, &MetaspaceStats::flat(0), &MetaspaceStats::flat(0)),
            "GC(0) Metaspace: 0K(0K)->0K(0K)"
        );
    }

    /// gci i33/ms: with a compressed class space, HotSpot 25's
    /// `print_metaspace_change` -- `Metaspace`, `NonClass`, `Class`, each
    /// `usedK(committedK)->usedK(committedK)`.
    #[test]
    fn gci_i33ms_metaspace_line_has_the_class_space() {
        use crate::vm::vm_init::MetaspaceStats;
        const K: u64 = 1024;
        let at = |used: u64, committed: u64, class: (u64, u64)| MetaspaceStats {
            used,
            committed,
            reserved: 1_114_112 * K,
            class_space: Some((class.0, class.1, 1_048_576 * K)),
        };
        let before = at(4_137 * K + 5, 4_224 * K, (345 * K, 384 * K));
        let after = at(4_140 * K, 4_224 * K, (346 * K, 384 * K));
        assert_eq!(
            render_xlog_metaspace_line(0, &before, &after),
            "GC(0) Metaspace: 4137K(4224K)->4140K(4224K) NonClass: 3792K(3840K)->3794K(3840K) Class: 345K(384K)->346K(384K)"
        );
        // A before without a class space reads its class part as 0.
        assert!(render_xlog_metaspace_line(1, &MetaspaceStats::flat(K), &after)
            .contains(" Class: 0K(0K)->346K(384K)"));
    }

    /// gcg g3/x: HotSpot 25.0.3 Serial's debug `Heap Before GC` / `Metaspace
    /// Before GC` blocks (the measured `GC(0)` of `-Xlog:gc*=debug`, mode
    /// `sysgc` of `GcgX3XlogProbe`), and `invocations=` from the beans.
    #[test]
    fn gcg_g3x_heap_debug_blocks_are_hotspot_serials_text() {
        use crate::runtime::unified_logging::{ExitSpace, SerialExitHeap};
        use cratonvm_gc::gc_metrics::{
            GcBeanSnapshot, JmxCollector, JmxPoolUsage, JMX_SERIAL_OLD_COLLECTOR,
            JMX_SERIAL_YOUNG_COLLECTOR,
        };
        let s = |capacity: u64, used: u64, bounds: [u64; 3]| ExitSpace {
            capacity,
            used,
            bounds,
        };
        let heap = SerialExitHeap {
            young: s(
                19648 * 1024,
                9269 * 1024,
                [0x17535600000, 0x17536b50000, 0x17536b50000],
            ),
            eden: s(
                17472 * 1024,
                0x90d730,
                [0x17535600000, 0x17535f0d730, 0x17536710000],
            ),
            from: s(
                2176 * 1024,
                0,
                [0x17536710000, 0x17536710000, 0x17536930000],
            ),
            to: s(
                2176 * 1024,
                0,
                [0x17536930000, 0x17536930000, 0x17536b50000],
            ),
            old: s(
                43712 * 1024,
                1557 * 1024,
                [0x17536b50000, 0x17539600000, 0x17539600000],
            ),
            the: s(
                43712 * 1024,
                0x185738,
                [0x17536b50000, 0x17536cd5738, 0x17539600000],
            ),
            metaspace: (92 * 1024, 92 * 1024, 92 * 1024),
            class_space: None,
        };
        let (h, m) = render_xlog_heap_debug_blocks(0, "Before", (0, 0), &heap);
        assert_eq!(
            h,
            [
                "GC(0) Heap Before GC invocations=0 (full 0):",
                "GC(0)  DefNew     total 19648K, used 9269K [0x0000017535600000, 0x0000017536b50000, 0x0000017536b50000)",
                "GC(0)   eden space 17472K,  53% used [0x0000017535600000, 0x0000017535f0d730, 0x0000017536710000)",
                "GC(0)   from space 2176K,   0% used [0x0000017536710000, 0x0000017536710000, 0x0000017536930000)",
                "GC(0)   to   space 2176K,   0% used [0x0000017536930000, 0x0000017536930000, 0x0000017536b50000)",
                "GC(0)  Tenured    total 43712K, used 1557K [0x0000017536b50000, 0x0000017539600000, 0x0000017539600000)",
                "GC(0)   the  space 43712K,   3% used [0x0000017536b50000, 0x0000017536cd5738, 0x0000017539600000)",
            ]
        );
        assert_eq!(
            m,
            [
                "GC(0) Metaspace Before GC invocations=0 (full 0):",
                "GC(0)  Metaspace       used 92K, committed 92K, reserved 92K",
            ]
        );
        let collector = |name: &'static str, count: u64| JmxCollector {
            name,
            count,
            time_ms: 0,
            pools: &[],
            action: "end of minor GC",
        };
        let snap = GcBeanSnapshot::now(
            vec![
                collector(JMX_SERIAL_YOUNG_COLLECTOR, 3),
                collector(JMX_SERIAL_OLD_COLLECTOR, 1),
            ],
            Vec::<JmxPoolUsage>::new(),
        );
        assert_eq!(serial_invocations(&snap), (4, 1));
        let (h, _) = render_xlog_heap_debug_blocks(4, "After", (5, 1), &heap);
        assert_eq!(h[0], "GC(4) Heap After GC invocations=5 (full 1):");
    }

    /// gcf f4/r: the `gc,age` block in HotSpot 25.0.3 Serial's text (the
    /// measured `GC(0)` block of `-Xlog:gc+age=trace`), empty ages skipped.
    #[test]
    fn gcf_f4r_age_lines_are_hotspot_serials_text() {
        use crate::runtime::unified_logging::LogLevel;
        let mut ages = [0u64; 16];
        ages[1] = 112_328;
        ages[3] = 1_000;
        let lines = render_xlog_age_lines(0, 1_114_112, 15, 15, &ages);
        assert_eq!(
            lines,
            vec![
                (
                    LogLevel::Debug,
                    "GC(0) Desired survivor size 1114112 bytes, new threshold 15 (max threshold 15)"
                        .to_string()
                ),
                (LogLevel::Trace, "GC(0) Age table:".to_string()),
                (
                    LogLevel::Trace,
                    "GC(0) - age   1:     112328 bytes,     112328 total".to_string()
                ),
                (
                    LogLevel::Trace,
                    "GC(0) - age   3:       1000 bytes,     113328 total".to_string()
                ),
            ]
        );
    }

    /// gcg g5/yg (vg3 `x3_heapdbg_full`): the `gc,age` block prints on a
    /// young pause only, never on a `Pause Full`, as on HotSpot Serial.
    #[test]
    fn gcg_g5yg_age_lines_print_on_young_pauses_only() {
        assert!(xlog_age_lines_printed_for(false));
        assert!(!xlog_age_lines_printed_for(true));
    }

    /// gcg g4/yg (review finding 8): the `gc,cpu` clock exists on Windows as
    /// on Unix, and it does not run backwards across a busy loop.
    #[cfg(any(unix, windows))]
    #[test]
    fn gcg_g4yg_process_cpu_times_exist_on_windows_and_unix() {
        let (u0, s0) = process_cpu_secs().expect("process CPU times");
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let (u1, s1) = process_cpu_secs().expect("process CPU times");
        assert!(u0 >= 0.0 && s0 >= 0.0, "({u0}, {s0})");
        assert!(u1 >= u0 && s1 >= s0, "({u0}, {s0}) -> ({u1}, {s1})");
        assert!(
            render_xlog_cpu_line(4, u1 - u0, s1 - s0, 1_000_000).starts_with("GC(4) User="),
            "the line renders"
        );
    }

    /// gcg h1/ob (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full`):
    /// the young half of an escalated pause ends with the pause's young pools
    /// and the old generation as the major's decision saw it; the full half
    /// starts there. The two `gc` lines then read as HotSpot Serial's pair.
    #[test]
    fn gch_h1ob_escalation_mid_figures_replace_only_the_tenured_share() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        const M: usize = 1024 * 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let before = [
            pool(JMX_POOL_EDEN, 17_472 * K, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 34_490 * K, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 2_176 * K, 2_176 * K),
            pool(JMX_POOL_TENURED, 20_000 * K, 43_712 * K),
        ];
        let mid = escalation_mid_pools(&after, 43_708 * K);
        assert_eq!(mid[0], after[0], "eden as the pause left it");
        assert_eq!(mid[1], after[1], "survivors as the pause left them");
        assert_eq!((mid[2].used, mid[2].committed), (43_708 * K, 43_712 * K));
        // A mark above the end's commit lifts the commit (`used <= committed`).
        assert_eq!(
            escalation_mid_pools(&after, 50_000 * K)[2].committed,
            50_000 * K
        );
        let young = render_xlog_heap_lines(48, &before, &mid).expect("Serial's pools");
        assert_eq!(young[1], "GC(48) Tenured: 34490K(43712K)->43708K(43712K)");
        let full = render_xlog_heap_lines(49, &mid, &after).expect("Serial's pools");
        assert_eq!(full[1], "GC(49) Tenured: 43708K(43712K)->20000K(43712K)");
        // The heap figure: the end's young share plus the mark's old share.
        let heap_after = (2_176 * K + 20_000 * K) as usize;
        let heap_mid = escalation_mid_heap_used(heap_after, &after, 43_708 * K);
        assert_eq!(heap_mid as u64, 2_176 * K + 43_708 * K);
        assert_eq!(
            render_xlog_gc_line(
                48,
                "Pause Young",
                "Allocation Failure",
                50 * M,
                heap_mid,
                61 * M,
                7_320_000
            ),
            "GC(48) Pause Young (Allocation Failure) 50M->44M(61M) 7.320ms"
        );
        assert_eq!(escalation_mid_invocations((48, 1)), (49, 1));
    }

    /// gcg h1/ob: the debug `Heap After GC` block of the young half keeps the
    /// end's spaces with the tenured space at the mark and the metaspace as
    /// the pause found it.
    #[test]
    fn gch_h1ob_escalation_mid_exit_heap_moves_the_tenured_space_only() {
        let mut after = crate::runtime::unified_logging::SerialExitHeap::default();
        after.eden.used = 5;
        after.old.used = 100;
        after.the.used = 100;
        after.the.bounds = [0x1000, 0x1064, 0x2000];
        after.metaspace = (7, 7, 7);
        let mid = escalation_mid_exit_heap(&after, 300, Some((9, 9, 9)));
        assert_eq!(mid.eden, after.eden);
        assert_eq!((mid.old.used, mid.the.used), (300, 300));
        assert_eq!(mid.the.bounds, [0x1000, 0x1000 + 300, 0x2000]);
        assert_eq!(mid.metaspace, (9, 9, 9));
        assert_eq!(
            escalation_mid_exit_heap(&after, 300, None).metaspace,
            (7, 7, 7)
        );
    }

    /// gcg h1/ob: the `Copy` notification of an escalated pause ends at the
    /// mark with the young half's pools; the `MarkSweepCompact` one starts
    /// there with them; any other bean's record is untouched.
    #[test]
    fn gch_h1ob_escalated_notifications_split_at_the_mark() {
        use cratonvm_gc::gc_metrics::{
            GcNotificationRecord, JMX_SERIAL_OLD_COLLECTOR, JMX_SERIAL_YOUNG_COLLECTOR,
        };
        let t0 = std::time::Instant::now();
        let mark = t0 + std::time::Duration::from_millis(3);
        let end = t0 + std::time::Duration::from_millis(10);
        let rec = |collector| GcNotificationRecord {
            collector,
            action: "x",
            cause: "Allocation Failure",
            id: 1,
            start: t0,
            end,
            timestamp_ms: 100,
            before: Vec::new(),
            after: Vec::new(),
        };
        let mid = escalation_mid_pools(&[], 0);
        let mut recs = vec![
            rec(JMX_SERIAL_YOUNG_COLLECTOR),
            rec(JMX_SERIAL_OLD_COLLECTOR),
            rec("Other"),
        ];
        split_escalated_notifications(&mut recs, mark, &mid);
        assert_eq!((recs[0].start, recs[0].end), (t0, mark));
        assert_eq!((recs[1].start, recs[1].end), (mark, end));
        assert_eq!((recs[2].start, recs[2].end), (t0, end));
        assert_eq!(
            recs[0].timestamp_ms, 93,
            "the young stamp moves back with its end"
        );
        assert_eq!((recs[1].timestamp_ms, recs[2].timestamp_ms), (100, 100));
    }

    /// gcg h1/ob: one `getrusage` pair brackets an escalated pause; its two
    /// `gc,cpu` lines share the delta by wall time and sum to it.
    #[test]
    fn gch_h1ob_cpu_delta_splits_by_wall_time() {
        let ((yu, ys), (fu, fs)) = split_cpu_delta((0.4, 0.2), 1_000, 4_000);
        assert!((yu - 0.1).abs() < 1e-9 && (ys - 0.05).abs() < 1e-9);
        assert!((yu + fu - 0.4).abs() < 1e-9 && (ys + fs - 0.2).abs() < 1e-9);
        assert_eq!(split_cpu_delta((1.0, 1.0), 0, 0).0, (0.0, 0.0));
    }

    /// gch h2/ob: the escalated-pause split is default ON, `=0` its kill.
    #[test]
    fn gch_h2ob_escalated_pause_split_is_default_on_with_a_zero_kill() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_ESCALATED_PAUSE_SPLIT", None)],
            || assert!(escalated_pause_split_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_ESCALATED_PAUSE_SPLIT", Some("0"))],
            || assert!(!escalated_pause_split_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_ESCALATED_PAUSE_SPLIT", Some("1"))],
            || assert!(escalated_pause_split_enabled()),
        );
    }

    /// gch h2/ob: the delivery-failure line names the bean, the action and
    /// what failed, as one space-free token per key.
    #[test]
    fn gch_h2ob_gc_notification_failure_line_is_scrapeable() {
        let line = render_gc_notification_failure(
            3,
            "G1 Young Generation",
            "end of minor GC",
            "System.gc()",
            "java.lang.IllegalStateException: no builder",
        );
        assert_eq!(
            line,
            "[GC] gc_notification_failed: n=3 collector=G1_Young_Generation \
             action=end_of_minor_GC cause=System.gc() \
             error=java.lang.IllegalStateException:_no_builder"
        );
    }

    /// gch h3/os: a thrown delivery names its class and, when it has one, its
    /// message on one line, cut to the limit.
    #[test]
    fn gch_h3os_thrown_delivery_error_carries_its_message() {
        let ae = "java.lang.AssertionError";
        assert_eq!(thrown_error_text(ae, None), ae);
        assert_eq!(thrown_error_text(ae, Some("")), ae);
        assert_eq!(
            thrown_error_text(
                "java.lang.NoClassDefFoundError",
                Some("Foo$$Lambda/0x1\nat x")
            ),
            "java.lang.NoClassDefFoundError:Foo$$Lambda/0x1 at x"
        );
        let long = "m".repeat(GC_NOTIFICATION_FAILURE_MESSAGE_CHARS + 40);
        let text = thrown_error_text("E", Some(&long));
        assert_eq!(
            text.len(),
            "E:".len() + GC_NOTIFICATION_FAILURE_MESSAGE_CHARS
        );
        // The rendered line keeps the value one token.
        let line =
            render_gc_notification_failure(1, "Copy", "end of minor GC", "System.gc()", &text);
        assert_eq!(line.split(' ').count(), 7, "{line}");
    }
}

#[cfg(test)]
mod gci_i11_go_g1_observability_tests {
    //! gci i11/go: G1's `-Xlog:gc` / JFR / notification names
    //! (`gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-FIXED-20261016`
    //! findings 4 and 5, `gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (a)).
    use super::*;

    const M: usize = 1024 * 1024;

    /// HotSpot 25 G1 never says `Allocation Failure`: both allocation doors
    /// are `G1 Evacuation Pause` on G1 and keep Serial's spelling elsewhere;
    /// every requester's door keeps its cause on both; the recovery
    /// compaction is `G1 Compaction Pause`.
    #[test]
    fn gci_i11_go_g1_causes_are_hotspots() {
        for door in [GcDoor::AllocationThreshold, GcDoor::AllocationFailure] {
            assert_eq!(g1_door_cause(door, true), "G1 Evacuation Pause");
            assert_eq!(g1_door_cause(door, false), "Allocation Failure");
        }
        for door in [
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
        ] {
            assert_eq!(g1_door_cause(door, true), door.cause(), "{door:?}");
            assert_eq!(g1_door_cause(door, false), door.cause(), "{door:?}");
        }
        assert_eq!(GcDoor::G1CompactionPause.cause(), "G1 Compaction Pause");
        assert_eq!(
            g1_door_cause(GcDoor::G1HumongousAllocation, true),
            "G1 Humongous Allocation"
        );
        assert_eq!(
            g1_door_cause(GcDoor::G1CompactionPause, true),
            "G1 Compaction Pause"
        );
    }

    /// `Pause Young (Normal)` unless the mixed count moved; the `-Xlog:gc`
    /// line then reads as HotSpot 25 G1's.
    #[test]
    fn gci_i11_go_g1_pause_labels() {
        assert_eq!(g1_young_pause_type(3, 3, false, false), G1YoungPauseType::Normal);
        assert_eq!(g1_young_pause_type(3, 4, false, false), G1YoungPauseType::Mixed);
        // Precedence: Mixed > Prepare Mixed > Concurrent Start > Normal.
        assert_eq!(g1_young_pause_type(3, 4, true, true), G1YoungPauseType::Mixed);
        assert_eq!(g1_young_pause_type(3, 3, true, true), G1YoungPauseType::PrepareMixed);
        assert_eq!(
            g1_young_pause_type(3, 3, false, true),
            G1YoungPauseType::ConcurrentStart
        );
        assert_eq!(
            G1YoungPauseType::ConcurrentStart.xlog_label(),
            "Pause Young (Concurrent Start)"
        );
        assert_eq!(
            G1YoungPauseType::PrepareMixed.xlog_label(),
            "Pause Young (Prepare Mixed)"
        );
        for (t, j) in [
            (G1YoungPauseType::Normal, "Normal"),
            (G1YoungPauseType::ConcurrentStart, "Concurrent Start"),
            (G1YoungPauseType::PrepareMixed, "Prepare Mixed"),
            (G1YoungPauseType::Mixed, "Mixed"),
        ] {
            assert_eq!(t.jfr_type(), j);
            assert_eq!(t.xlog_label(), format!("Pause Young ({j})"));
        }
        // Only the allocation doors drive a cycle; the forced ones only under
        // `CRATONVM_G1_ALLOC_MARK_DRIVE`.
        assert!(g1_door_drives_mark_cycle(GcDoor::AllocationThreshold, false));
        for door in [
            GcDoor::AllocationFailure,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            assert!(g1_door_drives_mark_cycle(door, true), "{door:?}");
            assert!(!g1_door_drives_mark_cycle(door, false), "{door:?}");
        }
        for door in [
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
        ] {
            assert!(!g1_door_drives_mark_cycle(door, true), "{door:?}");
        }
        assert_eq!(G1YoungPauseType::Normal.xlog_label(), "Pause Young (Normal)");
        assert_eq!(G1YoungPauseType::Mixed.xlog_label(), "Pause Young (Mixed)");
        let line = render_xlog_gc_line(
            5,
            G1YoungPauseType::Normal.xlog_label(),
            g1_door_cause(GcDoor::AllocationFailure, true),
            24 * M + 5,
            3 * M,
            256 * M,
            2_345_000,
        );
        assert_eq!(
            line,
            "GC(5) Pause Young (Normal) (G1 Evacuation Pause) 24M->3M(256M) 2.345ms"
        );
        let mixed = render_xlog_gc_line(
            9,
            G1YoungPauseType::Mixed.xlog_label(),
            g1_door_cause(GcDoor::AllocationThreshold, true),
            60 * M,
            20 * M,
            256 * M,
            7_000_000,
        );
        assert_eq!(
            mixed,
            "GC(9) Pause Young (Mixed) (G1 Evacuation Pause) 60M->20M(256M) 7.000ms"
        );
    }

    /// HotSpot G1's four `gc,heap` lines: counts at both edges, Eden's
    /// capacity the target after minus the survivors after, Survivor's
    /// `ceil(target before / 8)`, none for Old and Humongous.
    #[test]
    fn gci_i11_go_g1_heap_lines_have_hotspots_shape() {
        let census = |eden: usize, survivor: usize, old: usize, humongous: usize| {
            cratonvm_gc::g1::G1RegionCensus {
                eden: (eden * M, eden),
                survivor: (survivor * M, survivor),
                old: (old * M, old),
                humongous: (humongous * M, humongous),
                free: 200,
                region_size: M,
            }
        };
        let before = G1RegionEdge {
            census: census(25, 0, 0, 2),
            young_target_regions: 25,
            survivor_ratio: G1_SURVIVOR_RATIO,
        };
        let after = G1RegionEdge {
            census: census(0, 3, 2, 1),
            young_target_regions: 25,
            survivor_ratio: G1_SURVIVOR_RATIO,
        };
        assert_eq!(
            render_xlog_g1_heap_lines(0, before, after),
            [
                "GC(0) Eden regions: 25->0(22)".to_string(),
                "GC(0) Survivor regions: 0->3(4)".to_string(),
                "GC(0) Old regions: 0->2".to_string(),
                "GC(0) Humongous regions: 2->1".to_string(),
            ]
        );
        // A target smaller than the survivors after never underflows.
        let tiny = G1RegionEdge {
            census: census(0, 3, 2, 1),
            young_target_regions: 1,
            survivor_ratio: G1_SURVIVOR_RATIO,
        };
        let lines = render_xlog_g1_heap_lines(1, tiny, tiny);
        assert_eq!(lines[0], "GC(1) Eden regions: 0->0(0)");
        assert_eq!(lines[1], "GC(1) Survivor regions: 3->3(1)");
    }

    /// `jdk.ExecuteVMOperation` names: HotSpot 25's per backend and door.
    #[test]
    fn gci_i11_go_vm_operation_names() {
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            assert_eq!(vm_operation_name(door, true), "G1CollectForAllocation");
            assert_eq!(vm_operation_name(door, false), "SerialCollectForAllocation");
        }
        for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
            assert_eq!(vm_operation_name(door, true), "G1CollectFull");
            assert_eq!(vm_operation_name(door, false), "SerialGCCollect");
        }
        for g1 in [true, false] {
            assert_eq!(
                vm_operation_name(GcDoor::MetadataThreshold, g1),
                "CollectForMetadataAllocation"
            );
            assert_eq!(vm_operation_name(GcDoor::HeapInspection, g1), "GC_HeapInspection");
            assert_eq!(vm_operation_name(GcDoor::HeapDump, g1), "HeapDumper");
        }
    }

    /// The recovery compaction has its own space-free `door=` token, distinct
    /// from every other door's.
    #[test]
    fn gci_i11_go_compaction_door_token() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let line = render_pause_line(2, GcDoor::G1CompactionPause, facts, 0);
        assert!(line.contains(" door=g1-compaction "), "{line}");
        assert!(!render_pause_line(2, GcDoor::AllocationFailure, facts, 0)
            .contains("g1-compaction"));
    }
}

#[cfg(test)]
mod gci_i12_ge_g1_event_model_tests {
    //! gci i12/ge: the G1 cycle's id and `-Xlog:gc` lines
    //! (`gci-i11go-...-review-findings-20261013` item 1), the JFR young / old
    //! kind (item 3) and the one-report scope of a G1 full collection
    //! (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (b)).
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::runtime::unified_logging::LogTag;
    use std::sync::atomic::Ordering;

    const M: usize = 1024 * 1024;

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    /// HotSpot 25 G1's cycle lines: the open line, then `Pause Remark` /
    /// `Pause Cleanup` (start, end, cpu) and the cycle's end under the
    /// cycle's id; the heap change is the remark's.
    #[test]
    fn gci_i12_ge_g1_cycle_lines_have_hotspots_shape() {
        assert_eq!(render_xlog_g1_cycle_open_line(8), "GC(8) Concurrent Mark Cycle");
        let lines = render_xlog_g1_remark_lines(
            8,
            (55 * M, 40 * M, 128 * M),
            1_234_000,
            Some(100_000),
            Some(25_400_000),
            Some((0.02, 0.0)),
        );
        let text: Vec<(LogTag, &str)> = lines.iter().map(|(t, l)| (*t, l.as_str())).collect();
        assert_eq!(
            text,
            vec![
                (LogTag::GcStart, "GC(8) Pause Remark"),
                (LogTag::Gc, "GC(8) Pause Remark 55M->40M(128M) 1.234ms"),
                (LogTag::GcCpu, text[2].1),
                (LogTag::GcStart, "GC(8) Pause Cleanup"),
                (LogTag::Gc, "GC(8) Pause Cleanup 40M->40M(128M) 0.100ms"),
                (LogTag::GcCpu, text[5].1),
                (LogTag::Gc, "GC(8) Concurrent Mark Cycle 25.400ms"),
            ]
        );
        assert!(text[2].1.starts_with("GC(8) User="), "{}", text[2].1);
        assert!(text[5].1.ends_with("Real=0.00s"), "{}", text[5].1);
        // An incomplete remark: its lines only, the cycle stays open; no cpu
        // line without a sample, no end line without a known start.
        let lines = render_xlog_g1_remark_lines(9, (3 * M, 3 * M, 8 * M), 500_000, None, None, None);
        let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(text, vec!["GC(9) Pause Remark", "GC(9) Pause Remark 3M->3M(8M) 0.500ms"]);
        let lines =
            render_xlog_g1_remark_lines(9, (3 * M, 2 * M, 8 * M), 500_000, Some(10_000), None, None);
        assert_eq!(lines.len(), 4, "no cycle end line without its start");
    }

    /// G1's initial mark writes no JFR row of its own (its concurrent-start
    /// collection's `GC Pause` is the only one under that id), even while a
    /// recording would take one: `gen_cycle_jfr_row` answers `None` for it.
    #[test]
    fn gci_i12_ge_the_g1_initial_mark_writes_no_row() {
        use super::NonCollectionPause as P;
        assert!(g1_initial_mark_has_no_row(P::G1InitialMark));
        let shared = vm(GcAlgorithm::G1);
        assert!(gen_cycle_jfr_row(&shared, P::G1InitialMark, Some((3, 0))).is_none());
        assert!(gen_cycle_jfr_row(&shared, P::G1Remark, None).is_none(), "no open cycle");
    }

    /// A G1 marking pause takes its cycle edge on every run (no recording):
    /// the initial mark takes ONE id and opens the cycle; the remark finds it
    /// and re-opens it; a folded initial mark takes the report's id. Off G1
    /// and for other pauses, no edge.
    #[test]
    fn gci_i12_ge_a_g1_cycle_takes_its_id_on_every_run() {
        use super::NonCollectionPause as P;
        let shared = vm(GcAlgorithm::G1);
        let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let open = g1_cycle_edge(&shared, P::G1InitialMark, None).expect("a G1 initial mark");
        assert_eq!(open.cycle_id, before);
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
        assert_eq!(g1_open_cycle_id(&shared), Some(before));
        open.cycle.mark_started(open.cycle_id, 77);
        let remark = g1_cycle_edge(&shared, P::G1Remark, None).expect("the open cycle");
        assert_eq!((remark.cycle_id, remark.mark_start_ns), (before, 77));
        assert_eq!(g1_open_cycle_id(&shared), Some(before), "re-opened for a retry");
        assert_eq!(remark.cycle_start_ns, 77);
        // A retried remark: no JFR marking start, the -Xlog cycle start kept.
        let retry = g1_cycle_edge(&shared, P::G1Remark, None).expect("still open");
        assert_eq!((retry.mark_start_ns, retry.cycle_start_ns), (0, 77));
        assert_eq!(
            shared.mem.gc_cycle_count.load(Ordering::Relaxed),
            before + 1,
            "the remark takes no id"
        );
        let folded = g1_cycle_edge(&shared, P::G1InitialMark, Some(500)).expect("folded");
        assert_eq!(folded.cycle_id, 500);
        assert!(!folded.xlog, "a folded pause prints no cycle line");
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
        assert!(g1_cycle_edge(&shared, P::FrameTrace, None).is_none());
        let generational = vm(GcAlgorithm::Generational);
        assert!(g1_cycle_edge(&generational, P::G1InitialMark, None).is_none());
        assert_eq!(g1_open_cycle_id(&generational), None);
    }

    /// Item 3: the kind follows HotSpot's operation, not the cause text.
    #[test]
    fn gci_i12_ge_the_jfr_kind_follows_the_operation() {
        let full = [
            GcDoor::SystemGc,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
        ];
        const GEN: (bool, bool) = (true, false);
        const G1: (bool, bool) = (false, true);
        const ZGC: (bool, bool) = (false, false);
        for backend in [GEN, G1] {
            for door in full {
                assert!(jfr_old_event_rule(door, false, backend, false), "{door:?}");
            }
            for door in [
                GcDoor::AllocationThreshold,
                GcDoor::AllocationFailure,
                GcDoor::G1HumongousAllocation,
            ] {
                assert!(!jfr_old_event_rule(door, false, backend, false), "{door:?}");
                assert!(jfr_old_event_rule(door, true, backend, false), "old wins");
            }
        }
        // `-XX:+ExplicitGCInvokesConcurrent`: G1's System.gc / GC.run are a
        // concurrent start (young); Serial ignores the flag.
        for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
            assert!(!jfr_old_event_rule(door, false, G1, true), "{door:?}");
            assert!(jfr_old_event_rule(door, false, GEN, true), "{door:?}");
        }
        assert!(jfr_old_event_rule(GcDoor::HeapDump, false, G1, true));
        // Metadata: Serial's full collection, G1's concurrent-start young pause.
        assert!(jfr_old_event_rule(GcDoor::MetadataThreshold, false, GEN, false));
        assert!(!jfr_old_event_rule(GcDoor::MetadataThreshold, false, G1, false));
        assert!(jfr_old_event_rule(GcDoor::G1CompactionPause, false, G1, false));
        // ZGC: the historical rule (`System.gc` only), unchanged.
        assert!(jfr_old_event_rule(GcDoor::SystemGc, false, ZGC, false));
        assert!(!jfr_old_event_rule(GcDoor::DiagnosticCommand, false, ZGC, false));
    }

    /// Which G1 full collections are ONE `Pause Full` on HotSpot G1: not
    /// `Metadata GC Threshold` (a concurrent-start young pause), not
    /// `System.gc()` / `GC.run` under `-XX:+ExplicitGCInvokesConcurrent`.
    #[test]
    fn gci_i12_ge_which_full_collections_are_one_pause_full() {
        for door in [
            GcDoor::SystemGc,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
        ] {
            assert!(g1_door_is_a_pause_full(door, false), "{door:?}");
        }
        assert!(!g1_door_is_a_pause_full(GcDoor::SystemGc, true));
        assert!(!g1_door_is_a_pause_full(GcDoor::DiagnosticCommand, true));
        assert!(g1_door_is_a_pause_full(GcDoor::HeapInspection, true));
        for door in [
            GcDoor::MetadataThreshold,
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            assert!(!g1_door_is_a_pause_full(door, false), "{door:?}");
        }
    }

    /// The scope folds its owner's collection pauses always and its marking
    /// pauses while the full collection's own cycle runs; nobody else's.
    #[test]
    fn gci_i12_ge_a_full_scope_folds_only_its_owners_pauses() {
        assert!(g1_full_scope_folds(true, false, true));
        assert!(g1_full_scope_folds(true, false, false));
        assert!(g1_full_scope_folds(true, true, true));
        assert!(!g1_full_scope_folds(true, true, false));
        assert!(!g1_full_scope_folds(false, false, true));
        assert!(!g1_full_scope_folds(false, true, true));

        let shared = vm(GcAlgorithm::G1);
        assert!(g1_full_scope_fold(&shared, false).is_none(), "no scope open");
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the id thread");
        let scope = |owner| crate::vm::realms::heap_realm::G1FullReportScope {
            owner,
            gc_id: None,
            pause_ns: 0,
            pauses: 0,
            marking_folded: true,
            marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ref_census: None,
        };
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other));
        assert!(g1_full_scope_fold(&shared, false).is_none(), "another thread's scope");
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(std::thread::current().id()));
        let before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
        let (id, _) = g1_full_scope_fold(&shared, false).expect("the owner's pause");
        assert_eq!(id, before);
        let (again, marking) = g1_full_scope_fold(&shared, true).expect("its own cycle");
        assert_eq!(again, id, "one id per report");
        assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), before + 1);
        marking.add(5);
        g1_full_scope_add_collection(&shared, 7);
        {
            let guard = shared.mem.alloc_ladder.g1_full_report.lock();
            let s = (*guard).as_ref().expect("still open");
            assert_eq!((s.pause_ns, s.pauses), (7, 1));
            assert_eq!(s.marking_ns.load(Ordering::Relaxed), 5);
            assert_eq!(s.longest_ns.load(Ordering::Relaxed), 7, "the longest folded pause");
        }
        {
            let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
            if let Some(s) = &mut *guard {
                s.marking_folded = false;
            }
        }
        assert!(g1_full_scope_fold(&shared, true).is_none(), "a later cycle is its own");
        assert!(g1_full_scope_fold(&shared, false).is_some());
        let generational = vm(GcAlgorithm::Generational);
        *generational.mem.alloc_ladder.g1_full_report.lock() =
            Some(scope(std::thread::current().id()));
        assert!(g1_full_scope_fold(&generational, false).is_none(), "G1 only");
    }

    /// gci i24/rv (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
    /// item 16): the remark of the `System.gc()` stop-the-world cycle (no
    /// open concurrent cycle) inside a one-report full collection files its
    /// `gc+ref` block under the report's id; with no scope -- the default --
    /// the answer is `None` and `remark_reference_xlog_id` keeps its ordinal.
    #[test]
    fn gci_i24rv_a_folded_remark_files_under_the_reports_id() {
        let shared = vm(GcAlgorithm::G1);
        assert_eq!(g1_full_scope_marking_id(&shared), None, "no scope open");
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the id thread");
        let scope = |owner, gc_id, marking_folded| crate::vm::realms::heap_realm::G1FullReportScope {
            owner,
            gc_id,
            pause_ns: 0,
            pauses: 0,
            marking_folded,
            marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ref_census: None,
        };
        let me = std::thread::current().id();
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), true));
        assert_eq!(g1_full_scope_marking_id(&shared), Some(41));
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, None, true));
        assert_eq!(g1_full_scope_marking_id(&shared), None, "no pause took the id yet");
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), false));
        assert_eq!(g1_full_scope_marking_id(&shared), None, "its marking is done");
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other, Some(41), true));
        assert_eq!(g1_full_scope_marking_id(&shared), None, "another thread's scope");
        let generational = vm(GcAlgorithm::Generational);
        *generational.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(41), true));
        assert_eq!(g1_full_scope_marking_id(&generational), None, "G1 only");
        // The remark's id asks the open cycle first, then the scope, then
        // the ordinal.
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let at = src.find("\nfn remark_reference_xlog_id(").expect("the id");
        let body = &src[at..at + src[at..].find("\n\u{7d}\n").expect("its end")];
        let cycle = body.find("g1_open_cycle_id(shared)").expect("the cycle");
        let scoped = body
            .find(".or_else(|| super::gc_events::g1_full_scope_marking_id(shared))")
            .expect("the scope");
        let ordinal = body.find(".gc_cycle_count").expect("the ordinal");
        assert!(cycle < scoped && scoped < ordinal);
    }

    /// gci i25/sg (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
    /// item 16): inside a one-report G1 full collection every
    /// reference-processing round (young, remark, compaction) hands its
    /// census to the scope -- the owner's collection rounds always, its
    /// remark while its own cycle runs -- and the report prints ONE summed
    /// block under its id at the top-level indent. Nothing is absorbed with
    /// no scope (the default), before the id is taken, for another thread,
    /// for a later cycle's remark, or off G1.
    #[test]
    fn gci_i25sg_a_full_report_prints_one_summed_reference_block() {
        use crate::runtime::unified_logging::RefProcCensus;
        let young = RefProcCensus {
            discovered: [0, 2, 0, 0],
            processed: [0, 2, 0, 0],
            total_ns: 1_000_000,
            phase_ns: [1_000_000, 0, 0],
            ..RefProcCensus::default()
        };
        let remark = RefProcCensus {
            discovered: [1, 3, 0, 1],
            processed: [1, 3, 0, 1],
            total_ns: 2_000_000,
            phase_ns: [2_000_000, 0, 0],
            ..RefProcCensus::default()
        };
        let sum = ref_census_sum(Some(&young), &remark);
        assert_eq!(sum.discovered, [1, 5, 0, 1]);
        assert_eq!(sum.processed, [1, 5, 0, 1]);
        assert_eq!(sum.dropped, [0; 4]);
        assert_eq!((sum.total_ns, sum.phase_ns), (3_000_000, [3_000_000, 0, 0]));
        assert_eq!(ref_census_sum(None, &young), young);

        let shared = vm(GcAlgorithm::G1);
        assert!(
            !g1_full_scope_absorb_ref_census(&shared, false, &young),
            "no scope open: deferred as before"
        );
        let other = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("the id thread");
        let me = std::thread::current().id();
        let scope = |owner, gc_id, marking_folded| crate::vm::realms::heap_realm::G1FullReportScope {
            owner,
            gc_id,
            pause_ns: 0,
            pauses: 0,
            marking_folded,
            marking_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            longest_ns: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ref_census: None,
        };
        let taken = |s: &SharedVm| {
            (*s.mem.alloc_ladder.g1_full_report.lock())
                .as_ref()
                .and_then(|scope| scope.ref_census)
        };
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, None, true));
        assert!(!g1_full_scope_absorb_ref_census(&shared, false, &young), "no id yet");
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(other, Some(9), true));
        assert!(!g1_full_scope_absorb_ref_census(&shared, false, &young), "another thread");
        *shared.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(9), true));
        assert!(g1_full_scope_absorb_ref_census(&shared, false, &young), "the young round");
        assert!(g1_full_scope_absorb_ref_census(&shared, true, &remark), "its own remark");
        assert_eq!(taken(&shared), Some(sum));
        {
            let mut guard = shared.mem.alloc_ladder.g1_full_report.lock();
            if let Some(s) = &mut *guard {
                s.marking_folded = false;
            }
        }
        assert!(
            !g1_full_scope_absorb_ref_census(&shared, true, &remark),
            "a later cycle's remark reports its own block"
        );
        assert!(g1_full_scope_absorb_ref_census(&shared, false, &young), "the compaction round");
        let all = ref_census_sum(Some(&sum), &young);
        assert_eq!(taken(&shared), Some(all));
        let generational = vm(GcAlgorithm::Generational);
        *generational.mem.alloc_ladder.g1_full_report.lock() = Some(scope(me, Some(9), true));
        assert!(!g1_full_scope_absorb_ref_census(&generational, false, &young), "G1 only");

        // The report's block: one, under its id, top-level indent.
        assert!(g1_full_report_reference_lines(9, None).is_empty());
        let lines: Vec<String> = g1_full_report_reference_lines(9, Some(&all))
            .into_iter()
            .map(|(_, _, t)| t)
            .collect();
        assert_eq!(lines.len(), 17, "{lines:?}");
        assert!(lines.iter().all(|l| l.starts_with("GC(9) ")), "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("GC(9) Reference Processing: "))
                .count(),
            1,
            "{lines:?}"
        );
        assert!(lines.contains(&"GC(9) Reference Processing: 4.0ms".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"GC(9)   WeakReference Discovered: 7, Dropped: 0, Processed: 7".to_string()),
            "{lines:?}"
        );

        // Wiring: both producers offer the census to the scope before
        // deferring it; the report prints the block right after `gc,start`.
        let alloc = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let body_of = |src: &str, sig: &str| -> String {
            let at = src.find(sig).unwrap_or_else(|| panic!("{sig}"));
            let rest = &src[at..];
            rest[..rest.find("\n\u{7d}\n").expect("its end")].to_string()
        };
        let post = body_of(alloc.as_str(), "\npub(super) fn process_references_after_gc(");
        let absorb = post
            .find("g1_full_scope_absorb_ref_census(shared, false, &census)")
            .expect("the collection round offers its census");
        let defer = post
            .find("defer_xlog_reference_processing_at(")
            .expect("and defers otherwise");
        assert!(absorb < defer);
        let rows = body_of(alloc.as_str(), "\nfn remark_reference_rows(");
        let absorb = rows
            .find("g1_full_scope_absorb_ref_census(shared, true, &census)")
            .expect("the remark offers its census");
        let defer = rows
            .find("defer_xlog_reference_processing(")
            .expect("and defers otherwise");
        assert!(absorb < defer);
        let own = include_str!("gc_events.rs").replace("\r\n", "\n");
        let report = body_of(own.as_str(), "\n    fn xlog_lines(\n");
        let start = report.find("LogTag::GcStart").expect("gc,start");
        let block = report
            .find("g1_full_report_reference_lines(gc_id, refs)")
            .expect("the block");
        let heap = report.find("render_xlog_g1_heap_lines(").expect("gc,heap");
        assert!(start < block && block < heap);
    }

    /// The report: nothing without the switch or off G1; with it, ONE `G1
    /// Old Generation` collection whose time is the folded pauses' sum, and
    /// the scope closed; a report that folded nothing books nothing; a dropped
    /// report closes its scope.
    #[test]
    fn gci_i12_ge_a_full_report_books_one_old_collection() {
        let key = "CRATONVM_G1_FULL_GC_ONE_REPORT";
        let shared = vm(GcAlgorithm::G1);
        let counts = |s: &SharedVm| -> Vec<(u64, u64)> {
            s.mem
                .backend_gc_beans()
                .expect("G1 beans")
                .collectors()
                .iter()
                .map(|c| (c.count, c.time_ms))
                .collect()
        };
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert!(g1_full_report_begin(&shared, GcDoor::SystemGc).is_none());
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("1"))], || {
            let generational = vm(GcAlgorithm::Generational);
            assert!(g1_full_report_begin(&generational, GcDoor::SystemGc).is_none());

            let report = g1_full_report_begin(&shared, GcDoor::SystemGc).expect("opened");
            assert!(
                g1_full_report_begin(&shared, GcDoor::SystemGc).is_none(),
                "one scope at a time"
            );
            report.finish();
            assert!(shared.mem.alloc_ladder.g1_full_report.lock().is_none());
            assert_eq!(counts(&shared)[1], (0, 0), "nothing folded: no report");

            let report = g1_full_report_begin(&shared, GcDoor::SystemGc).expect("opened");
            let (_, marking) = g1_full_scope_fold(&shared, true).expect("folds");
            marking.add(1_000_000);
            g1_full_report_marking_done(Some(&report));
            assert!(g1_full_scope_fold(&shared, true).is_none());
            g1_full_scope_add_collection(&shared, 2_000_000);
            report.finish();
            let c = counts(&shared);
            assert_eq!(c[1], (1, 3), "G1 Old Generation: one collection, 3 ms");
            assert_eq!((c[0].0, c[2].0), (0, 0), "no young / concurrent booking");
            assert!(shared.mem.alloc_ladder.g1_full_report.lock().is_none());

            let report = g1_full_report_begin(&shared, GcDoor::SystemGc).expect("opened");
            drop(report);
            assert!(
                shared.mem.alloc_ladder.g1_full_report.lock().is_none(),
                "an unwound report closes its scope"
            );
        });
    }

    /// The report's notification is the `G1 Old Generation` record only,
    /// ending at its start plus the folded pauses' sum.
    #[test]
    fn gci_i12_ge_the_report_notifies_the_old_bean_only() {
        use cratonvm_gc::gc_metrics::{
            BackendBeanShape, BackendGcBeans, GcBeanSnapshot, JmxPoolSample,
        };
        let b = BackendGcBeans::new(BackendBeanShape::G1);
        let s = [JmxPoolSample::default(); 3];
        let before = GcBeanSnapshot::now(b.collectors(), b.pools(&s));
        b.note_collection(1_000);
        b.note_full_collection(4_000_000);
        let after = GcBeanSnapshot::now(b.collectors(), b.pools(&s));
        let recs = cratonvm_gc::gc_metrics::gc_notifications_between(&before, &after, "System.gc()", 1);
        assert_eq!(recs.len(), 2, "another thread's young pause moved its bean too");
        let recs = g1_full_report_records(recs, 4_000_000);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].collector, "G1 Old Generation");
        assert_eq!(recs[0].action, "end of major GC");
        assert_eq!(
            recs[0].end.duration_since(recs[0].start),
            std::time::Duration::from_nanos(4_000_000)
        );
    }
}

#[cfg(test)]
mod gci_i13_gp_g1_pause_shape_tests {
    //! gci i13/gp (`gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018` items 3
    //! and 5): a full collection aborting an open G1 cycle, and G1's metadata
    //! concurrent start, both opt-in.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::runtime::unified_logging::LogTag;
    use cratonvm_types::flags::with_thread_overrides;

    const ABORT: &str = "CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE";
    const META: &str = "CRATONVM_G1_METADATA_CONCURRENT_START";

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    /// HotSpot 25 G1's aborted cycle: `Concurrent Mark Abort` on
    /// `gc,marking`, then the cycle's end on `gc`; no end line without a
    /// known start; never a `Pause Remark` / `Pause Cleanup` line.
    #[test]
    fn gci_i13_gp_an_aborted_cycle_prints_hotspots_two_lines() {
        let lines = render_xlog_g1_aborted_cycle_lines(8, Some(25_400_000));
        let text: Vec<(LogTag, &str)> = lines.iter().map(|(t, l)| (*t, l.as_str())).collect();
        assert_eq!(
            text,
            vec![
                (LogTag::GcMarking, "GC(8) Concurrent Mark Abort"),
                (LogTag::Gc, "GC(8) Concurrent Mark Cycle 25.400ms"),
            ]
        );
        let lines = render_xlog_g1_aborted_cycle_lines(9, None);
        assert_eq!(lines.len(), 1, "no end line without the cycle's start");
        assert_eq!(lines[0].1, "GC(9) Concurrent Mark Abort");
    }

    /// The abort mark reaches the cycle's remark edge exactly once; the remark
    /// then books no bean collection; an incomplete aborted remark re-arms the
    /// mark for its retry, a completed one does not.
    #[test]
    fn gci_i13_gp_the_remark_of_an_aborted_cycle_reports_the_abort() {
        use super::NonCollectionPause as P;
        let shared = vm(GcAlgorithm::G1);
        let open = g1_cycle_edge(&shared, P::G1InitialMark, None).expect("a G1 initial mark");
        let id = open.cycle_id;
        open.cycle.mark_started(id, 77);
        // Not aborted: an ordinary remark.
        let plain = g1_cycle_edge(&shared, P::G1Remark, None).expect("the open cycle");
        assert!(!plain.aborted);
        // Aborted: the remark edge says so, and takes the mark.
        open.cycle.mark_aborted(id);
        let aborted = g1_cycle_edge(&shared, P::G1Remark, None).expect("still open");
        assert!(aborted.aborted);
        assert_eq!(aborted.cycle_start_ns, 77, "the end line keeps the cycle's start");
        assert!(aborted.sampler.is_none() && aborted.cpu_start.is_none());
        assert!(!open.cycle.take_aborted(id), "taken by the edge");
        // An incomplete remark (no cleanup point) re-arms the abort ...
        g1_cycle_edge_finish(P::G1Remark, aborted, 1_000, false);
        assert!(open.cycle.take_aborted(id), "re-armed for the retry");
        // ... a completed one does not.
        open.cycle.mark_aborted(id);
        let mut done = g1_cycle_edge(&shared, P::G1Remark, None).expect("still open");
        assert!(done.aborted);
        done.cleanup_after_ns = Some(400);
        g1_cycle_edge_finish(P::G1Remark, done, 1_000, false);
        assert!(!open.cycle.take_aborted(id));
        // The pause timer drops the remark's bean booking for an aborted cycle.
        open.cycle.mark_aborted(id);
        let timer = non_collection_pause_start(&shared, P::G1Remark).expect("a G1 remark timer");
        assert!(timer.g1_cycle.as_ref().is_some_and(|e| e.aborted));
        assert!(timer.beans.is_none(), "an aborted cycle books no G1 Concurrent GC collection");
        assert!(timer.jfr.as_ref().map_or(true, |r| r.aborted));
        let timer = non_collection_pause_start(&shared, P::G1Remark).expect("a G1 remark timer");
        assert!(timer.g1_cycle.as_ref().is_some_and(|e| !e.aborted));
    }

    /// gci i19/ob (`gci-i12ge-...-review-findings-20261013` residual r3): the
    /// edge carries its VM's backend key, and only the completed aborted
    /// remark (the branch that prints `Concurrent Mark Abort`) writes the
    /// `jdk.ConcurrentModeFailure` row, under the cycle's id.
    #[test]
    fn gci_i19_ob_an_aborted_cycle_writes_a_concurrent_mode_failure_row() {
        use super::NonCollectionPause as P;
        let shared = vm(GcAlgorithm::G1);
        let open = g1_cycle_edge(&shared, P::G1InitialMark, None).expect("a G1 initial mark");
        assert_eq!(open.vm_identity, shared.vm_identity);
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let start = src.find("\nfn g1_cycle_edge_finish(").expect("g1_cycle_edge_finish");
        let rest = &src[start + 1..];
        let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
        let abort_line = body
            .find("render_xlog_g1_aborted_cycle_lines(")
            .expect("the aborted branch");
        let row = body
            .find("jfr_vm_more::concurrent_mode_failure(")
            .expect("the failure row");
        let plain_lines = body
            .find("render_xlog_g1_remark_lines(")
            .expect("the ordinary remark");
        assert!(abort_line < row && row < plain_lines, "in the aborted branch only");
        assert!(body[row..plain_lines].contains("edge.vm_identity,"));
    }

    /// The abort is taken only on G1, with the switch on, for a `Pause Full`
    /// door, and only while a cycle marks (a unit VM marks nothing).
    #[test]
    fn gci_i13_gp_the_abort_needs_its_switch_and_a_marking_cycle() {
        let g1 = vm(GcAlgorithm::G1);
        let generational = vm(GcAlgorithm::Generational);
        with_thread_overrides(&[(ABORT, Some("1"))], || {
            assert!(g1_full_gc_aborts_open_cycle_enabled());
            assert_eq!(g1_full_gc_abort_open_cycle(&g1, GcDoor::SystemGc), None, "no cycle marks");
            assert_eq!(
                g1_full_gc_abort_open_cycle(&generational, GcDoor::SystemGc),
                None
            );
        });
        for value in [None, Some(""), Some("0")] {
            with_thread_overrides(&[(ABORT, value)], || {
                assert!(!g1_full_gc_aborts_open_cycle_enabled(), "{value:?}");
            });
        }
    }

    /// Item 5: under its switch a `Metadata GC Threshold` full collection is
    /// HotSpot's `Pause Full (Metadata GC Threshold)`; without it, unchanged.
    #[test]
    fn gci_i13_gp_the_metadata_full_collection_is_a_pause_full_under_its_switch() {
        assert!(!g1_full_door_is_a_pause_full(GcDoor::MetadataThreshold, false, false));
        assert!(g1_full_door_is_a_pause_full(GcDoor::MetadataThreshold, false, true));
        assert!(g1_full_door_is_a_pause_full(GcDoor::MetadataThreshold, true, true));
        assert!(g1_full_door_is_a_pause_full(GcDoor::SystemGc, false, false));
        assert!(!g1_full_door_is_a_pause_full(GcDoor::SystemGc, true, true));
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            assert!(!g1_full_door_is_a_pause_full(door, false, true), "{door:?}");
        }
        for (value, on) in [(None, false), (Some("0"), false), (Some("1"), true), (Some("true"), true)] {
            with_thread_overrides(&[(META, value)], || {
                assert_eq!(g1_metadata_concurrent_start_enabled(), on, "{value:?}");
            });
        }
    }

    /// Item 5: the metadata pause is labelled a concurrent start only on its
    /// door, with no cycle marking, under the switch; the switch is read only
    /// for that door.
    #[test]
    fn gci_i13_gp_the_metadata_pause_is_a_concurrent_start() {
        assert!(g1_metadata_pause_is_concurrent_start(GcDoor::MetadataThreshold, true, || true));
        assert!(!g1_metadata_pause_is_concurrent_start(GcDoor::MetadataThreshold, false, || true));
        assert!(!g1_metadata_pause_is_concurrent_start(GcDoor::MetadataThreshold, true, || false));
        for door in [
            GcDoor::SystemGc,
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::DiagnosticCommand,
        ] {
            let asked = std::cell::Cell::new(false);
            assert!(!g1_metadata_pause_is_concurrent_start(door, true, || {
                asked.set(true);
                true
            }));
            assert!(!asked.get(), "{door:?}: the switch is not read");
        }
        // The label it feeds: idle beats IHOP only on the metadata door.
        assert_eq!(
            g1_young_pause_type(0, 0, false, true),
            G1YoungPauseType::ConcurrentStart
        );
    }

    fn facts(sequence: u64, degraded: u32) -> cratonvm_gc::gc_metrics::G1CycleFacts {
        cratonvm_gc::gc_metrics::G1CycleFacts {
            sequence,
            kind: cratonvm_gc::gc_metrics::g1_cycle_kind::YOUNG,
            cset_young: 0,
            cset_old: 0,
            regions_pinned_out: 0,
            rset_sources_scanned: 0,
            degraded,
        }
    }

    /// ef finding 2: the evacuation-failure verdict is this pause's record
    /// (the sequence moved) carrying the flag; nothing recorded, an older
    /// record, or no start reading is no failure.
    #[test]
    fn gci_i13_gp_an_evacuation_failure_is_this_pauses_record() {
        use cratonvm_gc::gc_metrics::g1_degraded as d;
        assert!(g1_pause_evacuation_failed(Some(4), Some(facts(5, d::EVACUATION_FAILURE))));
        assert!(g1_pause_evacuation_failed(
            Some(0),
            Some(facts(1, d::EVACUATION_FAILURE | d::EVACUATION_FAILURE_UNRESOLVED))
        ));
        assert!(!g1_pause_evacuation_failed(Some(5), Some(facts(5, d::EVACUATION_FAILURE))));
        assert!(!g1_pause_evacuation_failed(Some(4), Some(facts(5, d::NONE))));
        assert!(!g1_pause_evacuation_failed(None, Some(facts(5, d::EVACUATION_FAILURE))));
        assert!(!g1_pause_evacuation_failed(Some(0), None));
    }

    /// ef finding 2: HotSpot 25's end line, the suffix right after the cause.
    #[test]
    fn gci_i13_gp_the_end_line_carries_the_evacuation_failure_suffix() {
        const M: usize = 1024 * 1024;
        let cause = "G1 Evacuation Pause";
        let line = render_xlog_gc_line(7, "Pause Young (Normal)", cause, 60 * M, 58 * M, 64 * M, 12_345_000);
        assert_eq!(
            with_evacuation_failure_suffix(line.clone(), cause, true),
            "GC(7) Pause Young (Normal) (G1 Evacuation Pause) (Evacuation Failure: Allocation) \
             60M->58M(64M) 12.345ms"
        );
        assert_eq!(with_evacuation_failure_suffix(line.clone(), cause, false), line);
    }
}

#[cfg(test)]
mod gci_i15ca_abort_cost_tests {
    //! gci i15/ca (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
    //! item 7's cost half): `g1_force_full_cycle_with` aborts the open cycle
    //! only when a full collection's report half marked it, under the same
    //! opt-in. Filter: `cargo test -p cratonvm-vm gci_i15ca`.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use cratonvm_types::flags::with_thread_overrides;

    const ABORT: &str = "CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE";

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    /// The query answers the marked open cycle's id and keeps the mark (the
    /// abort pause's edge takes it); an unmarked cycle, the switch off and a
    /// non-G1 heap answer `None`.
    #[test]
    fn gci_i15ca_the_full_cycle_learns_the_abort_from_the_report_mark() {
        use super::NonCollectionPause as P;
        let g1 = vm(GcAlgorithm::G1);
        let open = g1_cycle_edge(&g1, P::G1InitialMark, None).expect("a G1 initial mark");
        let id = open.cycle_id;
        with_thread_overrides(&[(ABORT, Some("1"))], || {
            assert_eq!(g1_open_cycle_marked_aborted(&g1), None, "open but not marked");
            assert_eq!(g1_mark_open_cycle_aborted(&g1), Some(id));
            assert_eq!(g1_open_cycle_marked_aborted(&g1), Some(id));
            assert_eq!(g1_open_cycle_marked_aborted(&g1), Some(id), "the query keeps the mark");
        });
        with_thread_overrides(&[(ABORT, None)], || {
            assert_eq!(g1_open_cycle_marked_aborted(&g1), None, "the switch is off by default");
        });
        assert!(open.cycle.take_aborted(id), "still marked for the abort pause's report");
        with_thread_overrides(&[(ABORT, Some("1"))], || {
            assert_eq!(g1_open_cycle_marked_aborted(&g1), None, "the mark was taken");
            let generational = vm(GcAlgorithm::Generational);
            assert_eq!(g1_open_cycle_marked_aborted(&generational), None);
        });
    }

    /// The abort pause's timer: a remark-kind timer over a cycle marked
    /// aborted books no `G1 Concurrent GC` collection and, once the pause
    /// stamps its cut, closes the cycle (the mark is not re-armed).
    #[test]
    fn gci_i15ca_the_abort_pause_closes_the_cycle_as_aborted() {
        use super::NonCollectionPause as P;
        let g1 = vm(GcAlgorithm::G1);
        let open = g1_cycle_edge(&g1, P::G1InitialMark, None).expect("a G1 initial mark");
        let id = open.cycle_id;
        assert_eq!(g1_mark_open_cycle_aborted(&g1), Some(id));
        let mut timer = non_collection_pause_start(&g1, P::G1Remark);
        assert!(timer.as_ref().is_some_and(|t| t.beans.is_none()));
        assert!(timer
            .as_ref()
            .and_then(|t| t.g1_cycle.as_ref())
            .is_some_and(|e| e.aborted && e.cycle_id == id));
        non_collection_pause_cleanup_point(&mut timer, Some(std::time::Instant::now()));
        non_collection_pause_seal(&mut timer);
        non_collection_pause_finish(timer);
        assert!(!open.cycle.take_aborted(id), "a closed abort is not re-armed");
    }
}

#[cfg(test)]
mod gci_i14_ox_g1_oome_report_tests {
    //! gci i14/ox (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
    //! item 3): each half of G1's allocation-failure rung reported as one
    //! `Pause Full (G1 Compaction Pause)`, default ON since 2026-10-17.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use cratonvm_types::flags::with_thread_overrides;

    const OOME: &str = "CRATONVM_G1_OOME_PAUSE_FULL_REPORT";
    const ONE: &str = "CRATONVM_G1_FULL_GC_ONE_REPORT";

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    /// (count, time) per G1 collector bean: young, old, concurrent.
    fn counts(s: &SharedVm) -> Vec<(u64, u64)> {
        s.mem
            .backend_gc_beans()
            .expect("G1 beans")
            .collectors()
            .iter()
            .map(|c| (c.count, c.time_ms))
            .collect()
    }

    /// The gate: G1 and the switch, nothing else.
    #[test]
    fn gci_i14_ox_the_oome_report_needs_g1_and_its_switch() {
        assert!(g1_oome_full_report_applies(true, true));
        assert!(!g1_oome_full_report_applies(true, false));
        assert!(!g1_oome_full_report_applies(false, true));
        with_thread_overrides(&[(OOME, None)], || {
            assert!(g1_oome_pause_full_report_enabled(), "default ON since 2026-10-17");
        });
        with_thread_overrides(&[(OOME, Some("0"))], || {
            assert!(!g1_oome_pause_full_report_enabled());
            assert!(g1_oome_full_report_begin(&vm(GcAlgorithm::G1)).is_none());
        });
        with_thread_overrides(&[(OOME, Some("1"))], || {
            assert!(g1_oome_pause_full_report_enabled());
            assert!(g1_oome_full_report_begin(&vm(GcAlgorithm::Generational)).is_none());
        });
    }

    /// Two halves, two `G1 Old Generation` collections on the `G1 Compaction
    /// Pause` door, each the sum of what it folded; no young or concurrent
    /// booking; independent of the System.gc() one-report switch; a half that
    /// folded nothing books nothing; one scope at a time.
    #[test]
    fn gci_i14_ox_two_halves_book_two_old_collections() {
        let shared = vm(GcAlgorithm::G1);
        with_thread_overrides(&[(OOME, Some("1")), (ONE, None)], || {
            let first = g1_oome_full_report_begin(&shared).expect("opened");
            assert_eq!(first.door, GcDoor::G1CompactionPause);
            assert_eq!(first.door.cause(), "G1 Compaction Pause");
            assert!(
                g1_oome_full_report_begin(&shared).is_none(),
                "one scope at a time"
            );
            assert!(
                g1_full_report_begin(&shared, GcDoor::SystemGc).is_none(),
                "nor a System.gc() report inside it"
            );
            let (_, marking) = g1_full_scope_fold(&shared, true).expect("the cycle folds");
            marking.add(1_000_000);
            g1_full_report_marking_done(Some(&first));
            g1_full_scope_add_collection(&shared, 2_000_000);
            first.finish();
            assert!(shared.mem.alloc_ladder.g1_full_report.lock().is_none());
            assert_eq!(counts(&shared)[1], (1, 3), "first Pause Full: 3 ms");

            let second = g1_oome_full_report_begin(&shared).expect("opened again");
            let _ = g1_full_scope_fold(&shared, false).expect("the rung's pause folds");
            g1_full_scope_add_collection(&shared, 4_000_000);
            second.finish();
            let c = counts(&shared);
            assert_eq!(c[1], (2, 7), "HotSpot's two Pause Full per OOME");
            assert_eq!((c[0].0, c[2].0), (0, 0), "no young / concurrent booking");

            let empty = g1_oome_full_report_begin(&shared).expect("opened");
            empty.finish();
            assert_eq!(counts(&shared)[1].0, 2, "nothing folded: no report");
        });
    }
}

/// gci i14/mp finding 9 (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`,
/// opt-in `CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS`): HotSpot G1's cycle beans.
#[cfg(test)]
mod gci_i14_mp_cycle_bean_tests {
    use super::*;
    use cratonvm_gc::gc_metrics::{GcNotificationRecord, JMX_G1_CONCURRENT_COLLECTOR};

    fn rec(collector: &'static str, id: u64, start: std::time::Instant) -> GcNotificationRecord {
        GcNotificationRecord {
            collector,
            action: "end of concurrent GC pause",
            cause: "No GC",
            id,
            start,
            end: start + std::time::Duration::from_millis(10),
            timestamp_ms: 0,
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    #[test]
    fn gci_i14_mp_the_initial_mark_books_no_bean_under_the_opt_in() {
        let p = NonCollectionPause::G1InitialMark;
        assert!(non_collection_pause_books_a_bean(p, || false), "default: as before");
        assert!(!non_collection_pause_books_a_bean(p, || true), "opt-in: HotSpot's young pause counted it");
        // The remark keeps its bean either way; the flag is not even asked.
        let r = NonCollectionPause::G1Remark;
        assert!(non_collection_pause_books_a_bean(r, || panic!("not asked for the remark")));
        // A pause with no bean books none.
        assert!(!non_collection_pause_books_a_bean(NonCollectionPause::HeapDump, || true));
    }

    #[test]
    fn gci_i14_mp_a_remark_and_cleanup_are_two_notifications() {
        let t0 = std::time::Instant::now();
        let cut = t0 + std::time::Duration::from_millis(6);
        let out = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 4, t0)], cut);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].id, out[1].id), (3, 4), "Remark then Cleanup");
        assert_eq!(out[0].end, cut);
        assert_eq!(out[1].start, cut);
        assert_eq!(out[1].end, t0 + std::time::Duration::from_millis(10));
        // Another bean's record, or a count that cannot hold two, passes through.
        let young = split_concurrent_remark_records(vec![rec("G1 Young Generation", 7, t0)], cut);
        assert_eq!(young.len(), 1);
        let one = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 1, t0)], cut);
        assert_eq!(one.len(), 1);
        // A cut past the pause is clamped into it.
        let late = t0 + std::time::Duration::from_secs(1);
        let out = split_concurrent_remark_records(vec![rec(JMX_G1_CONCURRENT_COLLECTOR, 2, t0)], late);
        assert_eq!(out[0].end, out[1].end);
    }
}

#[cfg(test)]
mod gci_i14hd_g1_heap_debug_tests {
    use super::*;

    /// gci i14/hd item 9: G1's debug blocks -- the Serial-measured headers
    /// over `print_heap_on`'s two lines (`gc,heap`) and the Metaspace line
    /// (`gc,metaspace`), each prefixed `GC(n) `.
    #[test]
    fn gci_i14hd_g1_heap_debug_blocks_are_hotspots() {
        const K: u64 = 1024;
        let h = crate::runtime::unified_logging::G1ExitHeap {
            reserved: 131072 * K,
            committed: 8192 * K,
            used: 3 * 1024 * K,
            bounds: [0x7f00_0000_0000, 0x7f00_0800_0000],
            region_size: 1024 * K,
            young_regions: 3,
            survivor_regions: 0,
            metaspace: (94 * K, 94 * K, 94 * K),
            // gci i33/ms: the `class space` line goes with the Metaspace one.
            class_space: Some((5 * K, 64 * K, 1_048_576 * K)),
        };
        let (heap, meta) = render_xlog_g1_heap_debug_blocks(4, "Before", (4, 1), &h);
        assert_eq!(
            heap,
            vec![
                "GC(4) Heap Before GC invocations=4 (full 1):",
                "GC(4)  garbage-first heap   total reserved 131072K, committed 8192K, used 3072K [0x00007f0000000000, 0x00007f0008000000)",
                "GC(4)   region size 1024K, 3 young (3072K), 0 survivors (0K)",
            ]
        );
        assert_eq!(
            meta,
            vec![
                "GC(4) Metaspace Before GC invocations=4 (full 1):",
                "GC(4)  Metaspace       used 94K, committed 94K, reserved 94K",
                "GC(4)   class space    used 5K, committed 64K, reserved 1048576K",
            ]
        );
    }

    /// gci i14/hd item 9: no G1 figures off G1, nor without the debug tags.
    #[test]
    fn gci_i14hd_g1_heap_debug_sample_only_on_g1_with_the_tags() {
        let serial = crate::vm::SharedVm::new(crate::config::VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Generational,
            max_heap_size: 64 * 1024 * 1024,
            ..Default::default()
        });
        assert!(g1_heap_debug_sample(&serial, true).is_none());
        let g1 = crate::vm::SharedVm::new(crate::config::VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::G1,
            max_heap_size: 64 * 1024 * 1024,
            ..Default::default()
        });
        assert!(g1_heap_debug_sample(&g1, false).is_none(), "no events, no walk");
        assert_eq!(g1_heap_debug_sample(&g1, true).is_some(), xlog_heap_debug_on());
    }
}

#[cfg(test)]
mod gci_i20_ob_requester_tests {
    //! gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1):
    //! `jdk.ExecuteVMOperation.caller` names the collection's requester.
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};

    fn vm() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        })
    }

    /// A registry id with no entry has no Java tid: no requester, so the
    /// row keeps `caller` 0 (and nothing panics on the lookup).
    #[test]
    fn gci_i20_ob_an_unknown_thread_is_no_requester() {
        let shared = vm();
        let id = crate::threading::jvm_thread::ThreadId(u64::MAX - 7);
        assert!(jfr_requester_of(&shared, id).is_none());
    }

    /// The owned requester lends the backend's caller record unchanged.
    #[test]
    fn gci_i20_ob_the_requester_lends_its_fields() {
        let who = JfrRequester {
            java_tid: 3,
            name: "main".to_string(),
            os_tid: 4242,
        };
        let c = who.as_caller();
        assert_eq!((c.java_tid, c.name, c.os_tid), (3, "main", 4242));
    }

    /// Every JFR collection row the reporter builds carries the field, the
    /// plain finish names nobody, and the `_by` door resolves its requester
    /// only while a backend recording runs on a reporting collection.
    #[test]
    fn gci_i20_ob_every_collection_row_carries_the_caller() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        // Every struct literal (this module spells its needles escaped, so
        // it is not one of them): the lines up to the first closing line.
        let needle = "&JfrCollection \u{7b}";
        let literals = src.matches(needle).count();
        assert!(literals >= 5, "the reporter's literals: {literals}");
        for (at, _) in src.match_indices(needle) {
            let body: Vec<&str> = src[at..]
                .lines()
                .skip(1)
                .take_while(|l| !l.trim_start().starts_with('\u{7d}'))
                .collect();
            let body = body.join("\n");
            assert!(
                body.contains("caller") || body.contains("..*c"),
                "a JfrCollection literal without `caller`: {}",
                &src[at..at + 40]
            );
        }
        let production = &src[..src.find("\nmod gci_i20_ob_requester_tests").expect("this module")];
        // The requester-less entry, while it exists, names nobody.
        if let Some(at) = src.find("\npub(super) fn gc_event_finish(") {
            let finish = &src[at..];
            let finish = &finish[..finish.find("\n\u{7d}\n").expect("its end")];
            assert!(finish.contains("gc_event_finish_with(shared, door, start, None)"));
        }
        let by = &src[src.find("\npub(super) fn gc_event_finish_by(").expect("by")..];
        let by = &by[..by.find("\n\u{7d}\n").expect("its end")];
        assert!(by.contains("any_recording()"));
        assert!(by.contains("s.events"));
        assert!(production.contains("caller: c.caller,"));
    }
}

#[cfg(test)]
mod gci_i23pk_g1_vm_operation_rows_tests {
    //! gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
    //! G1's Remark and Cleanup pause rows also write HotSpot's VM-operation
    //! rows (`G1PauseRemark`, `G1PauseCleanup`) to the backend; nothing else.
    use super::*;

    #[test]
    fn gci_i23pk_only_g1_remark_and_cleanup_rows_are_vm_operations() {
        assert_eq!(
            g1_cycle_pause_vm_operation(true, "Pause Remark"),
            Some("G1PauseRemark")
        );
        assert_eq!(
            g1_cycle_pause_vm_operation(true, "Pause Cleanup"),
            Some("G1PauseCleanup")
        );
        // G1's initial mark has no row of its own; its name is the young
        // pause's, which `gc_cycle` writes as a collection.
        assert_eq!(g1_cycle_pause_vm_operation(true, "GC Pause"), None);
        // The Generational cycle has no HotSpot VM operation.
        for name in ["Pause Remark", "Pause Cleanup", "Pause Init Mark"] {
            assert_eq!(g1_cycle_pause_vm_operation(false, name), None, "{name}");
        }
        // Every row the remark splitter makes maps on G1.
        for (name, _, _) in remark_cleanup_rows(1_000, 900, Some(600)) {
            assert!(g1_cycle_pause_vm_operation(true, name).is_some(), "{name}");
        }
    }

    /// The writer is called on the backend half only, per pause row, with
    /// the row's G1 bit; the only `GenCycleJfrRow` literal sets that bit from
    /// the cycle's backend.
    #[test]
    fn gci_i23pk_the_backend_half_writes_the_vm_operation() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let at = src
            .find("\nfn emit_gen_cycle_jfr_row(")
            .expect("the emitter");
        let body = &src[at..];
        let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
        let backend = body.find("if let Some(vm) = row.backend_vm \u{7b}").expect("backend half");
        let call = body
            .find("if let Some(op) = g1_cycle_pause_vm_operation(row.g1, name) \u{7b}")
            .expect("the call");
        let writer = body
            .find("jfr_vm_more::vm_operation_pause(")
            .expect("the writer");
        assert!(backend < call && call < writer);
        let at = src.find("\nfn gen_cycle_jfr_row(").expect("the builder");
        let builder = &src[at..];
        let builder = &builder[..builder.find("\n\u{7d}\n").expect("its end")];
        assert!(builder.contains("g1: !generational,"));
    }
}

#[cfg(test)]
mod gci_i27_ob_observability_tests {
    //! gci i27/ob: the `gc,jni` line (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016`
    //! item 9) and the G1 concurrent-start label taken from the drive's
    //! outcome (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`).
    use super::*;

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// HotSpot 25's `GCLockerTimingDebugLogger` text (measured on 25.0.3:
    /// `Thread blocked to start GC. Resumed after 0ms. Thread "main".`).
    #[test]
    fn gci_i27ob_gc_jni_line_is_hotspots() {
        assert_eq!(
            render_xlog_gc_jni_line("main"),
            "Thread blocked to start GC. Resumed after 0ms. Thread \"main\"."
        );
        // Printed by the requester before the collection's own lines, and
        // never for a pause folded into a G1 full collection's one report.
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let by = body(&src, "pub(super) fn gc_event_finish_by(");
        let jni = by.find("render_xlog_gc_jni_line(").expect("the jni line");
        let finish = by.find("gc_event_finish_with(").expect("the collection's lines");
        assert!(jni < finish);
        assert!(by.contains("s.full_scope_id.is_none()"));
        assert!(by.contains("xlog_serial_or_g1_lines(&shared.mem.heap)"));
    }

    /// The label of a deferred report is the drive's outcome.
    #[test]
    fn gci_i27ob_deferred_label_follows_the_drive() {
        assert_eq!(g1_deferred_young_label(true), G1YoungPauseType::ConcurrentStart);
        assert_eq!(g1_deferred_young_label(false), G1YoungPauseType::Normal);
        assert_eq!(
            g1_deferred_young_label(true).xlog_label(),
            "Pause Young (Concurrent Start)"
        );
        assert_eq!(g1_deferred_young_label(false).jfr_type(), "Normal");
    }

    /// Only the IHOP prediction is deferred (the metadata opt-in's cycle opens
    /// unconditionally), and only on the single-collection path.
    #[test]
    fn gci_i27ob_only_the_ihop_prediction_is_deferred() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let with = body(&src, "fn gc_event_finish_with(");
        let defer = with
            .find("return Some(G1DeferredYoungReport \u{7b}")
            .expect("the deferral");
        let gate = with
            .find("g1_concurrent_start_is_the_ihop_prediction(&start, door)")
            .expect("the IHOP gate");
        let switch = with
            .find("g1_concurrent_start_from_drive_enabled()")
            .expect("the kill switch");
        let emit = with
            .find("emit_collection_events(")
            .expect("the immediate report");
        assert!(gate < defer && switch < defer && defer < emit);
        let pred = body(&src, "fn g1_concurrent_start_is_the_ihop_prediction(");
        assert!(pred.contains("start.g1_concurrent_start"));
        assert!(pred.contains("g1_door_drives_mark_cycle(door,"));
        // A requester-less report is labelled by the prediction, as before.
        let by = body(&src, "pub(super) fn gc_event_finish_by(");
        assert!(by.contains("Some(id) => g1_park_deferred_young_report(shared, id, report),"));
        // A stale parked report is printed before this pause's lines.
        let stale = by
            .find("g1_emit_deferred_young_report(shared, id, false);")
            .expect("the stale flush");
        assert!(stale < by.find("render_xlog_gc_jni_line(").expect("the jni line"));
    }

    /// The drive resolves the parked report: `true` just before the initial
    /// mark prints its `Concurrent Mark Cycle` line, `false` at every other
    /// exit of the drive and after the forced door's drive decision.
    #[test]
    fn gci_i27ob_the_drive_resolves_the_parked_report() {
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let initial = body(&src, "pub(super) fn g1_concurrent_mark_cycle(");
        let started = initial
            .find("g1_emit_deferred_young_report(shared, thread.thread_id, true);")
            .expect("the started report");
        let open = initial
            .find("non_collection_pause_finish(pause_timer);")
            .expect("the open line");
        let resume = initial
            .find("retire_skip_spans_and_resume(shared, taken);")
            .expect("the release");
        assert!(resume < started && started < open);
        let drive = body(&src, "pub(super) fn g1_drive_mark_cycle(");
        assert_eq!(
            drive
                .matches("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
                .count(),
            2,
            "both exits of the drive"
        );
        let forced_drive = src
            .find("g1_drive_mark_cycle(shared, thread, MarkDoor::AllocFail);")
            .expect("the forced door's drive");
        let after = &src[forced_drive..];
        let flush = after
            .find("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
            .expect("the forced door's flush");
        assert!(flush < after.find("\n\u{7d}\n").expect("its end"));
    }

    /// gci i27/ob (`gci-i15rb-...-review-findings-20261014` finding 4's
    /// residual): JVMTI `ForceGarbageCollection` is HotSpot's
    /// `collect(_jvmti_force_gc)` -- its own cause, a full collection on both
    /// backends, the `System.gc()` VM operation, never a mark-cycle drive.
    #[test]
    fn gci_i27ob_jvmti_force_gc_has_hotspots_cause() {
        let door = GcDoor::JvmtiForceGc;
        assert_eq!(door.cause(), "JvmtiEnv ForceGarbageCollection");
        assert_eq!(g1_door_cause(door, true), "JvmtiEnv ForceGarbageCollection");
        assert_eq!(vm_operation_name(door, true), "G1CollectFull");
        assert_eq!(vm_operation_name(door, false), "SerialGCCollect");
        for explicit_concurrent in [false, true] {
            assert!(g1_door_is_a_pause_full(door, explicit_concurrent));
            assert!(jfr_old_event_rule(door, false, (true, false), explicit_concurrent));
            assert!(jfr_old_event_rule(door, false, (false, true), explicit_concurrent));
        }
        for alloc_drive in [false, true] {
            assert!(!g1_door_drives_mark_cycle(door, alloc_drive));
        }
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let jvmti = body(&src, "pub fn force_gc_for_jvmti(");
        assert!(jvmti.contains("force_full_collection(shared, thread, GcDoor::JvmtiForceGc);"));
        assert!(jvmti.contains("JVMTI_FORCE_GC_LEAVES_DRAINS"));
    }

    /// gcg-g3x item 12: under the debug blocks the `gc,metaspace` figure at
    /// each edge reuses the block's walk; the debug block is sampled before.
    #[test]
    fn gci_i27ob_one_metaspace_walk_per_edge() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let start = body(&src, "pub(super) fn gc_event_start(");
        let sample = start
            .find("let heap_debug_before = if events && xlog_heap_debug_on()")
            .expect("the debug edge first");
        let reuse = start
            .find("metaspace_edge_bytes(shared, heap_debug_before.as_ref())")
            .expect("the before figure reuses it");
        assert!(sample < reuse);
        let lines = body(&src, "fn xlog_collection_lines(");
        // gci i30/ob: the after edge goes through `metaspace_after_bytes`,
        // which reuses the debug block's walk first.
        assert!(lines.contains("metaspace_after_bytes(shared, start)"));
        assert!(!lines.contains("metaspace_used_bytes(shared)"));
        let after = body(&src, "fn metaspace_after_bytes(");
        let debug = after
            .find("start.heap_debug_after.as_ref()")
            .expect("the debug block first");
        // gci i33/ms: the walk states HotSpot's statistics.
        let walk = after.find("metaspace_stats_now(shared)").expect("a walk last");
        assert!(debug < walk);
    }
}

/// gci i30/ob: observability residuals -- the stop-the-world full mark's
/// remark (mr finding 9), the G1 cycle beans flip (h4g1 item 6), G1 Full GC's
/// phases (g3x item 5), the metaspace after-figure (g3x item 12), the
/// `gc,start` decoration (g3x item 7) and the TLAB line carrier (tl item 6).
#[cfg(test)]
mod gci_i30_ob_tests {
    use super::*;

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Only a G1 remark with no open concurrent cycle is the stop-the-world
    /// full mark's.
    #[test]
    fn gci_i30ob_only_a_cycleless_g1_remark_is_the_stw_full_mark() {
        assert!(stw_full_mark_remark(NonCollectionPause::G1Remark, true, false));
        assert!(!stw_full_mark_remark(NonCollectionPause::G1Remark, true, true));
        assert!(!stw_full_mark_remark(NonCollectionPause::G1Remark, false, false));
        assert!(!stw_full_mark_remark(NonCollectionPause::G1InitialMark, true, false));
        assert!(!stw_full_mark_remark(NonCollectionPause::GenRemark, true, false));
    }

    /// The stop-the-world full mark's remark books no collection and queues
    /// no notification; its finish stamps every pool instead.
    #[test]
    fn gci_i30ob_the_stw_full_mark_books_nothing() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let seal = body(&src, "pub(super) fn non_collection_pause_seal(");
        let guard = seal.find("_ if b.stw_full_mark => {}").expect("no booking");
        let book = seal
            .find("None => b.set.note_non_collection_pause(work_ns)")
            .expect("the ordinary booking");
        assert!(guard < book);
        let finish = body(&src, "pub(super) fn non_collection_pause_finish(");
        let stamp = finish
            .find("stw_full_mark_stamps_every_pool(&b.set, &after);")
            .expect("every pool stamped");
        let records = finish
            .find("gc_notifications_between(")
            .expect("the notification records");
        assert!(stamp < records, "it returns before any record is made");
        let start = body(&src, "pub(super) fn non_collection_pause_start(");
        assert!(start.contains(
            "stw_full_mark_remark(kind, shared.mem.heap.is_g1(), g1_cycle.is_some())"
        ));
    }

    /// mr finding 9, end to end on a G1 VM: a remark timer with no open cycle
    /// (`g1_stw_full_mark_cycle`'s) leaves `G1 Concurrent GC`'s count alone;
    /// one over an open cycle books it as before.
    #[test]
    fn gci_i30ob_a_cycleless_remark_leaves_the_concurrent_bean() {
        use crate::config::{GcAlgorithm, VmConfig};
        use cratonvm_types::flags::with_thread_overrides;
        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        let concurrent = |s: &SharedVm| {
            s.mem
                .backend_gc_beans()
                .expect("G1 beans")
                .collectors()
                .iter()
                .find(|c| c.name == cratonvm_gc::gc_metrics::JMX_G1_CONCURRENT_COLLECTOR)
                .map(|c| c.count)
                .expect("the concurrent bean")
        };
        with_thread_overrides(&[("CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS", Some("0"))], || {
            let before = concurrent(&g1);
            let mut timer = non_collection_pause_start(&g1, NonCollectionPause::G1Remark);
            assert!(timer.as_ref().is_some_and(|t| t.g1_cycle.is_none()
                && t.beans.as_ref().is_some_and(|b| b.stw_full_mark)));
            non_collection_pause_seal(&mut timer);
            non_collection_pause_finish(timer);
            assert_eq!(concurrent(&g1), before, "the STW full mark books no concurrent pause");
            let _open =
                g1_cycle_edge(&g1, NonCollectionPause::G1InitialMark, None).expect("opened");
            let mut timer = non_collection_pause_start(&g1, NonCollectionPause::G1Remark);
            assert!(timer
                .as_ref()
                .is_some_and(|t| t.beans.as_ref().is_some_and(|b| !b.stw_full_mark)));
            non_collection_pause_seal(&mut timer);
            non_collection_pause_finish(timer);
            assert_eq!(concurrent(&g1), before + 1, "a concurrent remark books one");
        });
    }

    /// vi20 Batch 3 `sg25_f_fulljfr_on`: HotSpot 25 names the G1 full
    /// collection `G1Full` in `jdk.GarbageCollection` (`G1Old` is its
    /// concurrent cycle's tracer).
    #[test]
    fn gci_i30ob_the_g1_full_collection_is_g1full_in_jfr() {
        assert_eq!(G1_FULL_JFR_NAME, "G1Full");
    }

    /// Hand-back 1 (vi30-ob S1): a G1 full collection leaves `G1 Survivor
    /// Space`'s collection usage at 0 used / 0 committed, as HotSpot's
    /// `G1FullCollector` does; the other pools keep their samples.
    #[test]
    fn gci_i30ob_a_g1_full_collection_leaves_the_survivor_empty() {
        use cratonvm_gc::gc_metrics::JmxPoolUsage;
        let pool = |name: &'static str, used: u64, committed: u64| JmxPoolUsage {
            name,
            init: Some(0),
            used,
            committed,
            max: None,
            collection_used: None,
            managers: &[],
        };
        let rows = g1_full_collection_usage_rows(&[
            pool("G1 Eden Space", 0, 4 << 20),
            pool(cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR, 1 << 20, 2 << 20),
            pool("G1 Old Gen", 9 << 20, 16 << 20),
        ]);
        assert_eq!((rows[0].used, rows[0].committed), (0, 4 << 20));
        assert_eq!((rows[1].used, rows[1].committed), (0, 0));
        assert_eq!((rows[2].used, rows[2].committed), (9 << 20, 16 << 20));
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let stamp = body(&src, "fn stw_full_mark_stamps_every_pool(");
        assert!(stamp.contains("g1_full_collection_usage_rows(&after.pools)"));
        let finish = body(&src, "    pub(super) fn finish(self) {");
        assert!(finish.contains("g1_full_collection_usage_rows(&after.pools)"));
    }

    /// Hand-back 2 (vi34-ob S1): the `System.gc()` door re-stamps the
    /// Survivor pool alone at its end, after every pause it ran, for a
    /// stop-the-world cycle without the one-report scope; on a fresh G1 VM the
    /// stamp reads HotSpot's empty Survivor.
    #[test]
    fn gci_i30ob_the_full_door_ends_with_an_empty_survivor_stamp() {
        use crate::config::{GcAlgorithm, VmConfig};
        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        g1_full_door_leaves_the_survivor_empty(&g1);
        let set = g1.mem.backend_gc_beans().expect("G1 beans");
        let sampler = g1.mem.heap.jmx_pool_sampler().expect("a sampler");
        let pools = set.pools(&sampler.sample());
        let survivor = pools
            .iter()
            .find(|p| p.name == cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR)
            .expect("the survivor pool");
        assert_eq!(survivor.collection_used, Some(0));
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let f = body(&src, "fn force_full_collection(");
        let compaction = f
            .find("g1_full_compaction_pause(shared, thread, door);")
            .expect("the compaction pause");
        let stamp = f
            .find("super::gc_events::g1_full_door_leaves_the_survivor_empty(shared);")
            .expect("the door-end stamp");
        let finish = f.find("report.finish()").expect("the one report");
        assert!(compaction < stamp && stamp < finish);
        // gci i41/rs: the door-end call survives only under the `=0` kill.
        assert!(f.contains("if stw_cycle && full_report.is_none()"));
        assert!(f.contains("&& !g1_full_door_survivor_stamp_in_pause_enabled()"));
    }

    /// gci i41/rs (`gci-i39rv-waves-i35-i38-review-residuals-FIXED-20261016` item
    /// 1): the in-pause stamp. On a fresh G1 VM the shared helper stamps
    /// HotSpot's empty Survivor; a pause with no bean snapshot (a folded
    /// one-report pause, or no start at all) stamps nothing; and the pause
    /// takes the stamp after its seal and before its release.
    #[test]
    fn gci_i41rs_the_compaction_pause_stamps_the_survivor_inside_the_pause() {
        use crate::config::{GcAlgorithm, VmConfig};
        let g1 = SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::G1,
            ..VmConfig::default()
        });
        let set = g1.mem.backend_gc_beans().expect("G1 beans");
        let sampler = g1.mem.heap.jmx_pool_sampler().expect("a sampler");
        let survivor_collection_used = || {
            set.pools(&sampler.sample())
                .iter()
                .find(|p| p.name == cratonvm_gc::gc_metrics::JMX_POOL_G1_SURVIVOR)
                .expect("the survivor pool")
                .collection_used
        };
        // No start: nothing stamped.
        let untouched = survivor_collection_used();
        g1_full_door_compaction_stamps_the_survivor(&g1, &None);
        assert_eq!(survivor_collection_used(), untouched, "no pause, no stamp");
        g1_stamp_the_survivor_as_a_full_collection_leaves_it(set, &set.pools(&sampler.sample()));
        assert_eq!(survivor_collection_used(), Some(0));
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let pause = body(&src, "fn run_collection_pause(");
        let before = pause
            .find("let g1_full_compactions_before = g1_full_compaction_pause_count(shared);")
            .expect("the count before the collection");
        let roots = pause
            .find("collect_roots_registry_appended(shared, thread)")
            .expect("the roots");
        let seal = pause.find("gc_event_seal(shared, &mut gc_event);").expect("the seal");
        let stamp = pause
            .find("super::gc_events::g1_full_door_compaction_stamps_the_survivor(shared, &gc_event);")
            .expect("the in-pause stamp");
        let release = pause.find(".complete_gc(result.pointer_map)").expect("the release");
        assert!(before < roots && seal < stamp && stamp < release);
        let helper = body(&src, "fn g1_door_compaction_pause_stamps_survivor(");
        assert!(helper.contains("&& switch_on()"), "the switch is read last");
    }

    /// Hand-back 3 (vi34-ob `.hse`): a `GCTraceTime` start line needs its
    /// scope's tag set selected too -- `gc` for the pause start lines,
    /// `gc,phases` for a phase -- so `-Xlog:gc+start` alone prints none.
    #[test]
    fn gci_i30ob_start_lines_need_their_scope_tag_set() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        for writer in [
            "fn xlog_collection_lines(",
            "fn xlog_escalated_collection_lines(",
            "    fn xlog_lines(",
        ] {
            let b = body(&src, writer);
            assert!(b.contains("xlog_start_line_on(LogTag::GcStart, LogTag::Gc)"), "{writer}");
            assert!(!b.contains("if xlog_on(LogTag::GcStart)"), "{writer}");
            assert!(!b.contains("|| xlog_on(LogTag::GcPhasesStart))"), "{writer}");
        }
        let remark = body(&src, "fn g1_cycle_edge_finish(");
        assert!(remark.contains("if !xlog_on(crate::runtime::unified_logging::LogTag::Gc)"));
        let helper = body(&src, "fn xlog_start_line_on(");
        assert!(helper.contains("xlog_on(scope) && xlog_on(start)"));
    }

    /// h4g1 item 6: the cycle-bean shape is the default (`=0` kills it).
    #[test]
    fn gci_i30ob_hotspot_cycle_beans_are_default_on() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let f = body(&src, "pub(super) fn g1_jmx_hotspot_cycle_beans_enabled(");
        assert!(f.contains("runtime_flag_default_on(\"CRATONVM_G1_JMX_HOTSPOT_CYCLE_BEANS\")"));
        assert!(f.contains("WARNING gci-user-flip-20261015"));
    }

    /// HotSpot 25 G1 Full GC's five info phases, in Serial's line shape; the
    /// times sum to the report's duration.
    #[test]
    fn gci_i30ob_g1_full_gc_phases_are_hotspots() {
        let (s, e) = g1_full_gc_phase_lines(7, 0, 3_218_000);
        assert_eq!(s, "GC(7) Phase 1: Mark live objects");
        assert_eq!(e, "GC(7) Phase 1: Mark live objects 3.218ms");
        assert_eq!(g1_full_gc_phase_lines(7, 1, 0).1, "GC(7) Phase 2: Prepare compaction 0.000ms");
        assert_eq!(g1_full_gc_phase_lines(7, 2, 0).0, "GC(7) Phase 3: Adjust pointers");
        assert_eq!(g1_full_gc_phase_lines(7, 3, 0).0, "GC(7) Phase 4: Compact heap");
        assert_eq!(g1_full_gc_phase_lines(7, 4, 0).0, "GC(7) Phase 5: Reset Metadata");
        let ns = g1_full_phase_ns(5, 9);
        assert_eq!(ns, [5, 0, 0, 9, 0]);
        assert_eq!(ns.iter().sum::<u64>(), 14);
        // The reference block sits inside Phase 1.
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let lines = body(&src, "    fn xlog_lines(");
        let open = lines.find("phase(0, true, false);").expect("phase 1 opens");
        let refs = lines
            .find("g1_full_report_reference_lines(gc_id, refs)")
            .expect("the reference block");
        let close = lines.find("phase(0, false, true);").expect("phase 1 closes");
        assert!(open < refs && refs < close);
    }

    /// g3x item 12: no class unloaded, no second walk.
    #[test]
    fn gci_i30ob_metaspace_after_is_the_before_without_an_unload() {
        assert!(metaspace_after_is_the_before(3, 3));
        assert!(!metaspace_after_is_the_before(3, 4));
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let split = body(&src, "fn xlog_escalated_collection_lines(");
        assert!(split.contains("metaspace_after_bytes(shared, start)"));
    }

    /// g3x item 7: every Serial / G1 collection's `gc,start` line carries the
    /// pause's start.
    #[test]
    fn gci_i30ob_gc_start_is_decorated_with_the_pause_start() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let plain = body(&src, "fn xlog_collection_lines(");
        assert!(plain.contains("Some(start.t0),"));
        let split = body(&src, "fn xlog_escalated_collection_lines(");
        assert!(split.contains("Some(start.t0),"));
        assert!(split.contains("Some(start.t0 + std::time::Duration::from_nanos(young_ns)),"));
        let remark = body(&src, "fn g1_cycle_edge_finish(");
        assert!(remark.contains("edge.pause_start + std::time::Duration::from_nanos(remark_ns),"));
        assert!(remark.contains("log_unified_at(&[tag], LogLevel::Info, &line, at);"));
    }

    /// tl item 6: the TLAB line is printed from the start record, so a
    /// deferred G1 young report (which owns its `GcEventStart`) keeps it.
    #[test]
    fn gci_i30ob_the_tlab_line_travels_with_the_start() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let deferred = src
            .find("pub(super) struct G1DeferredYoungReport {")
            .expect("the deferred report");
        assert!(src[deferred..].contains("    start: GcEventStart,"));
        for writer in ["fn xlog_collection_lines(", "fn xlog_escalated_collection_lines("] {
            let b = body(&src, writer);
            assert!(b.contains("log_tlab_totals(start.tlab_totals.as_deref(),"), "{writer}");
        }
    }
}

/// gci i45/sl: `-Xlog:safepoint` (`gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016`):
/// the door / pause -> HotSpot 25 VM-operation name map and the emitter's
/// call sites (open inside the pause, end at the seal, print after the
/// release and after the pause's own lines).
#[cfg(test)]
mod gci_i45_sl_safepoint_line_tests {
    use super::*;

    const GEN: (bool, bool) = (true, false);
    const G1: (bool, bool) = (false, true);
    const ZGC: (bool, bool) = (false, false);

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Serial-shaped Generational and G1 collections carry HotSpot 25's
    /// operation names.
    #[test]
    fn gci_i45_sl_collection_names() {
        use super::GcDoor as D;
        for door in [D::AllocationThreshold, D::AllocationFailure] {
            assert_eq!(
                safepoint_vm_operation(door, GEN, false),
                Some("SerialCollectForAllocation")
            );
        }
        for door in [
            D::AllocationThreshold,
            D::AllocationFailure,
            D::G1CompactionPause,
            D::G1HumongousAllocation,
        ] {
            assert_eq!(
                safepoint_vm_operation(door, G1, false),
                Some("G1CollectForAllocation")
            );
            assert_eq!(
                safepoint_vm_operation(door, G1, true),
                Some("G1CollectForAllocation")
            );
        }
        for door in [D::SystemGc, D::DiagnosticCommand, D::JvmtiForceGc] {
            assert_eq!(safepoint_vm_operation(door, GEN, false), Some("SerialGCCollect"));
            // Serial has no concurrent cycle: the flag changes nothing.
            assert_eq!(safepoint_vm_operation(door, GEN, true), Some("SerialGCCollect"));
            assert_eq!(safepoint_vm_operation(door, G1, false), Some("G1CollectFull"));
        }
        // `-XX:+ExplicitGCInvokesConcurrent`: `VM_G1TryInitiateConcMark`.
        for door in [D::SystemGc, D::DiagnosticCommand] {
            assert_eq!(
                safepoint_vm_operation(door, G1, true),
                Some("G1TryInitiateConcMark")
            );
        }
        // `_jvmti_force_gc` is not a user-requested concurrent cause.
        assert_eq!(
            safepoint_vm_operation(D::JvmtiForceGc, G1, true),
            Some("G1CollectFull")
        );
        for shape in [GEN, G1] {
            assert_eq!(
                safepoint_vm_operation(D::MetadataThreshold, shape, false),
                Some("CollectForMetadataAllocation")
            );
            // Part of the one inspection / dump operation the walk prints.
            assert_eq!(safepoint_vm_operation(D::HeapInspection, shape, false), None);
            assert_eq!(safepoint_vm_operation(D::HeapDump, shape, false), None);
        }
        // ZGC: out of scope, no line.
        assert_eq!(safepoint_vm_operation(D::SystemGc, ZGC, false), None);
        assert_eq!(safepoint_vm_operation(D::AllocationThreshold, ZGC, false), None);
    }

    /// Non-collection pauses: HotSpot's operation, or no line where HotSpot
    /// runs no safepoint of its own.
    #[test]
    fn gci_i45_sl_non_collection_names() {
        use super::NonCollectionPause as P;
        assert_eq!(non_collection_safepoint_name(P::FrameTrace), Some("ThreadDump"));
        assert_eq!(non_collection_safepoint_name(P::HeapDump), Some("HeapDumper"));
        assert_eq!(
            non_collection_safepoint_name(P::HeapInspection),
            Some("GC_HeapInspection")
        );
        assert_eq!(
            non_collection_safepoint_name(P::Redefinition),
            Some("RedefineClasses")
        );
        assert_eq!(non_collection_safepoint_name(P::G1Remark), Some("G1PauseRemark"));
        for kind in [
            P::LoopExit,
            P::IcGrace,
            P::GenInitialMark,
            P::GenRemark,
            P::G1InitialMark,
            P::ZgcMarkStart,
        ] {
            assert_eq!(non_collection_safepoint_name(kind), None, "{kind:?}");
        }
    }

    /// The emitter's call sites: opened inside the pause, ended at the seal,
    /// printed after the pause's own lines.
    #[test]
    fn gci_i45_sl_call_sites() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let start = body(&src, "pub(super) fn gc_event_start(");
        assert!(start.contains("safepoint_line_begin_for(shared, None);"));
        let seal = body(&src, "pub(super) fn gc_event_seal(");
        let sealed = seal
            .find("s.sealed = Some((end, after, committed));")
            .expect("the seal");
        let sp = seal
            .find("unified_logging::safepoint_line_seal();")
            .expect("the line's end");
        assert!(sealed < sp);
        // gci i46/lg: `gc_event_finish_by` prints (or holds) it through
        // `safepoint_line_finish_by`.
        for (sig, needle) in [
            (
                "pub(super) fn gc_event_finish_by(",
                "safepoint_line_finish_by(shared, door, requester);",
            ),
            (
                "pub(super) fn gc_event_finish(",
                "safepoint_line_finish(collection_safepoint_name(shared, door));",
            ),
        ] {
            let f = body(&src, sig);
            let lines = f.find("gc_event_finish_with(").expect("the collection's lines");
            let line = f.find(needle).expect("the safepoint line");
            assert!(lines < line, "{sig}");
        }
        let nc_start = body(&src, "pub(super) fn non_collection_pause_start(");
        // gci i46/lg: through `non_collection_safepoint_begin`.
        let open = nc_start
            .find("non_collection_safepoint_begin(shared, kind, fold.is_some() || stw_full_mark);")
            .expect("opened");
        let none = nc_start.rfind("return None;").expect("the no-timer exit");
        assert!(open < none, "opened even when no timer is kept");
        assert!(body(&src, "fn non_collection_safepoint_begin(")
            .contains("safepoint_line_begin_for(shared, name);"));
        assert!(body(&src, "pub(super) fn non_collection_pause_seal(")
            .contains("unified_logging::safepoint_line_seal();"));
        let nc_finish = body(&src, "pub(super) fn non_collection_pause_finish(");
        let guard = nc_finish
            .find("let _safepoint_line = SafepointLineOnDrop;")
            .expect("printed on drop");
        let exit = nc_finish.find("let Some(t) = timer else").expect("the early exit");
        assert!(guard < exit);
        let begin = body(&src, "fn safepoint_line_begin_for(");
        let gate = begin
            .find("xlog_safepoint_lines_wanted()")
            .expect("gated on the tag");
        let read = begin.find("safepoint_stats()").expect("the barrier's figures");
        assert!(gate < read, "nothing read unless the tag is on");
    }
}

/// gci i46/lg: the safepoint residuals of
/// `gci-i44sp-xlog-safepoint-prints-nothing-FIXED-20261016` -- 2 (a frame-trace
/// pause's operation from its initiator's scope), 3 (a parked G1 young
/// report's line held) and 6 (a pause with no HotSpot operation is no
/// safepoint of the barrier's count).
#[cfg(test)]
mod gci_i46_lg_safepoint_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Residual 6: the pauses with no HotSpot operation are no safepoint,
    /// but for ZGC's mark start (out of scope; a safepoint there).
    #[test]
    fn gci_i46_lg_which_pauses_are_no_safepoint() {
        use super::NonCollectionPause as P;
        for kind in [
            P::LoopExit,
            P::IcGrace,
            P::GenInitialMark,
            P::GenRemark,
            P::G1InitialMark,
        ] {
            assert!(
                non_collection_pause_is_no_safepoint(kind, non_collection_safepoint_name(kind)),
                "{kind:?}"
            );
        }
        assert!(!non_collection_pause_is_no_safepoint(P::ZgcMarkStart, None));
        for kind in [
            P::FrameTrace,
            P::HeapDump,
            P::HeapInspection,
            P::Redefinition,
            P::G1Remark,
        ] {
            assert!(
                !non_collection_pause_is_no_safepoint(kind, non_collection_safepoint_name(kind)),
                "{kind:?}"
            );
        }
        // A remark folded into a G1 full collection's one operation.
        assert!(non_collection_pause_is_no_safepoint(P::G1Remark, None));
    }

    /// Residual 6 end to end on a VM's barrier: a loop-exit handshake and a
    /// folded remark count nothing, a heap-dump walk and ZGC's mark start
    /// count one each; `CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT=0` counts all.
    #[test]
    fn gci_i46_lg_the_barrier_counts_only_safepoints() {
        use super::NonCollectionPause as P;
        let run = |shared: &SharedVm, kind: P, part_of_another: bool| {
            let barrier = &shared.mem.gc_barrier;
            assert!(barrier.request_stw(ThreadId(0), 1));
            barrier.wait_for_all();
            non_collection_safepoint_begin(shared, kind, part_of_another);
            barrier.complete_gc(cratonvm_types::PointerMap::default());
        };
        let shared = SharedVm::new(VmConfig::default());
        let count = |s: &SharedVm| s.mem.gc_barrier.safepoint_stats().count;
        run(&shared, P::LoopExit, false);
        run(&shared, P::IcGrace, false);
        run(&shared, P::G1Remark, true);
        assert_eq!(count(&shared), 0);
        run(&shared, P::HeapDump, false);
        run(&shared, P::ZgcMarkStart, false);
        assert_eq!(count(&shared), 2);
        let killed = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JMX_SAFEPOINT_HOTSPOT_COUNT", Some("0"))],
            || SharedVm::new(VmConfig::default()),
        );
        run(&killed, P::LoopExit, false);
        assert_eq!(count(&killed), 1);
    }

    /// Residual 2: a frame-trace pause is `ThreadDump` unless its initiator
    /// holds a scope -- `None` (a single thread's `getStackTrace`, a
    /// handshake) or `PrintThreads` (`jcmd Thread.print`); the kill switch
    /// restores `ThreadDump`; other pauses ignore the scope.
    #[test]
    fn gci_i46_lg_frame_trace_pauses_take_the_scopes_operation() {
        use super::NonCollectionPause as P;
        let shared = SharedVm::new(VmConfig::default());
        let barrier = &shared.mem.gc_barrier;
        assert_eq!(non_collection_safepoint_name_on(&shared, P::FrameTrace), Some("ThreadDump"));
        {
            let _handshake = barrier.frame_trace_op_scope(None);
            assert_eq!(non_collection_safepoint_name_on(&shared, P::FrameTrace), None);
            assert_eq!(non_collection_safepoint_name_on(&shared, P::HeapDump), Some("HeapDumper"));
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_XLOG_SAFEPOINT_FRAME_TRACE_OPS", Some("0"))],
                || {
                    assert_eq!(
                        non_collection_safepoint_name_on(&shared, P::FrameTrace),
                        Some("ThreadDump")
                    );
                },
            );
            let _print = barrier.frame_trace_op_scope(Some("PrintThreads"));
            assert_eq!(
                non_collection_safepoint_name_on(&shared, P::FrameTrace),
                Some("PrintThreads")
            );
        }
        assert_eq!(non_collection_safepoint_name_on(&shared, P::FrameTrace), Some("ThreadDump"));
        // A handshake-scoped frame trace is no safepoint of the count.
        let _handshake = barrier.frame_trace_op_scope(None);
        assert!(barrier.request_stw(ThreadId(0), 1));
        barrier.wait_for_all();
        non_collection_safepoint_begin(&shared, P::FrameTrace, false);
        barrier.complete_gc(cratonvm_types::PointerMap::default());
        assert_eq!(barrier.safepoint_stats().count, 0);
    }

    /// Residual 3: the parked-report probe reads the requester's slot.
    #[test]
    fn gci_i46_lg_a_parked_report_is_found_by_its_requester() {
        let shared = SharedVm::new(VmConfig::default());
        assert!(!g1_young_report_parked_for(&shared, ThreadId(5)));
        shared
            .mem
            .alloc_ladder
            .g1_deferred_young
            .lock()
            .push(crate::vm::realms::heap_realm::G1DeferredYoungSlot {
                requester: 5,
                report: Box::new(()),
            });
        assert!(g1_young_report_parked_for(&shared, ThreadId(5)));
        assert!(!g1_young_report_parked_for(&shared, ThreadId(6)));
        shared.mem.alloc_ladder.g1_deferred_young.lock().clear();
    }

    /// Residual 3: the finish holds the line only on G1, under
    /// `-Xlog:safepoint`, with the switch on and a report parked, after
    /// printing an earlier held line of the requester; and the order in
    /// `gc_event_finish_by` (park, then the line) stays.
    #[test]
    fn gci_i46_lg_the_finish_holds_while_a_report_is_parked() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let f = body(&src, "fn safepoint_line_finish_by(");
        let wanted = f.find("xlog_safepoint_lines_wanted()").expect("gated on the tag");
        let release = f.find("safepoint_line_release_held(id.0);").expect("earlier held first");
        let parked = f.find("g1_young_report_parked_for(shared, id)").expect("the probe");
        let hold = f.find("safepoint_line_hold(name, id.0);").expect("held");
        assert!(f.contains("shared.mem.heap.is_g1()"));
        assert!(f.contains("xlog_safepoint_after_parked_report_enabled()"));
        assert!(wanted < release && release < parked && parked < hold);
        let by = body(&src, "pub(super) fn gc_event_finish_by(");
        let park = by
            .find("Some(id) => g1_park_deferred_young_report(shared, id, report),")
            .expect("the park");
        let line = by
            .find("safepoint_line_finish_by(shared, door, requester);")
            .expect("the line");
        assert!(park < line);
    }
}

/// gci i47w/xl47: the G1 full-collection door's one operation
/// (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`) and the
/// remark / cleanup split (residual 1 of
/// `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp-RETIRED-UNFIXED-20261018`).
/// `cargo test -j 5 -p cratonvm-vm --lib gci_i47w_xl47_`.
#[cfg(test)]
mod gci_i47w_xl47_safepoint_tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};

    fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm,
            ..VmConfig::default()
        })
    }

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// The fold is the calling thread's, on its VM, while held (nested holds
    /// stack); another thread and another VM are outside it; off G1 it is
    /// nothing.
    #[test]
    fn gci_i47w_xl47_the_door_fold_is_this_threads_while_held() {
        let g1 = vm(GcAlgorithm::G1);
        let other = vm(GcAlgorithm::G1);
        assert!(!g1_door_folds_this_pause(&g1));
        {
            let _outer = g1_door_safepoint_fold(&g1);
            assert!(g1_door_folds_this_pause(&g1));
            assert!(!g1_door_folds_this_pause(&other), "per VM");
            {
                let _inner = g1_door_safepoint_fold(&g1);
                assert!(g1_door_folds_this_pause(&g1));
            }
            assert!(g1_door_folds_this_pause(&g1), "the outer hold stays");
            let shared = &g1;
            let elsewhere = std::thread::scope(|s| {
                s.spawn(|| g1_door_folds_this_pause(shared))
                    .join()
                    .expect("joined")
            });
            assert!(!elsewhere, "per OS thread");
        }
        assert!(!g1_door_folds_this_pause(&g1));
        assert!(g1.mem.alloc_ladder.g1_door_safepoint_folds.lock().is_empty());
        let gen = vm(GcAlgorithm::Generational);
        let _hold = g1_door_safepoint_fold(&gen);
        assert!(!g1_door_folds_this_pause(&gen));
        assert!(gen.mem.alloc_ladder.g1_door_safepoint_folds.lock().is_empty());
    }

    /// A folded collection pause opens no line and is no safepoint of the
    /// barrier's count, decided before the line would read the figures; the
    /// `gc,jni` line is skipped for it too.
    #[test]
    fn gci_i47w_xl47_a_folded_pause_opens_no_line_and_counts_nothing() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let start = body(&src, "pub(super) fn gc_event_start(");
        let fold = start
            .find("if g1_door_folds_this_pause(shared) {")
            .expect("the fold test");
        let uncount = start
            .find("shared.mem.gc_barrier.note_pause_not_a_safepoint();")
            .expect("not a safepoint");
        let line = start
            .find("safepoint_line_begin_for(shared, None);")
            .expect("the line");
        assert!(fold < uncount && uncount < line, "the line only in the else arm");
        let by = body(&src, "pub(super) fn gc_event_finish_by(");
        assert!(by.contains("&& !g1_door_folds_this_pause(shared)"));
    }

    /// The remark's cleanup point splits the `safepoint` line (not for an
    /// aborted cycle's remark), and the head prints between the remark's and
    /// the cleanup's `gc` lines.
    #[test]
    fn gci_i47w_xl47_the_remark_line_splits_at_the_cleanup() {
        assert_eq!(G1_PAUSE_CLEANUP_OPERATION, "G1PauseCleanup");
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let cut = body(&src, "pub(super) fn non_collection_pause_cleanup_point(");
        let kind = cut
            .find("if t.kind != NonCollectionPause::G1Remark")
            .expect("G1 remark only");
        let aborted = cut
            .find("if !t.g1_cycle.as_ref().is_some_and(|e| e.aborted) {")
            .expect("not an aborted cycle's");
        let split = cut
            .find("safepoint_line_split(at, G1_PAUSE_CLEANUP_OPERATION);")
            .expect("the split");
        assert!(kind < aborted && aborted < split);
        let edge = body(&src, "fn g1_cycle_edge_finish(");
        let count = edge.find("gc_starts += 1;").expect("counts the start lines");
        let flush = edge
            .find("safepoint_line_flush_head();")
            .expect("the head");
        let write = edge
            .find("log_unified_at(&[tag], LogLevel::Info, &line, at);")
            .expect("the gc lines");
        assert!(count < flush && flush < write);
    }
}

/// gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`
/// CS1, `gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017` vi28-dg
/// E4): a G1 cycle opens only after a young-only pause, and a user-requested
/// concurrent cycle's young pause is its forced `Concurrent Start`.
#[cfg(test)]
mod gci_i48_cs48_concurrent_start_tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use cratonvm_types::flags::with_thread_overrides;

    const SWITCH: &str = "CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE";

    fn body(src: &str, sig: &str) -> String {
        let start = src
            .find(&format!("\n{sig}"))
            .unwrap_or_else(|| panic!("`{sig}` not found"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n\u{7d}\n")
            .unwrap_or_else(|| panic!("end of `{sig}` not found"));
        rest[..stop].to_string()
    }

    /// Only a mixed pause holds the start back, and the switch is read only
    /// for one.
    #[test]
    fn gci_i48_cs48_only_a_mixed_pause_defers_the_start() {
        assert!(g1_mixed_pause_defers_concurrent_start(4, 5, || true));
        assert!(!g1_mixed_pause_defers_concurrent_start(4, 5, || false));
        assert!(!g1_mixed_pause_defers_concurrent_start(5, 5, || {
            panic!("the switch is read only for a mixed pause")
        }));
    }

    /// `System.gc()` / `GC.run` under the flag, with no cycle marking, is a
    /// forced concurrent start; any other door, the flag off, an open cycle
    /// or the kill switch is not.
    #[test]
    fn gci_i48_cs48_the_user_requested_start_is_forced() {
        for door in [GcDoor::SystemGc, GcDoor::DiagnosticCommand] {
            assert!(g1_door_is_user_requested(door));
            assert!(g1_user_requested_concurrent_start(door, true, true, || true));
            assert!(!g1_user_requested_concurrent_start(door, false, true, || true));
            assert!(!g1_user_requested_concurrent_start(door, true, false, || true));
            assert!(!g1_user_requested_concurrent_start(door, true, true, || false));
        }
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::JvmtiForceGc,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
            GcDoor::MetadataThreshold,
        ] {
            assert!(!g1_door_is_user_requested(door), "{door:?}");
            assert!(!g1_user_requested_concurrent_start(door, true, true, || true));
        }
    }

    /// The switch is default ON and `=0` kills it.
    #[test]
    fn gci_i48_cs48_the_switch_is_default_on() {
        with_thread_overrides(&[(SWITCH, None)], || {
            assert!(g1_concurrent_start_at_young_pause_enabled());
        });
        with_thread_overrides(&[(SWITCH, Some("0"))], || {
            assert!(!g1_concurrent_start_at_young_pause_enabled());
        });
    }

    /// The wait is per VM: one VM's mixed pause holds no other VM's drive.
    #[test]
    fn gci_i48_cs48_the_wait_is_per_vm() {
        let g1 = || {
            SharedVm::new(VmConfig {
                gc_algorithm: GcAlgorithm::G1,
                ..VmConfig::default()
            })
        };
        let (a, b) = (g1(), g1());
        assert!(!crate::runtime::interpreter::g1_start_waits_for_young_only_pause(&a));
        a.mem
            .alloc_ladder
            .g1_conc_start_waits_for_young_pause
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(crate::runtime::interpreter::g1_start_waits_for_young_only_pause(&a));
        assert!(!crate::runtime::interpreter::g1_start_waits_for_young_only_pause(&b));
    }

    /// The seal records the wait inside every G1 pause; the user-requested
    /// rule labels the pause and its report waits for the cycle.
    #[test]
    fn gci_i48_cs48_the_seal_records_and_the_label_defers() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let seal = body(&src, "pub(super) fn gc_event_seal(");
        let record = seal
            .find("g1_mixed_pause_defers_concurrent_start(")
            .expect("the record");
        let store = seal
            .find(".g1_conc_start_waits_for_young_pause")
            .expect("the per-VM store");
        let sealed = seal.find("s.sealed = Some(").expect("the seal");
        assert!(record < store && store < sealed);
        // Not under `s.events`: every G1 pause records it.
        let events = seal.find("if s.events \u{7b}").expect("the events arm");
        assert!(events < record);
        let label = body(&src, "    fn g1_pause_type(");
        assert!(label.contains("|| g1_user_requested_concurrent_start("));
        let with = body(&src, "fn gc_event_finish_with(");
        assert!(with.contains("|| g1_door_is_user_requested(door))"));
    }

    /// The drives decline after a mixed pause before the IHOP consult; the
    /// user-requested door skips the IHOP tick when it will open the cycle,
    /// resolves a leftover report and runs no compaction.
    #[test]
    fn gci_i48_cs48_the_drives_and_the_door() {
        let src = include_str!("gc_and_alloc.rs").replace("\r\n", "\n");
        let drive = body(&src, "pub(super) fn g1_drive_mark_cycle(");
        assert!(drive.contains(
            "if !g1_start_waits_for_young_only_pause(shared) && \
             shared.mem.heap.g1_should_start_marking() \u{7b}"
        ));
        let jit = include_str!("../../jit/helpers.rs").replace("\r\n", "\n");
        let gate = jit
            .find("!crate::runtime::interpreter::g1_start_waits_for_young_only_pause(vm)")
            .expect("the JIT driver's gate");
        let ihop = jit[gate..]
            .find("&& vm.mem.heap.g1_should_start_marking()")
            .expect("the IHOP consult after it");
        assert!(ihop < 120);
        let f = body(&src, "fn force_full_collection(");
        let user = f
            .find("let user_concurrent = g1_door_is_user_requested_concurrent(shared, door);")
            .expect("the door's rule");
        let tick = f
            .find("if !abort_pending && !user_start \u{7b}")
            .expect("the tick gate");
        let cycle = f.find("g1_force_full_cycle(shared, thread);").expect("the cycle");
        let flush = f
            .find("g1_emit_deferred_young_report(shared, thread.thread_id, false);")
            .expect("the leftover report");
        let compaction = f
            .find("if !user_concurrent \u{7b}\n            g1_full_compaction_pause(")
            .expect("no compaction for the user-requested cycle");
        assert!(user < tick && tick < cycle && cycle < flush && flush < compaction);
        let rule = body(&src, "fn g1_door_is_user_requested_concurrent(");
        assert!(rule.contains("!g1_door_forces_stw_full_cycle(door)"));
        assert!(rule.contains("!g1_force_full_cycle_is_stw(shared.config.explicit_gc)"));
        assert!(rule.contains("g1_concurrent_start_at_young_pause_enabled()"));
    }
}

#[cfg(test)]
mod gcj_j1yg_tlab_totals_alone_tests {
    use super::*;

    /// A `gc+tlab=debug` rule arms the collection's events on its own only on
    /// a Generational heap and only with the kill switch unset.
    #[test]
    fn gcj_j1yg_a_tlab_only_spec_arms_events_on_generational_only() {
        assert!(tlab_totals_alone_arms_events(true, true, true));
        assert!(!tlab_totals_alone_arms_events(false, true, true), "G1 / ZGC unchanged");
        assert!(!tlab_totals_alone_arms_events(true, false, true), "no gc+tlab rule");
        assert!(!tlab_totals_alone_arms_events(true, true, false), "the =0 kill");
    }

    /// The collection's start asks the new arm beside `gc_events_wanted`, and
    /// before the `TLAB totals` stash reads `events`.
    #[test]
    fn gcj_j1yg_gc_event_start_ors_the_tlab_only_arm_before_the_stash() {
        let src = include_str!("gc_events.rs").replace("\r\n", "\n");
        let start = src
            .find("pub(super) fn gc_event_start(")
            .expect("gc_event_start");
        let body = &src[start..];
        let arm = body
            .find("(gc_events_wanted() || xlog_tlab_totals_alone_wanted(shared))")
            .expect("the tlab-only arm");
        let stash = body
            .find("stash_tlab_totals(shared, events)")
            .expect("the stash");
        assert!(arm < stash);
    }
}
