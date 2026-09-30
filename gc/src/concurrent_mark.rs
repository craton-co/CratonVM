// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent marking for the garbage collector.
//!
//! Implements tri-color marking that runs concurrently with application
//! threads. The marking process has four phases:
//!
//! 1. **Initial Mark (STW, brief):** Mark objects directly reachable from
//!    thread stacks and static fields. This is a short STW pause.
//!
//! 2. **Concurrent Mark:** Traverse the object graph from the initial roots,
//!    marking all reachable objects. Application threads continue running;
//!    the SATB write barrier logs overwritten references.
//!
//! 3. **Remark (STW, brief):** Process SATB buffers and re-scan roots to
//!    catch any references modified during concurrent marking.
//!
//! 4. **Concurrent Sweep:** Walk the old generation, freeing unmarked objects
//!    back to the free list. Application threads continue running.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    GC_FLAG_COMPACT, GC_FLAG_HEADER, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;
use crate::old_gen::{MajorTrigger, OldGen};
use crate::satb::SatbQueue;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// Concurrent GC phase tracking
// ---------------------------------------------------------------------------

/// Current phase of the concurrent GC cycle.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentGcPhase {
    /// No concurrent GC activity.
    Idle = 0,
    /// Initial mark — brief STW to mark root-reachable objects.
    InitialMark = 1,
    /// Concurrent mark — marker threads traverse the heap.
    ConcurrentMark = 2,
    /// Remark — brief STW to process SATB buffers and re-scan roots.
    Remark = 3,
    /// Concurrent sweep — reclaim unmarked old-gen objects.
    ConcurrentSweep = 4,
}

impl From<u8> for ConcurrentGcPhase {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::InitialMark,
            2 => Self::ConcurrentMark,
            3 => Self::Remark,
            4 => Self::ConcurrentSweep,
            _ => Self::Idle,
        }
    }
}

/// Atomic phase tracker visible to all threads.
pub struct ConcurrentGcState {
    phase: AtomicU8,
    /// gen r4w3/oldgen3 — per-VM cycle census (this state is the heap's
    /// shared instance, so it outlives the per-cycle `ConcurrentMarker`s),
    /// not process-global statics. See [`ConcurrentCycleCensus`].
    census: CycleCensusCells,
    /// gen r4w4/concmark4 — the adaptive concurrent-start policy's memory
    /// (what the last cycles cost in old-gen growth, and the open cycle's
    /// baseline). Per VM for the same reason as `census`. A LEAF lock: it is
    /// taken with the old-gen lock held (trigger decisions, initial mark, the
    /// sweep's end) and nothing is ever acquired while it is held. See
    /// [`OldGenPolicy`].
    policy: Mutex<StartPolicyState>,
    /// gen r4w5/concmark5 — the concurrent-cycle SERVICE thread's side of the
    /// VM: who it is, the pending wake request and the shutdown flag. Per VM
    /// (this state is the heap's shared instance), not a process global. See
    /// [`ConcurrentGcState::serve`].
    service: ServiceCells,
    /// gen r4w5/concmark5 — the STW old-gen trigger's cadence census
    /// (`gengc-r4-oldgen-major-trigger-has-no-hysteresis`): a LEAF lock taken
    /// under the old-gen lock by `GenerationalHeap::major_trigger_decision`.
    /// See [`MajorCadenceCensus`].
    cadence: Mutex<MajorCadence>,
    /// gen r4w5/concmark5 — bytes the CONCURRENT sweeps on this state reported
    /// to `OldGen::note_concurrent_collection_end`, summed, so the cadence
    /// census can take them out of `OldGenTriggerStats::freed_bytes` (which
    /// sums both kinds of old-gen collection). Written under the old-gen lock.
    concurrent_freed: AtomicU64,
    /// gc-common w36-d — the finalizer channel between the STOP-THE-WORLD
    /// collections and the concurrent cycle's remark. See
    /// [`ConcurrentGcState::publish_finalizer_candidates`]. Per VM (this state
    /// is the heap's shared instance). A LEAF lock: taken with the old-gen
    /// lock held (remark) or with no lock held (the STW door), and nothing is
    /// acquired while it is held.
    finalizers: Mutex<FinalizerChannel>,
    /// gen r5w4/conc8 — the compact layouts generational concurrent remarks
    /// kept registered for their sweeps, until a census proves no instance is
    /// left (`gengc-r5w3-unload7-retained-layouts-are-never-released`). Per VM
    /// (this state is the heap's shared instance). A LEAF lock: taken with no
    /// lock held (the driver, the unload transaction) or under the old-gen lock
    /// (the sweep's end), and nothing is acquired while it is held. See
    /// [`ConcurrentGcState::note_layouts_retained`].
    retained_layouts: Mutex<RetainedLayouts>,
    /// gen r5w6/conc10 — the census of the two opt-ins that stop dead young
    /// objects from keeping old ones (`CRATONVM_GEN_Y2O_LIVE_SEED`,
    /// `CRATONVM_GEN_YOUNG_MIRROR_DEFER`). Per VM; see [`Y2oSeedCells`].
    y2o: Y2oSeedCells,
}

/// gen r5w6/conc10 — what the concurrent pauses' young→old seeding did under
/// `CRATONVM_GEN_Y2O_LIVE_SEED` (`GenerationalHeap::collect_young_to_old_seeds`),
/// and how often a young collection's root scan deferred a young user-loader
/// mirror under `CRATONVM_GEN_YOUNG_MIRROR_DEFER`. Printed as the
/// `[GC] conc_y2o:` line ([`ConcurrentGcState::y2o_census_line`]) only while
/// either flag is on.
#[derive(Default)]
struct Y2oSeedCells {
    /// Concurrent pauses that took their young→old seeds through the new call.
    pauses: AtomicU64,
    /// ... of which from the live young set.
    live_pauses: AtomicU64,
    /// Fallbacks to the all-young enumeration, by reason.
    fallback_young_walk: AtomicU64,
    fallback_old_walk: AtomicU64,
    fallback_owner: AtomicU64,
    /// Young objects enumerated / young objects that seeded, summed.
    young_objects: AtomicU64,
    seeding_objects: AtomicU64,
    /// Young `Reference`s whose slot 0 was hidden, and the OLD referents that
    /// hiding kept out of the seeds, summed.
    hidden_young_refs: AtomicU64,
    hidden_referents_skipped: AtomicU64,
    /// Root scans that deferred at least one young mirror, and the mirrors.
    mirror_defer_scans: AtomicU64,
    mirrors_deferred: AtomicU64,
    /// gcd d1/c — remarks that looked for the young objects only a retained
    /// dead finalizable reaches, how many of those fell back (reported
    /// nothing), and the young objects found, summed.
    finalizer_only_passes: AtomicU64,
    finalizer_only_fallbacks: AtomicU64,
    finalizer_only_young: AtomicU64,
}

/// gen r5w4/conc8 — see [`ConcurrentGcState::note_layouts_retained`].
#[derive(Debug, Default)]
struct RetainedLayouts {
    /// Class ids whose layout a remark put back and nothing has released yet.
    /// Sorted, no duplicates.
    pending: Vec<u32>,
    /// Ids a COMPLETE census found no instance of, waiting for the driver to
    /// unregister them ([`ConcurrentGcState::take_releasable_layouts`]).
    releasable: Vec<u32>,
}

/// gc-common w36-d — see [`ConcurrentGcState::publish_finalizer_candidates`].
#[derive(Default)]
struct FinalizerChannel {
    /// The OLD-generation addresses of every registered finalizable, as the
    /// last stop-the-world collection left them. Sorted.
    candidates: Vec<usize>,
    /// Candidates a remark found dead and RETAINED, not yet reported.
    retained: Vec<usize>,
}

/// gen r4w3/oldgen3 — the atomic cells behind [`ConcurrentCycleCensus`].
#[derive(Default)]
struct CycleCensusCells {
    initial_marks: AtomicU64,
    initial_marks_with_takeover: AtomicU64,
    frozen_threads: AtomicU64,
    sweep_slices: AtomicU64,
    sweep_epoch_stops: AtomicU64,
    overflow_rescan_passes: AtomicU64,
    satb_drained_in_mark: AtomicU64,
    // --- gen r4w4/concmark4 ------------------------------------------------
    cycles_completed: AtomicU64,
    stw_deferred: AtomicU64,
    stw_preempted: AtomicU64,
    frozen_objects_scanned: AtomicU64,
    mark_local_spills: AtomicU64,
    // --- gen r4w5/concmark5 ------------------------------------------------
    service_handoffs: AtomicU64,
    service_growth_signals: AtomicU64,
    service_wakes_requested: AtomicU64,
    service_wakes_periodic: AtomicU64,
    service_periodic_due: AtomicU64,
    service_backoff_skips: AtomicU64,
    service_cycle_attempts: AtomicU64,
    service_cycles_completed: AtomicU64,
    remark_refproc_hook_calls: AtomicU64,
    // --- gen r4w6/concsvc6 -------------------------------------------------
    trigger_to_start_samples: AtomicU64,
    trigger_to_start_total_us: AtomicU64,
    trigger_to_start_max_us: AtomicU64,
    stw_frag_preempted: AtomicU64,
    satb_outside_old_gen: AtomicU64,
    // --- gen r5w1/refs5 ----------------------------------------------------
    sweep_epoch_aborts: AtomicU64,
    reference_skip_published: AtomicU64,
    remark_refproc_retired: AtomicU64,
    start_verdicts: AtomicU64,
    start_verdicts_due: AtomicU64,
    // --- gen r5w3/unload7 --------------------------------------------------
    class_unload_remarks: AtomicU64,
    class_unload_edges: AtomicU64,
    class_unload_loaders: AtomicU64,
    class_unload_classes: AtomicU64,
    class_unload_layouts_retained: AtomicU64,
    // --- gen r5w4/conc8 ----------------------------------------------------
    class_unload_layouts_released: AtomicU64,
    class_unload_layout_censuses: AtomicU64,
    // --- gcd d1/c ----------------------------------------------------------
    sweep_reference_rows_dropped: AtomicU64,
    // --- gcd d2/g ----------------------------------------------------------
    class_unload_stw_layout_censuses: AtomicU64,
    // --- gcd d9/e: see [`ConcurrentGcState::cycle_census_line`] -------------
    preempt_alloc_failure: AtomicU64,
    preempt_ceiling: AtomicU64,
    preempt_in_mark: AtomicU64,
    preempt_in_sweep: AtomicU64,
    preempt_growth_max_permille: AtomicU64,
    requested_through_cycle: AtomicU64,
    open_lost: AtomicU64,
    abandoned: AtomicU64,
    swept_nothing: AtomicU64,
    mark_us_total: AtomicU64,
    mark_us_max: AtomicU64,
    sweep_us_total: AtomicU64,
    phase2_start_walks: AtomicU64,
    phase2_walk_us: AtomicU64,
    phase2_tams_slices: AtomicU64,
    /// gce e2/c — inline start requests (no service): latched by a direct
    /// old-gen allocation, and taken by the VM's allocation slow path.
    inline_start_latched: AtomicU64,
    inline_start_taken: AtomicU64,
    /// `[door][step]`, see [`GenConcDoor`] / [`GenConcDoorStep`].
    doors: [[AtomicU64; GEN_CONC_DOOR_STEPS]; GEN_CONC_DOORS],
}

/// gen r5w3/unload7 — which door asked the generational concurrent start
/// trigger (`gengc-r5w2-conc6-proposal-generational-start-census-by-door`).
///
/// The Generational twin of G1's `MarkDoor` census (`g1.rs`,
/// `note_mark_door`), kept separate because the generational driver has a
/// door G1 does not (its service thread, which calls the driver with G1's
/// `MaybeGc` token) and because the steps it counts differ: G1 records one
/// OUTCOME per visit, this records how far a visit got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenConcDoor {
    /// `maybe_gc`'s epilogue (interpreted allocation).
    MaybeGc = 0,
    /// The allocation-failure collection door (`maybe_gc_forced`), where
    /// compiled code takes its young collections.
    AllocFail = 1,
    /// `System.gc()` (`force_gc_from_native`).
    SystemGc = 2,
    /// The concurrent-cycle service thread (`CRATONVM_GEN_CONC_SERVICE_THREAD`).
    Service = 3,
    /// Any other `MarkDoor` (G1-only doors; never expected on Generational).
    Other = 4,
}

/// Number of [`GenConcDoor`] variants.
pub const GEN_CONC_DOORS: usize = 5;

impl GenConcDoor {
    /// Every door, in census order.
    pub const ALL: [GenConcDoor; GEN_CONC_DOORS] = [
        GenConcDoor::MaybeGc,
        GenConcDoor::AllocFail,
        GenConcDoor::SystemGc,
        GenConcDoor::Service,
        GenConcDoor::Other,
    ];

    /// The `[GC] conc_doors:` key stem.
    pub fn token(self) -> &'static str {
        match self {
            GenConcDoor::MaybeGc => "maybe_gc",
            GenConcDoor::AllocFail => "alloc_fail",
            GenConcDoor::SystemGc => "system_gc",
            GenConcDoor::Service => "service",
            GenConcDoor::Other => "other",
        }
    }
}

/// gen r5w3/unload7 — how far one visit of the generational driver got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenConcDoorStep {
    /// The visit reached the start trigger (no cycle was open).
    Asked = 0,
    /// The trigger said "due" to THIS visit (a verdict handed to the service
    /// thread is not due here: the service's own visit counts it).
    Due = 1,
    /// This visit's initial-mark pause ran and opened the cycle.
    Opened = 2,
}

/// Number of [`GenConcDoorStep`] variants.
pub const GEN_CONC_DOOR_STEPS: usize = 3;

/// gen r4w3/oldgen3 (2026-09-23) — a snapshot of one VM's generational
/// concurrent-cycle census, from [`ConcurrentGcState::census`]. For the
/// shutdown `[GC]` summary (lane `obs`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConcurrentCycleCensus {
    /// Initial-mark pauses that OPENED a cycle, how many of them had at least
    /// one thread FROZEN by the xt takeover rather than parked at a poll, and
    /// the frozen threads summed over those pauses — the measurement
    /// `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md` asks for.
    /// See [`ConcurrentGcState::note_initial_mark_takeover`].
    pub initial_marks: u64,
    pub initial_marks_with_takeover: u64,
    pub frozen_threads: u64,
    /// Concurrent-sweep slices run, and sweeps stopped between two slices
    /// because another collector freed or slid old-gen storage.
    pub sweep_slices: u64,
    pub sweep_epoch_stops: u64,
    /// Mark-queue overflow rescan passes run CONCURRENTLY (in Phase-2
    /// slices) instead of inside the remark pause.
    pub overflow_rescan_passes: u64,
    /// SATB entries drained by Phase-2 slices rather than by remark.
    pub satb_drained_in_mark: u64,
    // --- gen r4w4/concmark4 (2026-09-24) -----------------------------------
    /// Cycles whose sweep ran to its end (an authorised sweep; a sweep stopped
    /// between slices by an epoch move counts too, since it reclaimed).
    pub cycles_completed: u64,
    /// Young pauses whose STW old-gen collection DEFERRED to an open
    /// concurrent cycle (concurrent-first policy only).
    pub stw_deferred: u64,
    /// STW old-gen collections that ran while a cycle was open anyway
    /// (allocation failure, or the generation past the defer ceiling): the
    /// cycle could not finish in time. Each widens the adaptive start buffer.
    pub stw_preempted: u64,
    /// Old-gen objects held by FROZEN threads that the initial-mark pause
    /// scanned eagerly (`ConcurrentMarker::initial_mark_with_frozen`).
    pub frozen_objects_scanned: u64,
    /// Grey objects the marker-local stack spilled to the shared queue
    /// (a full local stack, or the end of a budgeted slice).
    pub mark_local_spills: u64,
    // --- gen r4w5/concmark5 (2026-09-24) -----------------------------------
    /// Cycles a mutator found due and HANDED OFF to the attached service
    /// thread instead of running inline ([`ConcurrentGcState::hand_off_to_service`]).
    pub service_handoffs: u64,
    /// Direct old-gen allocations (humongous arrays, young-spill objects) that
    /// found the cycle due and woke the service with no young collection
    /// involved ([`ConcurrentGcState::signal_old_gen_growth`]).
    pub service_growth_signals: u64,
    /// Service wakes by a request (a hand-off or a growth signal), and by the
    /// period alone.
    pub service_wakes_requested: u64,
    pub service_wakes_periodic: u64,
    /// Periodic wakes on which the service's own trigger check said "due".
    pub service_periodic_due: u64,
    /// Periodic wakes skipped by the back-off after cycles that opened and
    /// did not complete.
    pub service_backoff_skips: u64,
    /// Cycle attempts the service ran, and how many of them completed a sweep.
    pub service_cycle_attempts: u64,
    pub service_cycles_completed: u64,
    /// Remarks that ran the reference-processing callback under
    /// `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`: the VM driver's real
    /// remark-time reference processing (gen r5w1/refs5), or the no-op hook
    /// of the test entry point [`ConcurrentMarker::remark`].
    pub remark_refproc_hook_calls: u64,
    // --- gen r4w6/concsvc6 (2026-09-24) ------------------------------------
    /// Trigger-to-start latency: cycles whose initial mark found a recorded
    /// "the start became due at t" instant, the summed and the largest
    /// `initial mark − t`, in microseconds. See
    /// [`ConcurrentGcState::note_start_verdict`].
    pub trigger_to_start_samples: u64,
    pub trigger_to_start_total_us: u64,
    pub trigger_to_start_max_us: u64,
    /// STW old-gen collections that ran through an open cycle ONLY because a
    /// fragmentation compaction was armed (the occupancy verdict would have
    /// deferred). Not a pre-emption: the start policy is not told the cycle
    /// began too late
    /// (`gengc-r4w5-review5-fragmentation-preemption-feeds-the-concurrent-start-policy`).
    pub stw_frag_preempted: u64,
    /// SATB entries a Phase-2 slice drained that lie OUTSIDE the old
    /// generation (young old-values: logged by the barrier, dropped by the
    /// marker). Against `satb_drained_in_mark` this is the share the
    /// barrier-side filter of
    /// `gengc-r4w5-concmark5-satb-barrier-logs-young-old-values` would remove.
    pub satb_outside_old_gen: u64,
    // --- gen r5w1/refs5 (2026-09-26) ---------------------------------------
    /// GCAUD-4: cycles on this VM that reclaimed nothing (or stopped
    /// reclaiming) because another collector freed or slid old-gen storage
    /// under them — counted at remark (between initial mark and remark), at
    /// the first sweep slice (between remark and the sweep) and between two
    /// sweep slices. Was the process-global `SWEEP_EPOCH_ABORTS`, whose doc
    /// named only the second of the three sites.
    pub sweep_epoch_aborts: u64,
    /// Reference objects whose referent slot an initial-mark pause HID from
    /// the trace ([`ConcurrentMarker::set_reference_skip`]), summed over
    /// cycles. Zero unless remark-time reference processing is on.
    pub reference_skip_published: u64,
    /// Weak/soft/phantom rows the remark-time reference processing took out of
    /// the processor's active set against the cycle's bitmap (cleared,
    /// enqueued, or pruned because the `Reference` itself was dead), summed.
    /// See [`ConcurrentGcState::note_remark_refproc_retired`].
    pub remark_refproc_retired: u64,
    /// Times the concurrent START trigger was asked (either policy; the
    /// driver's `concurrent_cycle_due`, the service's periodic check), and how
    /// many answers were "due". `start_verdicts == 0` on a run whose old
    /// generation filled means no door ever asked — the JIT-heavy shape where
    /// every young collection is an allocation-failure collection
    /// (`maybe_gc_forced`), a door that did not consult the concurrent trigger
    /// before gen r5w2/conc6 made `CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` default-on
    /// (gen r5w1/refs5, the orchestrator's `GenR4W4SteadyPromotionProbe`
    /// finding: `concdrv_cycles_started=0`, `major=7`). With every door asking,
    /// `start_verdicts` is at least the number of collections a thread RAN
    /// while no cycle was open (the driver returns before the trigger while
    /// one is), plus the service's periodic checks, the growth poll and the
    /// STW major's cause classification (`concurrent_start_owed`), which ask
    /// too.
    pub start_verdicts: u64,
    pub start_verdicts_due: u64,
    // --- gen r5w3/unload7 (2026-09-26) -------------------------------------
    /// Remarks that ran with concurrent class unloading armed
    /// (`CRATONVM_GEN_CONC_CLASS_UNLOAD`): the cycle's initial mark and remark
    /// took their roots under `with_class_unload_marking` and the marker
    /// followed the loader / mirror / metadata side-table edges.
    pub class_unload_remarks: u64,
    /// Objects the marker newly marked through a side-table edge
    /// ([`ClassUnloadTables`]), summed over cycles.
    pub class_unload_edges: u64,
    /// Loaders and classes those remarks unloaded, and the unloaded classes
    /// whose compact layout was kept registered for the sweep (see
    /// [`ConcurrentGcState::note_class_unload`]). gen r5w4/conc8: the layout
    /// count covers EVERY generational remark that retained one, including a
    /// reference-processing remark without the side tables, since the census
    /// releases those too.
    pub class_unload_loaders: u64,
    pub class_unload_classes: u64,
    pub class_unload_layouts_retained: u64,
    // --- gen r5w4/conc8 (2026-09-26) ---------------------------------------
    /// Retained layouts unregistered again once a complete census found no
    /// instance of their class left ([`ConcurrentGcState::take_releasable_layouts`]),
    /// and the complete censuses run (sweeps that walked the whole generation
    /// with retained layouts pending). `layouts_retained - layouts_released`
    /// is what is still held.
    pub class_unload_layouts_released: u64,
    pub class_unload_layout_censuses: u64,
    // --- gcd d1/c (2026-09-27) ---------------------------------------------
    /// ACTIVE soft / weak / phantom rows the concurrent sweep dropped because
    /// it freed their `Reference` object (see
    /// [`ConcurrentGcState::note_sweep_reference_rows_dropped`]), summed.
    pub sweep_reference_rows_dropped: u64,
    // --- gcd d2/g (2026-09-27) ---------------------------------------------
    /// Complete retained-layout censuses taken after a STOP-THE-WORLD
    /// collection that reclaimed old-generation storage
    /// ([`ConcurrentGcState::complete_stw_layout_census`]).
    pub class_unload_stw_layout_censuses: u64,
    // --- gcd d9/e (2026-09-28) ---------------------------------------------
    /// `stw_preempted` split by WHY the STW collection could not defer
    /// ([`StwPreemptCause`]): a failed old-gen allocation, or the ceiling.
    pub preempt_alloc_failure: u64,
    pub preempt_ceiling: u64,
    /// `stw_preempted` split by the phase the open cycle was in: marking
    /// (`ConcurrentMark` / `Remark`), or sweeping.
    pub preempt_in_mark: u64,
    pub preempt_in_sweep: u64,
    /// The largest old-gen growth an open cycle had seen, from its initial
    /// mark to the STW collection that pre-empted it, in per-mille of the
    /// generation's capacity: the "how much faster would the cycle have to
    /// be" figure.
    pub preempt_growth_max_permille: u64,
    /// REQUESTED STW collections (`System.gc()`, the allocation ladder) that
    /// ran while a cycle was open. Never deferred, never a pre-emption sample;
    /// counted so a run's abandoned cycles can be attributed.
    pub requested_through_cycle: u64,
    /// Cycles claimed whose initial-mark pause was lost (another pause was
    /// already in progress), so no cycle opened.
    pub open_lost: u64,
    /// Cycles that OPENED (initial mark ran) and were abandoned
    /// (`ConcurrentMarker::abort_cycle`) before their sweep ended: stale in
    /// Phase 2, every remark attempt lost, the service stopped, or an unwind.
    pub abandoned: u64,
    /// Cycles whose remark ran but whose sweep freed nothing because it was
    /// never authorised or its snapshot went stale before the first slice.
    pub swept_nothing: u64,
    /// Initial mark to remark, per cycle that reached its remark: summed and
    /// largest, in microseconds; and remark to the sweep's end, summed.
    pub mark_us_total: u64,
    pub mark_us_max: u64,
    pub sweep_us_total: u64,
    /// Object-start walks Phase-2 slices had to run because the old generation
    /// changed since the previous one (a promotion, a direct allocation), and
    /// their summed time in microseconds; and the slices that used the initial
    /// mark's snapshot instead (`CRATONVM_GEN_CONC_MARK_TAMS_STARTS`).
    pub phase2_start_walks: u64,
    pub phase2_walk_us: u64,
    pub phase2_tams_slices: u64,
    /// gce e2/c — `CRATONVM_GEN_CONC_INLINE_START`: requests latched by a
    /// direct old-gen allocation with no service attached, and requests the
    /// VM took (each then ran the driver, which re-asks the trigger).
    pub inline_start_latched: u64,
    pub inline_start_taken: u64,
}

impl ConcurrentGcState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(ConcurrentGcPhase::Idle as u8),
            census: CycleCensusCells::default(),
            policy: Mutex::new(StartPolicyState::default()),
            service: ServiceCells::default(),
            cadence: Mutex::new(MajorCadence::default()),
            concurrent_freed: AtomicU64::new(0),
            finalizers: Mutex::new(FinalizerChannel::default()),
            retained_layouts: Mutex::new(RetainedLayouts::default()),
            y2o: Y2oSeedCells::default(),
        }
    }

    /// gen r5w6/conc10 — one concurrent pause's young→old seeding under
    /// `CRATONVM_GEN_Y2O_LIVE_SEED` (see [`Y2oSeedCells`]). Relaxed adds.
    pub fn note_y2o_seeds(
        &self,
        seeds: &crate::gen_heap::YoungToOldSeeds,
        hidden_young_refs: usize,
    ) {
        use crate::gen_heap::Y2oFallback;
        let c = &self.y2o;
        c.pauses.fetch_add(1, Ordering::Relaxed);
        if seeds.from_live_set {
            c.live_pauses.fetch_add(1, Ordering::Relaxed);
        }
        match seeds.fallback {
            Some(Y2oFallback::YoungWalkAnomaly) => {
                c.fallback_young_walk.fetch_add(1, Ordering::Relaxed);
            }
            Some(Y2oFallback::OldWalkIncomplete) => {
                c.fallback_old_walk.fetch_add(1, Ordering::Relaxed);
            }
            Some(Y2oFallback::UnresolvedOwnerCandidate) => {
                c.fallback_owner.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        c.young_objects
            .fetch_add(seeds.young_objects as u64, Ordering::Relaxed);
        c.seeding_objects
            .fetch_add(seeds.seeding_objects as u64, Ordering::Relaxed);
        c.hidden_young_refs
            .fetch_add(hidden_young_refs as u64, Ordering::Relaxed);
        c.hidden_referents_skipped
            .fetch_add(seeds.hidden_referents_skipped as u64, Ordering::Relaxed);
    }

    /// gcd d1/c (2026-09-27) — one remark's search for the YOUNG objects only a
    /// retained dead finalizable reaches (`GenerationalHeap::young_reached_only_through`):
    /// `Some(n)` found `n`, `None` fell back and reported nothing. Relaxed adds.
    pub fn note_y2o_finalizer_only_young(&self, found: Option<usize>) {
        let c = &self.y2o;
        c.finalizer_only_passes.fetch_add(1, Ordering::Relaxed);
        match found {
            Some(n) => {
                c.finalizer_only_young.fetch_add(n as u64, Ordering::Relaxed);
            }
            None => {
                c.finalizer_only_fallbacks.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// gen r5w6/conc10 — a young collection's root scan deferred `n > 0` young
    /// user-loader mirrors under `CRATONVM_GEN_YOUNG_MIRROR_DEFER`.
    pub fn note_young_mirrors_deferred(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.y2o.mirror_defer_scans.fetch_add(1, Ordering::Relaxed);
        self.y2o
            .mirrors_deferred
            .fetch_add(n as u64, Ordering::Relaxed);
    }

    /// gen r5w6/conc10 — the shutdown `[GC] conc_y2o:` line, only while
    /// `CRATONVM_GEN_Y2O_LIVE_SEED` or `CRATONVM_GEN_YOUNG_MIRROR_DEFER` is on
    /// (a default run's summary gains no line). Appended by
    /// [`Self::driver_census_line`].
    pub fn y2o_census_line(&self) -> Option<String> {
        if !crate::gen_heap::y2o_live_seed_enabled()
            && !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_YOUNG_MIRROR_DEFER")
        {
            return None;
        }
        let c = &self.y2o;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Some(format!(
            "[GC] conc_y2o: y2o_pauses={} y2o_live_pauses={} y2o_fallback_young_walk={} \
             y2o_fallback_old_walk={} y2o_fallback_owner={} y2o_young_objects={} \
             y2o_seeding_objects={} y2o_hidden_young_refs={} y2o_hidden_referents_skipped={} \
             young_mirror_defer_scans={} young_mirrors_deferred={} \
             y2o_finalizer_only_passes={} y2o_finalizer_only_fallbacks={} \
             y2o_finalizer_only_young={}",
            r(&c.pauses),
            r(&c.live_pauses),
            r(&c.fallback_young_walk),
            r(&c.fallback_old_walk),
            r(&c.fallback_owner),
            r(&c.young_objects),
            r(&c.seeding_objects),
            r(&c.hidden_young_refs),
            r(&c.hidden_referents_skipped),
            r(&c.mirror_defer_scans),
            r(&c.mirrors_deferred),
            r(&c.finalizer_only_passes),
            r(&c.finalizer_only_fallbacks),
            r(&c.finalizer_only_young),
        ))
    }

    /// gen r5w4/conc8 — a generational concurrent remark's unload transaction
    /// put these classes' compact layouts back (`memory::gc::with_retained_unloaded_layouts`)
    /// so its sweep can size their dead instances. They stay PENDING until a
    /// census proves no instance of the class is left anywhere
    /// (`gengc-r5w3-unload7-retained-layouts-are-never-released`):
    ///
    /// 1. the next cycle's initial-mark pause keeps, as census candidates,
    ///    the pending ids with no instance in the YOUNG generation
    ///    ([`ConcurrentMarker::set_layout_census`]). None can appear there
    ///    later: the class is out of the store, so nothing allocates it;
    /// 2. that cycle's sweep, walking the WHOLE old generation, notes every
    ///    candidate it finds a surviving (marked, or not sweep-eligible)
    ///    instance of. An instance promoted after the remark but unreachable
    ///    is sweep-eligible here, so it is freed rather than noted;
    /// 3. only a sweep that walked to the end with no epoch stop and no walk
    ///    break anywhere in the process (`walk_break_hits`) completes the
    ///    census; the candidates it saw no survivor of become RELEASABLE, and
    ///    the driver unregisters them under the class manager's write lock.
    ///
    /// Anything short of a complete census keeps the layout, which is the
    /// state before this: a leak, never a walk that cannot size an object.
    pub fn note_layouts_retained(&self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        let mut r = self.retained_layouts.lock();
        r.pending.extend_from_slice(ids);
        r.pending.sort_unstable();
        r.pending.dedup();
    }

    /// gen r5w4/conc8 — is any retained layout still pending?
    pub fn has_retained_layouts(&self) -> bool {
        !self.retained_layouts.lock().pending.is_empty()
    }

    /// gen r5w4/conc8 — the pending retained ids (a copy), for the census.
    pub fn retained_layouts_pending(&self) -> Vec<u32> {
        self.retained_layouts.lock().pending.clone()
    }

    /// gen r5w4/conc8 — a COMPLETE census found no surviving instance of
    /// `released` (each a candidate this cycle's initial mark proved absent from
    /// the young generation): move them from pending to releasable.
    fn complete_layout_census(&self, mut released: Vec<u32>) {
        self.census
            .class_unload_layout_censuses
            .fetch_add(1, Ordering::Relaxed);
        if released.is_empty() {
            return;
        }
        released.sort_unstable();
        released.dedup();
        let mut r = self.retained_layouts.lock();
        r.pending.retain(|id| released.binary_search(id).is_err());
        r.releasable.extend_from_slice(&released);
    }

    /// gen r5w4/conc8 — take (drain) the ids a complete census released, for
    /// the driver to unregister (`cratonvm_types::unregister_class_layout`).
    pub fn take_releasable_layouts(&self) -> Vec<u32> {
        std::mem::take(&mut self.retained_layouts.lock().releasable)
    }

    /// gcd d2/g (`gengc-r5w4-conc8-proposal-stw-majors-take-the-retained-layout-census`)
    /// — the census taken after a STOP-THE-WORLD collection that reclaimed
    /// old-generation storage: `survivors` is the class id of every object the
    /// two generations still hold (from COMPLETE walks of both, with the world
    /// stopped — the caller's contract), so a pending id not in it has no
    /// instance left anywhere a walker can meet one, and becomes releasable.
    /// This is the concurrent census's rule applied to what a finished
    /// collection left, instead of to what a sweep observed; a program whose
    /// old collections are all stop-the-world majors otherwise never released
    /// a retained layout. Returns how many ids it made releasable.
    pub fn complete_stw_layout_census(&self, survivors: &rustc_hash::FxHashSet<u32>) -> usize {
        self.census
            .class_unload_stw_layout_censuses
            .fetch_add(1, Ordering::Relaxed);
        let mut r = self.retained_layouts.lock();
        let before = r.pending.len();
        let mut released: Vec<u32> = Vec::new();
        r.pending.retain(|id| {
            if survivors.contains(id) {
                true
            } else {
                released.push(*id);
                false
            }
        });
        debug_assert_eq!(before, r.pending.len() + released.len());
        let n = released.len();
        r.releasable.extend_from_slice(&released);
        n
    }

    /// gen r5w4/conc8 — the driver unregistered `n` retained layouts.
    pub fn note_layouts_released(&self, n: usize) {
        if n != 0 {
            self.census
                .class_unload_layouts_released
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// gc-common w36-d — publish, at the end of a stop-the-world collection,
    /// the OLD-generation addresses of every registered finalizable (the
    /// collection's `finalizer_addrs`, mapped to where the collection left
    /// them), for the next remark.
    ///
    /// # Why the concurrent cycle needs them
    ///
    /// The VM hands its finalizable objects to the collector only through
    /// `collect_garbage_with_finalizers`; the concurrent old-gen cycle runs
    /// outside that call and its marker traces from the roots alone, so a
    /// finalizable that died OLD was unmarked at remark and the concurrent
    /// sweep FREED it — without `finalize()`, and leaving its `Finalizer` row
    /// on a freed (later recycled) address. The remark now RETAINS every
    /// published candidate it finds unmarked (and its subgraph), records it
    /// ([`Self::note_remark_retained_finalizers`]), and the next stop-the-world
    /// collection reports it with its own dead finalizers
    /// ([`Self::take_remark_retained_finalizers`]) — the "pending list the next
    /// post-GC pass drains" of the w6-d handoff, kept inside the collector so
    /// the VM's one consumer (`enqueue_resurrected_finalizers`) serves both.
    ///
    /// A candidate is published only by a stop-the-world collection, so an
    /// object registered while ALREADY old since the last one (a direct old
    /// allocation) is not protected until the next; it is the one residual.
    pub fn publish_finalizer_candidates(&self, mut old_addrs: Vec<usize>) {
        old_addrs.sort_unstable();
        old_addrs.dedup();
        self.finalizers.lock().candidates = old_addrs;
    }

    /// The published candidates (see [`Self::publish_finalizer_candidates`]).
    pub fn finalizer_candidates(&self) -> Vec<usize> {
        self.finalizers.lock().candidates.clone()
    }

    /// Record candidates a remark found dead and retained.
    pub fn note_remark_retained_finalizers(&self, addrs: &[usize]) {
        if !addrs.is_empty() {
            self.finalizers.lock().retained.extend_from_slice(addrs);
        }
    }

    /// Take (drain) the retained candidates, for the next stop-the-world
    /// collection to report. The caller must drop any that are no longer
    /// registered finalizables.
    pub fn take_remark_retained_finalizers(&self) -> Vec<usize> {
        std::mem::take(&mut self.finalizers.lock().retained)
    }

    /// gen r4w3/oldgen3 — record one initial-mark pause that OPENED a cycle
    /// (called by the driver after `ConcurrentMarker::initial_mark` ran), and
    /// how many threads `stw_take_over_and_wait` froze for it.
    ///
    /// A frozen thread may be stopped between a compiled reference store's
    /// SATB gate test (which read "clear" before this pause armed it) and the
    /// store itself; when it resumes it completes the store WITHOUT logging
    /// the old value. That is the hazard
    /// `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md` describes,
    /// and its two candidate fixes trade differently depending on one number:
    /// how often an opening pause has a non-empty takeover set at all. "Refuse
    /// the cycle on takeover" is free if `initial_marks_with_takeover` is ~0
    /// and starves the cycle if it tracks `initial_marks`. Relaxed: a census,
    /// read at shutdown.
    pub fn note_initial_mark_takeover(&self, frozen_threads: usize) {
        let c = &self.census;
        c.initial_marks.fetch_add(1, Ordering::Relaxed);
        if frozen_threads > 0 {
            c.initial_marks_with_takeover
                .fetch_add(1, Ordering::Relaxed);
            c.frozen_threads
                .fetch_add(frozen_threads as u64, Ordering::Relaxed);
        }
    }

    /// gen r4w3/oldgen3 — this VM's concurrent-cycle census. See
    /// [`ConcurrentCycleCensus`].
    pub fn census(&self) -> ConcurrentCycleCensus {
        let c = &self.census;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ConcurrentCycleCensus {
            initial_marks: r(&c.initial_marks),
            initial_marks_with_takeover: r(&c.initial_marks_with_takeover),
            frozen_threads: r(&c.frozen_threads),
            sweep_slices: r(&c.sweep_slices),
            sweep_epoch_stops: r(&c.sweep_epoch_stops),
            overflow_rescan_passes: r(&c.overflow_rescan_passes),
            satb_drained_in_mark: r(&c.satb_drained_in_mark),
            cycles_completed: r(&c.cycles_completed),
            stw_deferred: r(&c.stw_deferred),
            stw_preempted: r(&c.stw_preempted),
            frozen_objects_scanned: r(&c.frozen_objects_scanned),
            mark_local_spills: r(&c.mark_local_spills),
            service_handoffs: r(&c.service_handoffs),
            service_growth_signals: r(&c.service_growth_signals),
            service_wakes_requested: r(&c.service_wakes_requested),
            service_wakes_periodic: r(&c.service_wakes_periodic),
            service_periodic_due: r(&c.service_periodic_due),
            service_backoff_skips: r(&c.service_backoff_skips),
            service_cycle_attempts: r(&c.service_cycle_attempts),
            service_cycles_completed: r(&c.service_cycles_completed),
            remark_refproc_hook_calls: r(&c.remark_refproc_hook_calls),
            trigger_to_start_samples: r(&c.trigger_to_start_samples),
            trigger_to_start_total_us: r(&c.trigger_to_start_total_us),
            trigger_to_start_max_us: r(&c.trigger_to_start_max_us),
            stw_frag_preempted: r(&c.stw_frag_preempted),
            satb_outside_old_gen: r(&c.satb_outside_old_gen),
            sweep_epoch_aborts: r(&c.sweep_epoch_aborts),
            reference_skip_published: r(&c.reference_skip_published),
            remark_refproc_retired: r(&c.remark_refproc_retired),
            start_verdicts: r(&c.start_verdicts),
            start_verdicts_due: r(&c.start_verdicts_due),
            class_unload_remarks: r(&c.class_unload_remarks),
            class_unload_edges: r(&c.class_unload_edges),
            class_unload_loaders: r(&c.class_unload_loaders),
            class_unload_classes: r(&c.class_unload_classes),
            class_unload_layouts_retained: r(&c.class_unload_layouts_retained),
            class_unload_layouts_released: r(&c.class_unload_layouts_released),
            class_unload_layout_censuses: r(&c.class_unload_layout_censuses),
            sweep_reference_rows_dropped: r(&c.sweep_reference_rows_dropped),
            class_unload_stw_layout_censuses: r(&c.class_unload_stw_layout_censuses),
            preempt_alloc_failure: r(&c.preempt_alloc_failure),
            preempt_ceiling: r(&c.preempt_ceiling),
            preempt_in_mark: r(&c.preempt_in_mark),
            preempt_in_sweep: r(&c.preempt_in_sweep),
            preempt_growth_max_permille: r(&c.preempt_growth_max_permille),
            requested_through_cycle: r(&c.requested_through_cycle),
            open_lost: r(&c.open_lost),
            abandoned: r(&c.abandoned),
            swept_nothing: r(&c.swept_nothing),
            mark_us_total: r(&c.mark_us_total),
            mark_us_max: r(&c.mark_us_max),
            sweep_us_total: r(&c.sweep_us_total),
            phase2_start_walks: r(&c.phase2_start_walks),
            phase2_walk_us: r(&c.phase2_walk_us),
            phase2_tams_slices: r(&c.phase2_tams_slices),
            inline_start_latched: r(&c.inline_start_latched),
            inline_start_taken: r(&c.inline_start_taken),
        }
    }

    /// gcd d9/e (2026-09-28) — the shutdown `[GC] conc_cycles:` line: every
    /// concurrent cycle this VM claimed, by how it ended, and why the STW
    /// old-gen collection pre-empted the ones it did
    /// (`gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger`).
    /// Appended by [`Self::driver_census_line`] as its last line. Keys are
    /// `conccyc_`-prefixed; times are milliseconds.
    ///
    /// * `started` (initial marks that opened a cycle) =
    ///   `completed + abandoned + swept_nothing` plus at most one cycle
    ///   still open at exit; `open_lost` are claims whose initial-mark pause
    ///   was lost (never started).
    /// * `preempted` = `preempt_alloc_failure + preempt_ceiling` =
    ///   `preempt_in_mark + preempt_in_sweep` (`concpol_stw_preempted`).
    /// * `precedence` / `tams_starts`: whether the two gcd d9/e switches (default on since gce,
    ///   2026-09-29; `=0` turns each off) were on at exit.
    pub fn cycle_census_line(&self) -> String {
        let c = self.census();
        let ms = |us: u64| us / 1000;
        format!(
            "[GC] conc_cycles: conccyc_started={} conccyc_completed={} \
             conccyc_open_lost={} conccyc_abandoned={} conccyc_swept_nothing={} \
             conccyc_preempted={} conccyc_preempt_alloc_failure={} conccyc_preempt_ceiling={} \
             conccyc_preempt_in_mark={} conccyc_preempt_in_sweep={} \
             conccyc_preempt_growth_max_pct={}.{} conccyc_requested_through_cycle={} \
             conccyc_stw_deferred={} conccyc_mark_ms_total={} conccyc_mark_ms_max={} \
             conccyc_sweep_ms_total={} conccyc_phase2_start_walks={} \
             conccyc_phase2_walk_ms={} conccyc_phase2_tams_slices={} \
             conccyc_inline_start_latched={} conccyc_inline_start_taken={} \
             conccyc_precedence={} conccyc_tams_starts={}",
            c.initial_marks,
            c.cycles_completed,
            c.open_lost,
            c.abandoned,
            c.swept_nothing,
            c.stw_preempted,
            c.preempt_alloc_failure,
            c.preempt_ceiling,
            c.preempt_in_mark,
            c.preempt_in_sweep,
            c.preempt_growth_max_permille / 10,
            c.preempt_growth_max_permille % 10,
            c.requested_through_cycle,
            c.stw_deferred,
            ms(c.mark_us_total),
            ms(c.mark_us_max),
            ms(c.sweep_us_total),
            c.phase2_start_walks,
            ms(c.phase2_walk_us),
            c.phase2_tams_slices,
            c.inline_start_latched,
            c.inline_start_taken,
            gen_conc_precedence_enabled(),
            gen_conc_mark_tams_starts_enabled(),
        )
    }

    /// gen r5w3/unload7 — one visit of the generational driver on `door`
    /// reached `step` (`gengc-r5w2-conc6-proposal-generational-start-census-by-door`).
    /// One relaxed add, charged per collection that asks, never per
    /// allocation.
    pub fn note_door(&self, door: GenConcDoor, step: GenConcDoorStep) {
        self.census.doors[door as usize][step as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// `[door][step]` counts, in [`GenConcDoor::ALL`] / [`GenConcDoorStep`]
    /// order.
    pub fn door_census(&self) -> [[u64; GEN_CONC_DOOR_STEPS]; GEN_CONC_DOORS] {
        let mut out = [[0u64; GEN_CONC_DOOR_STEPS]; GEN_CONC_DOORS];
        for (d, row) in self.census.doors.iter().enumerate() {
            for (s, cell) in row.iter().enumerate() {
                out[d][s] = cell.load(Ordering::Relaxed);
            }
        }
        out
    }

    /// gen r5w3/unload7 — the shutdown `[GC] conc_doors:` line:
    /// `concdoor_<door>_{asked,due,opened}` for every [`GenConcDoor`]. A NEW
    /// line rather than more `[GC] conc_driver:` keys (whose tail a test
    /// pins). Printed after the driver line (see [`Self::driver_census_line`]).
    pub fn door_census_line(&self) -> String {
        let counts = self.door_census();
        let mut line = String::from("[GC] conc_doors:");
        for door in GenConcDoor::ALL {
            let [asked, due, opened] = counts[door as usize];
            let t = door.token();
            line.push_str(&format!(
                " concdoor_{t}_asked={asked} concdoor_{t}_due={due} concdoor_{t}_opened={opened}"
            ));
        }
        line
    }

    /// gen r5w3/unload7 — a remark ran with concurrent class unloading armed.
    pub fn note_class_unload_remark(&self) {
        self.census
            .class_unload_remarks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// gen r5w3/unload7 — what one remark's class-metadata unload did:
    /// `loaders` defining loaders and `classes` classes unloaded, of which
    /// `layouts_retained` kept their compact field layout registered so the
    /// concurrent sweep (and any later old-generation walk) can still size
    /// their dead instances.
    pub fn note_class_unload(&self, loaders: usize, classes: usize, layouts_retained: usize) {
        let c = &self.census;
        c.class_unload_loaders
            .fetch_add(loaders as u64, Ordering::Relaxed);
        c.class_unload_classes
            .fetch_add(classes as u64, Ordering::Relaxed);
        c.class_unload_layouts_retained
            .fetch_add(layouts_retained as u64, Ordering::Relaxed);
    }

    fn note_class_unload_edges(&self, n: u64) {
        if n != 0 {
            self.census
                .class_unload_edges
                .fetch_add(n, Ordering::Relaxed);
        }
    }

    /// gen r5w3/unload7 — the shutdown `[GC] conc_unload:` line. Printed only
    /// while `CRATONVM_GEN_CONC_CLASS_UNLOAD` is set, so a default run's
    /// summary gains no line from a feature it cannot use.
    pub fn class_unload_census_line(&self) -> Option<String> {
        if !crate::gc_flags().gen_conc_class_unload {
            return None;
        }
        let c = self.census();
        Some(format!(
            "[GC] conc_unload: concunload_remarks={} concunload_edges={} \
             concunload_loaders={} concunload_classes={} concunload_layouts_retained={} \
             concunload_layouts_released={} concunload_layout_censuses={} \
             concunload_stw_layout_censuses={}",
            c.class_unload_remarks,
            c.class_unload_edges,
            c.class_unload_loaders,
            c.class_unload_classes,
            c.class_unload_layouts_retained,
            c.class_unload_layouts_released,
            c.class_unload_layout_censuses,
            c.class_unload_stw_layout_censuses,
        ))
    }

    /// gen r5w1/refs5 — one remark ran the VM's reference-processing callback
    /// (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`), which retired `retired`
    /// references (cleared, or enqueued) against the cycle's bitmap. Counts
    /// the call in `remark_refproc_hook_calls`, the key the wave-5 probe reads.
    pub fn note_remark_refproc_retired(&self, retired: usize) {
        self.note_remark_refproc_hook();
        self.census
            .remark_refproc_retired
            .fetch_add(retired as u64, Ordering::Relaxed);
    }

    /// gcd d1/c (2026-09-27) — a concurrent sweep dropped `n` ACTIVE
    /// soft / weak / phantom rows whose `Reference` object it freed
    /// (`gengc-r5w5-conc9-concurrent-sweep-leaves-reference-rows-of-freed-references`).
    /// Printed as `concdrv_sweep_reference_rows_dropped`. One relaxed add per
    /// sweep; nothing for `0`.
    pub fn note_sweep_reference_rows_dropped(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.census
            .sweep_reference_rows_dropped
            .fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Get the current GC phase.
    #[inline]
    pub fn phase(&self) -> ConcurrentGcPhase {
        ConcurrentGcPhase::from(self.phase.load(Ordering::Acquire))
    }

    /// Set the GC phase (called by the GC coordinator).
    ///
    /// The phase is the MARKER's business: it says where the cycle is, for the
    /// drivers, the logs and the reports. It is no longer a barrier gate.
    ///
    /// gc-common w5-e (`common-g-proposal-one-satb-gate`, finished): "must this
    /// reference store log its old value?" has ONE answer on Generational and
    /// G1, `SatbQueue::is_active()`. The Rust barriers
    /// (`GenerationalHeap::satb_barrier`, `G1Collector::satb_pre_barrier_required`)
    /// test the queue, and the queue owns the compiled-code pre-gate: it takes
    /// one count of `gen_heap::arm_jit_ref_store_marker` BEFORE it stores
    /// ACTIVE and gives it back AFTER every INACTIVE store and on `Drop`
    /// (`satb.rs`, `SatbQueue::jit_gate_armed`). So this store touches no
    /// gate. It used to arm and disarm its own JIT count around the marking
    /// phases, which kept the JIT gate a superset of the PHASE gate by call-site
    /// order; the queue now makes the JIT gate a superset of the one gate that
    /// actually retains a value, by construction.
    pub fn set_phase(&self, phase: ConcurrentGcPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    /// Whether concurrent marking is active (SATB barrier should log).
    #[inline]
    pub fn is_marking_active(&self) -> bool {
        let p = self.phase.load(Ordering::Acquire);
        p == ConcurrentGcPhase::ConcurrentMark as u8 || p == ConcurrentGcPhase::Remark as u8
    }

    /// gen r4w2/concmark (2026-09-23) — claim the right to run ONE concurrent
    /// cycle on this state: `Idle → InitialMark`, atomically.
    ///
    /// # Why the cycle needs an owner
    ///
    /// Every generational cycle is built on the heap's SHARED SATB queue and
    /// this SHARED phase (`ConcurrentMarker::with_shared`), but each driver
    /// builds its OWN marker. Nothing used to stop two drivers overlapping: the
    /// driver checked only `old_gen_needs_gc()`, and `initial_mark` stores
    /// `InitialMark` unconditionally. Whichever overlapping cycle remarks first
    /// drains the shared queue and deactivates it, and the other one then marks
    /// with the barrier off and sweeps against an incomplete snapshot — a live
    /// object freed. Until Phase 2 was sliced the whole-phase old-gen lock hid
    /// most of this (a second driver blocked in `old_gen_needs_gc`); it never
    /// hid all of it, because the second driver could still open between the
    /// first one's Phase 2 and its remark.
    ///
    /// `false` means another cycle owns the state; the caller must not open one.
    /// Neither `Idle` nor `InitialMark` is a marking phase, so this transition
    /// needs no JIT-gate arming (see [`Self::set_phase`]).
    pub fn try_open_cycle(&self) -> bool {
        self.cas_quiet_phase(ConcurrentGcPhase::Idle, ConcurrentGcPhase::InitialMark)
    }

    /// `from → to` iff the phase is currently `from`, for two phases NEITHER of
    /// which is a marking phase, so the JIT gate's arm count cannot be affected
    /// (that is what lets this bypass [`Self::set_phase`]). gc-common w5-e:
    /// `set_phase` no longer arms anything either (the SATB queue owns the JIT
    /// gate), so the restriction now only keeps every marking transition on
    /// the one plain writer.
    fn cas_quiet_phase(&self, from: ConcurrentGcPhase, to: ConcurrentGcPhase) -> bool {
        debug_assert!(
            !matches!(
                from,
                ConcurrentGcPhase::ConcurrentMark | ConcurrentGcPhase::Remark
            ) && !matches!(
                to,
                ConcurrentGcPhase::ConcurrentMark | ConcurrentGcPhase::Remark
            ),
            "cas_quiet_phase may only move between non-marking phases"
        );
        self.phase
            .compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl Default for ConcurrentGcState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConcurrentGcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConcurrentGcState({:?})", self.phase())
    }
}

// ---------------------------------------------------------------------------
// gen r4w4/concmark4 — the old-generation collection policy
// ---------------------------------------------------------------------------

/// gen r4w4/concmark4 (2026-09-24) — the STW old-gen trigger's occupancy
/// floor, in percent. The same number as `OldGen::major_trigger_verdict`'s
/// `capacity * 75 / 100`, restated rather than moved, so the verdict (which is
/// all the legacy policy is) cannot drift through an edit to this file.
pub const OLD_STW_FLOOR_PCT: usize = 75;
/// The concurrent cycle's initiating occupancy until one cycle has been
/// measured: HotSpot's `InitiatingHeapOccupancyPercent` default.
pub const CONC_START_INITIAL_PCT: usize = 45;
/// The adaptive initiating occupancy never drops below this, so a cycle
/// measured as very expensive cannot make the collector run back to back.
pub const CONC_START_MIN_PCT: usize = 20;
/// While a cycle is open, the STW old-gen collection defers to it until the
/// generation is this full (or an old-gen allocation fails, or a collection is
/// requested) — the "the concurrent cycle cannot finish in time" fallback.
pub const CONC_DEFER_CEILING_PCT: usize = 90;

/// gcd d9/e (2026-09-28) — `CRATONVM_GEN_CONC_PRECEDENCE` (default on since gce, 2026-09-29; token
/// `gen-conc-precedence`): an open concurrent cycle takes precedence over the
/// STW old-gen collection until a real concurrent-mode failure.
///
/// Page: `docs/internal/gc/gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger-DONE-20260929.md`.
/// Two changes, both in this file's pure formulas and both off only when the
/// flag is `=0`:
///
/// 1. **No occupancy ceiling on the deferral.** With the flag the STW
///    collection defers to an open cycle at ANY occupancy until an old-gen
///    allocation has failed since the last collection (or the collection was
///    requested, or a fragmentation compaction is armed) — HotSpot CMS / G1's
///    rule: the full collection is the answer to a failed promotion, not to a
///    percentage ([`ConcurrentStartPolicy::stw_defers_with`]). Without it the
///    cycle is pre-empted at [`CONC_DEFER_CEILING_PCT`].
/// 2. **The start's growth hysteresis stops overriding a measured prediction.**
///    Once a cycle has been measured (or pre-empted), the initiating occupancy
///    `T` already leaves the predicted per-cycle growth before the floor; a
///    generation its last collection left at or above `T` then re-opens after
///    the minimum step `C/32` of growth instead of half the room to the floor
///    ([`ConcurrentStartPolicy::start_due_measured`]). Without it every cycle
///    after a pre-emption re-opens at the midpoint `(F + after) / 2`, whatever
///    growth the pre-emption measured, and is pre-empted again.
///
/// Default ON since the gce round (2026-09-29, triage of wave e1); `=0` restores
/// the gen r4w4 rule. Read per decision (a declared-flag lookup; decisions are
/// per young pause).
pub const GEN_CONC_PRECEDENCE_FLAG: &str = "CRATONVM_GEN_CONC_PRECEDENCE";

/// Is [`GEN_CONC_PRECEDENCE_FLAG`] on?
pub fn gen_conc_precedence_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(GEN_CONC_PRECEDENCE_FLAG)
}

/// gcd d9/e (2026-09-28) — `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` (default on since gce, 2026-09-29; token
/// `gen-conc-mark-tams-starts`): Phase-2 slices judge "is this an old-gen
/// object start the trace may mark" against the INITIAL MARK's object-start
/// snapshot instead of re-walking the generation after every promotion. See
/// [`ConcurrentMarker::phase2_object_starts`] for the soundness argument and
/// what it costs without the flag. Default ON since the gce round (2026-09-29);
/// `=0` restores the per-promotion re-walk.
pub const GEN_CONC_MARK_TAMS_STARTS_FLAG: &str = "CRATONVM_GEN_CONC_MARK_TAMS_STARTS";

/// Is [`GEN_CONC_MARK_TAMS_STARTS_FLAG`] on?
pub fn gen_conc_mark_tams_starts_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(GEN_CONC_MARK_TAMS_STARTS_FLAG)
}

/// gcd d9/e (2026-09-28) — why a STW old-gen collection ran through an open
/// concurrent cycle it could not defer to (the `[GC] conc_cycles:` split of
/// `concpol_stw_preempted`). See [`ConcurrentStartPolicy::preempt_cause`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StwPreemptCause {
    /// An old-gen allocation failed since the last collection: a real
    /// concurrent-mode failure (the cycle did not free room in time).
    AllocationFailure,
    /// No allocation failed, but the generation reached
    /// [`CONC_DEFER_CEILING_PCT`] (default policy only; never under
    /// [`GEN_CONC_PRECEDENCE_FLAG`]).
    Ceiling,
}

/// gen r4w4/concmark4 (2026-09-24) — which old-generation collection policy
/// the generational heap runs. Read from the process flags at each decision
/// (`CRATONVM_GC_NO_CONCURRENT_FIRST`; a tuning knob, not compatibility state).
///
/// # `Legacy` — the pre-2026-09-24 behaviour, kept as the A/B arm
///
/// One trigger, `OldGen::major_trigger_verdict` (the 75 % floor, with the
/// growth hysteresis only when `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` is set),
/// asked by both the STW old-gen collection inside the young pause and the
/// concurrent cycle right after it. Above the floor the two duplicate each
/// other, and with several mutators the next pause's STW collection makes the
/// open cycle stale
/// (`docs/internal/gc/gengc-r4w3-oldgen3-stw-major-and-concurrent-cycle-share-one-trigger-DONE-20260929.md`).
///
/// # `ConcurrentFirst` — the default
///
/// Two thresholds and a deferral, HotSpot G1's shape (IHOP below the
/// full-collection point):
///
/// * The concurrent cycle starts at an initiating occupancy `T` BELOW the STW
///   floor `F = 75 % of C` ([`ConcurrentStartPolicy::threshold`]):
///
///   ```text
///   T = clamp(F − (5/4 · G + C/32),  C · 20 %,  F)
///   ```
///
///   where `G` is the old-gen growth one cycle is predicted to see: promotion
///   rate × cycle duration, MEASURED as one number (old-gen bytes allocated
///   between a cycle's initial mark and its sweep's end), smoothed by an EWMA
///   (α = ½). Until the first cycle is measured `T = 45 % of C`. The product
///   is measured rather than composed from a rate and a duration because the
///   cycle runs on a mutator thread: the promotion rate DURING a cycle is not
///   the rate between cycles (single-threaded, it is zero), and a composed
///   `rate × duration` would start every single-threaded cycle far too early.
/// * The STW old-gen collection DEFERS to an open cycle unless it was
///   requested (`System.gc()`), an old-gen allocation has failed since the
///   last collection (the OOM ladder must never wait), or the generation is at
///   [`CONC_DEFER_CEILING_PCT`]. When it runs anyway the cycle could not
///   finish in time, and the predicted growth is widened to
///   `max(G, 2 · growth so far, C/8)` for the next start.
/// * The growth hysteresis is folded in: the concurrent START always uses it
///   (a cycle does not reopen over a generation its last collection left
///   above `T` until it has grown, [`ConcurrentStartPolicy::start_due`]), and
///   the STW arm's default is one match arm in [`Self::hysteresis`] — still
///   OFF, pending the orchestrator's A/B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldGenPolicy {
    Legacy,
    ConcurrentFirst,
}

impl OldGenPolicy {
    /// This process's policy.
    pub fn current() -> Self {
        Self::from_opt_out(crate::gc_flags().old_no_concurrent_first)
    }

    /// `CRATONVM_GC_NO_CONCURRENT_FIRST` set → [`Self::Legacy`].
    pub fn from_opt_out(no_concurrent_first: bool) -> Self {
        if no_concurrent_first {
            Self::Legacy
        } else {
            Self::ConcurrentFirst
        }
    }

    /// The STW trigger's effective growth hysteresis. An explicit
    /// `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` (`=1` on, `=0` off) wins; unset,
    /// each policy has its own default: OFF under `Legacy`, ON under
    /// `ConcurrentFirst` since gen r4w5 (2026-09-24).
    ///
    /// The flip's evidence (count-based, so host load cannot move it):
    /// `GenR4W5MajorCadenceProbe`, `--nojit -Xmx256m`, 545 young decisions.
    /// Hysteresis OFF: 518 STW majors (95 per 100 young), 517 back-to-back,
    /// all 518 freeing < 5 % of old gen, 2 concurrent cycles. Hysteresis ON:
    /// 0 STW majors, 89 concurrent cycles. Same checksum as HotSpot in both.
    ///
    /// **The hysteresis page's default flip is the `ConcurrentFirst` arm of
    /// this match** (`gengc-r4-oldgen-major-trigger-has-no-hysteresis`): the
    /// orchestrator measures the opt-in arms this wave, and a positive A/B
    /// makes that arm `true` — the legacy arm stays the historical OFF, an
    /// explicit `=0` still switches it off, and the same commit gives the
    /// `old-trigger-hysteresis` row in `types/src/flag_groups.rs` its
    /// `off_word: Some("0")` (so `CRATONVM_GC=-old-trigger-hysteresis` writes
    /// the explicit off instead of unsetting the key). It was NOT flipped on intuition: it
    /// changes when a generation parked above 75 % is collected, and
    /// `a_finalizable_promoted_by_resurrection_survives_the_same_cycle_major`
    /// (gen_heap) stages exactly the back-to-back major it would suppress —
    /// that test has to set `=0` when the flip lands.
    pub fn hysteresis(self, explicit: Option<bool>) -> bool {
        explicit.unwrap_or(match self {
            Self::Legacy => false,
            Self::ConcurrentFirst => true,
        })
    }

    /// Short name, for the census line.
    pub fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::ConcurrentFirst => "concurrent-first",
        }
    }
}

/// gen r4w4/concmark4 — the concurrent-start policy's pure formulas (no state,
/// so the tests pin them without a heap). See [`OldGenPolicy`] for the design.
pub struct ConcurrentStartPolicy;

impl ConcurrentStartPolicy {
    /// The STW trigger's floor in bytes — the same integer arithmetic as
    /// `OldGen::major_trigger_verdict`.
    #[inline]
    pub fn stw_floor(capacity: usize) -> usize {
        capacity * OLD_STW_FLOOR_PCT / 100
    }

    /// The initiating occupancy `T`, in bytes.
    ///
    /// * `fixed_pct` (`CRATONVM_GC_CONC_START_PERCENT`): `C · pct %`, the
    ///   percentage clamped to `[CONC_START_MIN_PCT, OLD_STW_FLOOR_PCT]`;
    /// * no measured cycle yet: `C · CONC_START_INITIAL_PCT %`;
    /// * otherwise `clamp(F − (G + G/4 + C/32), C · CONC_START_MIN_PCT %, F)`.
    pub fn threshold(
        capacity: usize,
        predicted_growth: Option<u64>,
        fixed_pct: Option<usize>,
    ) -> usize {
        let floor = Self::stw_floor(capacity);
        let min = capacity * CONC_START_MIN_PCT / 100;
        if let Some(pct) = fixed_pct {
            return capacity * pct.clamp(CONC_START_MIN_PCT, OLD_STW_FLOOR_PCT) / 100;
        }
        let Some(g) = predicted_growth else {
            return capacity * CONC_START_INITIAL_PCT / 100;
        };
        let g = usize::try_from(g).unwrap_or(usize::MAX);
        let buffer = g.saturating_add(g / 4).saturating_add(capacity / 32);
        // `clamp` cannot panic: `C·20/100 <= C·75/100` for every `C`.
        floor.saturating_sub(buffer).clamp(min, floor)
    }

    /// Should a concurrent cycle open now?
    ///
    /// Due once `used >= threshold` — immediately if the last old-gen
    /// collection (of either kind) left the generation BELOW the threshold, or
    /// if there has been none. If it left the generation at or above it (a
    /// live set that no collection can bring under `T`), a new cycle waits
    /// until the generation has grown by `max(C/32, (F − after)/2)`: half the
    /// room left before the STW floor, but never less than 1/32 of the
    /// generation. That is the growth hysteresis applied to the concurrent
    /// start; it re-opens a cycle before the STW floor whenever there is room
    /// for one (`C/32 < C/16`, the STW arm's minimum step).
    pub fn start_due(
        capacity: usize,
        used: usize,
        used_after_last: Option<usize>,
        threshold: usize,
    ) -> bool {
        if capacity == 0 || used == 0 || used < threshold {
            return false;
        }
        match used_after_last {
            None => true,
            Some(after) if after < threshold => true,
            Some(after) => {
                let grown = used.saturating_sub(after);
                let headroom = Self::stw_floor(capacity).saturating_sub(after);
                grown >= (capacity / 32).max(headroom / 2)
            }
        }
    }

    /// With a cycle open, does the STW old-gen collection this verdict would
    /// run DEFER to it? Only an occupancy verdict defers (`Occupancy`, or
    /// `WouldSuppress` on a no-hysteresis run), only below
    /// [`CONC_DEFER_CEILING_PCT`], and only when NO old-gen allocation has
    /// failed since the last collection (`failures_since_last`). The failure
    /// test is separate from the verdict on purpose: the verdict reports
    /// `Occupancy` ahead of `AllocationFailure` whenever the generation has
    /// grown enough, so a failure would otherwise hide behind an occupancy
    /// verdict and be deferred. `Requested` never defers. A failure is how a
    /// genuinely full generation reaches the `OutOfMemoryError` ladder.
    pub fn stw_defers(
        capacity: usize,
        used: usize,
        verdict: MajorTrigger,
        failures_since_last: u64,
    ) -> bool {
        Self::stw_defers_with(capacity, used, verdict, failures_since_last, false)
    }

    /// gcd d9/e — [`Self::stw_defers`] with the ceiling made optional:
    /// `defer_to_failure` (`CRATONVM_GEN_CONC_PRECEDENCE`) defers an occupancy
    /// verdict at ANY occupancy while no old-gen allocation has failed, so the
    /// only non-requested reason left to run the STW collection through an
    /// open cycle is a real failure. `false` is exactly [`Self::stw_defers`].
    pub fn stw_defers_with(
        capacity: usize,
        used: usize,
        verdict: MajorTrigger,
        failures_since_last: u64,
        defer_to_failure: bool,
    ) -> bool {
        failures_since_last == 0
            && matches!(
                verdict,
                MajorTrigger::Occupancy | MajorTrigger::WouldSuppress
            )
            && (defer_to_failure || used < capacity * CONC_DEFER_CEILING_PCT / 100)
    }

    /// gcd d9/e — for a running verdict that did NOT defer
    /// ([`Self::stw_defers_with`] answered `false`), why not: a failed old-gen
    /// allocation since the last collection, else the ceiling. (An occupancy
    /// verdict below the ceiling with no failure always defers, and every
    /// other running verdict of the peek — `AllocationFailure` — implies a
    /// failure, so these two partition the pre-emptions.)
    pub fn preempt_cause(failures_since_last: u64) -> StwPreemptCause {
        if failures_since_last > 0 {
            StwPreemptCause::AllocationFailure
        } else {
            StwPreemptCause::Ceiling
        }
    }

    /// gcd d9/e — [`Self::start_due`] once the start policy has MEASURED a
    /// cycle (or a pre-emption widened its prediction), under
    /// `CRATONVM_GEN_CONC_PRECEDENCE`.
    ///
    /// `threshold` then already leaves `5/4 · G + C/32` of predicted growth
    /// before the STW floor. A generation its last collection left at or
    /// above it (`after >= T`, e.g. the floating garbage of the last cycle, or
    /// a live set above a lowered `T`) is past the point where a cycle can be
    /// predicted to finish before the floor, so waiting for half the room to
    /// the floor — [`Self::start_due`]'s rule — only guarantees the next
    /// pre-emption. It re-opens after the minimum step `C/32` instead (the
    /// guard against back-to-back cycles over an unchanged generation). Below
    /// `T`, or with no previous collection, it is [`Self::start_due`].
    ///
    /// Where the prediction is small (single-threaded programs: the cycle runs
    /// on the only mutator, so `G ≈ 0` and `T ≈ F − C/32`) the two rules agree:
    /// `after >= T` leaves `(F − after) / 2 <= C/64 < C/32`.
    pub fn start_due_measured(
        capacity: usize,
        used: usize,
        used_after_last: Option<usize>,
        threshold: usize,
    ) -> bool {
        if capacity == 0 || used == 0 || used < threshold {
            return false;
        }
        match used_after_last {
            None => true,
            Some(after) if after < threshold => true,
            Some(after) => used.saturating_sub(after) >= capacity / 32,
        }
    }
}

/// gen r4w4/concmark4 — bytes ever allocated in `old_gen`, as far as its
/// trigger can tell: live `used()` plus everything old-gen collections
/// reported reclaiming (`OldGenTriggerStats::freed_bytes`). Its growth between
/// two instants is the old-gen allocation (promotion plus direct) between
/// them. A free no collection reports (a promotion buffer's unused tail) only
/// makes a sample smaller; callers `saturating_sub`.
pub fn old_gen_allocated_total(old_gen: &OldGen) -> u64 {
    (old_gen.used() as u64).saturating_add(old_gen.trigger_stats().freed_bytes)
}

/// gen r4w4/concmark4 — one completed concurrent cycle, as the policy saw it.
/// Read with [`ConcurrentGcState::last_cycle_report`] (the driver's
/// `-Xlog:gc` / `--verbose:gc` line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrentCycleReport {
    /// 1-based count of completed cycles on this VM.
    pub cycle: u64,
    pub capacity: usize,
    /// Old-gen `used()` at the initial mark and at the sweep's end.
    pub used_at_open: usize,
    pub used_at_end: usize,
    /// What the sweep reclaimed.
    pub freed_bytes: usize,
    /// Old-gen bytes allocated while the cycle ran (the policy's sample).
    pub growth_during: u64,
    /// Initial mark to the sweep's end, wall clock.
    pub duration_us: u64,
    /// The initiating occupancy the next cycle will use, in bytes.
    pub next_threshold: usize,
}

impl ConcurrentCycleReport {
    /// The `-Xlog:gc` (`gc,info`) line, HotSpot-shaped after G1's
    /// `Concurrent Mark Cycle`: `GC(c3) Concurrent Mark Cycle 90M->40M(128M)
    /// 12.345ms` — old-generation occupancy, since that is what the cycle
    /// collects.
    pub fn xlog_line(&self) -> String {
        const M: usize = 1024 * 1024;
        format!(
            "GC(c{}) Concurrent Mark Cycle {}M->{}M({}M) {:.3}ms",
            self.cycle,
            self.used_at_open / M,
            self.used_at_end / M,
            self.capacity / M,
            self.duration_us as f64 / 1000.0,
        )
    }

    /// The `--verbose:gc` line: every field, `concyc_`-prefixed keys.
    pub fn verbose_line(&self) -> String {
        format!(
            "[GC] concurrent-cycle: concyc_n={} concyc_used_open={} concyc_used_end={} \
             concyc_capacity={} concyc_freed={} concyc_growth_during={} concyc_us={} \
             concyc_next_start={} concyc_next_start_pct={}",
            self.cycle,
            self.used_at_open,
            self.used_at_end,
            self.capacity,
            self.freed_bytes,
            self.growth_during,
            self.duration_us,
            self.next_threshold,
            self.next_threshold * 100 / self.capacity.max(1),
        )
    }
}

/// The initial mark's baseline for the open cycle's growth sample.
#[derive(Debug, Clone, Copy)]
struct CycleBaseline {
    at: std::time::Instant,
    allocated_total: u64,
    used: usize,
}

/// gen r4w4/concmark4 — the adaptive start policy's memory, behind
/// `ConcurrentGcState::policy`.
#[derive(Debug, Default)]
struct StartPolicyState {
    /// EWMA (α = ½) of old-gen growth per completed cycle, widened by
    /// pre-emptions. `None` until the first cycle completes or is pre-empted.
    growth_ewma: Option<u64>,
    /// EWMA of cycle duration (reporting only; the formula uses the growth).
    duration_ewma_us: Option<u64>,
    /// The open cycle's initial-mark baseline.
    open: Option<CycleBaseline>,
    /// The open cycle has already fed a pre-emption sample (several STW
    /// collections inside one cycle widen the buffer once, and its end sample
    /// then does not average the widening away).
    preempt_sampled: bool,
    last: Option<ConcurrentCycleReport>,
    /// gen r4w6/concsvc6 — when the concurrent start FIRST said "due" with no
    /// cycle claimed since; `None` while it says "not due". Consumed by the
    /// next initial mark (the trigger-to-start latency sample). See
    /// [`ConcurrentGcState::note_start_verdict`].
    due_since: Option<std::time::Instant>,
    /// gcd d9/e — when the open cycle's remark began (the end of its marking,
    /// the start of its sweep), for the `[GC] conc_cycles:` phase times.
    remark_at: Option<std::time::Instant>,
}

/// α = ½ exponential moving average in integers.
#[inline]
fn ewma_half(prev: Option<u64>, sample: u64) -> u64 {
    match prev {
        None => sample,
        Some(p) => p / 2 + sample / 2 + (p & sample & 1),
    }
}

impl ConcurrentGcState {
    /// gen r4w4/concmark4 — is a cycle past its initial mark and not yet
    /// finished? (`InitialMark` alone is a driver that has claimed the state
    /// but may still lose its pause, so it is NOT a cycle the STW collection
    /// can defer to.)
    pub fn cycle_in_progress(&self) -> bool {
        matches!(
            self.phase(),
            ConcurrentGcPhase::ConcurrentMark
                | ConcurrentGcPhase::Remark
                | ConcurrentGcPhase::ConcurrentSweep
        )
    }

    /// gen r4w4/concmark4 — the initial mark opened a cycle: record its
    /// baseline. Called by `ConcurrentMarker::initial_mark`.
    pub fn note_cycle_open(&self, allocated_total: u64, used: usize) {
        let now = std::time::Instant::now();
        let mut p = self.policy.lock();
        p.open = Some(CycleBaseline {
            at: now,
            allocated_total,
            used,
        });
        p.preempt_sampled = false;
        p.remark_at = None;
        // gen r4w6/concsvc6: the trigger-to-start latency sample.
        if let Some(due) = p.due_since.take() {
            let us =
                u64::try_from(now.saturating_duration_since(due).as_micros()).unwrap_or(u64::MAX);
            let c = &self.census;
            c.trigger_to_start_samples.fetch_add(1, Ordering::Relaxed);
            c.trigger_to_start_total_us.fetch_add(us, Ordering::Relaxed);
            c.trigger_to_start_max_us.fetch_max(us, Ordering::Relaxed);
        }
    }

    /// gen r4w6/concsvc6 (2026-09-24) — one answer of the concurrent start
    /// trigger, for the trigger-to-start latency census
    /// (`concdrv_trigger_to_start_*`).
    ///
    /// `due` records the FIRST instant the start said "due" (later "due"
    /// answers keep it), and "not due" forgets it (a STW collection brought
    /// the generation back under the start, so no cycle is owed any more).
    /// The next initial mark turns the recorded instant into one sample
    /// ([`Self::note_cycle_open`]). An answer given while a cycle is claimed
    /// or open says nothing about the NEXT start and is ignored: with a
    /// service attached, mutators still ask the trigger while the service's
    /// cycle runs.
    ///
    /// What the sample measures depends on the driver: inline, the young
    /// epilogue that found the start due runs the initial mark at once, so the
    /// latency is the marker's set-up plus the pause request; with the service
    /// thread it adds the hand-off wake. A start found due only by the
    /// service's PERIODIC check, or by a STW decision (`GenerationalHeap::
    /// major_trigger_decision`), is measured from that check. Called with the
    /// old-gen lock held or not; takes the policy lock (leaf).
    pub fn note_start_verdict(&self, due: bool) {
        // gen r5w1/refs5: who asks, and how often the answer is yes — the
        // census that tells "no door asked" from "never due".
        self.census.start_verdicts.fetch_add(1, Ordering::Relaxed);
        if due {
            self.census
                .start_verdicts_due
                .fetch_add(1, Ordering::Relaxed);
        }
        if self.phase() != ConcurrentGcPhase::Idle {
            return;
        }
        let mut p = self.policy.lock();
        if due {
            if p.due_since.is_none() {
                p.due_since = Some(std::time::Instant::now());
            }
        } else {
            p.due_since = None;
        }
    }

    /// gen r4w4/concmark4 — the open cycle ended without a sweep (abandoned,
    /// or its remark refused the sweep): no sample, forget the baseline.
    pub fn note_cycle_dropped(&self) {
        let mut p = self.policy.lock();
        p.open = None;
        p.preempt_sampled = false;
        p.remark_at = None;
    }

    /// gcd d9/e — the open cycle's remark pause began: its marking took
    /// `now − initial mark` (`[GC] conc_cycles: conccyc_mark_ms_*`), and its
    /// sweep is timed from here. Called by `ConcurrentMarker::remark_begin`
    /// (once per cycle: a lost remark pause never reaches it). Leaf lock.
    pub fn note_remark_reached(&self) {
        let now = std::time::Instant::now();
        let mut p = self.policy.lock();
        let Some(open) = p.open else {
            return;
        };
        p.remark_at = Some(now);
        drop(p);
        let us =
            u64::try_from(now.saturating_duration_since(open.at).as_micros()).unwrap_or(u64::MAX);
        self.census.mark_us_total.fetch_add(us, Ordering::Relaxed);
        self.census.mark_us_max.fetch_max(us, Ordering::Relaxed);
    }

    /// gcd d9/e — a driver claimed the cycle (`try_open_cycle`) but lost its
    /// initial-mark pause, so no cycle opened (`conccyc_open_lost`).
    pub fn note_open_lost(&self) {
        self.census.open_lost.fetch_add(1, Ordering::Relaxed);
    }

    /// gcd d9/e — an OPENED cycle was abandoned before its sweep ended
    /// (`ConcurrentMarker::abort_cycle`; `conccyc_abandoned`).
    pub fn note_cycle_abandoned(&self) {
        self.census.abandoned.fetch_add(1, Ordering::Relaxed);
    }

    /// gcd d9/e — a cycle's sweep freed nothing because it was never
    /// authorised, or its snapshot was already stale at the first slice
    /// (`conccyc_swept_nothing`).
    pub fn note_swept_nothing(&self) {
        self.census.swept_nothing.fetch_add(1, Ordering::Relaxed);
    }

    /// gcd d9/e — a REQUESTED STW old-gen collection ran while a cycle was
    /// open (`conccyc_requested_through_cycle`).
    pub fn note_requested_through_cycle(&self) {
        self.census
            .requested_through_cycle
            .fetch_add(1, Ordering::Relaxed);
    }

    /// gen r4w4/concmark4 — the open cycle's sweep ended: feed the growth
    /// sample, and record the cycle's report. Called by
    /// `ConcurrentMarker::finish_sweep` under the old-gen lock.
    pub fn note_cycle_end(
        &self,
        allocated_total: u64,
        used: usize,
        capacity: usize,
        freed_bytes: usize,
    ) {
        let mut p = self.policy.lock();
        // No baseline: a sweep over a snapshot no `initial_mark` on this state
        // took (a hand-built test marker). Nothing to sample or report.
        let Some(base) = p.open.take() else {
            return;
        };
        let n = self
            .census
            .cycles_completed
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let growth = allocated_total.saturating_sub(base.allocated_total);
        let duration_us = u64::try_from(base.at.elapsed().as_micros()).unwrap_or(u64::MAX);
        // gcd d9/e: the sweep's share (remark to here), for `[GC] conc_cycles:`.
        if let Some(remark_at) = p.remark_at.take() {
            let sweep_us = u64::try_from(remark_at.elapsed().as_micros()).unwrap_or(u64::MAX);
            self.census
                .sweep_us_total
                .fetch_add(sweep_us, Ordering::Relaxed);
        }
        if !p.preempt_sampled {
            p.growth_ewma = Some(ewma_half(p.growth_ewma, growth));
        }
        p.duration_ewma_us = Some(ewma_half(p.duration_ewma_us, duration_us));
        p.preempt_sampled = false;
        let next_threshold = ConcurrentStartPolicy::threshold(
            capacity,
            p.growth_ewma,
            crate::gc_flags().conc_start_percent,
        );
        // gen r4w5/concmark5: the growth poll's hint follows the new threshold.
        self.service
            .start_hint
            .store(next_threshold, Ordering::Relaxed);
        p.last = Some(ConcurrentCycleReport {
            cycle: n,
            capacity,
            used_at_open: base.used,
            used_at_end: used,
            freed_bytes,
            growth_during: growth,
            duration_us,
            next_threshold,
        });
    }

    /// gen r4w4/concmark4 — a young pause's STW old-gen collection deferred to
    /// the open cycle.
    pub fn note_stw_deferred(&self) {
        self.census.stw_deferred.fetch_add(1, Ordering::Relaxed);
    }

    /// gen r4w4/concmark4 — a STW old-gen collection is running while a cycle
    /// is open, for a reason that does not defer (a failure or the ceiling):
    /// the cycle started too late to finish. Widen the predicted growth to
    /// `max(G, 2 · growth so far, C/8)`, once per cycle.
    pub fn note_stw_preempted(&self, allocated_total: u64, capacity: usize) {
        self.census.stw_preempted.fetch_add(1, Ordering::Relaxed);
        let mut p = self.policy.lock();
        let so_far = p
            .open
            .map_or(0, |b| allocated_total.saturating_sub(b.allocated_total));
        // gcd d9/e: the growth the cycle had seen when it was pre-empted, for
        // `[GC] conc_cycles: conccyc_preempt_growth_max_pct` (every
        // pre-emption, not only the one that widens the prediction).
        if capacity > 0 {
            let permille = so_far.saturating_mul(1000) / capacity as u64;
            self.census
                .preempt_growth_max_permille
                .fetch_max(permille, Ordering::Relaxed);
        }
        if p.preempt_sampled {
            return;
        }
        let widened = so_far.saturating_mul(2).max((capacity / 8) as u64);
        p.growth_ewma = Some(p.growth_ewma.map_or(widened, |g| g.max(widened)));
        p.preempt_sampled = true;
        // gen r4w5/concmark5: the growth poll's hint follows the lowered start.
        self.service.start_hint.store(
            ConcurrentStartPolicy::threshold(
                capacity,
                p.growth_ewma,
                crate::gc_flags().conc_start_percent,
            ),
            Ordering::Relaxed,
        );
    }

    /// gcd d9/e (2026-09-28) — [`Self::note_stw_preempted`] with the reason
    /// the STW collection could not defer ([`StwPreemptCause`]) and the phase
    /// the open cycle was in, for the `[GC] conc_cycles:` split. Called by
    /// `GenerationalHeap::major_trigger_decision_policy` under the old-gen
    /// lock; the prediction update is exactly [`Self::note_stw_preempted`]'s.
    pub fn note_stw_preempted_why(
        &self,
        cause: StwPreemptCause,
        allocated_total: u64,
        capacity: usize,
    ) {
        let c = &self.census;
        match cause {
            StwPreemptCause::AllocationFailure => {
                c.preempt_alloc_failure.fetch_add(1, Ordering::Relaxed);
            }
            StwPreemptCause::Ceiling => {
                c.preempt_ceiling.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.phase() == ConcurrentGcPhase::ConcurrentSweep {
            c.preempt_in_sweep.fetch_add(1, Ordering::Relaxed);
        } else {
            c.preempt_in_mark.fetch_add(1, Ordering::Relaxed);
        }
        self.note_stw_preempted(allocated_total, capacity);
    }

    /// gen r4w4/concmark4 — the initiating occupancy now, in bytes.
    pub fn concurrent_start_threshold(&self, capacity: usize) -> usize {
        let growth = self.policy.lock().growth_ewma;
        let t = ConcurrentStartPolicy::threshold(
            capacity,
            growth,
            crate::gc_flags().conc_start_percent,
        );
        // gen r4w5/concmark5: publish it for the direct-old-allocation poll's
        // lock-free pre-check (`service_growth_poll_armed`). A stale hint only
        // costs one extra (or one missed-until-the-period) full check; the
        // full check is what decides.
        self.service.start_hint.store(t, Ordering::Relaxed);
        t
    }

    /// gen r4w4/concmark4 — should a concurrent cycle open now? See
    /// [`ConcurrentStartPolicy::start_due`].
    pub fn concurrent_start_due(
        &self,
        capacity: usize,
        used: usize,
        used_after_last: Option<usize>,
    ) -> bool {
        let t = self.concurrent_start_threshold(capacity);
        // gcd d9/e: under `CRATONVM_GEN_CONC_PRECEDENCE`, a MEASURED
        // prediction (not a pinned `CRATONVM_GC_CONC_START_PERCENT`) is no
        // longer overridden by the growth hysteresis; see
        // `ConcurrentStartPolicy::start_due_measured`.
        let measured = gen_conc_precedence_enabled()
            && crate::gc_flags().conc_start_percent.is_none()
            && self.policy.lock().growth_ewma.is_some();
        let due = if measured {
            ConcurrentStartPolicy::start_due_measured(capacity, used, used_after_last, t)
        } else {
            ConcurrentStartPolicy::start_due(capacity, used, used_after_last, t)
        };
        // gen r4w6/concsvc6: every concurrent-first start verdict feeds the
        // trigger-to-start census.
        self.note_start_verdict(due);
        due
    }

    /// gen r4w6/concsvc6 — a STW old-gen collection runs through an open cycle
    /// only because a fragmentation compaction was armed: counted, and NOT a
    /// pre-emption sample (the cycle did not start too late; occupancy alone
    /// would have deferred). See [`ConcurrentCycleCensus::stw_frag_preempted`].
    pub fn note_stw_fragmentation_preempt(&self) {
        self.census
            .stw_frag_preempted
            .fetch_add(1, Ordering::Relaxed);
    }

    /// gen r4w4/concmark4 — the last completed cycle, if any.
    pub fn last_cycle_report(&self) -> Option<ConcurrentCycleReport> {
        self.policy.lock().last
    }

    /// gen r4w4/concmark4 — `(predicted growth per cycle, cycle duration µs)`,
    /// both EWMAs; `None` before the first measurement.
    pub fn policy_estimates(&self) -> (Option<u64>, Option<u64>) {
        let p = self.policy.lock();
        (p.growth_ewma, p.duration_ewma_us)
    }

    /// gen r4w4/concmark4 — frozen-thread objects the initial mark scanned.
    fn note_frozen_objects_scanned(&self, n: u64) {
        self.census
            .frozen_objects_scanned
            .fetch_add(n, Ordering::Relaxed);
    }

    /// gen r4w4/concmark4 — grey objects the local stack spilled.
    fn note_mark_local_spills(&self, n: u64) {
        if n != 0 {
            self.census
                .mark_local_spills
                .fetch_add(n, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// gen r4w5/concmark5 — the concurrent-cycle service thread
// ---------------------------------------------------------------------------

/// gen r4w5/concmark5 (2026-09-24) — how long the service thread sleeps
/// between two of its OWN trigger checks when nothing wakes it (milliseconds).
///
/// The period is the backstop, not the mechanism: a young collection's
/// epilogue hands a due cycle off at once ([`ConcurrentGcState::hand_off_to_service`])
/// and a direct old-gen allocation that crosses the start threshold signals at
/// once ([`ConcurrentGcState::signal_old_gen_growth`]). The period catches old-gen
/// growth through any path that has no poll site. One check is the old-gen
/// lock plus the policy lock, so 50 checks a second cost nothing measurable.
pub const GEN_CONC_SERVICE_PERIOD_MS: u64 = 20;

/// The service's periodic re-check backs off after cycles that OPENED and did
/// not complete (a lost remark, a stale epoch): `2^n − 1` periodic wakes are
/// skipped after `n` such cycles in a row, `n` capped here (63 periods,
/// about 1.3 s at the default period). A requested wake is never skipped: it
/// is the young-collection cadence the inline driver has always had.
const SERVICE_BACKOFF_MAX_SHIFT: u32 = 6;

/// gen r4w5/concmark5 — periodic wakes the service skips after `failures`
/// consecutive cycles that opened and did not complete. `0` → none.
pub fn service_backoff_skips(failures: u32) -> u64 {
    if failures == 0 {
        0
    } else {
        (1u64 << failures.min(SERVICE_BACKOFF_MAX_SHIFT)) - 1
    }
}

/// gen r4w5/concmark5 — why [`ConcurrentGcState::service_wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentServiceWake {
    /// A mutator found the cycle due (a young collection's epilogue, the
    /// `System.gc()` door) and handed it to the service instead of running it.
    HandedOff,
    /// A direct old-gen allocation (humongous array, young-spill object) found
    /// the cycle due; no young collection was involved.
    OldGenGrowth,
    /// The period elapsed with no request: the service asks the trigger itself.
    Periodic,
    /// The VM is shutting down: leave the loop.
    Shutdown,
}

/// The service's mutable half, behind one leaf mutex.
#[derive(Default)]
struct ServiceSlot {
    /// The attached service thread, if any.
    thread: Option<std::thread::ThreadId>,
    /// The first unserved request since the service last woke.
    pending: Option<ConcurrentServiceWake>,
    /// Set once by [`ConcurrentGcState::shutdown_service`]; never cleared.
    shutdown: bool,
}

/// gen r4w5/concmark5 — see [`ConcurrentGcState::serve`].
#[derive(Default)]
struct ServiceCells {
    /// A LEAF lock: taken under the old-gen lock by the direct-allocation
    /// poll, and nothing is acquired while it is held.
    slot: Mutex<ServiceSlot>,
    wake: Condvar,
    /// Lock-free mirrors for the hot pre-checks: a service is attached; a
    /// request is already pending (so a second poller need not lock).
    attached: AtomicBool,
    pending: AtomicBool,
    /// gcd d1/c — the lock-free mirror of `ServiceSlot::shutdown`
    /// ([`ConcurrentGcState::service_shutting_down`]). Set once, never cleared.
    shut_down: AtomicBool,
    /// The last initiating occupancy the policy computed, in bytes; `0` until
    /// the first computation (which makes the first poll a full check).
    start_hint: AtomicUsize,
    /// gce e1/c — with NO service attached, the direct-old-allocation poll
    /// latches a start request here instead of waking a service; the VM runs
    /// the driver inline at the requesting thread's next allocation slow path
    /// ([`ConcurrentGcState::take_inline_start_request`]). See
    /// `gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924`
    /// item 1.
    inline_start: AtomicBool,
}

/// gen r4w5/concmark5 — what the VM supplies to [`ConcurrentGcState::serve`].
///
/// The collector cannot do any of these itself: a stop-the-world pause in this
/// VM can only be requested by a thread that is in the thread registry, and
/// only the VM can move a thread in and out of the GC-blocked region.
pub trait ConcurrentServiceHooks {
    /// Become NOT counted by pauses (enter the GC-blocked region). Called
    /// before the service blocks on its condvar, so an idle service never
    /// holds a pause up.
    fn enter_idle(&mut self);
    /// Become a counted mutator again (leave the blocked region, waiting out a
    /// pause in progress). Called before [`Self::run_cycle`].
    fn leave_idle(&mut self);
    /// The concurrent cycle's trigger, asked on a PERIODIC wake while still
    /// idle. The VM answers `GenerationalHeap::concurrent_cycle_due()` — on the
    /// service thread that is the policy's verdict, never a hand-off. It takes
    /// only heap locks, which is safe from the blocked region.
    fn cycle_due(&mut self) -> bool;
    /// Run one cycle attempt as a counted mutator — the VM's existing driver
    /// (`maybe_concurrent_gc_at`), which re-asks the trigger, requests the
    /// initial-mark and remark pauses itself, polls safepoints between Phase-2
    /// and sweep slices, and returns when the cycle completed, was abandoned,
    /// or did not open.
    fn run_cycle(&mut self);
}

/// gen r4w5/concmark5 — the attached service thread's registration; dropping
/// it (a normal exit or an unwind) detaches the service, after which mutators
/// run due cycles inline again, exactly as before a service existed.
#[must_use = "dropping the attachment detaches the service thread"]
pub struct ConcurrentServiceAttachment<'s> {
    state: &'s ConcurrentGcState,
}

impl Drop for ConcurrentServiceAttachment<'_> {
    fn drop(&mut self) {
        self.state.detach_service_thread();
    }
}

impl ConcurrentGcState {
    /// gen r4w5/concmark5 — `CRATONVM_GEN_CONC_SERVICE_THREAD`: should the VM
    /// start a concurrent-cycle service thread for its generational heap?
    /// Opt-in; unset, no service is attached and every path below is inert.
    pub fn service_enabled() -> bool {
        crate::gc_flags().gen_conc_service_thread
    }

    /// Register the CURRENT thread as this VM's concurrent-cycle service.
    /// `None` if one is already attached or the service was shut down.
    pub fn attach_service_thread(&self) -> Option<ConcurrentServiceAttachment<'_>> {
        let mut slot = self.service.slot.lock();
        if slot.thread.is_some() || slot.shutdown {
            return None;
        }
        slot.thread = Some(std::thread::current().id());
        slot.pending = None;
        self.service.pending.store(false, Ordering::Relaxed);
        self.service.attached.store(true, Ordering::Release);
        Some(ConcurrentServiceAttachment { state: self })
    }

    fn detach_service_thread(&self) {
        let mut slot = self.service.slot.lock();
        self.service.attached.store(false, Ordering::Release);
        slot.thread = None;
        slot.pending = None;
        self.service.pending.store(false, Ordering::Relaxed);
    }

    /// Is a service thread attached? One Acquire load.
    #[inline]
    pub fn service_attached(&self) -> bool {
        self.service.attached.load(Ordering::Acquire)
    }

    /// Is the calling thread the attached service thread?
    pub fn on_service_thread(&self) -> bool {
        self.service_attached()
            && self.service.slot.lock().thread == Some(std::thread::current().id())
    }

    /// A caller found the concurrent cycle DUE. With a service attached and
    /// the caller not being it, post a [`ConcurrentServiceWake::HandedOff`]
    /// request and return `true`: the caller must NOT run the cycle itself.
    /// `false` (no service, or the caller is the service): run it as before.
    pub fn hand_off_to_service(&self) -> bool {
        if !self.service_attached() {
            return false;
        }
        let me = std::thread::current().id();
        let mut slot = self.service.slot.lock();
        match slot.thread {
            None => return false,
            Some(t) if t == me => return false,
            Some(_) => {}
        }
        // gen r5w5/conc9: a service told to stop still reads as attached until
        // its loop returns and detaches, but `service_wait` answers `Shutdown`
        // before it looks at `pending` and the detach then clears it, so a
        // request posted now was silently dropped (VM teardown runs pending
        // finalizers, which allocate). The caller runs the cycle itself, as
        // with no service.
        if slot.shutdown {
            return false;
        }
        self.census.service_handoffs.fetch_add(1, Ordering::Relaxed);
        self.post_service_request(&mut slot, ConcurrentServiceWake::HandedOff);
        true
    }

    /// The direct-old-allocation poll's lock-free pre-check: a service is
    /// attached, nothing is pending, no cycle is open, and `used` has reached
    /// the last published start threshold. Only then is the full check
    /// (`concurrent_start_due`, policy lock) worth taking.
    #[inline]
    pub fn service_growth_poll_armed(&self, used: usize) -> bool {
        self.service_attached()
            && !self.service.pending.load(Ordering::Relaxed)
            && self.phase() == ConcurrentGcPhase::Idle
            && used >= self.service.start_hint.load(Ordering::Relaxed)
    }

    /// gce e1/c — the direct-old-allocation poll's pre-check when NO service
    /// is attached: no request is latched yet, no cycle is open, and `used`
    /// has reached the last published start threshold. The service arm's
    /// twin ([`Self::service_growth_poll_armed`]); four relaxed loads.
    #[inline]
    pub fn inline_growth_poll_armed(&self, used: usize) -> bool {
        !self.service_attached()
            && !self.service.inline_start.load(Ordering::Relaxed)
            && self.phase() == ConcurrentGcPhase::Idle
            && used >= self.service.start_hint.load(Ordering::Relaxed)
    }

    /// gce e1/c — a direct old-gen allocation found the cycle due and no
    /// service is attached: latch the request for the VM's allocation slow
    /// path (it cannot run here: the caller holds the old-gen lock and is not
    /// at a point where the thread may take part in a pause). Returns `true`
    /// if this call set the latch.
    pub fn latch_inline_start_request(&self) -> bool {
        let set = !self.service.inline_start.swap(true, Ordering::Relaxed);
        if set {
            // gce e2/c: `[GC] conc_cycles: conccyc_inline_start_latched`.
            self.census
                .inline_start_latched
                .fetch_add(1, Ordering::Relaxed);
        }
        set
    }

    /// gce e1/c — is an inline start request latched? One relaxed load: the
    /// VM's per-allocation poll reads this before it pays the `swap` of
    /// [`Self::take_inline_start_request`].
    #[inline]
    pub fn inline_start_requested(&self) -> bool {
        self.service.inline_start.load(Ordering::Relaxed)
    }

    /// gce e1/c — consume the latched request (`true` once per latch). The
    /// caller then runs its driver (`maybe_concurrent_gc_at`), which re-asks
    /// the trigger, so a request gone stale (another door's cycle, a STW
    /// major in between) opens nothing.
    pub fn take_inline_start_request(&self) -> bool {
        let taken = self.service.inline_start.load(Ordering::Relaxed)
            && self.service.inline_start.swap(false, Ordering::Relaxed);
        if taken {
            // gce e2/c: `[GC] conc_cycles: conccyc_inline_start_taken`.
            self.census
                .inline_start_taken
                .fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// A direct old-gen allocation found the cycle due: wake the service.
    /// `false` if no service is attached.
    pub fn signal_old_gen_growth(&self) -> bool {
        let mut slot = self.service.slot.lock();
        if slot.thread.is_none() {
            return false;
        }
        self.census
            .service_growth_signals
            .fetch_add(1, Ordering::Relaxed);
        self.post_service_request(&mut slot, ConcurrentServiceWake::OldGenGrowth);
        true
    }

    fn post_service_request(&self, slot: &mut ServiceSlot, why: ConcurrentServiceWake) {
        if slot.pending.is_none() {
            slot.pending = Some(why);
        }
        self.service.pending.store(true, Ordering::Relaxed);
        self.service.wake.notify_one();
    }

    /// Block until a request, the period, or shutdown. A request posted while
    /// the service was busy is returned at once (it is kept in the slot, so no
    /// wake is lost between two waits).
    pub fn service_wait(&self, period: std::time::Duration) -> ConcurrentServiceWake {
        let mut slot = self.service.slot.lock();
        if !slot.shutdown && slot.pending.is_none() {
            // A spurious wake-up reads as a periodic one: harmless.
            let _ = self.service.wake.wait_for(&mut slot, period);
        }
        if slot.shutdown {
            return ConcurrentServiceWake::Shutdown;
        }
        match slot.pending.take() {
            Some(why) => {
                self.service.pending.store(false, Ordering::Relaxed);
                self.census
                    .service_wakes_requested
                    .fetch_add(1, Ordering::Relaxed);
                why
            }
            None => {
                self.census
                    .service_wakes_periodic
                    .fetch_add(1, Ordering::Relaxed);
                ConcurrentServiceWake::Periodic
            }
        }
    }

    /// Ask the service loop to return (VM teardown). Permanent: a later
    /// [`Self::attach_service_thread`] is refused.
    pub fn shutdown_service(&self) {
        let mut slot = self.service.slot.lock();
        slot.shutdown = true;
        self.service.shut_down.store(true, Ordering::Release);
        self.service.wake.notify_all();
    }

    /// gcd d1/c (2026-09-27) — has [`Self::shutdown_service`] run? One Acquire
    /// load. The service loop reads it before it leaves idle to run a cycle,
    /// and the VM's driver between two Phase-2 slices on the service thread,
    /// so no cycle STARTS after the stop and a cycle in hand gives up at its
    /// next slice (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`).
    #[inline]
    pub fn service_shutting_down(&self) -> bool {
        self.service.shut_down.load(Ordering::Acquire)
    }

    /// gen r4w5/concmark5 (2026-09-24) — **the concurrent-cycle service loop**,
    /// run by a dedicated VM thread
    /// (`docs/known-issues/gc/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md`).
    ///
    /// # What it replaces
    ///
    /// Without a service the cycle runs INLINE, start to finish, on whichever
    /// mutator's young-collection epilogue first found `concurrent_cycle_due`:
    /// that thread's own allocation stalls for the whole of Phase 2 and the
    /// sweep, and a program whose old generation grows without young
    /// collections (humongous arrays, young-spill objects) never has the start
    /// considered below the STW floor at all.
    ///
    /// # What it does (HotSpot's `G1ConcurrentMarkThread` shape)
    ///
    /// The thread attaches itself ([`Self::attach_service_thread`]) and parks
    /// idle (not counted by pauses). It wakes on
    /// * a HAND-OFF — once a service is attached, `concurrent_cycle_due` on any
    ///   other thread posts a request and answers `false`, so the mutator goes
    ///   back to its allocation;
    /// * a GROWTH signal from a direct old-gen allocation that crossed the
    ///   start threshold;
    /// * the PERIOD, when it asks the trigger itself ([`ConcurrentServiceHooks::cycle_due`]).
    ///
    /// It then becomes a counted mutator and runs the VM's cycle driver
    /// ([`ConcurrentServiceHooks::run_cycle`]), which requests the
    /// initial-mark and remark pauses itself and polls safepoints between
    /// slices — so the phases advance on the service's schedule, not a
    /// mutator's. The fallbacks are untouched: `major_trigger_decision` still
    /// runs the STW collection through an open cycle on a request, an old-gen
    /// allocation failure, or at 90 % (`CONC_DEFER_CEILING_PCT`; not under
    /// `CRATONVM_GEN_CONC_PRECEDENCE`, gcd d9/e), and a cycle
    /// that fails (lost remark, stale epoch) is abandoned by the driver
    /// exactly as before; the service then backs its periodic re-check off
    /// ([`service_backoff_skips`]).
    ///
    /// Returns `false` without running if a service is already attached or the
    /// service was shut down; `true` after [`Self::shutdown_service`]. The
    /// thread is IDLE (inside `enter_idle`) when this returns, whichever way.
    pub fn serve(
        &self,
        period: std::time::Duration,
        hooks: &mut dyn ConcurrentServiceHooks,
    ) -> bool {
        let Some(_attachment) = self.attach_service_thread() else {
            return false;
        };
        let mut failures = 0u32;
        let mut skip = 0u64;
        hooks.enter_idle();
        loop {
            let run = match self.service_wait(period) {
                ConcurrentServiceWake::Shutdown => break,
                ConcurrentServiceWake::HandedOff | ConcurrentServiceWake::OldGenGrowth => true,
                ConcurrentServiceWake::Periodic => {
                    if skip > 0 {
                        skip -= 1;
                        self.census
                            .service_backoff_skips
                            .fetch_add(1, Ordering::Relaxed);
                        false
                    } else {
                        let due = hooks.cycle_due();
                        if due {
                            self.census
                                .service_periodic_due
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        due
                    }
                }
            };
            if !run {
                continue;
            }
            // gcd d1/c: a stop that arrived after the wake (a hand-off taken
            // just before `shutdown_service`, or a periodic check that ran
            // across it) must not start a cycle: VM teardown is under way, and
            // its per-VM rows may already be released.
            if self.service_shutting_down() {
                break;
            }
            hooks.leave_idle();
            self.census
                .service_cycle_attempts
                .fetch_add(1, Ordering::Relaxed);
            let opened_before = self.census.initial_marks.load(Ordering::Relaxed);
            let completed_before = self.census.cycles_completed.load(Ordering::Relaxed);
            hooks.run_cycle();
            let opened = self.census.initial_marks.load(Ordering::Relaxed) > opened_before;
            let completed = self.census.cycles_completed.load(Ordering::Relaxed) > completed_before;
            if completed {
                self.census
                    .service_cycles_completed
                    .fetch_add(1, Ordering::Relaxed);
                failures = 0;
                skip = 0;
            } else if opened {
                failures = failures.saturating_add(1);
                skip = service_backoff_skips(failures);
            }
            hooks.enter_idle();
        }
        true
    }

    /// gen r4w5/concmark5 — one remark called the reference-processing
    /// callback (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`): the no-op hook of
    /// [`ConcurrentMarker::remark`], or (gen r5w1/refs5) the VM driver's real
    /// processing through [`Self::note_remark_refproc_retired`].
    fn note_remark_refproc_hook(&self) {
        self.census
            .remark_refproc_hook_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    /// gen r4w5/concmark5 — the shutdown `[GC] conc_driver:` line: whether a
    /// service thread is attached, what woke it and what it ran, and the
    /// remark hook's calls. Keys are `concdrv_`-prefixed.
    ///
    /// gen r4w6/concsvc6 — plus the cadence of the concurrent cycle itself,
    /// whoever drives it: `concdrv_cycles_started` (initial marks that opened
    /// a cycle) and `concdrv_cycles_completed` (sweeps that ended), the
    /// trigger-to-start latency (`concdrv_trigger_to_start_n` samples, their
    /// mean and maximum in µs; see [`Self::note_start_verdict`]), the
    /// fragmentation-only STW collections through an open cycle, and the SATB
    /// entries Phase 2 drained from outside the old generation.
    ///
    /// gen r5w3/unload7: the returned text now carries more than one line —
    /// the driver line first, then `[GC] conc_doors:` ([`Self::door_census_line`])
    /// and, under `CRATONVM_GEN_CONC_CLASS_UNLOAD`, `[GC] conc_unload:`
    /// ([`Self::class_unload_census_line`]), newline-separated — so the one
    /// printer (`VmHeap::print_gc_summary`, not this lane's file) emits the new
    /// lines without a second call site. The first line is unchanged.
    pub fn driver_census_line(&self) -> String {
        let mut text = self.driver_line_only();
        text.push('\n');
        text.push_str(&self.door_census_line());
        if let Some(unload) = self.class_unload_census_line() {
            text.push('\n');
            text.push_str(&unload);
        }
        // gen r5w6/conc10: under either opt-in, `[GC] conc_y2o:`.
        if let Some(y2o) = self.y2o_census_line() {
            text.push('\n');
            text.push_str(&y2o);
        }
        // gcd d2/g: under `CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, the young
        // sweep's precise-root exemptions (process-wide counts; this is the
        // one generational summary printer this lane can reach).
        if let Some(prp) = crate::gen_heap::precise_root_promote_census_line() {
            text.push('\n');
            text.push_str(&prp);
        }
        // gcd d9/e: how every claimed cycle ended, and why the pre-empted ones
        // were (always printed, last).
        text.push('\n');
        text.push_str(&self.cycle_census_line());
        text
    }

    /// The `[GC] conc_driver:` line alone (see [`Self::driver_census_line`]).
    fn driver_line_only(&self) -> String {
        let c = self.census();
        let avg_us = c
            .trigger_to_start_total_us
            .checked_div(c.trigger_to_start_samples)
            .unwrap_or(0);
        format!(
            "[GC] conc_driver: concdrv_service_enabled={} concdrv_service_attached={} \
             concdrv_handoffs={} concdrv_growth_signals={} concdrv_wakes_requested={} \
             concdrv_wakes_periodic={} concdrv_periodic_due={} concdrv_backoff_skips={} \
             concdrv_service_attempts={} concdrv_service_cycles_completed={} \
             concdrv_remark_refproc_hook_calls={} concdrv_cycles_started={} \
             concdrv_cycles_completed={} concdrv_trigger_to_start_n={} \
             concdrv_trigger_to_start_avg_us={avg_us} concdrv_trigger_to_start_max_us={} \
             concdrv_stw_frag_preempted={} concdrv_satb_drained_in_mark={} \
             concdrv_satb_outside_old_gen={} concdrv_sweep_epoch_aborts={} \
             concdrv_reference_skip_published={} concdrv_remark_refproc_retired={} \
             concdrv_start_asked={} concdrv_start_due={} \
             concdrv_sweep_reference_rows_dropped={}",
            Self::service_enabled(),
            self.service_attached(),
            c.service_handoffs,
            c.service_growth_signals,
            c.service_wakes_requested,
            c.service_wakes_periodic,
            c.service_periodic_due,
            c.service_backoff_skips,
            c.service_cycle_attempts,
            c.service_cycles_completed,
            c.remark_refproc_hook_calls,
            c.initial_marks,
            c.cycles_completed,
            c.trigger_to_start_samples,
            c.trigger_to_start_max_us,
            c.stw_frag_preempted,
            c.satb_drained_in_mark,
            c.satb_outside_old_gen,
            c.sweep_epoch_aborts,
            c.reference_skip_published,
            c.remark_refproc_retired,
            c.start_verdicts,
            c.start_verdicts_due,
            c.sweep_reference_rows_dropped,
        )
    }
}

// ---------------------------------------------------------------------------
// gen r4w5/concmark5 — the STW old-gen trigger's cadence, counted
// ---------------------------------------------------------------------------

/// gen r4w5/concmark5 (2026-09-24) — how often the young pauses' STW old-gen
/// collection ran, and how well: the COUNT-based measurement
/// `gengc-r4-oldgen-major-trigger-has-no-hysteresis-FIXED-20260924.md` needs, since
/// wave 4's wall-clock A/B could not decide the hysteresis flip on a noisy
/// host. Every figure is a count of collector events, so two runs of the same
/// deterministic program compare exactly whatever the host is doing.
///
/// Read with [`ConcurrentGcState::major_cadence`]; printed at shutdown as
/// `[GC] major_cadence:` (`majcad_` keys).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MajorCadenceCensus {
    /// Young collections that asked the STW old-gen trigger
    /// (`major_trigger_decision`: Phase 5 of the moving cycle, and the
    /// non-moving cycle when its old sweep is enabled).
    pub young_decisions: u64,
    /// STW old-gen collections those decisions ran — confirmed by the
    /// generation's own collection count moving, not by the decision alone.
    pub majors: u64,
    /// ...of which requested (`System.gc()`), for the reader to subtract.
    pub requested_majors: u64,
    /// Majors whose PREVIOUS young collection also ran one: the "every young
    /// pause is also a full old-gen collection" signature the hysteresis page
    /// is about.
    pub back_to_back: u64,
    /// Majors that freed less than 5 % of the old generation's capacity.
    pub low_yield_5pct: u64,
    // --- gen r4w6/concsvc6 (2026-09-24): the majors by cause ---------------
    /// "The concurrent cycle was too slow", first half: the major ran while a
    /// cycle was OPEN, because the generation reached the defer ceiling or an
    /// old-gen allocation failed ([`MajorCause::FallbackCycleOpen`]).
    pub fallback_cycle_open: u64,
    /// Second half: no cycle was open, but the concurrent start was due (or a
    /// driver had claimed the cycle and not yet run its initial mark), so the
    /// STW collection ran where a concurrent cycle should have been running
    /// ([`MajorCause::FallbackStartLate`]).
    pub fallback_start_late: u64,
    /// The remaining majors by cause (with `requested_majors`, these and the
    /// two fallbacks partition `majors`).
    pub other_occupancy: u64,
    pub other_alloc_failure: u64,
    pub other_fragmentation: u64,
}

/// gen r4w6/concsvc6 (2026-09-24) — why a young pause's STW old-gen decision
/// said "run", as `GenerationalHeap::major_trigger_decision` classifies it.
/// Counted per cause in [`MajorCadenceCensus`] once the major is confirmed to
/// have run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MajorCause {
    /// `System.gc()` (or the OOM ladder's request).
    Requested,
    /// Concurrent-first only: a cycle was open and the collection could not
    /// defer to it (the 90 % ceiling, or an old-gen allocation failure; under
    /// `CRATONVM_GEN_CONC_PRECEDENCE` only the failure). gcd d9/e: which one
    /// is on `[GC] conc_cycles:` ([`StwPreemptCause`]).
    FallbackCycleOpen,
    /// Concurrent-first only: no cycle open, but the concurrent start was due
    /// or claimed — the cycle had not started in time.
    FallbackStartLate,
    /// The occupancy verdict (75 % floor, hysteresis as configured), with no
    /// concurrent cycle owed.
    Occupancy,
    /// An old-gen allocation failure, with no concurrent cycle owed.
    AllocationFailure,
    /// Only an armed fragmentation compaction (`CRATONVM_GC_OLD_OOM_COMPACT` /
    /// `CRATONVM_GC_OLD_FRAG_COMPACT`) made it run.
    Fragmentation,
}

impl MajorCause {
    /// Is this the concurrent cycle failing to keep up?
    pub fn is_concurrent_fallback(self) -> bool {
        matches!(self, Self::FallbackCycleOpen | Self::FallbackStartLate)
    }
}

impl MajorCadenceCensus {
    /// Majors that ran because the concurrent cycle was too slow (either
    /// half; see [`MajorCause::is_concurrent_fallback`]).
    pub fn fallback_conc_too_slow(&self) -> u64 {
        self.fallback_cycle_open
            .saturating_add(self.fallback_start_late)
    }
    /// Majors per 100 young decisions, in tenths (`123` = 12.3).
    pub fn majors_per_100_young_tenths(&self) -> u64 {
        self.majors
            .saturating_mul(1000)
            .checked_div(self.young_decisions)
            .unwrap_or(0)
    }

    /// The shutdown line.
    pub fn line(&self) -> String {
        let t = self.majors_per_100_young_tenths();
        let too_slow = self.fallback_conc_too_slow();
        format!(
            "[GC] major_cadence: majcad_young_decisions={} majcad_majors={} \
             majcad_per_100_young={}.{} majcad_back_to_back={} majcad_low_yield_5pct={} \
             majcad_requested={} majcad_fallback_conc_too_slow={too_slow} \
             majcad_fallback_cycle_open={} majcad_fallback_start_late={} majcad_other={} \
             majcad_other_occupancy={} majcad_other_alloc_failure={} \
             majcad_other_fragmentation={}",
            self.young_decisions,
            self.majors,
            t / 10,
            t % 10,
            self.back_to_back,
            self.low_yield_5pct,
            self.requested_majors,
            self.fallback_cycle_open,
            self.fallback_start_late,
            self.majors.saturating_sub(too_slow),
            self.other_occupancy,
            self.other_alloc_failure,
            self.other_fragmentation,
        )
    }
}

/// A decision that said "run", waiting for the next decision to see whether
/// (and how well) it ran.
#[derive(Debug, Clone, Copy)]
struct PendingMajor {
    stw_collections: u64,
    stw_freed: u64,
    capacity: usize,
    cause: MajorCause,
}

/// The cadence census's state, behind `ConcurrentGcState::cadence`.
#[derive(Debug, Default)]
struct MajorCadence {
    census: MajorCadenceCensus,
    pending: Option<PendingMajor>,
    /// The previous resolved young decision ran a major.
    prev_ran: bool,
}

impl MajorCadence {
    /// The census with the pending decision (if any) resolved against the
    /// generation's STW counters NOW, and whether that decision's major ran.
    /// Pure: the shutdown line uses it without consuming anything.
    fn resolved(&self, stw_collections: u64, stw_freed: u64) -> (MajorCadenceCensus, bool) {
        let mut c = self.census;
        let Some(p) = self.pending else {
            return (c, false);
        };
        if stw_collections <= p.stw_collections {
            // Decided but not run (a guard in the caller skipped it).
            return (c, false);
        }
        c.majors += 1;
        match p.cause {
            MajorCause::Requested => c.requested_majors += 1,
            MajorCause::FallbackCycleOpen => c.fallback_cycle_open += 1,
            MajorCause::FallbackStartLate => c.fallback_start_late += 1,
            MajorCause::Occupancy => c.other_occupancy += 1,
            MajorCause::AllocationFailure => c.other_alloc_failure += 1,
            MajorCause::Fragmentation => c.other_fragmentation += 1,
        }
        if self.prev_ran {
            c.back_to_back += 1;
        }
        let freed = stw_freed.saturating_sub(p.stw_freed);
        if freed.saturating_mul(20) < p.capacity as u64 {
            c.low_yield_5pct += 1;
        }
        (c, true)
    }

    /// The wave-5 form: the cause is only "requested or not" (a non-requested
    /// run is counted as [`MajorCause::Occupancy`]).
    fn note_decision(
        &mut self,
        stw_collections: u64,
        stw_freed: u64,
        capacity: usize,
        requested: bool,
        runs: bool,
    ) {
        let cause = if requested {
            MajorCause::Requested
        } else {
            MajorCause::Occupancy
        };
        self.note_decision_caused(stw_collections, stw_freed, capacity, runs.then_some(cause));
    }

    /// gen r4w6/concsvc6 — `cause` is `Some` iff the decision said "run".
    fn note_decision_caused(
        &mut self,
        stw_collections: u64,
        stw_freed: u64,
        capacity: usize,
        cause: Option<MajorCause>,
    ) {
        let (c, ran) = self.resolved(stw_collections, stw_freed);
        self.census = c;
        self.prev_ran = ran;
        self.census.young_decisions += 1;
        self.pending = cause.map(|cause| PendingMajor {
            stw_collections,
            stw_freed,
            capacity,
            cause,
        });
    }
}

impl ConcurrentGcState {
    /// gen r4w5/concmark5 — one young collection's STW old-gen decision, with
    /// the generation's counters as the decision is taken:
    /// `stw_collections` = `OldGenTriggerStats::collections −
    /// concurrent_collections`, `freed_total` = `OldGenTriggerStats::freed_bytes`
    /// (this state subtracts what its concurrent sweeps reported). Resolves the
    /// PREVIOUS decision (did its major run, what did it free) and records this
    /// one. Called under the old-gen lock.
    pub fn note_major_decision(
        &self,
        stw_collections: u64,
        freed_total: u64,
        capacity: usize,
        requested: bool,
        runs: bool,
    ) {
        let stw_freed = freed_total.saturating_sub(self.concurrent_freed.load(Ordering::Relaxed));
        self.cadence
            .lock()
            .note_decision(stw_collections, stw_freed, capacity, requested, runs);
    }

    /// gen r4w6/concsvc6 — [`Self::note_major_decision`] with the decision's
    /// CAUSE (`Some` iff it said "run"), so the census splits the majors into
    /// "the concurrent cycle was too slow" and the rest
    /// (`majcad_fallback_*` / `majcad_other_*`). Called under the old-gen
    /// lock by `GenerationalHeap::major_trigger_decision`.
    pub fn note_major_decision_caused(
        &self,
        stw_collections: u64,
        freed_total: u64,
        capacity: usize,
        cause: Option<MajorCause>,
    ) {
        let stw_freed = freed_total.saturating_sub(self.concurrent_freed.load(Ordering::Relaxed));
        self.cadence
            .lock()
            .note_decision_caused(stw_collections, stw_freed, capacity, cause);
    }

    /// gen r4w5/concmark5 — the cadence census, the last decision resolved
    /// against the counters given (same meaning as in
    /// [`Self::note_major_decision`]). Call under the old-gen lock.
    pub fn major_cadence(&self, stw_collections: u64, freed_total: u64) -> MajorCadenceCensus {
        let stw_freed = freed_total.saturating_sub(self.concurrent_freed.load(Ordering::Relaxed));
        self.cadence.lock().resolved(stw_collections, stw_freed).0
    }

    /// gen r4w5/concmark5 — a concurrent sweep reported `bytes` to
    /// `OldGen::note_concurrent_collection_end`. Under the old-gen lock.
    pub fn note_concurrent_freed(&self, bytes: usize) {
        self.concurrent_freed
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Mark queue (work list for concurrent marking)
// ---------------------------------------------------------------------------

/// Thread-safe work queue for concurrent marking.
///
/// Uses a sharded approach: multiple `VecDeque`s behind separate `Mutex`es,
/// selected by hashing the pointer value. This reduces lock contention when
/// multiple marker threads push/pop concurrently, since threads operating on
/// different pointer ranges will typically hit different shards.
pub struct MarkQueue {
    shards: Vec<Mutex<VecDeque<*mut u8>>>,
    /// Round-11 perf: bitmask hint of which shards are currently
    /// non-empty (bit `k` set ⇒ shard `k` *may* hold work). Updated
    /// under the corresponding shard lock — `push` sets bit `k` after a
    /// `push_back`, `pop` clears bit `k` when it drains the shard empty.
    ///
    /// `pop` consults this to jump straight to a populated shard instead
    /// of probing all [`MARK_QUEUE_SHARDS`] mutexes. The mask is only a
    /// *hint*: a concurrent thread may flip a bit between the load and
    /// the lock, so a set bit can be stale (shard already drained). To
    /// stay correct `pop` falls back to a full probe if the hint-directed
    /// lookup finds nothing — it never reports the queue empty while a
    /// shard still holds work.
    nonempty_shards: AtomicU8,
    /// Round-9 gc HIGH-5 fix — set when any `push` is dropped because
    /// its target shard hit [`MARK_QUEUE_SHARD_CAP`]. The marker checks
    /// this flag at the end of remark and, if true, falls back to a
    /// full re-walk of the old generation (every live object is marked
    /// conservatively). This trades CPU for correctness: a pathological
    /// or hostile Java app graph that would previously crash the VM via
    /// `panic!` now just makes the mark phase longer.
    overflowed: AtomicBool,
    /// Termination-detection state for a MULTI-worker drain
    /// (gengc-mark2 2026-09-20). Untouched by the single-threaded
    /// [`Self::pop`] path, which every production drain still uses.
    term: Mutex<DrainTermination>,
    /// Condvar paired with [`Self::term`].
    term_cv: Condvar,
    /// Lock-free mirror of `DrainTermination::idle`, so [`Self::push`] can ask
    /// "is anyone parked waiting for work?" with one relaxed load instead of
    /// taking the termination lock on every push. Maintained under `term`, so
    /// it never disagrees except for the instant between the two updates, and
    /// a stale read costs at most one spurious `notify_all` — never a missed
    /// wakeup, because a worker re-checks every shard under `term` before it
    /// parks (see [`MarkQueue::pop_or_terminate`]).
    idle_hint: AtomicUsize,
}

/// Coordinator state for [`MarkQueue::pop_or_terminate`].
///
/// Deliberately the same shape as `young_mark`'s `DrainState`, for the same
/// reason: every quantity a termination decision depends on must be observable
/// under ONE lock, or the decision races the work it is deciding about.
#[derive(Debug)]
struct DrainTermination {
    /// Workers currently parked in the acquisition path with no work.
    idle: usize,
    /// Workers still registered on this drain. Decremented by [`MarkWorker`]'s
    /// `Drop`, so a worker that unwinds out of its scan does not park its
    /// peers forever.
    live: usize,
    /// Set once the closure is complete; every worker returns on seeing it.
    done: bool,
}

/// Number of shards for the mark queue. Must be a power of two for fast modulo.
const MARK_QUEUE_SHARDS: usize = 8;

// gc-concmark MEDIUM fix — the `nonempty_shards` hint is an `AtomicU8`, so it
// has exactly one bit per shard for at most 8 shards. The hint is set/cleared
// with `1u8 << idx` where `idx` ranges over `0..MARK_QUEUE_SHARDS`. If anyone
// bumps `MARK_QUEUE_SHARDS` above 8 while tuning, every `1u8 << idx` for
// `idx >= 8` overflows the shift width: in debug builds it panics, and in
// release builds the shift amount wraps mod 8, so shards >= 8 silently alias
// the low shards' bits — the hint becomes wrong and `pop` can skip a populated
// shard (the full-probe fallback still keeps it *correct*, just slower, but the
// hint is also actively corrupted for shards 0..8). Turn that latent landmine
// into a compile error: if you raise the shard count past 8, you must also
// widen `nonempty_shards` to `AtomicU16`/`U32`/`U64` (and the `1u8 <<` /
// `!(1u8 <<` masks below) to match. The shift expressions are written
// `1u8 << idx`, so the matching atomic width is `u8` ⇒ 8 shards max.
const _: () = assert!(
    MARK_QUEUE_SHARDS <= 8,
    "nonempty_shards is AtomicU8 (8 bits); widen it (and the `1u8 <<` masks in \
     push/pop) to AtomicU16/U32/U64 before raising MARK_QUEUE_SHARDS above 8"
);
// Sanity: the shard count must also be a power of two, because both
// `shard_for` and the round-robin `pop` cursor index with `& (SHARDS - 1)`.
const _: () = assert!(
    MARK_QUEUE_SHARDS.is_power_of_two(),
    "MARK_QUEUE_SHARDS must be a power of two (masked with `& (SHARDS - 1)`)"
);
/// `log2(MARK_QUEUE_SHARDS)`: how many top bits of the hash `shard_for` keeps.
const MARK_QUEUE_SHARD_BITS: u32 = MARK_QUEUE_SHARDS.trailing_zeros();
// `shard_for` shifts right by `64 - SHARD_BITS`; a single shard would make that
// a 64-bit shift, which overflows.
const _: () = assert!(
    MARK_QUEUE_SHARD_BITS >= 1,
    "MARK_QUEUE_SHARDS must be at least 2 (shard_for shifts by 64 - log2(SHARDS))"
);

/// Round-5 HIGH #6 — defensive cap on a single mark-queue shard.
///
/// The original `MarkQueue::push` was an unbounded `Vec` (well, `VecDeque`)
/// growth. A pathological mutator graph (e.g. a malicious or buggy
/// classloader that produces a deeply circular object graph during a
/// concurrent-mark cycle) could OOM the marker thread by pushing
/// hundreds of millions of pointers before any are drained. The cap
/// below converts that silent OOM into a deterministic panic, which is
/// strictly better than crashing the entire VM with an allocator
/// failure deep in `VecDeque::push_back`.
///
/// 1 million entries per shard * 8 shards * 8 bytes = 64 MiB — chosen
/// large enough that any realistic mark cycle stays well below it, but
/// small enough that the panic is reproducible in tests.
///
/// TODO(round-5+): the real fix is an overflow-handling strategy —
/// either spill the queue to a backing region, or drop the explicit
/// queue entirely and fall back to a "mark-everything-dirty" sweep
/// pass guided by the card table. Both are too invasive for this
/// hotfix; the cap below is the defensive interim.
const MARK_QUEUE_SHARD_CAP: usize = 1 << 20;

thread_local! {
    /// Per-thread round-robin cursor used by [`MarkQueue::pop`] to choose
    /// the shard probe order. Bumping this on every `pop` distributes
    /// marker threads across shards instead of stacking them all on
    /// shard 0 (which is what the original "start at 0" loop did,
    /// defeating the entire point of sharding under contention).
    static POP_CURSOR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// SAFETY: The raw pointers in the queue are heap object addresses managed
// by the GC; they are valid for the duration of the marking phase. The
// sharded Mutex design ensures exclusive access to each shard.
unsafe impl Send for MarkQueue {}
unsafe impl Sync for MarkQueue {}

impl MarkQueue {
    pub fn new() -> Self {
        let shards = (0..MARK_QUEUE_SHARDS)
            .map(|_| Mutex::new(VecDeque::with_capacity(4096 / MARK_QUEUE_SHARDS)))
            .collect();
        Self {
            shards,
            nonempty_shards: AtomicU8::new(0),
            overflowed: AtomicBool::new(false),
            term: Mutex::new(DrainTermination {
                idle: 0,
                live: 0,
                done: false,
            }),
            term_cv: Condvar::new(),
            idle_hint: AtomicUsize::new(0),
        }
    }

    /// Select shard index from a pointer value.
    ///
    /// gen r4w2/concmark (2026-09-23): a Fibonacci hash of the 8-byte granule,
    /// taking the TOP bits. It used to be `(ptr >> 3) & (SHARDS - 1)`, the
    /// LOW bits — and legacy objects are `HEADER_SIZE (16) + n * SLOT_SIZE
    /// (16)` bytes, so in a region of them every start is 16-aligned, `ptr >> 3`
    /// is even, and only shards 0, 2, 4, 6 were ever used: half the sharding,
    /// and half the effective `MARK_QUEUE_SHARD_CAP` before the O(old gen)
    /// overflow rescan fires
    /// (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 1).
    /// The multiply mixes every address bit into the top bits, so any stride
    /// spreads. Computed in `u64` so the constant fits on every target.
    #[inline]
    fn shard_for(ptr: *mut u8) -> usize {
        let granule = (ptr as usize as u64) >> 3;
        let mixed = granule.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> (64 - MARK_QUEUE_SHARD_BITS)) as usize
    }

    /// Push an object onto the mark queue (it becomes gray).
    ///
    /// Round-9 gc HIGH-5 fix — replace the previous `panic!` (which
    /// was reachable from any hostile/buggy Java program that built a
    /// wide live graph) with a graceful overflow flag. The push is
    /// dropped; the bitmap entry remains marked, so the object itself
    /// isn't swept, but its outgoing edges won't be scanned via the
    /// queue. The marker compensates by running a conservative full
    /// re-walk of the old generation at the end of remark (see
    /// [`Self::has_overflowed`] and the remark fallback). This trades
    /// CPU for correctness — no crash, just longer mark.
    pub fn push(&self, obj_ptr: *mut u8) {
        let idx = Self::shard_for(obj_ptr);
        let mut shard = self.shards[idx].lock();
        if shard.len() >= MARK_QUEUE_SHARD_CAP {
            // Drop the push, set the overflow flag, and let the remark
            // phase fall back to a full conservative re-walk.
            // `Relaxed` is sufficient — the flag is read once during
            // STW remark after every push has happened-before.
            self.overflowed.store(true, Ordering::Relaxed);
            return;
        }
        shard.push_back(obj_ptr);
        // Round-11 perf: mark this shard non-empty in the hint mask.
        // Done under the shard lock so the bit is set before the lock
        // (and therefore the queued pointer) becomes visible to a
        // concurrent `pop`.
        self.nonempty_shards.fetch_or(1u8 << idx, Ordering::Release);
        drop(shard);
        // gengc-mark2 2026-09-20 — wake a parked marker, if there is one.
        //
        // Costs exactly one relaxed load on the single-threaded path that
        // every production drain uses today, where `idle_hint` is 0 for the
        // whole drain (the lone worker only parks once, at the very end, and
        // by then nothing is pushing). `notify_all` is reached only when a
        // peer is genuinely waiting.
        if self.idle_hint.load(Ordering::Relaxed) > 0 {
            self.term_cv.notify_all();
        }
    }

    /// Round-9 gc HIGH-5 — true iff any push since the last
    /// [`Self::clear`] was dropped due to per-shard capacity. The
    /// remark phase consults this and triggers a full re-walk if set.
    #[inline]
    pub fn has_overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed)
    }

    /// Pop an object from the mark queue for scanning.
    /// Returns `None` if all shards are empty.
    ///
    /// Each calling thread maintains its own round-robin cursor so that
    /// concurrent markers spread contention evenly across shards instead
    /// of always hammering shard 0 first.
    pub fn pop(&self) -> Option<*mut u8> {
        let start = POP_CURSOR.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        }) & (MARK_QUEUE_SHARDS - 1);

        // Round-11 perf: consult the non-empty bitmask hint and visit
        // only the shards it flags, instead of probing all 8 mutexes.
        // The mask is a hint — a bit can be stale either way — so this
        // pass is best-effort and is backed by the full probe below.
        let hint = self.nonempty_shards.load(Ordering::Acquire);
        if hint != 0 {
            for offset in 0..MARK_QUEUE_SHARDS {
                let idx = (start + offset) & (MARK_QUEUE_SHARDS - 1);
                if hint & (1u8 << idx) == 0 {
                    continue;
                }
                let mut shard = self.shards[idx].lock();
                match shard.pop_front() {
                    Some(ptr) => {
                        if shard.is_empty() {
                            self.nonempty_shards
                                .fetch_and(!(1u8 << idx), Ordering::Release);
                        }
                        return Some(ptr);
                    }
                    None => {
                        // Stale set bit — shard was drained by another
                        // thread. Clear it so future pops skip it.
                        self.nonempty_shards
                            .fetch_and(!(1u8 << idx), Ordering::Release);
                    }
                }
            }
        }

        // Fallback: the hint found nothing, but a concurrent `push` may
        // have populated a shard whose bit we hadn't observed. Probe
        // every shard so `pop` never reports empty while work remains.
        for offset in 0..MARK_QUEUE_SHARDS {
            let idx = (start + offset) & (MARK_QUEUE_SHARDS - 1);
            let mut shard = self.shards[idx].lock();
            if let Some(ptr) = shard.pop_front() {
                if shard.is_empty() {
                    self.nonempty_shards
                        .fetch_and(!(1u8 << idx), Ordering::Release);
                }
                return Some(ptr);
            }
        }
        None
    }

    /// Push multiple objects at once.
    pub fn push_batch(&self, ptrs: &[*mut u8]) {
        for &ptr in ptrs {
            self.push(ptr);
        }
    }

    /// Number of pending objects in the queue (sum across all shards).
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Whether the queue is empty (all shards empty).
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.lock().is_empty())
    }

    /// Clear all entries from all shards.
    pub fn clear(&self) {
        for shard in &self.shards {
            shard.lock().clear();
        }
        // Round-11 perf: every shard is now empty, so reset the
        // non-empty hint mask. Safe to clear unconditionally — `clear`
        // is only called during STW transitions.
        self.nonempty_shards.store(0, Ordering::Release);
        // Round-9 gc HIGH-5: reset the overflow flag so the next
        // cycle starts clean. Safe to clear unconditionally — `clear`
        // is only called during STW transitions.
        self.overflowed.store(false, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------
    // Termination detection for a multi-worker drain
    // (gengc-mark-markqueue-has-no-termination-detection-FIXED-20260923)
    // -----------------------------------------------------------------

    /// Open a drain of this queue by exactly `workers` marker threads.
    ///
    /// # Why this exists
    ///
    /// [`Self::pop`] returns `None` when every shard is empty **at the moment
    /// it looks**. With ONE marker that is a sound termination test: an empty
    /// queue plus "I am the only scanner" means the closure is complete. With
    /// two or more it is not — thread B can observe every shard empty while
    /// thread A is inside `scan_object`, about to push three children. B
    /// leaves, and whatever A pushes after that is scanned by nobody. The
    /// symptom is under-marking, which surfaces as a freed live object some
    /// cycles later, arbitrarily far from the cause.
    ///
    /// Every production drain is single-threaded today, so this is the
    /// primitive that has to exist BEFORE a second marker does, not a fix for
    /// a live bug. `young_mark::drain_parallel` solved the same problem for
    /// the young collector; this is the same argument applied to a queue whose
    /// work lives in shards rather than in the coordinator.
    ///
    /// # The protocol
    ///
    /// ```text
    /// queue.begin_drain(n);                    // coordinator, before spawning
    /// // ... in each of the n threads, INCLUDING the coordinator:
    /// let _w = queue.worker();                 // registration guard
    /// while let Some(p) = queue.pop_or_terminate() { scan(p); /* may push */ }
    /// ```
    ///
    /// The guard must outlive the loop: `live` is what makes "everyone is
    /// idle" decidable, and a worker that unwinds out of `scan` must not park
    /// its peers forever.
    ///
    /// `begin_drain` is NOT itself synchronised against a drain already in
    /// progress; call it from the coordinating thread before any worker
    /// starts, which is the only shape `ConcurrentMarker` needs.
    pub fn begin_drain(&self, workers: usize) {
        let mut g = self.term.lock();
        g.idle = 0;
        g.live = workers;
        g.done = false;
        self.idle_hint.store(0, Ordering::Relaxed);
    }

    /// Register the calling thread as one of the workers [`Self::begin_drain`]
    /// counted. Exactly one per worker; hold it for the whole drain loop.
    pub fn worker(&self) -> MarkWorker<'_> {
        MarkWorker { queue: self }
    }

    /// Pop the next object to scan, or `None` once the closure is **provably**
    /// complete.
    ///
    /// # Why the decision is sound
    ///
    /// `idle` counts only workers that have already finished scanning and have
    /// published themselves as out of work, under `term`. So a worker holding
    /// `term` and observing `idle == live` knows that
    ///
    /// * no live worker is inside a scan, hence none can push; and
    /// * no live worker can start one, because leaving the idle set requires
    ///   `term`, which this worker holds.
    ///
    /// The queue state is therefore frozen for the duration of the check, and
    /// the `pop` performed under `term` immediately before the increment is
    /// decisive. A worker that is between its fast-path `pop` and its
    /// `term.lock()` has NOT incremented `idle`, so it holds `idle < live`
    /// open and correctly prevents the decision — which is why `idle` is
    /// incremented after, never before, the re-check.
    ///
    /// # A lost wakeup costs throughput, never progress
    ///
    /// `push` notifies outside `term` (the same shape as
    /// `young_mark::drain_parallel`), so a `notify_all` can slip between a
    /// peer publishing `idle` and actually parking. That peer then sleeps
    /// through available work. It cannot sleep FOREVER: the worker that
    /// eventually finds the queue empty declares `done` and calls
    /// `notify_all` while holding `term`, which no parked worker can miss. So
    /// the worst case is that one marker does another's share, not a hang.
    /// Taking `term` on the push path to close the window would put a mutex
    /// acquisition on the hot path of a work-starved drain, which is the wrong
    /// trade.
    pub fn pop_or_terminate(&self) -> Option<*mut u8> {
        // Fast path: work is visible, so no coordination is needed at all.
        // This is the whole loop for a busy marker.
        if let Some(ptr) = self.pop() {
            return Some(ptr);
        }
        let mut g = self.term.lock();
        loop {
            if g.done {
                return None;
            }
            // Re-check under `term`: a peer may have pushed between the fast
            // path above and this acquisition.
            if let Some(ptr) = self.pop() {
                return Some(ptr);
            }
            g.idle += 1;
            self.idle_hint.store(g.idle, Ordering::Relaxed);
            // `>=`, and against `live` rather than the original worker count:
            // a worker that has already left (normally or by unwinding) can
            // never arrive here, so waiting for the full count would park the
            // survivors forever. Same rule as `young_mark::drain_parallel`.
            if g.idle >= g.live {
                g.done = true;
                g.idle -= 1;
                self.idle_hint.store(g.idle, Ordering::Relaxed);
                self.term_cv.notify_all();
                return None;
            }
            self.term_cv.wait(&mut g);
            g.idle -= 1;
            self.idle_hint.store(g.idle, Ordering::Relaxed);
        }
    }

    /// True once some worker has declared this drain complete. Diagnostics and
    /// tests; workers learn it from `pop_or_terminate` returning `None`.
    pub fn drain_is_done(&self) -> bool {
        self.term.lock().done
    }
}

/// Registration of one marker thread on a [`MarkQueue`] drain.
///
/// Dropping it — including while unwinding out of a scan — decrements the live
/// worker count and wakes the peers, so a panicking marker cannot leave the
/// others parked waiting for an `idle` count they can never reach. The
/// equivalent of `young_mark`'s `WorkerExit`.
pub struct MarkWorker<'q> {
    queue: &'q MarkQueue,
}

impl Drop for MarkWorker<'_> {
    fn drop(&mut self) {
        {
            let mut g = self.queue.term.lock();
            g.live = g.live.saturating_sub(1);
        }
        // Deliberately does NOT set `done`: a parked peer wakes, re-checks the
        // queue, and reaches `idle >= live` on its own if the closure really
        // is complete. Setting it here would discard work this worker had
        // already published before it left.
        self.queue.term_cv.notify_all();
    }
}

impl Default for MarkQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Concurrent Marker
// ---------------------------------------------------------------------------

/// The concurrent marker traverses the object graph, marking live objects
/// in the mark bitmap.
/// gen r5w3/unload7 (2026-09-26) — the class-loader side tables a
/// generational concurrent cycle follows when it may UNLOAD classes
/// (`CRATONVM_GEN_CONC_CLASS_UNLOAD`,
/// `docs/known-issues/gc/gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md`).
///
/// CratonVM keeps three class-loader relationships in VM side tables instead
/// of traceable Java fields:
///
/// * `loader_pin`: a live instance of a user-loader class keeps its defining
///   loader live (HotSpot: instance → klass → class loader data);
/// * `mirror_pin`: a live user loader keeps the mirrors of the classes it
///   defined live (HotSpot: the CLD holds its mirrors);
/// * `metadata_pin`: a live user loader keeps its classes' statics, class
///   locks, condy values, `ClassValue` results and the other
///   loader-conditional roots live — the rows `vm::memory::roots::collect_roots`
///   DEFERRED instead of rooting.
///
/// A root set taken under `gc_quiescence::with_class_unload_marking` leaves
/// user-loader mirrors and those roots to the tables, so the marker must follow
/// them: [`ConcurrentMarker::scan_object_into`] adds, for every scanned object,
/// the `loader_pin` row of its class and — when the object is a loader — its
/// `mirror_pin` and `metadata_pin` rows.
///
/// # A SNAPSHOT, taken in the initial-mark pause after the root scan
///
/// Not the live registries: `collect_roots` REBUILDS this VM's
/// `metadata_pin` rows at every pause (`replace_metadata_pins(.., &[])` first),
/// and the young pauses that run between the initial mark and the remark
/// record nothing for a cycle's loaders (hazard 1 on the page). A marker that
/// looked the rows up live in Phase 2 would find the table a young pause left
/// behind, not the rows the initial mark deferred, and the deferred values
/// would be swept. So the cycle follows the rows AS DEFERRED — the snapshot is
/// part of the cycle's snapshot-at-the-beginning, like the roots. A row added
/// later names an object allocated later (not sweep-eligible) or an owner the
/// remark re-captures ([`ConcurrentMarker::add_class_unload_tables`]).
///
/// The values are addresses: an OLD one does not move during a cycle (a free
/// or a compaction moves `reclaim_epoch` and the cycle is abandoned); a YOUNG
/// one may, but the marker drops young addresses (`markable_old_object`) and
/// every young object's old referents are already roots
/// (`collect_young_to_old_roots`), so a stale young entry costs nothing.
#[derive(Debug, Default)]
pub struct ClassUnloadTables {
    /// `class_id -> defining loader address`, the row inside THIS old
    /// generation when several VMs share the id (`loader_pin::snapshot_where`).
    /// `None` when the registry is empty.
    loaders: Option<FxHashMap<u32, usize>>,
    /// `loader address -> class mirror addresses`.
    mirrors: Option<FxHashMap<usize, Vec<usize>>>,
    /// `loader address -> deferred metadata object addresses`.
    metadata: Option<FxHashMap<usize, Vec<usize>>>,
}

impl ClassUnloadTables {
    /// Read the three registries once. Call inside a pause, after the root
    /// scan whose deferrals the marker must follow.
    pub fn capture(old_gen_base: usize, old_gen_size: usize) -> Self {
        let end = old_gen_base.saturating_add(old_gen_size);
        Self {
            loaders: cratonvm_types::loader_pin::snapshot_where(|a| {
                a >= old_gen_base && a < end
            }),
            mirrors: cratonvm_types::mirror_pin::snapshot(),
            metadata: cratonvm_types::metadata_pin::snapshot(),
        }
    }

    /// True when every registry was empty: nothing to follow.
    pub fn is_empty(&self) -> bool {
        self.loaders.as_ref().is_none_or(|m| m.is_empty())
            && self.mirrors.as_ref().is_none_or(|m| m.is_empty())
            && self.metadata.as_ref().is_none_or(|m| m.is_empty())
    }

    /// The defining loader of `class_id` in this snapshot.
    #[inline]
    pub fn loader_of(&self, class_id: u32) -> Option<usize> {
        self.loaders.as_ref()?.get(&class_id).copied()
    }

    /// gen r5w6/conc10 — every loader this snapshot names (deduplicated): the
    /// fail-safe root set when the young classes cannot be enumerated
    /// completely (a young walk that stepped over an unparseable stretch),
    /// so no loader is judged dead under an instance nobody could see.
    pub fn all_loaders(&self) -> Vec<usize> {
        let mut loaders: Vec<usize> = self
            .loaders
            .as_ref()
            .map(|m| m.values().copied().collect())
            .unwrap_or_default();
        loaders.sort_unstable();
        loaders.dedup();
        loaders
    }

    /// Fold a LATER capture into this one: its loader rows win (a row can only
    /// have been re-pointed by a collection that moved a young loader), and
    /// its mirror / metadata rows are added to this snapshot's, so the result
    /// holds every row either capture saw.
    fn absorb(&mut self, later: ClassUnloadTables) {
        if let Some(loaders) = later.loaders {
            self.loaders.get_or_insert_with(FxHashMap::default).extend(loaders);
        }
        for (mine, theirs) in [
            (&mut self.mirrors, later.mirrors),
            (&mut self.metadata, later.metadata),
        ] {
            let Some(theirs) = theirs else {
                continue;
            };
            let mine = mine.get_or_insert_with(FxHashMap::default);
            for (owner, values) in theirs {
                let row = mine.entry(owner).or_default();
                for v in values {
                    if !row.contains(&v) {
                        row.push(v);
                    }
                }
            }
        }
    }

    /// Every `(owner, values)` row of the mirror and metadata tables.
    fn owner_rows(&self) -> impl Iterator<Item = (usize, &[usize])> + '_ {
        self.mirrors
            .iter()
            .flatten()
            .chain(self.metadata.iter().flatten())
            .map(|(&owner, values)| (owner, values.as_slice()))
    }
}

pub struct ConcurrentMarker {
    /// Mark bitmap covering the old generation.
    pub bitmap: MarkBitmap,
    /// Work queue of gray objects to scan.
    pub queue: MarkQueue,
    /// Global SATB queue for write barrier entries.
    ///
    /// fork6 GC_STRESS fix — MUST be the same instance the heap's
    /// `satb_barrier` logs into (`GenerationalHeap::enable_concurrent_gc`),
    /// or the write barrier feeds a queue nobody drains while `remark`
    /// drains a queue nobody fills. A marker built via [`Self::new`] gets a
    /// private queue (tests); production cycles use [`Self::with_shared`]
    /// with the heap's handles.
    pub satb_queue: Arc<SatbQueue>,
    /// Phase tracker. Same sharing requirement as `satb_queue`: the VM's
    /// cycle drivers and root builders (`generational_concurrent_mark_open`)
    /// read `is_marking_active()` of the instance attached via
    /// `enable_concurrent_gc`. The heap's `satb_barrier` gates on the QUEUE
    /// (gc-common w5-e), not on this.
    pub state: Arc<ConcurrentGcState>,
    /// TAMS equivalent (G1MARK-3): the old-gen object starts as of the
    /// INITIAL MARK — the point the cycle's snapshot is taken. The sweep
    /// (which runs OUTSIDE any STW) may only free objects present here, so an
    /// old-gen allocation landing at any time after the mark opened (a young
    /// GC on another thread promoting survivors, or a direct large-object
    /// allocation) is implicitly live for this cycle and cannot be swept.
    ///
    /// gengc-mark 2026-09-20 moved this from REMARK to INITIAL MARK. Remark is
    /// too late: it also admits everything allocated DURING the concurrent
    /// trace, and those are exactly the objects the cycle had no way to mark
    /// (nothing here allocates black, and the SATB pre-barrier logs only OLD
    /// slot values). See `initial_mark` for the full argument. Remark now only
    /// NARROWS this set, to entries still allocated at the pause.
    ///
    /// Emptiness is no longer the abort-safe signal — the set is populated
    /// from the moment the cycle opens. The sweep is gated on
    /// [`Self::sweep_eligible_epoch`] instead, which only a completed remark
    /// stamps.
    ///
    /// gengc-mark2 2026-09-20: was `Mutex<HashSet<usize>>`; now an
    /// [`OldGenObjectStarts`] bitmap, and `None` rather than an emptied set
    /// when no cycle is open. `None` and `Some(empty)` are treated alike by
    /// every reader — see `concurrent_sweep` — so this is a representation
    /// change only.
    sweep_eligible: Mutex<Option<OldGenObjectStarts>>,
    /// `OldGen::reclaim_epoch` as of the INITIAL MARK that produced
    /// `sweep_eligible`, or `None` when no cycle is open.
    ///
    /// gengc-mark 2026-09-20. Split out from [`Self::sweep_eligible_epoch`]
    /// when the eligibility snapshot moved from remark to initial mark. The two
    /// stamps answer different questions and must not be conflated:
    ///
    /// * this one says "the layout the snapshot describes is still the current
    ///   layout", and is checked at REMARK, so a `free`/`compact` by another
    ///   old-gen collector ANYWHERE in the cycle is caught rather than only one
    ///   that happens after remark;
    /// * `sweep_eligible_epoch` says "remark ran and authorised a sweep", and
    ///   stays `None` until it does — which is the property the sweep's
    ///   abort-safe default rests on.
    initial_mark_epoch: Mutex<Option<u64>>,
    /// GCAUD-4 — `OldGen::reclaim_epoch` as of the remark that AUTHORISED a
    /// sweep of `sweep_eligible`, or `None` when no sweep is authorised.
    ///
    /// `sweep_eligible` and `bitmap` are both keyed on a bare old-gen ADDRESS,
    /// and `concurrent_sweep` runs OUTSIDE any stop-the-world: between remark
    /// and the sweep's `old_gen` lock acquisition, another thread's young GC
    /// can run a full `old_gen_gc` — either the sliding `compact` (every
    /// survivor's address changes) or the in-place sweep (blocks return to the
    /// free list and the next `alloc` re-issues those addresses to NEW
    /// objects). Either way an address in `sweep_eligible` stops naming the
    /// object it named at remark, while the bitmap bit at that address still
    /// describes the OLD occupant.
    ///
    /// The TAMS filter reads "existed at remark AND unmarked ⇒ free it", so a
    /// live object that inherited a dead object's address satisfies both
    /// halves and is freed — a use-after-free manufactured by two collectors
    /// that individually behave correctly. The epoch is the identity the bare
    /// address lacks; on a mismatch the sweep frees nothing.
    sweep_eligible_epoch: Mutex<Option<u64>>,
    /// gen r4w2/concmark (2026-09-23) — the last old-gen object-start walk,
    /// keyed on the generation's identity and [`OldGen::free_list_seq`].
    ///
    /// Phase 2 used to rebuild the set at the top of EVERY slice and again at
    /// remark — an O(old gen) walk each time, which is what forced slices to be
    /// large. The rebuild exists so an object promoted between two slices is
    /// markable in the next one (see `concurrent_mark_budget`), and that is
    /// exactly what the key captures: a walk is a function of the free list
    /// (which blocks are allocated) and of the allocated objects' headers
    /// (kind and size, which nothing rewrites while the object is allocated),
    /// and every free-list mutation — `alloc`, `free`, `compact`'s rebuild,
    /// `release_unused_tail`, a coalesce — bumps `free_list_seq`. So an
    /// unchanged key proves the rebuild would reproduce this set exactly; a
    /// changed one rebuilds. Mutators allocate old-gen objects only under the
    /// old-gen lock with the header written before the lock is released
    /// (`try_alloc_*_old`), and every walk here runs under that lock, so no
    /// walk can observe a block between its `alloc` and its header.
    starts_cache: Mutex<Option<ObjectStartsCache>>,
    /// gen r4w2/concmark (2026-09-23) — set when any object-start walk this
    /// cycle did not cover every allocated byte (`OldGen::scan_region` broke on
    /// an implausible header). Objects behind the break are not in the set, so
    /// references to them are neither marked nor TRACED, and an older,
    /// eligible object reachable only through one of them would be swept live.
    /// `remark` refuses to authorise a sweep for such a cycle; see
    /// `gengc-r4-mark-walk-desync-tail-is-untraced-FIXED-20260923.md`.
    walk_desynced: AtomicBool,
    /// gen r4w3/oldgen3 — a sliced concurrent sweep between two slices; `None`
    /// before the first slice and after the last. See
    /// [`Self::concurrent_sweep_budget`].
    sweep_progress: Mutex<Option<SweepProgress>>,
    /// gen r4w3/oldgen3 — a mark-queue overflow rescan being run in Phase-2
    /// slices. See [`Self::concurrent_mark_budget`].
    overflow_rescan: Mutex<OverflowRescan>,
    /// gen r5w1/refs5 — the REFERENT-SLOT SKIP SET: one bit per old-gen
    /// `java.lang.ref.Reference` object whose slot 0 (the referent) this
    /// cycle's trace must not follow. See [`Self::set_reference_skip`].
    ///
    /// A second [`MarkBitmap`] over the same span as `bitmap`, built on first
    /// use only (a cycle that never publishes a set allocates nothing), and
    /// lock-free to query: [`Self::scan_object_into`] asks it once per scanned
    /// OBJECT, only while [`Self::reference_skip_armed`] says a set is live.
    reference_skip: std::sync::OnceLock<MarkBitmap>,
    /// `true` from [`Self::set_reference_skip`] (with at least one bit set)
    /// until [`Self::abort_cycle`] or the sweep's end. The one load the scan
    /// pays when no set is published.
    reference_skip_armed: AtomicBool,
    /// The span `bitmap` covers, kept so [`Self::reference_skip`] can be built
    /// over exactly the same one (`MarkBitmap` no longer exposes it).
    old_gen_base: usize,
    old_gen_size: usize,
    /// gen r5w3/unload7 — the class-loader side tables this cycle follows
    /// (see [`ClassUnloadTables`]); `None` unless the driver armed concurrent
    /// class unloading. Written only inside the cycle's pauses (by the thread
    /// that runs the cycle, which is also the only scanner), read per scanned
    /// object while [`Self::class_unload_armed`] is set.
    class_unload: RwLock<Option<ClassUnloadTables>>,
    /// The one load [`Self::scan_object_into`] pays when no tables are armed.
    class_unload_armed: AtomicBool,
    /// gen r5w4/conc8 — the retained-layout census candidates the driver
    /// handed this cycle in its initial-mark pause
    /// ([`Self::set_layout_census`]); taken by the sweep's first slice.
    layout_census: Mutex<Option<Vec<u32>>>,
    /// gen r5w5/conc9 — set by a driver whose remark runs reference
    /// processing ([`Self::record_finalizer_retention_closure`]): the remark's
    /// dead-finalizer retention then RECORDS every object it marks, so the
    /// reference pass can treat them as marked only for `finalize()` (see
    /// [`Self::take_finalizer_retention_closure`]). `false` (every other
    /// caller) is the retention exactly as before.
    record_retention_closure: AtomicBool,
    /// gen r5w5/conc9 — what the last recording retention marked: the retained
    /// finalizables and every object their drain newly marked. Written and
    /// taken inside the same remark pause.
    retention_closure: Mutex<Vec<usize>>,
    /// gcd d9/e — the INITIAL MARK's object-start set (the walk that also
    /// seeded `starts_cache`), kept for Phase-2 slices under
    /// `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` ([`Self::phase2_object_starts`]).
    /// Set by `initial_mark`, dropped by `abort_cycle` and at the sweep's
    /// start; `None` otherwise.
    tams_starts: Mutex<Option<Arc<OldGenObjectStarts>>>,
}

/// gen r5w4/conc8 — the process-wide count of old-generation walk breaks
/// (`WALK_DESYNC_HITS` + `SCAN_REGION_BREAK_HITS`). A walk that breaks leaves
/// the rest of its region unvisited without telling its caller, so a census
/// that must have seen EVERY object compares this before and after: unchanged
/// means no walk anywhere broke in between (a break in another VM's walk only
/// makes the census inconclusive, the safe direction).
fn walk_break_hits() -> u64 {
    crate::old_gen::WALK_DESYNC_HITS
        .load(Ordering::Relaxed)
        .wrapping_add(crate::old_gen::SCAN_REGION_BREAK_HITS.load(Ordering::Relaxed))
}

/// gen r5w4/conc8 — one sweep's retained-layout census (see
/// [`ConcurrentGcState::note_layouts_retained`]).
struct LayoutCensus {
    /// Pending retained ids with no young instance at this cycle's initial mark.
    candidates: rustc_hash::FxHashSet<u32>,
    /// Candidates the sweep found a SURVIVING old-generation instance of.
    survivors: rustc_hash::FxHashSet<u32>,
    /// [`walk_break_hits`] when the sweep began.
    breaks_at_start: u64,
}

impl LayoutCensus {
    /// A surviving object (marked, or not sweep-eligible) the sweep walked.
    #[inline]
    fn observe_survivor(&mut self, obj_ptr: *mut u8) {
        // SAFETY: `walk_objects_from` yields only object starts whose header
        // tags it validated and whose size it derived from this header, under
        // the old-gen lock the sweep slice holds; `class_id` is a plain `u32`
        // field of that header.
        let class_id = unsafe { (*(obj_ptr as *const ObjectHeader)).class_id.as_u32() };
        if self.candidates.contains(&class_id) {
            self.survivors.insert(class_id);
        }
    }

    /// The candidates to release if the census was complete: no walk broke
    /// since it began. `None` (release nothing) otherwise.
    fn released(self) -> Option<Vec<u32>> {
        if walk_break_hits() != self.breaks_at_start {
            return None;
        }
        let survivors = self.survivors;
        Some(
            self.candidates
                .into_iter()
                .filter(|id| !survivors.contains(id))
                .collect(),
        )
    }
}

/// gen r4w3/oldgen3 — what a sliced concurrent sweep carries from one slice
/// to the next. See [`ConcurrentMarker::concurrent_sweep_budget`].
struct SweepProgress {
    /// The TAMS-narrowed eligibility snapshot remark authorised.
    eligible: OldGenObjectStarts,
    /// `OldGen::reclaim_epoch` after the previous slice's OWN frees. Anything
    /// else that frees or slides old-gen storage moves it, and then neither
    /// the bitmap nor the resume offset can be trusted.
    epoch: u64,
    /// Where the next slice resumes — an offset `OldGen::walk_objects_from`
    /// returned, i.e. the start of an object that was allocated then.
    next: usize,
    freed_objects: usize,
    freed_bytes: usize,
    /// gen r5w4/conc8 — `Some` while retained layouts are pending and the
    /// driver handed this cycle census candidates.
    layout_census: Option<LayoutCensus>,
}

/// gen r4w3/oldgen3 — a mark-queue overflow rescan in progress across Phase-2
/// slices, and how many passes this cycle has started.
#[derive(Default)]
struct OverflowRescan {
    /// `Some(resume offset)` while a pass is part-way through the generation.
    cursor: Option<usize>,
    passes: u32,
}

/// gen r5w1/refs5 — the remark between its two halves
/// ([`ConcurrentMarker::remark_begin`] → [`ConcurrentMarker::remark_finish`]),
/// with the old-gen guard possibly released in between. Dropping it without
/// `remark_finish` leaves the phase at `Remark` with the SATB queue active; the
/// driver's [`ConcurrentCycle`] guard then abandons the cycle on its way out,
/// as for any other early exit.
#[must_use = "a remark begun must be finished (remark_finish) inside the same pause"]
pub struct RemarkProgress {
    authorised: bool,
    scanned: usize,
    object_starts: Arc<OldGenObjectStarts>,
}

impl RemarkProgress {
    /// Whether this remark authorised a sweep. The reference-processing
    /// callback must not run on a refused remark: nothing can be judged dead.
    pub fn sweep_authorised(&self) -> bool {
        self.authorised
    }
}

/// gen r4w3/oldgen3 — at most this many overflow rescan passes run
/// concurrently per cycle; after that the flag is left set and remark's
/// `drain_closure` applies its own bounded fallback, exactly as before. The
/// same bound `drain_closure` uses.
const CONCURRENT_OVERFLOW_RESCAN_PASSES: u32 = 8;

/// gen r4w4/concmark4 — entries the marker-local stack holds before a newly
/// greyed child goes to the shared queue instead (512 KiB of pointers). Far
/// below the queue's own `MARK_QUEUE_SHARDS * MARK_QUEUE_SHARD_CAP`, so the
/// local stack never hides an overflow the queue would have reported.
const LOCAL_MARK_STACK_CAP: usize = 64 * 1024;

/// One cached [`OldGenObjectStarts`] and the identity it is valid for. See
/// [`ConcurrentMarker::starts_cache`].
struct ObjectStartsCache {
    /// `OldGen::base_ptr`, so a marker handed a different generation (tests do)
    /// can never reuse another generation's set.
    base: usize,
    /// `OldGen::free_list_seq` when the walk ran.
    seq: u64,
    starts: Arc<OldGenObjectStarts>,
}

/// gen r4w2/concmark — object-start walks the marker REUSED because
/// `free_list_seq` had not moved (see `ConcurrentMarker::starts_cache`).
pub static CONC_MARK_WALKS_REUSED: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — object-start walks the marker actually ran (initial
/// mark, a slice or remark whose cache key had moved).
pub static CONC_MARK_WALKS_BUILT: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles whose remark refused to authorise a sweep because
/// an object-start walk desynced (see `ConcurrentMarker::walk_desynced`).
/// Non-zero means `WALK_DESYNC_HITS`/`SCAN_REGION_BREAK_HITS` are non-zero too.
pub static CONC_MARK_WALK_DESYNC_ABORTS: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — `ConcurrentMarker::try_open_cycle` refusals: a driver
/// found another cycle already owning the shared phase.
pub static CONC_CYCLE_OPEN_REFUSED: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — Phase-2 slices the generational driver ran (bumped by
/// `maybe_concurrent_gc_at`). Slices per completed cycle is the number to read
/// against `CRATONVM_GEN_CONC_MARK_SLICE`.
pub static CONC_MARK_SLICES: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — remark pauses the driver lost to another thread's pause
/// and then RETRIED after joining it (the sliced driver only).
pub static CONC_REMARK_RETRIES: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles abandoned because every remark attempt was lost
/// (in the legacy `CRATONVM_GEN_CONC_MARK_SLICE=0` arm, the first loss).
pub static CONC_REMARK_ABANDONS: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles the driver abandoned BEFORE the remark pause
/// because another old-gen collector had freed or slid storage since the
/// initial mark (`ConcurrentMarker::cycle_is_stale`). Slicing lets young
/// pauses — including ones that reclaim old-gen blocks in place — run inside
/// Phase 2, so this is the counter that says whether they are starving cycles.
pub static CONC_CYCLE_STALE_ABANDONS: AtomicU64 = AtomicU64::new(0);

/// gen r4w2/concmark — objects per Phase-2 slice when the flag is unset.
///
/// The slice bounds how long the marker holds the old-gen lock (every
/// promotion and every direct old-gen allocation waits for it) and how long it
/// goes without a safepoint poll (every other thread's pause waits for it).
/// Scanning one object is a header decode plus one load per slot, so 32 Ki
/// objects is on the order of a millisecond on ordinary objects; with the
/// object-start cache a slice no longer pays for a walk unless the old
/// generation changed since the previous one.
pub const DEFAULT_GEN_CONC_MARK_SLICE: usize = 32 * 1024;

/// `CRATONVM_GEN_CONC_MARK_SLICE=<n>` — objects scanned per Phase-2 slice of
/// the generational concurrent old-gen cycle (gen r4w2/concmark, 2026-09-23).
///
/// * unset → [`DEFAULT_GEN_CONC_MARK_SLICE`];
/// * `n > 0` → slices of `n` objects, with the old-gen lock released and a
///   safepoint polled between slices, and a lost remark pause retried;
/// * `0` → the pre-2026-09-23 driver: one unbounded `concurrent_mark` under a
///   single old-gen lock hold, no polls, and a lost remark aborts the cycle.
///   Kept as the A/B arm and the kill switch.
///
/// gen r4w3/oldgen3: the same knob sizes Phase-4 SWEEP slices (objects walked
/// per slice, `ConcurrentMarker::concurrent_sweep_budget`), and `0` likewise
/// restores the single-hold sweep.
///
/// A value that does not parse keeps the default. Read once per process: it
/// is a tuning knob, not compatibility state.
pub fn gen_conc_mark_slice() -> usize {
    static SLICE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SLICE.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_GEN_CONC_MARK_SLICE")
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
            .unwrap_or(DEFAULT_GEN_CONC_MARK_SLICE)
    })
}

/// Ownership of one open generational concurrent cycle, from
/// [`ConcurrentMarker::try_open_cycle`] until [`Self::complete`].
///
/// gen r4w2/concmark (2026-09-23). Dropping it without `complete` — an early
/// return on a lost pause, a stale epoch, or an unwind — ABANDONS the cycle:
/// the shared phase goes back to `Idle` and the SATB queue is deactivated, so
/// no exit path can leave the barrier armed and every later driver locked out.
///
/// `complete` is for the one exit where the sweep itself has already returned
/// the phase to `Idle`. That store is the hand-off: from that instant another
/// driver may open the NEXT cycle, so nothing on this side may write the
/// phase again — which is why `ConcurrentMarker::finish_cycle` is now a
/// compare-and-swap from `ConcurrentSweep` rather than a blind `Idle` store.
#[must_use = "dropping the ownership abandons the cycle"]
pub struct ConcurrentCycle<'m> {
    marker: &'m ConcurrentMarker,
    open: bool,
}

impl ConcurrentCycle<'_> {
    /// The sweep ran and returned the phase to `Idle`: release ownership
    /// WITHOUT touching the shared state again.
    pub fn complete(mut self) {
        self.open = false;
        self.marker.finish_cycle();
    }

    /// Abandon the cycle now. Same as dropping it; named for call sites.
    pub fn abort(self) {
        drop(self);
    }
}

impl Drop for ConcurrentCycle<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        let m = self.marker;
        if m.state.phase() == ConcurrentGcPhase::InitialMark && !m.satb_queue.is_active() {
            // `initial_mark` never ran (the initial-mark pause was lost): there
            // is no barrier to take down and no snapshot to drop, and the
            // thread-bucket walk `deactivate_and_discard` does would be pure
            // cost on the path that allocation storms take most often.
            let _ = m
                .state
                .cas_quiet_phase(ConcurrentGcPhase::InitialMark, ConcurrentGcPhase::Idle);
            // gcd d9/e: `[GC] conc_cycles: conccyc_open_lost`.
            m.state.note_open_lost();
        } else {
            m.abort_cycle();
        }
    }
}

impl ConcurrentMarker {
    /// GCAUD-4: count one cycle that reclaims nothing (more) because old-gen
    /// storage was reclaimed or relocated under it. Per VM, on the shared
    /// census ([`ConcurrentCycleCensus::sweep_epoch_aborts`]).
    ///
    /// gen r5w1/refs5 (`gengc-r4-mark-counters-and-comments-that-lie`): this
    /// was the process-global `pub static SWEEP_EPOCH_ABORTS`, read only by
    /// one test, documented as "between remark and the sweep" although remark
    /// also counted a reclaim between initial mark and remark, and summed
    /// across every VM in the process.
    fn note_sweep_epoch_abort(&self) {
        self.state
            .census
            .sweep_epoch_aborts
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Create a new concurrent marker for a heap region with PRIVATE SATB
    /// queue + phase state (tests / single-threaded callers only — mutator
    /// write barriers cannot see these instances; see [`Self::with_shared`]).
    pub fn new(old_gen_base: usize, old_gen_size: usize) -> Self {
        Self::with_shared(
            old_gen_base,
            old_gen_size,
            Arc::new(SatbQueue::new()),
            Arc::new(ConcurrentGcState::new()),
        )
    }

    /// Create a concurrent marker wired to the heap's shared SATB queue and
    /// phase state (the instances registered via
    /// `GenerationalHeap::enable_concurrent_gc`), so mutator `satb_barrier`
    /// logs are visible to this cycle's `remark`.
    pub fn with_shared(
        old_gen_base: usize,
        old_gen_size: usize,
        satb_queue: Arc<SatbQueue>,
        state: Arc<ConcurrentGcState>,
    ) -> Self {
        // gen r5w1/refs5 (`gengc-r4-mark-counters-and-comments-that-lie`): the
        // mark bitmap is anchored at the RAW old-gen base, while
        // `OldGenObjectStarts` anchors at `base & !7` because an `OldGen`'s
        // `Vec<u8>` base carries no alignment guarantee, and the bitmap's
        // `locate` refuses any address whose offset from ITS base is not
        // 8-aligned. On a misaligned base every `try_mark` of a real (8-aligned)
        // object would answer `false`: nothing marked, everything eligible
        // swept. The allocator has always returned an aligned base, so this
        // has never fired; it is stated here, where the disagreement lives,
        // and `remark_begin` refuses to authorise a sweep over such a bitmap
        // rather than trusting it (fail closed, never a crash in release).
        debug_assert!(
            old_gen_base % 8 == 0,
            "ConcurrentMarker: old-gen base {old_gen_base:#x} is not 8-aligned; \
             the mark bitmap could mark nothing"
        );
        Self {
            bitmap: MarkBitmap::new(old_gen_base, old_gen_size),
            queue: MarkQueue::new(),
            satb_queue,
            state,
            sweep_eligible: Mutex::new(None),
            initial_mark_epoch: Mutex::new(None),
            sweep_eligible_epoch: Mutex::new(None),
            starts_cache: Mutex::new(None),
            walk_desynced: AtomicBool::new(false),
            sweep_progress: Mutex::new(None),
            overflow_rescan: Mutex::new(OverflowRescan::default()),
            reference_skip: std::sync::OnceLock::new(),
            reference_skip_armed: AtomicBool::new(false),
            old_gen_base,
            old_gen_size,
            class_unload: RwLock::new(None),
            class_unload_armed: AtomicBool::new(false),
            layout_census: Mutex::new(None),
            record_retention_closure: AtomicBool::new(false),
            retention_closure: Mutex::new(Vec::new()),
            tams_starts: Mutex::new(None),
        }
    }

    /// gen r5w5/conc9 — this cycle's remark runs reference processing
    /// (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`), so its dead-finalizer
    /// retention must record what it marks. Call before the remark.
    ///
    /// # Why (`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`)
    ///
    /// The retention ([`Self::retain_dead_finalizer_candidates`]) runs in
    /// [`Self::remark_begin`], BEFORE the reference-processing seam, and marks
    /// every dead finalizable plus everything reachable only through it. The
    /// seam's verdict (`remark_is_marked`) then calls all of them live, so a
    /// `WeakReference` to a dead finalizable, or to an object reachable only
    /// through one, was NOT cleared at the remark: `finalize()` could resurrect
    /// the object while the weak reference still named it. HotSpot clears
    /// soft and weak references before it discovers finalizable objects. The
    /// post-collection path gets that from the collectors' resurrection
    /// closure (`ReferenceProcessor::note_resurrected_finalizables`); this is
    /// the same closure for the concurrent remark.
    pub fn record_finalizer_retention_closure(&self) {
        self.record_retention_closure.store(true, Ordering::Release);
    }

    /// gen r5w5/conc9 — the objects the last recording remark retention
    /// marked (retained finalizables first, then what their drain newly
    /// marked), taken once. Empty unless
    /// [`Self::record_finalizer_retention_closure`] was called and the remark
    /// found a dead finalizable. Every entry was UNMARKED at the first fixed
    /// point (the strong closure) and is marked now, which is exactly the
    /// processor's "marked only by the resurrection pass" contract. The
    /// closure can be SHORT (a scan that failed closed marked the whole
    /// generation without recording it): that is the depth-1 answer for what
    /// is missing, never a strongly reachable object in the set.
    pub fn take_finalizer_retention_closure(&self) -> Vec<usize> {
        std::mem::take(&mut *self.retention_closure.lock())
    }

    /// gen r5w4/conc8 — hand this cycle the retained-layout census candidates:
    /// the pending retained class ids ([`ConcurrentGcState::retained_layouts_pending`])
    /// of which the young generation holds no instance. Must be called inside
    /// the initial-mark pause, where the young walk that proved that ran. The
    /// sweep's walk then notes which of them still have an old-generation
    /// survivor, and a complete sweep releases the rest (see
    /// [`ConcurrentGcState::note_layouts_retained`]). An empty list arms
    /// nothing.
    pub fn set_layout_census(&self, candidates: Vec<u32>) {
        *self.layout_census.lock() = (!candidates.is_empty()).then_some(candidates);
    }

    /// gen r5w3/unload7 — arm concurrent class unloading for this cycle with
    /// the side tables its initial-mark root scan deferred to (see
    /// [`ClassUnloadTables`]). Must be called inside the initial-mark pause,
    /// after that root scan and BEFORE [`Self::initial_mark_with_frozen`], so
    /// no object is scanned without the edges. An empty capture arms nothing
    /// (no user loader has pinned anything, so the root scan deferred
    /// nothing).
    pub fn set_class_unload_tables(&self, tables: ClassUnloadTables) {
        let armed = !tables.is_empty();
        *self.class_unload.write() = armed.then_some(tables);
        self.class_unload_armed.store(armed, Ordering::Release);
    }

    /// gen r5w3/unload7 — fold the REMARK's capture into the cycle's tables.
    /// Call inside the remark pause, after its root scan and BEFORE
    /// [`Self::remark_begin`], which then marks the rows of every owner that
    /// is already live (see [`Self::mark_rows_of_live_owners`]). A remark
    /// root set taken under `with_class_unload_marking` deferred the CURRENT
    /// static values (and the other rows) again; an owner that turned black
    /// before this pause will not be scanned again, so its fresh rows are
    /// marked by that pre-pass rather than by a scan.
    pub fn add_class_unload_tables(&self, later: ClassUnloadTables) {
        let mut guard = self.class_unload.write();
        match guard.as_mut() {
            Some(tables) => tables.absorb(later),
            None => {
                if later.is_empty() {
                    return;
                }
                *guard = Some(later);
            }
        }
        self.class_unload_armed.store(true, Ordering::Release);
    }

    /// gen r5w3/unload7 — is this cycle following the class-loader side
    /// tables?
    pub fn class_unload_armed(&self) -> bool {
        self.class_unload_armed.load(Ordering::Acquire)
    }

    /// gen r5w3/unload7 — drop the tables (the mark is over, or the cycle).
    fn disarm_class_unload(&self) {
        self.class_unload_armed.store(false, Ordering::Release);
        *self.class_unload.write() = None;
    }

    /// gen r5w3/unload7 — the side-table edges of one scanned object: the
    /// `loader_pin` row of `class_id`, and the `mirror_pin` / `metadata_pin`
    /// rows `obj_ptr` owns. Each target is marked and handed to `push` exactly
    /// as a field referent is. Only called while the tables are armed.
    fn push_class_unload_edges<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        class_id: u32,
        object_starts: &OldGenObjectStarts,
        push: &mut F,
    ) {
        let guard = self.class_unload.read();
        let Some(tables) = guard.as_ref() else {
            return;
        };
        let mut newly = 0u64;
        let mut edge = |addr: usize| {
            let p = addr as *mut u8;
            if markable_old_object(p, object_starts) && self.bitmap.try_mark(addr) {
                newly += 1;
                push(p);
            }
        };
        if let Some(loader) = tables.loader_of(class_id) {
            edge(loader);
        }
        let owner = obj_ptr as usize;
        for table in [&tables.mirrors, &tables.metadata] {
            if let Some(values) = table.as_ref().and_then(|m| m.get(&owner)) {
                for &v in values {
                    edge(v);
                }
            }
        }
        drop(guard);
        self.state.note_class_unload_edges(newly);
    }

    /// gen r5w3/unload7 — mark (and queue) the mirror / metadata rows of every
    /// owner this cycle cannot judge dead: one already marked, or one that is
    /// not sweep-eligible (young, allocated after the initial mark, or not an
    /// old-gen object start at all — `remark_is_marked`'s verdict). Returns
    /// how many objects it marked.
    ///
    /// Two callers, one rule:
    ///
    /// * the initial mark (`initial_mark_with_frozen`): the owners that are
    ///   YOUNG. The concurrent trace never scans a young object, so a young
    ///   loader's rows would otherwise never be followed (hazard 2 on the
    ///   page, its loader half; the instance half is the driver's young walk);
    /// * the remark (`remark_begin`): every owner already live, so the rows
    ///   the REMARK's root scan deferred are followed for a loader that turned
    ///   black before the pause. An owner marked later in the remark is
    ///   scanned, and its scan follows the merged tables.
    ///
    /// Over-approximate in the safe direction: a stale young key (a loader
    /// that died young, whose row no reconcile pruned yet) keeps its rows one
    /// more cycle.
    fn mark_rows_of_live_owners(&self, object_starts: &OldGenObjectStarts) -> usize {
        if !self.class_unload_armed() {
            return 0;
        }
        let mut targets: Vec<usize> = Vec::new();
        {
            let guard = self.class_unload.read();
            let Some(tables) = guard.as_ref() else {
                return 0;
            };
            let eligible = self.sweep_eligible.lock();
            for (owner, values) in tables.owner_rows() {
                let live = self.bitmap.is_marked(owner)
                    || !eligible.as_ref().is_some_and(|e| e.contains(owner));
                if live {
                    targets.extend_from_slice(values);
                }
            }
        }
        let mut marked = 0usize;
        for addr in targets {
            let p = addr as *mut u8;
            if markable_old_object(p, object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(p);
                marked += 1;
            }
        }
        self.state.note_class_unload_edges(marked as u64);
        marked
    }

    /// gen r5w1/refs5 — publish this cycle's REFERENT-SLOT SKIP SET: the
    /// addresses of the old-gen `java.lang.ref.Reference` objects (weak, soft
    /// and phantom; never a `Finalizer` row, whose object IS the finalizable,
    /// nor a `Cleaner` row, whose layout is not referent-at-slot-0) whose
    /// slot 0 the trace must NOT follow. Must be called inside the
    /// initial-mark pause, BEFORE [`Self::initial_mark`] /
    /// [`Self::initial_mark_with_frozen`] (which scans frozen objects), so no
    /// object is scanned without the skip in force. Returns how many addresses
    /// landed in the set (addresses outside the old generation are ignored:
    /// a young `Reference`'s referent is kept by `collect_young_to_old_roots`,
    /// over-retention, unchanged).
    ///
    /// # Why (`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`)
    ///
    /// The marker traces every reference slot, a `Reference`'s referent
    /// included, so every referent of a live `Reference` is marked and a
    /// remark-time reference pass would be told "alive" for all of them.
    /// Hiding slot 0 — G1's INT-8 (`G1Collector::set_reference_skip_set`) —
    /// leaves the bitmap with an untainted verdict for each referent.
    ///
    /// # The one-commit rule
    ///
    /// Hiding without processing is a use-after-free: a referent the trace no
    /// longer reaches is unmarked while its `Reference` still points at it, and
    /// the sweep frees it. So a caller that publishes a set MUST run
    /// [`Self::remark_with_reference_processing`] (or the
    /// [`Self::remark_begin`] / [`Self::remark_finish`] pair) with a callback
    /// that clears or resurrects every unmarked referent of a live `Reference`
    /// in the set. The VM driver does both behind one flag
    /// (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`).
    ///
    /// # No re-keying
    ///
    /// Old-gen objects do not move during a cycle: a compaction or a free by
    /// another collector moves `reclaim_epoch` and the cycle is abandoned (and
    /// with it the sweep that could have freed a hidden referent). A
    /// `Reference` promoted mid-cycle is not in the set, so its referent is
    /// traced — over-retention, safe.
    pub fn set_reference_skip(&self, refs: &[usize]) -> usize {
        let skip = self
            .reference_skip
            .get_or_init(|| MarkBitmap::new(self.old_gen_base, self.old_gen_size));
        // No scan can be running: this is the initial-mark pause, before the
        // cycle's first scan (the `clear` exclusivity contract).
        self.reference_skip_armed.store(false, Ordering::Release);
        skip.clear();
        let mut published = 0usize;
        for &addr in refs {
            // `try_mark` answers `false` for an address outside the span (a
            // young `Reference`), which is exactly the filter wanted.
            if skip.try_mark(addr) {
                published += 1;
            }
        }
        if published > 0 {
            self.reference_skip_armed.store(true, Ordering::Release);
            self.state
                .census
                .reference_skip_published
                .fetch_add(published as u64, Ordering::Relaxed);
        }
        published
    }

    /// gen r5w1/refs5 — whether this cycle hides slot 0 of the object at
    /// `obj`. One relaxed load when no set is published.
    #[inline]
    fn referent_slot_hidden(&self, obj: *mut u8) -> bool {
        self.reference_skip_armed.load(Ordering::Acquire)
            && self
                .reference_skip
                .get()
                .is_some_and(|skip| skip.is_marked(obj as usize))
    }

    /// gen r5w1/refs5 — withdraw the skip set (abort, or the sweep's end).
    /// Only the gate: the bits are cleared by the next
    /// [`Self::set_reference_skip`], which runs inside a pause.
    fn disarm_reference_skip(&self) {
        self.reference_skip_armed.store(false, Ordering::Release);
    }

    /// gen r4w2/concmark — open a cycle on the shared phase, or `None` when
    /// another driver already owns one (see
    /// [`ConcurrentGcState::try_open_cycle`] for why this must be exclusive).
    pub fn try_open_cycle(&self) -> Option<ConcurrentCycle<'_>> {
        if self.state.try_open_cycle() {
            Some(ConcurrentCycle {
                marker: self,
                open: true,
            })
        } else {
            CONC_CYCLE_OPEN_REFUSED.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// gen r4w2/concmark — `true` once another old-gen collector has freed or
    /// slid storage since this cycle's initial mark. `remark` is then certain
    /// to refuse the sweep, so a driver can abandon the cycle without paying
    /// for the remark pause. `false` for a cycle that never ran `initial_mark`.
    pub fn cycle_is_stale(&self, old_gen: &OldGen) -> bool {
        matches!(
            *self.initial_mark_epoch.lock(),
            Some(opened) if opened != old_gen.reclaim_epoch()
        )
    }

    /// The object-start set for `old_gen` as it is NOW: the cached walk when
    /// `free_list_seq` has not moved since it was taken, a fresh walk (which
    /// then becomes the cache) otherwise. See [`Self::starts_cache`].
    fn object_starts_for(&self, old_gen: &OldGen) -> Arc<OldGenObjectStarts> {
        self.object_starts_for_noting(old_gen).0
    }

    /// [`Self::object_starts_for`], also saying how long a fresh walk took
    /// (`None`: the cached walk was reused). gcd d9/e: for the Phase-2 walk
    /// census (`[GC] conc_cycles: conccyc_phase2_start_walks`).
    fn object_starts_for_noting(
        &self,
        old_gen: &OldGen,
    ) -> (Arc<OldGenObjectStarts>, Option<std::time::Duration>) {
        let base = old_gen.base_ptr() as usize;
        let seq = old_gen.free_list_seq();
        let mut cache = self.starts_cache.lock();
        if let Some(c) = cache.as_ref() {
            if c.base == base && c.seq == seq {
                CONC_MARK_WALKS_REUSED.fetch_add(1, Ordering::Relaxed);
                return (Arc::clone(&c.starts), None);
            }
        }
        let t0 = std::time::Instant::now();
        let starts = Arc::new(old_gen_object_starts(old_gen));
        let took = t0.elapsed();
        self.note_walk(&starts);
        *cache = Some(ObjectStartsCache {
            base,
            seq,
            starts: Arc::clone(&starts),
        });
        (starts, Some(took))
    }

    /// gcd d9/e (2026-09-28) — the object-start set a PHASE-2 slice judges
    /// markability against.
    ///
    /// # Default: the generation as it is now
    ///
    /// [`Self::object_starts_for`]: the cached walk while `free_list_seq`
    /// stands still, a fresh O(old gen) walk the moment it moves. Every
    /// promotion moves it, so under steady promotion EVERY young collection
    /// between two slices costs the next slice a whole-generation walk — the
    /// cycle's marking time then scales with (young collections during the
    /// cycle) × (old-gen size), and a slow cycle is what lets the STW
    /// collection pre-empt it (`gengc-r4w3-oldgen3-stw-major-and-concurrent-
    /// cycle-share-one-trigger`). The walks and their time are counted on
    /// `[GC] conc_cycles:` (`conccyc_phase2_start_walks`,
    /// `conccyc_phase2_walk_ms`).
    ///
    /// # `CRATONVM_GEN_CONC_MARK_TAMS_STARTS`: the initial mark's snapshot
    ///
    /// Decided once per cycle, by `initial_mark` (which keeps the set in
    /// `tams_starts` only when the flag is on then). Phase 2 then uses the set
    /// `initial_mark` walked (G1's rule: objects above
    /// TAMS are implicitly live and are not traced by concurrent marking). An
    /// address outside it — an object allocated after the initial mark — is
    /// then not markable in Phase 2, so the slice neither marks nor traces it.
    /// Why that loses nothing the snapshot-at-the-beginning invariant needs:
    ///
    /// * such an object is not sweep-eligible (the eligibility snapshot IS the
    ///   initial mark's walk, only narrowed at remark), so it survives this
    ///   cycle whether it is marked or not;
    /// * every object that existed at the initial mark and was reachable then
    ///   is marked through the snapshot's own edges: from a root, from the
    ///   young→old seeds (every young object's old referents at the pause —
    ///   a young holder promoted later was seeded while it was young), or,
    ///   when a mutator later removes the old edge it was reached by, through
    ///   the SATB log of the overwritten value;
    /// * the remark still walks the CURRENT generation (`remark_begin` calls
    ///   [`Self::object_starts_for`]), re-marks the roots and the young→old
    ///   seeds against it and drains inside the pause, so an object reachable
    ///   only through a post-snapshot holder that a root or a young object
    ///   still names is traced there exactly as today.
    ///
    /// What it gives up is defence in depth for one shape: an old object
    /// reachable ONLY through a post-snapshot holder P that an already-scanned
    /// (black) old object names and no root or young object does, whose
    /// snapshot-time path was removed WITHOUT an SATB log. Today a slice
    /// after the promotion marks P and traces it; with the flag only a sound
    /// barrier covers it. Opt-in until the gce triage (2026-09-29) flipped it on after clean batteries; `=0` restores the per-promotion re-walk.
    ///
    /// The epoch check at the top of every slice is unchanged: a free or a
    /// compaction since the initial mark still abandons the cycle, so no
    /// snapshot address can name a recycled block.
    fn phase2_object_starts(&self, old_gen: &OldGen) -> Arc<OldGenObjectStarts> {
        // `Some` only when the flag was on at this cycle's initial mark.
        if let Some(starts) = self.tams_starts.lock().as_ref() {
            self.state
                .census
                .phase2_tams_slices
                .fetch_add(1, Ordering::Relaxed);
            return Arc::clone(starts);
        }
        let (starts, walked) = self.object_starts_for_noting(old_gen);
        if let Some(took) = walked {
            let c = &self.state.census;
            c.phase2_start_walks.fetch_add(1, Ordering::Relaxed);
            c.phase2_walk_us.fetch_add(
                u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        starts
    }

    /// Record a fresh walk: count it, and latch `walk_desynced` if it did not
    /// cover every allocated byte.
    fn note_walk(&self, starts: &OldGenObjectStarts) {
        CONC_MARK_WALKS_BUILT.fetch_add(1, Ordering::Relaxed);
        if !starts.walk_complete() {
            self.walk_desynced.store(true, Ordering::Relaxed);
        }
    }

    /// Abort the cycle WITHOUT sweeping — the mark bitmap is not final (e.g.
    /// the remark STW could not be acquired). Deactivates the SATB barrier
    /// (discarding the drained entries — they only matter to a sweep that is
    /// no longer happening) and returns the phase to Idle so the write
    /// barrier stops logging. The next `old_gen_needs_gc` trigger starts a
    /// fresh cycle.
    pub fn abort_cycle(&self) {
        // gen r4/mark (2026-09-23) — CLOSE THE MUTATOR GATE FIRST, exactly as
        // `remark` was fixed to do on 2026-09-20. This used to deactivate the
        // queue first and return the phase to Idle last, and an abort runs
        // OUTSIDE any pause: for that whole window mutators passed the phase
        // gate (`is_marking_active()`, and the still-armed JIT gate) and had
        // their log refused by the INACTIVE queue. Harmless since round 2 made
        // the refusal a drop, but it breaks `is_marking_active() =>
        // satb_queue.is_active()` and bumps `SatbQueue::late_log_drops`, whose
        // contract is "zero unless some caller's gate is WIDER than the
        // queue's" — which is the one signal that would catch a real ordering
        // regression. The post-conditions are unchanged.
        // gcd d9/e: an OPENED cycle (past its initial mark) being abandoned,
        // for `[GC] conc_cycles: conccyc_abandoned`.
        if self.state.cycle_in_progress() {
            self.state.note_cycle_abandoned();
        }
        self.state.set_phase(ConcurrentGcPhase::Idle);
        // An abort is not a safepoint, and this caller discards the entries
        // anyway -- so it takes the form that cannot claim a snapshot.
        // `deactivate_and_drain` now requires a `StopTheWorldToken`; this site
        // could not honestly produce one.
        self.satb_queue.deactivate_and_discard();
        self.queue.clear();
        // No sweep will run for this cycle; make sure a later cycle's sweep
        // can never consume this cycle's stale eligibility snapshot.
        *self.sweep_eligible.lock() = None;
        *self.initial_mark_epoch.lock() = None;
        *self.sweep_eligible_epoch.lock() = None;
        // gen r4w2/concmark: and the cached walk, which is up to 1/64th of the
        // old generation and describes a layout no one will ask about again.
        *self.starts_cache.lock() = None;
        *self.tams_starts.lock() = None;
        // gen r4w3/oldgen3: a sweep abandoned between two slices, and an
        // overflow rescan abandoned part-way. Neither may be resumed by
        // anything: the resume offsets describe this cycle's layout.
        *self.sweep_progress.lock() = None;
        *self.overflow_rescan.lock() = OverflowRescan::default();
        // gen r5w1/refs5: no sweep will run, so no hidden referent can be
        // freed; the skip set describes nothing any more.
        self.disarm_reference_skip();
        // gen r5w3/unload7: nor will anything be scanned again.
        self.disarm_class_unload();
        // gen r4w4/concmark4: an abandoned cycle measured nothing the start
        // policy can use (its growth sample would stop at an arbitrary point).
        self.state.note_cycle_dropped();
        // No other cycle can have opened on the shared state yet, although the
        // phase is already `Idle`: the next cycle's `initial_mark` (which is
        // what re-activates the queue this call just deactivated) runs inside a
        // stop-the-world pause, and that pause cannot complete until THIS
        // thread reaches a safepoint — which it does not do inside this call.
        // A driver that opened in between (`try_open_cycle`) is still waiting
        // for that pause.
    }

    /// Mark the cycle complete after the sweep: phase back to Idle.
    ///
    /// gen r4w2/concmark (2026-09-23): only FROM `ConcurrentSweep`. The sweep
    /// already returns the phase to `Idle` on every path, and that store is
    /// the hand-off to the next cycle (see [`ConcurrentCycle`]); a blind
    /// `Idle` store here used to be able to land after another driver had
    /// opened the next cycle, reopening the shared state under it so that a
    /// THIRD driver could open concurrently with it. Now it can only complete
    /// a cycle that is still this one.
    pub fn finish_cycle(&self) {
        let _ = self
            .state
            .cas_quiet_phase(ConcurrentGcPhase::ConcurrentSweep, ConcurrentGcPhase::Idle);
    }

    /// Phase 1: Initial Mark (called during brief STW pause).
    ///
    /// Marks objects directly reachable from roots. Only marks old-gen objects;
    /// young-gen objects are handled by the minor GC.
    ///
    /// Returns the number of root objects marked.
    pub fn initial_mark(&self, roots: &[*mut u8], old_gen: &OldGen) -> usize {
        self.state.set_phase(ConcurrentGcPhase::InitialMark);
        self.bitmap.clear();
        self.queue.clear();
        // gen r4w3/oldgen3: a fresh cycle starts with no rescan in flight and
        // its full budget of concurrent overflow passes; any sweep progress
        // left over belongs to a cycle that is over.
        *self.overflow_rescan.lock() = OverflowRescan::default();
        *self.sweep_progress.lock() = None;

        // Arm the barrier BEFORE anything is marked. `set_phase`'s own doc
        // states the rule this follows -- a logging gate must be armed for a
        // SUPERSET of the interval in which it is required, never a subset --
        // and the queue gate is one such gate. Activating after the root scan
        // was safe only because this runs at a stop-the-world; arming first
        // costs nothing and removes the dependence.
        self.satb_queue.activate();

        // gen r4w2/concmark: ONE walk, two sets. `object_starts` becomes the
        // sweep-eligibility snapshot below and is narrowed in place at remark,
        // so it cannot be shared; the second copy seeds the slice cache, so the
        // first Phase-2 slice (which usually follows this pause directly) does
        // not walk the generation again.
        self.walk_desynced.store(false, Ordering::Relaxed);
        let objects = old_gen.walk_objects();
        let object_starts = OldGenObjectStarts::build_from(old_gen, &objects);
        self.note_walk(&object_starts);
        let cached = Arc::new(OldGenObjectStarts::build_from(old_gen, &objects));
        // gcd d9/e: under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` (decided once
        // per cycle, here) the same immutable set is the TAMS snapshot Phase 2
        // judges against (`phase2_object_starts`); no extra walk.
        *self.tams_starts.lock() = gen_conc_mark_tams_starts_enabled().then(|| Arc::clone(&cached));
        *self.starts_cache.lock() = Some(ObjectStartsCache {
            base: old_gen.base_ptr() as usize,
            seq: old_gen.free_list_seq(),
            starts: cached,
        });
        drop(objects);
        let mut count = 0;
        for &root_ptr in roots {
            if markable_old_object(root_ptr, &object_starts)
                && self.bitmap.try_mark(root_ptr as usize)
            {
                self.queue.push(root_ptr);
                count += 1;
            }
        }

        // TAMS (gengc-mark 2026-09-20) — the sweep's eligibility snapshot is
        // taken HERE, at initial mark, not at remark.
        //
        // The rule a snapshot-based concurrent collector needs is "an object
        // allocated after the mark started is implicitly live for this cycle"
        // (G1's TAMS, HotSpot's `top_at_mark_start`). The snapshot used to be
        // taken at REMARK, which is a strictly LARGER set — it also contains
        // everything allocated DURING the concurrent phase — and the extra
        // members are exactly the objects the cycle cannot have marked:
        //
        //   * a young GC promotes survivor X into the old gen while the
        //     concurrent trace is running;
        //   * X is stored into old object O, which the trace has already
        //     scanned (O is black);
        //   * the SATB pre-barrier logs the OLD value of O's slot, not X --
        //     logging X is the job of an allocate-black / post-write barrier,
        //     and there is none;
        //   * remark rescans roots and young->old edges, which finds X only if
        //     something young still points at it.
        //
        // X is then unmarked AND in the remark snapshot, which is precisely the
        // sweep's licence to free it: a live object reclaimed. Anchoring the
        // snapshot at initial mark makes X ineligible by construction; it is
        // simply collected one cycle later. `reclaim_epoch` is stamped into
        // `initial_mark_epoch` at the same instant, so the GCAUD-4 identity
        // check covers the whole cycle rather than only its tail.
        //
        // AUTHORISATION IS SEPARATE FROM THE SNAPSHOT. `sweep_eligible_epoch`
        // is deliberately left `None` here and is set only by a `remark` that
        // completes: the sweep's abort-safe default is "no authorising stamp ⇒
        // free nothing", and moving the SNAPSHOT earlier must not accidentally
        // move the AUTHORISATION earlier with it. A cycle whose remark STW
        // could not be acquired (the driver's `abort_cycle` path, and any
        // caller that skips remark) therefore still reclaims nothing — the
        // same guarantee the old "empty snapshot ⇒ free nothing" arrangement
        // gave, now stated as an explicit gate instead of an emergent one.
        *self.sweep_eligible.lock() = Some(object_starts);
        *self.initial_mark_epoch.lock() = Some(old_gen.reclaim_epoch());
        *self.sweep_eligible_epoch.lock() = None;

        // gen r4w4/concmark4: the start policy's growth sample is measured
        // from here to the sweep's end (see `OldGenPolicy`).
        self.state
            .note_cycle_open(old_gen_allocated_total(old_gen), old_gen.used());

        self.state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        count
    }

    /// [`Self::initial_mark`], then — still inside the initial-mark pause —
    /// SCAN every old-gen object in `frozen`: the objects held in the
    /// registers and stacks of threads the xt takeover FROZE for this pause.
    ///
    /// gen r4w4/concmark4 (2026-09-24) — closes
    /// `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md`.
    ///
    /// # The window
    ///
    /// Compiled reference stores test the JIT SATB gate once and skip the
    /// pre-barrier when it reads clear; nothing between that test and the
    /// store is a safepoint, so a thread that ARRIVES at a pause can never be
    /// between them. A thread the takeover FREEZES can: it tested the gate
    /// before this pause armed it, and after the pause it completes
    /// `O.f = B` without logging `A`, the value `O.f` held at the snapshot. If
    /// a second mutator has meanwhile copied `A` into an object the trace has
    /// already scanned, `A` is reachable, unmarked, and swept.
    ///
    /// # Why scanning `O` inside the pause closes it
    ///
    /// SATB's obligation is to mark everything reachable AT THE SNAPSHOT, and
    /// the one value the frozen store can hide is `O.f`'s value at the
    /// snapshot — which is still in `O` while the world is stopped. Scanning
    /// `O` here greys `A` (and every other reference `O` holds) before the
    /// frozen thread can overwrite anything, so the unlogged store loses
    /// nothing. `O` is in `frozen`: both gated emitters
    /// (`emit_gated_compact_ref_putfield`, the gated `aastore`) load the
    /// receiver into RAX from its frame slot BEFORE the gate test and store
    /// through it, so the receiver sits in a register and in the frame for the
    /// whole window, and the takeover's conservative scan reports both (a
    /// derived pointer resolved to its base, `CRATONVM_XT_TAKEOVER_INTERIOR`).
    /// A YOUNG `O` needs nothing: its old-gen referents are already initial-mark
    /// roots (`collect_young_to_old_roots`).
    ///
    /// No cycle is refused and no thread is waited for, so a JIT-heavy run
    /// that freezes someone at every pause (the starvation the page feared for
    /// its "refuse on takeover" option) costs only the scan of the few objects
    /// the frozen threads hold. `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` turns
    /// it off (bisection only). Returns [`Self::initial_mark`]'s count.
    pub fn initial_mark_with_frozen(
        &self,
        roots: &[*mut u8],
        frozen: &[*mut u8],
        old_gen: &OldGen,
    ) -> usize {
        let count = self.initial_mark(roots, old_gen);
        // gen r5w3/unload7: with class unloading armed, the rows of the owners
        // the trace will never scan (young loaders) are marked now, inside the
        // pause; see `mark_rows_of_live_owners`. One load when unarmed.
        if self.class_unload_armed() {
            let object_starts = self.object_starts_for(old_gen);
            self.mark_rows_of_live_owners(&object_starts);
        }
        if frozen.is_empty() || crate::gc_flags().gen_conc_no_frozen_eager_scan {
            return count;
        }
        // The walk `initial_mark` just cached: same lock hold, no allocation
        // in between, so this is a cache hit, not a second walk.
        let object_starts = self.object_starts_for(old_gen);
        let mut scanned = 0u64;
        for &obj_ptr in frozen {
            if !markable_old_object(obj_ptr, &object_starts) {
                continue;
            }
            // Marked (it is a root, so usually already by `initial_mark`) and
            // scanned NOW. If it was not yet marked it is black after this
            // scan and need not be queued; if it was, the queued copy is
            // scanned again later, which only re-marks what is marked.
            let _ = self.bitmap.try_mark(obj_ptr as usize);
            if !self.scan_object(obj_ptr, old_gen, &object_starts) {
                // Fail closed exactly as the trace does.
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            scanned += 1;
        }
        self.state.note_frozen_objects_scanned(scanned);
        count
    }

    /// Phase 2: Concurrent Mark — process the mark queue until empty.
    ///
    /// Called by marker thread(s). This runs concurrently with application
    /// threads. The SATB barrier ensures correctness by logging overwritten
    /// references.
    ///
    /// Returns the number of objects scanned.
    pub fn concurrent_mark(&self, old_gen: &OldGen) -> usize {
        self.concurrent_mark_budget(old_gen, usize::MAX).0
    }

    /// Phase 2 in a BOUNDED SLICE: scan at most `budget` objects, then return.
    ///
    /// Returns `(objects scanned, closure complete)`. `true` for the second
    /// element means the queue drained (or the cycle degraded to
    /// `mark_all_old_gen`) and no further slice is needed.
    ///
    /// # Why this exists (gengc-mark-concurrent-mark-holds-the-old-gen-lock)
    ///
    /// The driver runs Phase 2 as
    ///
    /// ```text
    /// if let Some(guard) = shared.mem.heap.old_gen_lock() {
    ///     marker.concurrent_mark(&*guard);
    /// }
    /// ```
    ///
    /// — one acquisition of the heap's `Mutex<OldGen>` held for the whole
    /// transitive closure over the old generation, which is the longest phase
    /// of the cycle and unbounded in the size of the live old-gen graph. Every
    /// promotion path takes that same lock, so for the duration of the
    /// "concurrent" mark no thread can tenure a survivor or allocate a large
    /// object directly into the old generation. A young GC that needs to
    /// promote blocks on a lock held by the phase whose entire purpose is not
    /// to block mutators.
    ///
    /// This entry point is the collector-side half of the fix. The driver
    /// (`maybe_concurrent_gc_at` in `vm/src/runtime/interpreter/gc_and_alloc.rs`,
    /// gen r4w2/concmark 2026-09-23) now loops
    /// `lock → slice → unlock → safepoint poll` and so bounds, by the slice
    /// budget rather than by the size of the heap, both the time any promoting
    /// thread waits for the old-gen lock and the time any other thread's pause
    /// waits for the marker to arrive. `CRATONVM_GEN_CONC_MARK_SLICE=0`
    /// restores the single unbounded call.
    ///
    /// # What the caller must know about slicing
    ///
    /// * **The object-start set must describe the generation as it is at THIS
    ///   slice.** An object promoted between two slices has to be markable in
    ///   the next one: refusing to enqueue it means refusing to scan its
    ///   children, one of which may be an older object that IS sweep-eligible
    ///   and would then be freed while live. This is the correctness question
    ///   the whole-phase lock was masking.
    /// * **It is cached, not rebuilt, while that is provably the same set**
    ///   (gen r4w2/concmark): keyed on `OldGen::free_list_seq`, which every
    ///   promotion and every other allocation or free moves. A slice after a
    ///   young GC that promoted anything therefore still rebuilds; a slice
    ///   after one that did not, or after no pause at all, reuses the previous
    ///   walk instead of paying O(old gen) again. See `starts_cache`.
    /// * The cycle is still abandoned at remark if `reclaim_epoch` moved, so a
    ///   `free` or `compact` landing between slices fails the cycle closed
    ///   exactly as one landing anywhere else in the cycle does; and a slice
    ///   that finds the epoch moved drops the queue and reports the closure
    ///   complete (see [`Self::cycle_is_stale`]).
    ///
    /// A `budget` of 0 is raised to 1, so a caller cannot construct a loop
    /// that makes no progress.
    pub fn concurrent_mark_budget(&self, old_gen: &OldGen, budget: usize) -> (usize, bool) {
        let budget = budget.max(1);
        // gen r4/mark (2026-09-23): stop tracing a cycle that is already dead.
        //
        // The queue holds bare addresses pushed by earlier slices (and by
        // `initial_mark`). If another old-gen collector freed or slid storage
        // since the cycle opened, `reclaim_epoch` has moved, `remark` is
        // guaranteed to fail the cycle closed, and every queued address may now
        // name a recycled block or a stale free-list header. Scanning them is
        // pure waste at best; at worst an incoherent stale header trips
        // `scan_object`'s refusal and degrades the slice to `mark_all_old_gen`
        // — an O(old gen) pass for a cycle nobody will sweep. A cycle that
        // never went through `initial_mark` (tests that seed the queue by hand)
        // has no epoch and is left alone.
        if self.cycle_is_stale(old_gen) {
            self.queue.clear();
            // gen r4w3/oldgen3: a rescan cursor is an offset into a layout
            // that no longer exists.
            self.overflow_rescan.lock().cursor = None;
            return (0, true);
        }
        let mut rescan = self.overflow_rescan.lock();
        // gen r4w3/oldgen3 — drain the SATB log in Phase 2, not only at remark
        // (`docs/internal/gc-common-round-20260923/common-g-gen-satb-queue-unbounded-during-concurrent-mark-FIXED-20260923.md`,
        // whose proposed fix names this file's owner). The shards are
        // unbounded, so a mutation-heavy cycle grew them by eight bytes per
        // logged store for its whole length and remark then marked all of it
        // inside the pause. Draining with `drain()` while the queue stays
        // ACTIVE is exactly remark's first pass, run earlier: every entry is
        // an old value some mutator overwrote since initial mark, marking it
        // is conservative, and whatever is logged after this drain is still
        // in the queue for remark. Only while the queue is active (a cycle is
        // marking) — `has_pending` is one lock-free load.
        let satb_pending = self.satb_queue.is_active() && self.satb_queue.has_pending();
        // gen r4w2/concmark: an empty queue needs no object-start set, and a
        // driver that re-enters Phase 2 after losing its remark pause usually
        // finds exactly that. Same result as the pop below returning `None`.
        // gen r4w3/oldgen3: unless an overflow rescan or the SATB log still
        // has work for this slice.
        if self.queue.is_empty()
            && !satb_pending
            && !Self::overflow_rescan_due(&self.queue, &rescan)
        {
            return (0, true);
        }
        let mut scanned = 0usize;
        // gen r4w2/concmark: cached across slices while `free_list_seq` is
        // unchanged; rebuilt (and so still covering anything promoted since the
        // previous slice) the moment it moves. See `starts_cache`.
        // gcd d9/e: through `phase2_object_starts`, which counts those walks
        // and, under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS`, uses the initial
        // mark's snapshot instead.
        let object_starts = self.phase2_object_starts(old_gen);
        if satb_pending {
            // The same admission `remark` applies to its SATB entries. The
            // epoch was checked above, so every old-gen address in the log
            // still names the object it named when it was logged.
            let entries = self.satb_queue.drain();
            self.state
                .census
                .satb_drained_in_mark
                .fetch_add(entries.len() as u64, Ordering::Relaxed);
            // gen r4w6/concsvc6: how many of them the barrier need never have
            // logged (outside the old generation; `markable_old_object`
            // rejects every one). A range test, counted only.
            let mut outside = 0u64;
            for addr in entries {
                let ptr = addr as *mut u8;
                outside += u64::from(!old_gen.contains(ptr as *const u8));
                if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                    self.queue.push(ptr);
                }
            }
            if outside != 0 {
                self.state
                    .census
                    .satb_outside_old_gen
                    .fetch_add(outside, Ordering::Relaxed);
            }
        }

        // gen r4w4/concmark4 — the marker-local stack (costs page item 4): the
        // slice pops and pushes through a private LIFO `Vec`, touching the
        // sharded queue only to refill an empty stack or to spill. Whatever is
        // left when the budget runs out is spilled back below, so between
        // slices (and for the stale check, remark and `abort_cycle`) the queue
        // is again the whole grey set.
        let use_local = !crate::gc_flags().gen_conc_mark_no_local_stack;
        let mut local: Vec<*mut u8> = Vec::new();
        while scanned < budget {
            if use_local {
                let (n, degraded) =
                    self.drain_local(old_gen, &object_starts, &mut local, budget - scanned);
                scanned += n;
                if degraded {
                    // Degraded path: every old-gen object is marked for this
                    // cycle, so there is nothing left to slice.
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    rescan.cursor = None;
                    return (scanned, true);
                }
                if scanned >= budget {
                    break;
                }
                // Both the stack and the queue are empty: fall through to the
                // overflow rescan, exactly where the queue arm's `pop` found
                // nothing.
            } else if let Some(obj_ptr) = self.queue.pop() {
                if !self.scan_object(obj_ptr, old_gen, &object_starts) {
                    // Degraded path: every old-gen object is marked for this
                    // cycle, so there is nothing left to slice.
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    rescan.cursor = None;
                    return (scanned, true);
                }
                scanned += 1;
                continue;
            }

            // gen r4w3/oldgen3 — MARK-STACK OVERFLOW IS RECOVERED HERE, NOT
            // ONLY IN THE REMARK PAUSE (item 2 of
            // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`).
            //
            // A `push` dropped at `MARK_QUEUE_SHARD_CAP` leaves an object MARKED
            // but never scanned, and `drain_closure` repairs that with up to
            // eight O(old gen) rescans of every marked object — inside the
            // remark pause, the one place this cycle is supposed to be brief.
            // The same rescan is sound here: rescanning a marked object while
            // mutators run can only find more to mark, and anything a mutator
            // hides from it by overwriting a slot is in the SATB log remark
            // drains. So when a slice empties the queue with the flag set,
            // clear the flag and walk the generation in budgeted pieces
            // (`OldGen::walk_objects_from`), rescanning marked objects and
            // draining what they push, and only report the closure complete
            // once a whole pass has run with no new overflow.
            //
            // Why a pass started after the flag was cleared is enough: every
            // object dropped BEFORE it is marked and does not move (the cycle
            // aborts above if anything frees or slides old-gen storage), so the
            // pass reaches it; every object dropped DURING it sets the flag
            // again, which starts another pass here or — once
            // `CONCURRENT_OVERFLOW_RESCAN_PASSES` are spent — is left set for
            // remark's own fallback, exactly as before. Remark also re-arms the
            // flag if it ever finds a pass part-way (see
            // `remark_with_reference_processing`).
            if rescan.cursor.is_none() {
                if !Self::overflow_rescan_due(&self.queue, &rescan) {
                    return (scanned, true);
                }
                // Reset before the pass so an overflow during it is observable
                // — the same protocol `drain_closure` follows.
                self.queue.overflowed.store(false, Ordering::Relaxed);
                rescan.passes += 1;
                rescan.cursor = Some(0);
                self.state
                    .census
                    .overflow_rescan_passes
                    .fetch_add(1, Ordering::Relaxed);
            }
            let from = rescan.cursor.unwrap_or(0);
            let mut visited = 0usize;
            let mut degraded = false;
            let next = old_gen.walk_objects_from(from, budget - scanned, |obj_ptr, _size| {
                visited += 1;
                if !degraded
                    && self.bitmap.is_marked(obj_ptr as usize)
                    && !self.scan_object(obj_ptr, old_gen, &object_starts)
                {
                    degraded = true;
                }
            });
            // Every object walked is work done, marked or not.
            scanned += visited.max(1);
            if degraded {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                rescan.cursor = None;
                return (scanned, true);
            }
            // `Some` = the budget ran out part-way (the loop ends); `None` =
            // the pass finished, and the loop drains what it pushed before
            // deciding whether another pass is due.
            rescan.cursor = next;
        }

        // gen r4w4/concmark4: the budget ran out with grey objects still on
        // the local stack — hand them back to the shared queue.
        self.spill_local(&mut local);
        let done = self.queue.is_empty()
            && rescan.cursor.is_none()
            && !Self::overflow_rescan_due(&self.queue, &rescan);
        (scanned, done)
    }

    /// gen r4w3/oldgen3 — should a Phase-2 slice start (or continue) a
    /// concurrent overflow rescan? A pass part-way through always continues;
    /// a new one starts while the flag is set and the per-cycle pass budget
    /// is not spent.
    fn overflow_rescan_due(queue: &MarkQueue, rescan: &OverflowRescan) -> bool {
        rescan.cursor.is_some()
            || (queue.has_overflowed() && rescan.passes < CONCURRENT_OVERFLOW_RESCAN_PASSES)
    }

    /// gen r4w4/concmark4 — scan grey objects through the marker-local stack
    /// `local`, refilled one object at a time from the shared queue when it is
    /// empty, until both are empty or `budget` objects have been scanned.
    /// Returns `(objects scanned, degraded)`; `degraded` means a scan refused
    /// an object (the caller marks everything, as with the queue).
    ///
    /// # Why a local stack (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 4)
    ///
    /// Every `MarkQueue::push` takes a shard mutex and a `fetch_or` on the
    /// non-empty mask, and every `pop` a mutex, a `fetch_and` and a
    /// thread-local cursor bump — per grey object, on a marker that is single-
    /// threaded in every production drain. Here a child goes on a `Vec` and
    /// comes off it: no lock, no RMW. The queue stays the spill space, so its
    /// cap, its overflow flag and the overflow rescan keep their meaning:
    /// past [`LOCAL_MARK_STACK_CAP`] entries a child is pushed to the queue
    /// instead (counted in `mark_local_spills`), and a caller that stops with
    /// entries left calls [`Self::spill_local`].
    ///
    /// LIFO (depth-first) rather than the queue's FIFO: the frontier — and so
    /// the memory, and the overflow risk the round-2 note worried about — is
    /// bounded by the graph's depth × fan-out instead of its breadth. The
    /// fixed point is the same: a child is marked before it is pushed, on
    /// either path, so the set of objects scanned does not depend on order.
    ///
    /// `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` restores the queue-only drain.
    fn drain_local(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        local: &mut Vec<*mut u8>,
        budget: usize,
    ) -> (usize, bool) {
        let mut scanned = 0usize;
        let mut spills = 0u64;
        let mut degraded = false;
        while scanned < budget {
            let Some(obj_ptr) = local.pop().or_else(|| self.queue.pop()) else {
                break;
            };
            let queue = &self.queue;
            let ok = self.scan_object_into(obj_ptr, old_gen, object_starts, &mut |p| {
                if local.len() < LOCAL_MARK_STACK_CAP {
                    local.push(p);
                } else {
                    spills += 1;
                    queue.push(p);
                }
            });
            if !ok {
                local.clear();
                degraded = true;
                break;
            }
            scanned += 1;
        }
        self.state.note_mark_local_spills(spills);
        (scanned, degraded)
    }

    /// gen r4w4/concmark4 — hand every entry left on a marker-local stack
    /// back to the shared queue (each is marked and unscanned; a push the
    /// queue drops at its cap sets the overflow flag, as for any push).
    fn spill_local(&self, local: &mut Vec<*mut u8>) {
        let n = local.len() as u64;
        for p in local.drain(..) {
            self.queue.push(p);
        }
        self.state.note_mark_local_spills(n);
    }

    /// gen r4w4/concmark4 — drain the queue to its closure with no budget,
    /// through the local stack unless `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK`.
    /// `None` = a scan refused an object (the caller degrades). For the remark
    /// pause, where the per-object queue cost is pause time.
    fn drain_unbounded(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> Option<usize> {
        if !crate::gc_flags().gen_conc_mark_no_local_stack {
            let mut local: Vec<*mut u8> = Vec::new();
            let (n, degraded) = self.drain_local(old_gen, object_starts, &mut local, usize::MAX);
            return (!degraded).then_some(n);
        }
        let mut n = 0usize;
        while let Some(obj_ptr) = self.queue.pop() {
            if !self.scan_object(obj_ptr, old_gen, object_starts) {
                return None;
            }
            n += 1;
        }
        Some(n)
    }

    /// Phase 3: Remark (called during brief STW pause).
    ///
    /// Processes all SATB buffer entries and re-scans roots to catch any
    /// references modified during the concurrent mark phase.
    ///
    /// Returns the number of objects the remark scanned — no longer "the
    /// additional objects discovered", which it double-counted (see
    /// [`Self::remark_with_reference_processing`]).
    ///
    /// gen r4w5/concmark5 (2026-09-24): the Generational-side hook for
    /// remark-time reference processing
    /// (`docs/internal/gc/gengc-r4w5-concmark5-remark-reference-processing-design-DONE-20260929.md`).
    /// With `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` set, this remark goes
    /// through the reference-processing seam with a NO-OP callback: it is
    /// called at the seam's place in the pause (first fixed point, barrier
    /// active, phase `Remark`), counts itself (`remark_refproc_hook_calls`),
    /// clears nothing and keeps nothing; the bitmap is exactly the unhooked
    /// one. Unset — the default — this is the plain remark.
    ///
    /// gen r5w1/refs5: this entry point is now tests-only. The VM driver
    /// (`gen_concurrent_remark_pause`) has called the seam directly since
    /// gc-common w8-c (its JNI weak-global callback), so this no-op hook never
    /// ran in a real VM and the wave-5 probe's `concdrv_remark_refproc_hook_calls`
    /// stayed 0. Under the same flag the driver now runs the REAL remark-time
    /// reference processing (skip set at initial mark, callback at remark),
    /// which counts itself through [`ConcurrentGcState::note_remark_refproc_retired`].
    pub fn remark(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
    ) -> usize {
        if crate::gc_flags().gen_conc_remark_refproc_hook {
            let state = &self.state;
            let mut hook = |_is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
                state.note_remark_refproc_hook();
                Vec::new()
            };
            let hook_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut hook;
            return self.remark_with_reference_processing(stw, roots, old_gen, Some(hook_dyn));
        }
        self.remark_with_reference_processing(stw, roots, old_gen, None)
    }

    /// [`Self::remark`] with a remark-time reference-processing seam
    /// (gen r4w2/concmark, 2026-09-23; step 1 of
    /// `gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md`).
    ///
    /// `process`, when given, is called ONCE, inside the pause, after the
    /// closure over the SATB log and the fresh roots has reached its first
    /// fixed point and while the SATB barrier is still ACTIVE, with the cycle's
    /// liveness verdict: `is_marked(addr)` is "the bit is set in this cycle's
    /// bitmap, OR `addr` is not sweep-eligible" (young, allocated after the
    /// initial mark, or outside the old generation — none of which this cycle
    /// may judge dead). It is the same shape as G1's
    /// `VmHeap::g1_final_remark_and_cleanup` callback. The addresses it returns
    /// (policy-retained soft referents, finalizable objects about to be
    /// resurrected, submitted cleaner actions) are marked and their closure is
    /// drained before the phase advances, so they are in the bitmap
    /// `concurrent_sweep` is handed. A referent clear the callback performs
    /// therefore still runs under an active barrier; it should use the
    /// SATB-suppressed store, exactly as G1's does.
    ///
    /// Not invoked when this remark refuses the sweep (epoch moved, walk
    /// desynced): nothing can be judged dead on such a cycle.
    ///
    /// # Lock order
    ///
    /// The caller's callback will typically hold the VM's reference-processor
    /// lock while it queries `is_marked`, and `is_marked` takes
    /// `sweep_eligible` PER QUERY. This function holds no marker lock across
    /// the call, so the order is only ever `ref_processor → sweep_eligible`
    /// and nothing takes the reverse (round-3 `refdriver`'s note on the page).
    ///
    /// The CALLER's old-gen guard (`old_gen`) is held across the callback
    /// here. A callback that may take the old-gen lock itself (any heap
    /// accessor that locks the generation, e.g. `is_heap_addr` or the soft
    /// policy's `soft_ref_policy_free_mb`) must use the
    /// [`Self::remark_begin`] / [`Self::remark_finish`] pair instead and run
    /// with the guard released (gen r5w1/refs5; the design page's option (b)).
    ///
    /// # Return value
    ///
    /// The objects this remark SCANNED (each grey object once; an overflow
    /// rescan pass counts every marked object it rescans), plus the walk-gap
    /// seeds a recovering remark marked. gen r5w1/refs5: it used to add one
    /// per newly marked SATB entry, root and keep address ON TOP of the scan
    /// count, which scans those same objects again — a double count
    /// (`gengc-r4-mark-counters-and-comments-that-lie`).
    pub fn remark_with_reference_processing(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
        process: Option<&mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize>>,
    ) -> usize {
        let progress = self.remark_begin(stw, roots, old_gen);
        let keep = match process {
            Some(process) if progress.sweep_authorised() => {
                // Per-query `sweep_eligible` lock; see "Lock order" above.
                let is_marked = |addr: usize| -> bool { self.remark_is_marked(addr) };
                Some(process(&is_marked))
            }
            _ => None,
        };
        self.remark_finish(stw, progress, keep.as_deref(), old_gen)
    }

    /// gen r5w1/refs5 — the seam's liveness verdict, for a callback that runs
    /// between [`Self::remark_begin`] and [`Self::remark_finish`]: the bit is
    /// set in this cycle's bitmap, OR `addr` is not sweep-eligible (young,
    /// allocated after the initial mark, or outside the old generation — none
    /// of which this cycle may judge dead). Takes `sweep_eligible` per query
    /// and holds nothing else, so a caller holding the VM's reference-processor
    /// lock keeps the order `ref_processor → sweep_eligible`. Needs no old-gen
    /// lock: between the two halves only the driver thread touches the bitmap.
    pub fn remark_is_marked(&self, addr: usize) -> bool {
        self.bitmap.is_marked(addr)
            || !self
                .sweep_eligible
                .lock()
                .as_ref()
                .is_some_and(|e| e.contains(addr))
    }

    /// gen r5w1/refs5 — the FIRST half of the remark: everything
    /// [`Self::remark_with_reference_processing`] does before its callback
    /// (phase `Remark`, the TAMS narrowing and the sweep authorisation, the
    /// SATB drain with the queue left ACTIVE, the root rescan, the first
    /// closure, the dead-finalizer retention). Hand the result to
    /// [`Self::remark_finish`] — inside the same pause; the barrier stays
    /// active in between.
    ///
    /// # Why the remark can be split around the callback
    ///
    /// So the VM can run remark-time reference processing with the old-gen
    /// guard RELEASED: its consumers call heap accessors that take the
    /// old-gen lock (a non-reentrant mutex), and it adds an `old-gen →
    /// ref_processor` edge the lock ranking never had. Releasing the guard
    /// inside the pause is sound: every counted mutator is parked, so only an
    /// uncounted thread (in the GC-blocked region) could reach the old
    /// generation in the gap, and what it could allocate lies above the
    /// initial-mark snapshot (not sweep-eligible, so implicitly live); nothing
    /// frees old-gen storage outside a collection, and a free would move
    /// `reclaim_epoch`, which the sweep re-checks. The object-start set taken
    /// here is reused by the second half for the same reason.
    pub fn remark_begin(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
    ) -> RemarkProgress {
        self.state.set_phase(ConcurrentGcPhase::Remark);
        // gcd d9/e: the marking phase's length (`[GC] conc_cycles:`).
        self.state.note_remark_reached();
        // gen r4w3/oldgen3: a concurrent overflow rescan that did not finish
        // (the driver reached remark without a slice reporting the closure
        // complete — no production path does, but a caller may) cleared the
        // overflow flag at its start, so `drain_closure` below would not know
        // objects may still be marked-but-unscanned. Re-arm the flag: the
        // pause then runs its own full rescan, as it did before rescans could
        // run concurrently.
        {
            let mut rescan = self.overflow_rescan.lock();
            if rescan.cursor.take().is_some() {
                self.queue.overflowed.store(true, Ordering::Relaxed);
            }
        }
        let mut scanned = 0;
        // gen r4w2/concmark: the last slice's walk when nothing was allocated
        // or freed in the old generation since it (see `starts_cache`), which
        // takes an O(old gen) walk out of the pause in the common case.
        let mut object_starts = self.object_starts_for(old_gen);
        // gen r4w6/oldpin6: over a desynced walk, an opt-in plan to sweep
        // anyway (`remark_walk_gap_plan`); its fresh grid replaces the cached
        // one for the narrowing and the closure below.
        let gap_plan = if self.walk_desynced.load(Ordering::Relaxed) {
            self.remark_walk_gap_plan(old_gen)
        } else {
            None
        };
        if let Some(plan) = gap_plan.as_ref() {
            object_starts = Arc::clone(&plan.starts);
        }
        let mut sweep_authorised = false;

        // TAMS (G1MARK-3, re-anchored gengc-mark 2026-09-20): the snapshot of
        // what the sweep may free was taken at INITIAL MARK (see there for
        // why). Remark only NARROWS it, to objects that are still allocated —
        // and only while the old gen's identity is intact.
        //
        // GCAUD-4: `sweep_eligible` is keyed on bare addresses, which stop
        // naming the same objects the moment another collector frees or slides
        // old-gen storage. If that happened between initial mark and now, the
        // snapshot is meaningless and, worse, actively wrong (a freed address
        // reissued to a live object is "eligible" with a clear bit). Fail
        // closed: empty the snapshot so this cycle reclaims nothing and the
        // next trigger starts fresh against the current layout.
        //
        // Reaching this point is also what AUTHORISES a sweep: only a remark
        // that completes stamps `sweep_eligible_epoch`, which `concurrent_sweep`
        // requires. `initial_mark` leaves it `None` on purpose — see there.
        //
        // Note this REPLACES the previous full `object_starts.clone()` into the
        // snapshot, so the remark pause no longer pays for building a second
        // copy of a set with one entry per old-gen object.
        {
            let epoch_now = old_gen.reclaim_epoch();
            let mut eligible = self.sweep_eligible.lock();
            let opened_at = *self.initial_mark_epoch.lock();
            let mut authorised = self.sweep_eligible_epoch.lock();
            // gengc-mark2 2026-09-20: the narrowing is now a word-wise AND of
            // two bitmaps instead of a `HashSet::retain` with one SipHash per
            // surviving entry. `retain_intersection` returns `false` only if
            // the two walks disagree about the generation's EXTENT, i.e. the
            // backing storage itself moved — which no `OldGen` operation does,
            // but which would silently corrupt the snapshot if it ever did.
            // Treat it exactly like an epoch mismatch: fail closed.
            let geometry_ok = opened_at == Some(epoch_now)
                && eligible
                    .as_mut()
                    .map(|e| e.retain_intersection(&object_starts))
                    .unwrap_or(true);
            // gen r4w2/concmark — a desynced walk anywhere in this cycle means
            // some allocated objects were never in the object-start set, so
            // references to them were neither marked nor traced, and an
            // eligible object reachable only through one of them has a clear
            // bit while live. Refuse the sweep, exactly as `OldGen::compact`
            // refuses to slide over a walk that does not cover `used()`.
            // gen r4w6/oldpin6: ...unless the gap plan accounts for every
            // byte the walk missed; the closure below then seeds the gap words
            // and rescans every marked object (see `seed_remark_walk_gaps`).
            let walk_ok = !self.walk_desynced.load(Ordering::Relaxed) || gap_plan.is_some();
            if geometry_ok && !walk_ok && opened_at.is_some() {
                CONC_MARK_WALK_DESYNC_ABORTS.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    eligible = eligible.as_ref().map_or(0, |e| e.len()),
                    "concurrent cycle will reclaim nothing: an old-gen object walk \
                     desynced this cycle (see WALK_DESYNC_HITS / SCAN_REGION_BREAK_HITS), \
                     so objects behind the break were never traced",
                );
            }
            // gen r5w1/refs5: a mark bitmap anchored at a base that is not
            // 8-aligned cannot hold a mark for any 8-aligned object (see
            // `with_shared`), so its "unmarked" means nothing. Never observed;
            // refused rather than trusted.
            let base_ok = self.old_gen_base % 8 == 0;
            if !base_ok {
                tracing::warn!(
                    base = self.old_gen_base,
                    "concurrent cycle will reclaim nothing: the mark bitmap's old-gen \
                     base is not 8-aligned, so no object could be marked",
                );
            }
            // gen r5w6/conc10 (old9's cross-lane request,
            // `gengc-r5w5-old9-concurrent-mark-bitmap-does-not-follow-old-gen-growth`):
            // the bitmap was sized from `old_gen_info()` BEFORE the initial-mark
            // pause took the lock, and nothing re-checked it. An in-place growth
            // in between (`OldGen::grow_after_refusal`, reachable only under
            // `CRATONVM_GC_OLD_BORROW_YOUNG`) leaves the grown span unmarkable
            // (`MarkBitmap::try_mark` refuses an address past its span) while
            // its objects are in the eligible snapshot: swept while live. Fail
            // closed, as for a misaligned base; the next cycle is built at the
            // new size. `remark_is_marked` answers from the same refusal (no
            // eligible set ⇒ everything live).
            let covers_ok = bitmap_covers_generation(
                self.old_gen_base,
                self.old_gen_size,
                old_gen.base_ptr() as usize,
                old_gen.capacity(),
            );
            if !covers_ok {
                tracing::warn!(
                    bitmap_base = self.old_gen_base,
                    bitmap_size = self.old_gen_size,
                    old_gen_base = old_gen.base_ptr() as usize,
                    old_gen_capacity = old_gen.capacity(),
                    "concurrent cycle will reclaim nothing: the old generation grew (or \
                     moved) after this cycle's mark bitmap was sized, so objects past the \
                     bitmap's span could not be marked",
                );
            }
            let base_ok = base_ok && covers_ok;
            if geometry_ok && walk_ok && base_ok {
                *authorised = Some(epoch_now);
                sweep_authorised = true;
            } else {
                let stranded = eligible.as_ref().map_or(0, |e| e.len());
                // `!geometry_ok`: a desync refusal was counted above under its
                // own name and is not an epoch race.
                if !geometry_ok && stranded > 0 && opened_at.is_some() {
                    // `opened_at.is_some()` distinguishes the real race (a cycle
                    // WAS opened and another collector moved the ground under
                    // it) from a caller that reached `remark` without an
                    // `initial_mark` at all, which is a programming error, not
                    // an interleaving, and is already fatal to the sweep below.
                    self.note_sweep_epoch_abort();
                    tracing::info!(
                        snapshot_epoch = ?opened_at,
                        current_epoch = epoch_now,
                        eligible = stranded,
                        "concurrent cycle will reclaim nothing: old-gen storage was \
                         reclaimed or relocated between initial mark and remark, so the \
                         address-keyed eligibility snapshot no longer identifies the same \
                         objects",
                    );
                }
                *eligible = None;
                *authorised = None;
            }
        }

        // gen r4w2/concmark — a refused remark does no marking
        // (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 6).
        //
        // With no authorising stamp the sweep frees nothing and clears the
        // bitmap, so both closures below — the SATB drain and the
        // `drain_closure`s, which can be O(live old gen) and all run inside the
        // pause — computed a result nobody reads. Keep only the post-conditions
        // a completed remark establishes: phase `ConcurrentSweep` (mutator gate
        // closed FIRST, as below), SATB queue inactive and empty, mark queue
        // empty.
        if !sweep_authorised {
            self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
            let _ = self.satb_queue.deactivate_and_drain(stw);
            self.queue.clear();
            return RemarkProgress {
                authorised: false,
                scanned: 0,
                object_starts,
            };
        }
        // gen r4w6/oldpin6: the walk-gap recovery's marks, before any closure.
        if let Some(plan) = gap_plan.as_ref() {
            scanned += self.seed_remark_walk_gaps(old_gen, plan);
        }

        // Process SATB entries: these are old reference values that were
        // overwritten during concurrent marking. We must mark them to
        // prevent live objects from being collected.
        //
        // gc-concmark HIGH fix — keep the SATB barrier ACTIVE across the
        // remark closure. The previous code called
        // `deactivate_and_drain()` HERE (before the closure below), which
        // flipped the gate straight to INACTIVE while the mark bitmap was
        // still being computed. Between that INACTIVE store and the start
        // of `concurrent_sweep`, the SATB pre-barrier (gated on
        // `satb_queue.is_active()`) stops logging, so a mutator that
        // overwrites a still-live old-gen reference is NOT recorded — its
        // target object's bit stays clear in the bitmap and the sweep
        // frees it while it is still reachable (floating-garbage free →
        // use-after-free). Correctness of the sweep requires the bitmap
        // to be FINAL, which means every overwritten old reference up to
        // the moment mutators are quiesced must be marked.
        //
        // Fix: drain WITHOUT deactivating so the barrier keeps logging
        // any concurrent overwrites into the queue while we build the
        // closure below; we run a final `deactivate_and_drain()` (which
        // also captures late writers via its shard-lock barrier) only
        // after the closure, and re-mark whatever it returns before the
        // gate is allowed to go INACTIVE. See the closing block.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }

        // Re-scan roots (some may have changed during concurrent mark).
        for &root_ptr in roots {
            if markable_old_object(root_ptr, &object_starts)
                && self.bitmap.try_mark(root_ptr as usize)
            {
                self.queue.push(root_ptr);
            }
        }
        // gen r5w3/unload7: the rows the remark's own root scan deferred, for
        // every owner already live (see `mark_rows_of_live_owners`); an owner
        // the closure below marks is scanned, and its scan follows them. No-op
        // unless class unloading is armed.
        self.mark_rows_of_live_owners(&object_starts);

        // Drain the queue fully (mark transitive closure from new roots),
        // including the overflow fallback. NOTE: the SATB barrier is STILL
        // ACTIVE at this point (we used `drain()` above, not
        // `deactivate_and_drain()`), so any reference a mutator overwrites
        // while we compute this closure is logged and will be captured by
        // the final drain below.
        scanned += self.drain_closure(old_gen, &object_starts);

        // gc-common w36-d — RETAIN THE DEAD FINALIZABLES. At this first fixed
        // point "unmarked and sweep-eligible" is "unreachable", so a published
        // finalizer candidate in that state died old; the sweep would free it
        // without `finalize()`. Mark every such candidate BEFORE tracing any
        // (two that reach only each other are both kept and both reported),
        // drain their closure, and hand them to the next stop-the-world
        // collection to report (`ConcurrentGcState::publish_finalizer_candidates`).
        // Before the reference-processing seam below, so what it judges sees
        // them kept, as HotSpot's JNI weak processing sees the final-reachable
        // set. Nothing to do, and no lock beyond one leaf read, without
        // candidates.
        scanned += self.retain_dead_finalizer_candidates(old_gen, &object_starts);

        // gen r4w2/concmark — the remark-time reference-processing seam sits
        // HERE, at the FIRST fixed point, with the barrier still active and
        // the phase still `Remark`: the caller runs it between this half and
        // `remark_finish` (gen r5w1/refs5 split the function at this point).
        RemarkProgress {
            authorised: true,
            scanned,
            object_starts,
        }
    }

    /// gen r5w1/refs5 — the SECOND half of the remark. `keep` is what the
    /// reference-processing callback returned (policy-retained soft
    /// referents, finalizables about to be resurrected, submitted cleaner
    /// actions): each is marked and its closure drained, then the barrier goes
    /// off with the final quiescing drain and the phase becomes
    /// `ConcurrentSweep`. `None` — no callback ran — skips the keep closure
    /// entirely (byte-for-byte the pre-seam remark). A refused remark
    /// (`!progress.sweep_authorised()`) has already established its
    /// post-conditions in [`Self::remark_begin`]; this returns 0 for it.
    ///
    /// The caller must hold the old-gen guard again (`old_gen`) and still be
    /// inside the pause `remark_begin` ran in. Returns the whole remark's
    /// count (see [`Self::remark_with_reference_processing`]).
    pub fn remark_finish(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        progress: RemarkProgress,
        keep: Option<&[usize]>,
        old_gen: &OldGen,
    ) -> usize {
        let RemarkProgress {
            authorised,
            mut scanned,
            object_starts,
        } = progress;
        if !authorised {
            return scanned;
        }
        if let Some(keep) = keep {
            for &addr in keep {
                let ptr = addr as *mut u8;
                if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                    self.queue.push(ptr);
                }
            }
            scanned += self.drain_closure(old_gen, &object_starts);
        }

        // gc-concmark HIGH fix — final quiescing drain.
        //
        // Now that the closure from the snapshot + roots is complete, flip
        // the barrier off ATOMICALLY with a final drain. `deactivate_and_drain`
        // transitions ACTIVE→DRAINING (loggers keep logging), drains, then
        // takes each shard lock exclusively to capture any late writer that
        // observed ACTIVE before the CAS, and only then stores INACTIVE.
        //
        // This closes the window the old code left open: between the moment
        // the gate went INACTIVE and the sweep, an overwritten live old-gen
        // reference could go unlogged and be swept while reachable. Here the
        // gate is not allowed to go INACTIVE until every entry logged up to
        // the drain barrier has been returned to us — and we mark every one
        // of them (plus its transitive closure) BEFORE returning, so the
        // bitmap handed to `concurrent_sweep` is final.
        //
        // In a true STW remark mutators are already stopped, so this drain
        // typically returns nothing; in a (mostly-)concurrent remark it
        // reaps the stragglers. Either way the bitmap is final on exit.
        //
        // gengc-mark 2026-09-20 — CLOSE THE MUTATOR GATE FIRST.
        //
        // The generational heap's `satb_barrier` (gen_heap.rs) gates on
        // `ConcurrentGcState::is_marking_active()`, i.e. on the PHASE, while
        // the log it writes into is gated on `SatbQueue::is_active()`. G1
        // asserts the invariant that ties those two together --
        // `is_marking_active() => satb_queue.is_active()` (`g1.rs`, the
        // `assert_satb_invariant` note) -- and this function used to break it
        // for the whole width of the closure below: the phase stayed `Remark`
        // (barrier ON) while the queue had already gone INACTIVE. A mutator
        // storing a reference in that window passes the phase gate, appends to
        // its per-thread bucket for a queue nobody will drain again, and the
        // entry sits there until the NEXT cycle's `flush_all_thread_satb_buffers`
        // replays it as a root -- by which time the address may name a
        // different object entirely.
        //
        // Moving the phase transition ahead of the deactivation restores the
        // superset rule: `ConcurrentSweep` turns the mutator barrier off while
        // the queue is still ACTIVE, and the drain immediately afterwards
        // therefore captures every entry any mutator could still have logged.
        // The observable post-conditions of `remark` are unchanged (phase ==
        // ConcurrentSweep, queue inactive).
        //
        // gc-common w5-e — history now: `satb_barrier` gates on the QUEUE
        // (`common-g-proposal-one-satb-gate`), so the half-open window above
        // can no longer be expressed and this order is not load-bearing for
        // the barrier. It is kept so the phase (which the VM's root builders
        // still read) never claims marking with a closed queue.
        self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        let late_entries = self.satb_queue.deactivate_and_drain(stw);
        for addr in late_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }
        // Mark the transitive closure of any late entries (again with the
        // overflow fallback). The gate is INACTIVE now, but mutators are
        // quiesced past the drain barrier, so no further live overwrite can
        // escape the bitmap.
        scanned += self.drain_closure(old_gen, &object_starts);

        // Phase was already advanced to `ConcurrentSweep` above, ahead of the
        // deactivation, to keep `is_marking_active() => satb_queue.is_active()`
        // true at every instant.

        // gen r5w3/unload7: the bitmap is final; nothing scans again this
        // cycle, so the side tables (one row per user-loader class, mirror
        // and deferred root) are released now rather than at the sweep's end.
        if self.class_unload_armed() {
            self.disarm_class_unload();
        }

        scanned
    }

    /// gc-common w36-d — the remark's finalizer retention (see the call site
    /// and [`ConcurrentGcState::publish_finalizer_candidates`]). Marks every
    /// published candidate that is an old-gen object start, unmarked, and
    /// sweep-eligible (so this cycle would free it); drains their closure;
    /// records them for the next stop-the-world collection to report. Returns
    /// the objects the drain scanned.
    fn retain_dead_finalizer_candidates(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> usize {
        let candidates = self.state.finalizer_candidates();
        if candidates.is_empty() {
            return 0;
        }
        let mut retained: Vec<usize> = Vec::new();
        {
            let eligible = self.sweep_eligible.lock();
            let Some(eligible) = eligible.as_ref() else {
                return 0;
            };
            for &addr in &candidates {
                if markable_old_object(addr as *mut u8, object_starts)
                    && !self.bitmap.is_marked(addr)
                    && eligible.contains(addr)
                {
                    retained.push(addr);
                }
            }
        }
        if retained.is_empty() {
            return 0;
        }
        // gen r5w5/conc9: a remark that runs reference processing records
        // the closure (see `record_finalizer_retention_closure`).
        if self.record_retention_closure.load(Ordering::Acquire) {
            let scanned = self.retain_and_record_closure(old_gen, object_starts, &retained);
            self.state.note_remark_retained_finalizers(&retained);
            return scanned;
        }
        // Every candidate first, then one drain: the mutually-reachable rule.
        // gen r5w1/refs5: counted once, by the drain that scans them (the
        // remark's count is "objects scanned").
        for &addr in &retained {
            if self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }
        let scanned = self.drain_closure(old_gen, object_starts);
        self.state.note_remark_retained_finalizers(&retained);
        scanned
    }

    /// gen r5w5/conc9 — [`Self::retain_dead_finalizer_candidates`]'s marking,
    /// RECORDING what it marks into [`Self::retention_closure`]. Every
    /// candidate is marked before any is traced (the mutually-reachable rule,
    /// unchanged), then one marker-local depth-first drain scans them and
    /// everything it newly marks; each child `scan_object_into` hands to `push`
    /// was marked by that very call, i.e. was unmarked at the strong closure's
    /// fixed point. Runs inside the remark pause on the driver thread, so the
    /// local stack needs no termination protocol; the shared queue is empty
    /// on entry (the strong closure just drained it) and is left empty.
    ///
    /// A scan that fails closed marks the whole generation, exactly as the
    /// shared drain does ([`Self::drain_closure`]); the objects that marks are
    /// not recorded (the set is then short, never wrong). Returns the objects
    /// scanned.
    fn retain_and_record_closure(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        retained: &[usize],
    ) -> usize {
        let mut closure: Vec<usize> = Vec::with_capacity(retained.len());
        let mut stack: Vec<*mut u8> = Vec::with_capacity(retained.len());
        for &addr in retained {
            if self.bitmap.try_mark(addr) {
                closure.push(addr);
                stack.push(addr as *mut u8);
            }
        }
        let mut scanned = 0usize;
        let mut children: Vec<*mut u8> = Vec::new();
        while let Some(obj_ptr) = stack.pop() {
            children.clear();
            let ok = self.scan_object_into(obj_ptr, old_gen, object_starts, &mut |child| {
                children.push(child)
            });
            if !ok {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            scanned += 1;
            closure.extend(children.iter().map(|&c| c as usize));
            stack.extend_from_slice(&children);
        }
        *self.retention_closure.lock() = closure;
        scanned
    }

    /// Drain the mark queue to its transitive closure, then run the
    /// graceful overflow fallback (Round-9 gc HIGH-5) until no shard has
    /// overflowed. Returns the number of objects scanned. Extracted so the
    /// remark closure can be re-run after the final SATB quiescing drain
    /// (gc-concmark fix) without duplicating the overflow logic.
    fn drain_closure(&self, old_gen: &OldGen, object_starts: &OldGenObjectStarts) -> usize {
        let mut discovered = 0;

        // Drain the queue fully (mark transitive closure). gen r4w4/concmark4:
        // through the marker-local stack (`drain_unbounded`).
        match self.drain_unbounded(old_gen, object_starts) {
            Some(n) => discovered += n,
            None => {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return discovered;
            }
        }

        // Round-9 gc HIGH-5 fix — graceful overflow fallback.
        //
        // If any `push` during this cycle was dropped because a queue
        // shard hit `MARK_QUEUE_SHARD_CAP`, the transitive closure
        // above is incomplete: outgoing references from the dropped
        // objects were never scanned. To preserve correctness we run
        // a conservative full re-walk of the old generation, marking
        // every reachable object found through any allocated object's
        // outgoing references. This is O(N) instead of O(reachable)
        // and may take many seconds on a multi-GB heap, but it
        // **cannot** crash the VM the way the previous `panic!` did.
        //
        // The loop iterates because the rescan itself may overflow:
        // each pass clears the flag, scans everything currently
        // allocated, and repeats until a pass completes with no new
        // overflow. In practice one or two passes suffice; the cap is
        // generous enough that any realistic graph terminates immediately.
        let mut rescan_passes = 0;
        while self.queue.has_overflowed() {
            // Reset before the rescan so a fresh overflow in this pass
            // is observable. Push/pop happen-before this load: STW.
            self.queue.overflowed.store(false, Ordering::Relaxed);
            rescan_passes += 1;
            if rescan_passes > 8 {
                // Hard guard: if we somehow can't converge, abandon
                // the queue and mark every allocated object directly.
                // The sweep that follows will keep all live objects;
                // garbage retention is the price of forward progress.
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            // gen r4w3/oldgen3: `for_each_object`, not `walk_objects()` — the
            // same walk without a `Vec` of every old-gen object built inside
            // the remark pause, once per pass.
            let mut degraded = false;
            old_gen.for_each_object(|obj_ptr, _size| {
                if degraded || !self.bitmap.is_marked(obj_ptr as usize) {
                    return;
                }
                // Already gray/black: re-scan its outgoing refs to
                // pick up children we may have dropped.
                if self.scan_object(obj_ptr, old_gen, object_starts) {
                    discovered += 1;
                } else {
                    degraded = true;
                }
            });
            if degraded {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return discovered;
            }
            // Drain anything the rescan re-enqueued.
            match self.drain_unbounded(old_gen, object_starts) {
                Some(n) => discovered += n,
                None => {
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    return discovered;
                }
            }
        }

        discovered
    }

    /// Phase 4: Concurrent Sweep — reclaim unmarked old-gen objects.
    ///
    /// Walks all allocated objects in the old generation and frees those
    /// that are not marked in the bitmap.
    ///
    /// Returns the number of objects freed.
    ///
    /// gen r4w3/oldgen3 (2026-09-23): the whole sweep in one call — one
    /// unbounded [`Self::concurrent_sweep_budget`] slice, which walks to the
    /// end of the generation and finishes the cycle. Kept for the
    /// `CRATONVM_GEN_CONC_MARK_SLICE=0` driver arm and the tests; the sliced
    /// driver calls the budgeted form directly.
    pub fn concurrent_sweep(&self, old_gen: &mut OldGen) -> usize {
        let mut freed = 0usize;
        loop {
            let (n, done) = self.concurrent_sweep_budget(old_gen, usize::MAX);
            freed += n;
            if done {
                return freed;
            }
        }
    }

    /// Phase 4 in a BOUNDED SLICE: walk at most `budget` old-gen objects,
    /// free the eligible unmarked ones among them, and return `(objects freed
    /// by this slice, sweep finished)`.
    ///
    /// gen r4w3/oldgen3 (2026-09-23),
    /// `gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`.
    /// The driver used to run the whole sweep — an O(old gen) walk, one `free`
    /// per dead object and a coalesce — under ONE old-gen lock hold with no
    /// safepoint poll, so every promotion and every other thread's pause
    /// waited for all of it: Phase 2's problem, moved one phase later. The
    /// driver now loops `lock → slice → unlock → safepoint poll` here, as it
    /// does for Phase 2.
    ///
    /// # What has to hold between two slices
    ///
    /// * **The epoch.** The bitmap and the eligibility snapshot are keyed on
    ///   bare addresses. A `free` or `compact` by ANOTHER collector between
    ///   slices (a young pause's in-place old-gen sweep, say) may hand an
    ///   address to a new object, so a slice that finds
    ///   `OldGen::reclaim_epoch` moved stops the sweep — reclaiming nothing
    ///   more this cycle, the same fail-closed answer the first slice gives.
    ///   This sweep's own frees bump the epoch too, so the stamp is re-taken
    ///   after each slice's frees, under the same lock hold.
    /// * **The resume offset.** Each slice starts where
    ///   `OldGen::walk_objects_from` said the previous one stopped: the start
    ///   of an object that was allocated then. With the epoch unchanged
    ///   nothing but this sweep has freed anything, and this sweep frees only
    ///   BELOW that offset, so the object is still there. An allocation in
    ///   between (a promotion into a hole this sweep freed, a buffer carved
    ///   and its tail released, a coalesce on an allocation failure) fills
    ///   free space, which never contains the offset — the walk returns the
    ///   first object of the NEXT region, never a free-block start that a
    ///   later allocation could straddle.
    /// * **TAMS.** An object allocated between slices is not in the snapshot
    ///   (its block was free at remark, or it was freed after — which moved
    ///   the epoch), so it is walked past and never freed. The same rule that
    ///   protects an object promoted between remark and a one-shot sweep.
    ///
    /// The free list is coalesced once, when the sweep finishes, and the
    /// generation's trigger bookkeeping is told what the cycle reclaimed
    /// (`OldGen::note_concurrent_collection_end`) — both on a normal finish
    /// and on an epoch stop after some slices had already freed.
    ///
    /// A `budget` of 0 is raised to 1.
    pub fn concurrent_sweep_budget(&self, old_gen: &mut OldGen, budget: usize) -> (usize, bool) {
        self.concurrent_sweep_budget_into(old_gen, budget, None)
    }

    /// [`Self::concurrent_sweep_budget`], also appending every span this slice
    /// returned to the free list to `freed` as `(start, len)` — ascending,
    /// non-overlapping; with `no_oldgen_coalesce` one span per object.
    ///
    /// For the VM's address-keyed side tables, which must drop rows keyed
    /// inside a freed span BEFORE the span can be handed out again: a direct
    /// old-gen allocation (a humongous array) between two slices would
    /// otherwise land on the span and make the dead row look live to every
    /// later `survived_in_place` sweep (gc-common w5-g,
    /// `common-w3g-concurrent-sweep-direct-old-alloc-window`). `None` is
    /// exactly [`Self::concurrent_sweep_budget`].
    pub fn concurrent_sweep_budget_into(
        &self,
        old_gen: &mut OldGen,
        budget: usize,
        mut freed: Option<&mut Vec<(usize, usize)>>,
    ) -> (usize, bool) {
        let budget = budget.max(1);
        let mut progress = self.sweep_progress.lock();
        if progress.is_none() {
            match self.begin_sweep(old_gen) {
                Some(p) => *progress = Some(p),
                // Nothing authorised, or the snapshot was already stale:
                // `begin_sweep` has finished the cycle.
                None => return (0, true),
            }
        }
        let Some(p) = progress.as_mut() else {
            return (0, true);
        };
        self.state
            .census
            .sweep_slices
            .fetch_add(1, Ordering::Relaxed);

        // GCAUD-4, between slices: see the doc above.
        if p.epoch != old_gen.reclaim_epoch() {
            self.note_sweep_epoch_abort();
            self.state
                .census
                .sweep_epoch_stops
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                snapshot_epoch = p.epoch,
                current_epoch = old_gen.reclaim_epoch(),
                freed_so_far = p.freed_objects,
                "concurrent sweep stopped between slices: old-gen storage was reclaimed or \
                 relocated by another collector, so the address-keyed mark bitmap and \
                 eligibility snapshot no longer identify the same objects",
            );
            let finished = progress.take();
            self.finish_sweep(old_gen, finished);
            return (0, true);
        }

        // One slice: collect the dead RUNS (adjacent eligible unmarked
        // objects), then free them. The walk borrows the generation, so the
        // frees wait for it; nothing between the two touches old gen.
        //
        // Runs, not objects — the in-place STW sweep's argument
        // (`gen_heap::old_gen_gc`): `walk_objects_from` yields back-to-back
        // `(base, size)` pairs inside a region, so "the next dead object
        // starts where the run ends" means their union is one contiguous span
        // of freed objects; a region boundary or a desync break leaves a gap
        // and ends the run. The trailing coalesce would have merged the pieces
        // anyway. Not under `no_oldgen_coalesce`, whose point is unmerged
        // blocks.
        let batch = !crate::gc_flags().no_oldgen_coalesce;
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut freed_objects = 0usize;
        let mut freed_bytes = 0usize;
        let next = {
            let bitmap = &self.bitmap;
            let eligible = &p.eligible;
            // gen r5w4/conc8: every object this sweep does NOT free is a
            // survivor for the retained-layout census (one `Option` test per
            // object when no census runs).
            let census = &mut p.layout_census;
            old_gen.walk_objects_from(p.next, budget, |obj_ptr, total_size| {
                let addr = obj_ptr as usize;
                if bitmap.is_marked(addr) || !eligible.contains(addr) {
                    if let Some(c) = census.as_mut() {
                        c.observe_survivor(obj_ptr);
                    }
                    return;
                }
                freed_objects += 1;
                freed_bytes += total_size;
                let extends = batch && runs.last().is_some_and(|&(start, len)| start + len == addr);
                if extends {
                    if let Some((_, len)) = runs.last_mut() {
                        *len += total_size;
                    }
                } else {
                    runs.push((addr, total_size));
                }
            })
        };
        for &(start, len) in &runs {
            // SAFETY: `[start, start + len)` is the union of back-to-back
            // `(base, size)` pairs `walk_objects_from` yielded in this slice
            // for objects that existed at initial mark and remark (the
            // eligibility snapshot), were not marked by the final closure,
            // and still occupy those addresses (the epoch check above), so
            // returning the span is returning exactly those objects.
            unsafe { old_gen.free(start as *mut u8, len) };
        }
        if let Some(out) = freed.as_deref_mut() {
            out.extend_from_slice(&runs);
        }
        p.freed_objects += freed_objects;
        p.freed_bytes += freed_bytes;
        // Re-stamp AFTER this slice's own frees, under the same lock hold, so
        // only someone ELSE's free is caught by the next slice.
        p.epoch = old_gen.reclaim_epoch();
        match next {
            Some(resume) => {
                p.next = resume;
                (freed_objects, false)
            }
            None => {
                let mut finished = progress.take();
                // gen r5w4/conc8: the walk reached the end with the epoch
                // intact — the one exit that completes a retained-layout
                // census (an epoch stop above drops it with the progress).
                if let Some(census) = finished.as_mut().and_then(|p| p.layout_census.take()) {
                    if let Some(released) = census.released() {
                        self.state.complete_layout_census(released);
                    }
                }
                self.finish_sweep(old_gen, finished);
                (freed_objects, true)
            }
        }
    }

    /// The gates the first sweep slice passes through. `Some` authorises the
    /// sweep; `None` means it frees nothing, and the cycle has been finished
    /// (bitmap cleared, phase `Idle`).
    fn begin_sweep(&self, old_gen: &mut OldGen) -> Option<SweepProgress> {
        // TAMS (G1MARK-3, re-anchored gengc-mark 2026-09-20): the bitmap was
        // finalized at the remark STW, but this sweep runs OUTSIDE any STW — an
        // old-gen allocation landing after the mark opened is unmarked yet
        // fully live. Only objects that existed AT INITIAL MARK (the
        // `sweep_eligible` snapshot, narrowed at remark) may be freed; later
        // allocations are implicitly live for this cycle.
        // gengc-mark2 2026-09-20: `Option::take` where this used to
        // `mem::take` a `HashSet`. `None` (no cycle, or one abandoned by
        // `abort_cycle`/remark) and `Some(empty)` (a cycle over an empty
        // generation) take the same "free nothing" exit, exactly as the empty
        // `HashSet` did for both cases before.
        let eligible = self.sweep_eligible.lock().take();
        let snapshot_epoch = self.sweep_eligible_epoch.lock().take();
        // Belt and braces: the cycle is over either way, so leave no stamp a
        // later sweep could mistake for an authorisation.
        *self.initial_mark_epoch.lock() = None;
        // gen r4w2/concmark: the mark is over; drop the cached walk (up to
        // 1/64th of the old generation) rather than carry it to the marker's
        // drop. The sweep walks for itself.
        *self.starts_cache.lock() = None;
        *self.tams_starts.lock() = None;
        let Some(eligible) = eligible.filter(|e| !e.is_empty()) else {
            self.finish_sweep(old_gen, None);
            return None;
        };
        // NO AUTHORISING STAMP ⇒ FREE NOTHING (gengc-mark 2026-09-20).
        //
        // `sweep_eligible_epoch` is set by `remark` and by nothing else, so
        // `None` here means remark did not run for this snapshot: the bitmap is
        // not final, the SATB log was never drained, and every unmarked-looking
        // object may simply be one the closure had not reached. Before the
        // snapshot moved to initial mark this was implied by the snapshot being
        // empty; now it has to be checked, because the snapshot is populated
        // from the moment the cycle opens.
        //
        // Distinct from the epoch MISMATCH below: nothing raced, so this is not
        // counted as an abort. It is the abort-safe default for a cycle the
        // caller abandoned (the driver's `!remark_done` path does call
        // `abort_cycle`, which clears the snapshot outright; this catches the
        // caller that forgets).
        let Some(snapshot_epoch) = snapshot_epoch else {
            tracing::debug!(
                eligible = eligible.len(),
                "concurrent sweep skipped: remark never authorised this snapshot",
            );
            self.finish_sweep(old_gen, None);
            return None;
        };

        // GCAUD-4: `eligible` and `bitmap` are keyed on bare old-gen
        // addresses, and this sweep is the one phase of the cycle that runs
        // outside a stop-the-world. If any other collector freed or relocated
        // old-gen storage since remark, an address in `eligible` may now name
        // a DIFFERENT object — a live one, whose bit is clear only because the
        // bit describes its predecessor. Both halves of the TAMS filter would
        // then be satisfied by a live object and the sweep would free it.
        //
        // Fail closed: reclaim nothing this cycle. The next `old_gen_needs_gc`
        // trigger starts a fresh cycle against the current layout, so the
        // garbage is collected one cycle later rather than the live object
        // being collected now.
        if snapshot_epoch != old_gen.reclaim_epoch() {
            self.note_sweep_epoch_abort();
            // `info!`: a whole concurrent sweep's work is being thrown
            // away, at most once per cycle. As `debug!` this line could not
            // print in a release build, so the abandonment was unobservable
            // outside a debug run (the count is on the `[GC] conc_driver:`
            // line as `concdrv_sweep_epoch_aborts` since gen r5w1/refs5).
            tracing::info!(
                snapshot_epoch = ?Some(snapshot_epoch),
                current_epoch = old_gen.reclaim_epoch(),
                eligible = eligible.len(),
                "concurrent sweep abandoned: old-gen storage was reclaimed or \
                 relocated since remark, so the address-keyed mark bitmap and \
                 eligibility snapshot no longer identify the same objects",
            );
            self.finish_sweep(old_gen, None);
            return None;
        }

        // gen r5w4/conc8: the retained-layout census rides on this walk; its
        // break baseline is taken now, before the first slice walks anything.
        let layout_census = self
            .layout_census
            .lock()
            .take()
            .map(|candidates| LayoutCensus {
                candidates: candidates.into_iter().collect(),
                survivors: rustc_hash::FxHashSet::default(),
                breaks_at_start: walk_break_hits(),
            });
        Some(SweepProgress {
            eligible,
            epoch: snapshot_epoch,
            next: 0,
            freed_objects: 0,
            freed_bytes: 0,
            layout_census,
        })
    }

    /// End the sweep (and with it the cycle): coalesce once if anything was
    /// freed — the same amortised merge the in-place STW sweep does (see
    /// `OldGen::coalesce_free_blocks`: this reclaimer never compacts, and
    /// `free` alone leaves every reclaimed run an isolated block, so without
    /// it the generation fragments monotonically) — tell the generation's
    /// trigger what an authorised sweep reclaimed, clear the bitmap, and
    /// return the phase to `Idle` (the hand-off to the next cycle; see
    /// [`ConcurrentCycle`]).
    fn finish_sweep(&self, old_gen: &mut OldGen, progress: Option<SweepProgress>) {
        if let Some(p) = progress {
            if p.freed_objects > 0 {
                old_gen.coalesce_free_blocks();
            }
            // gen r4w3/oldgen3 — the wave-2 `oldgen2` cross-lane request: a
            // concurrent cycle is an old-gen collection too, and the
            // hysteresis must measure growth from its end, not from the last
            // STW collection's.
            old_gen.note_concurrent_collection_end(p.freed_bytes);
            // gen r4w5/concmark5 — and the same figure on the state, so the
            // STW cadence census can tell the two kinds of frees apart.
            self.state.note_concurrent_freed(p.freed_bytes);
            // gen r4w4/concmark4 — the start policy's growth sample, AFTER
            // `note_concurrent_collection_end` so `freed_bytes` (half of
            // `old_gen_allocated_total`) already includes this sweep.
            self.state.note_cycle_end(
                old_gen_allocated_total(old_gen),
                old_gen.used(),
                old_gen.capacity(),
                p.freed_bytes,
            );
        } else {
            // gcd d9/e: `conccyc_swept_nothing` — a cycle whose remark ran
            // (this is the sweep's gate) but which reclaims nothing.
            self.state.note_swept_nothing();
            self.state.note_cycle_dropped();
        }
        self.disarm_reference_skip();
        if self.class_unload_armed() {
            self.disarm_class_unload();
        }
        self.bitmap.clear();
        self.state.set_phase(ConcurrentGcPhase::Idle);
    }

    fn mark_all_old_gen(&self, old_gen: &OldGen) -> usize {
        let mut marked = 0;
        // gen r4w3/oldgen3: `for_each_object` — no `Vec` of the generation.
        old_gen.for_each_object(|obj_ptr, _size| {
            if self.bitmap.try_mark(obj_ptr as usize) {
                marked += 1;
            }
        });
        marked
    }

    /// Scan an object's reference fields and mark any old-gen targets.
    ///
    /// Returns `false` when the queued pointer names an object with an
    /// inconsistent or implausible header. Callers respond by marking every
    /// old-gen object for this cycle, retaining garbage rather than under-marking
    /// live objects or dereferencing a bogus field extent.
    fn scan_object(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> bool {
        let queue = &self.queue;
        self.scan_object_into(obj_ptr, old_gen, object_starts, &mut |p| queue.push(p))
    }

    /// [`Self::scan_object`] with the newly greyed children handed to `push`
    /// instead of the shared [`MarkQueue`]. gen r4w4/concmark4: the
    /// marker-local stack's entry point (see [`Self::drain_local`]); every
    /// child passed to `push` has already been marked.
    fn scan_object_into<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        push: &mut F,
    ) -> bool {
        // SAFETY: obj_ptr was popped from the mark queue, which only contains
        // pointers to valid old-gen objects verified by old_gen.contains() before
        // being enqueued. The header is readable for the lifetime of the GC cycle.
        let header_ptr = obj_ptr as *const ObjectHeader;
        let Some(total_size) = concurrent_mark_object_size(header_ptr) else {
            let snapshot = ConcurrentMarkHeaderSnapshot::read(header_ptr);
            tracing::warn!(
                "concurrent mark: skipping object at {:p} with inconsistent header \
                 (kind_tag={}, element_tag={}, class_id={}, array_length={}, num_slots={}, \
                 gc_flags=0x{:02x}); marking all old-gen objects for this cycle",
                obj_ptr,
                snapshot.kind_tag,
                snapshot.element_tag,
                snapshot.class_id,
                snapshot.array_length(),
                snapshot.num_slots(),
                snapshot.gc_flags,
            );
            return false;
        };
        // gen r5w6/conc10 (unload7's r5w3 request, repeated by old9): the
        // WHOLE claimed extent, not its last byte. With an interior decommit
        // (`CRATONVM_GC_OLD_INTERIOR_DECOMMIT`) both ends can be committed
        // while a granule between them is released, and the slot loops below
        // would fault reading it; `contains_range` checks every granule the
        // extent spans. Without holes it is exactly the old test (the start is
        // an old-gen object start; the end below `readable_end`).
        if total_size < HEADER_SIZE || !old_gen.contains_range(obj_ptr, total_size) {
            let snapshot = ConcurrentMarkHeaderSnapshot::read(header_ptr);
            tracing::warn!(
                "concurrent mark: skipping object at {:p} with implausible extent {} \
                 (kind_tag={}, element_tag={}, class_id={}, array_length={}, num_slots={}, \
                 gc_flags=0x{:02x}); marking all old-gen objects for this cycle",
                obj_ptr,
                total_size,
                snapshot.kind_tag,
                snapshot.element_tag,
                snapshot.class_id,
                snapshot.array_length(),
                snapshot.num_slots(),
                snapshot.gc_flags,
            );
            return false;
        }
        let header = unsafe { &*header_ptr };

        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Reference array: compact 8-byte pointer per element.
                //
                // gen r4w3/oldgen3 (item 4 of
                // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`):
                // bound the walk by the extent VALIDATED above, as the object
                // arm below already does. `header.array_length()` is a second
                // read of a header this module treats as racy; if it read
                // larger than the snapshot `concurrent_mark_object_size` sized,
                // the loop would visit elements past the `old_gen.contains`
                // check. `min` keeps the re-read's count whenever it is the
                // smaller (the normal case: the two agree), so a well-formed
                // array is walked exactly as before.
                let validated = total_size.saturating_sub(ARRAY_DATA_OFFSET) / ref_element_size();
                let len = (header.array_length() as usize).min(validated);
                for i in 0..len {
                    // SAFETY: i < array_length, offset is within the allocated array object.
                    let slot_ptr =
                        unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    // SAFETY: slot_ptr points to a valid 8-byte reference element in the array.
                    let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if markable_old_object(ref_ptr, object_starts)
                            && self.bitmap.try_mark(ref_ptr as usize)
                        {
                            push(ref_ptr);
                        }
                    }
                }
            }
            // Primitive arrays have no references to scan.
        } else if crate::is_compact_object(header) {
            // gen r5w1/refs5: a `Reference` in this cycle's skip set does not
            // trace its referent (field index 0, `Reference.referent`, first
            // in declaration order with no superclass fields ahead of it; the
            // same index G1's INT-8 skip hides). See `set_reference_skip`.
            let hide_referent = self.referent_slot_hidden(obj_ptr);
            // HIB-DCAST-LATEPHASE.1: `compact_oop_scan` returns `None` both
            // for "this is a legacy object" (its documented contract) and,
            // via its internal `class_layout_for_fields(..)?`, for "this IS
            // a compact object (`GC_FLAG_COMPACT` set) but its class's
            // layout is not registered right now". Gating on
            // `is_compact_object(header)` (the header bit, independent of
            // the registry) rather than `compact_oop_scan(..).is_some()`
            // keeps the second case out of the legacy arm below, which would
            // misread this object's packed compact body under the legacy
            // `num_slots * SLOT_SIZE` formula — an UNBOUNDED stride (this
            // loop has no `body_bytes` cap, unlike `scan_dirty_cards`'s
            // twin) past the object's real extent. A compact object whose
            // layout cannot be resolved has no provably-safe reference slots
            // to visit; skip it.
            //
            // gen r4w2/concmark (2026-09-23): "skip it" was an UNDER-mark. The
            // object is live (it is on the queue) and its reference slots are
            // exactly what the unresolvable layout would have told us, so
            // skipping it leaves every child it alone keeps alive unmarked and
            // sweep-eligible. Fail closed instead, like the size screen above:
            // `false` makes the caller mark every old-gen object this cycle.
            // `concurrent_mark_object_size` already resolved the same
            // `(class_id, num_slots)` layout moments ago, so this arm is a
            // registry change racing the scan, not a steady-state path.
            //
            // gc-common w3-e: borrowing form (`with_compact_oop_scan`) -- no
            // layout `Arc` clone/drop per scanned object; `None` is the same
            // unresolvable-layout answer and still fails the cycle closed.
            let scanned = crate::heap::with_compact_oop_scan(header, |layout, body| {
                // gen r5w1/refs5: the displacement of field 0 when it is a
                // reference field, i.e. the referent slot to hide.
                let hidden_disp = if hide_referent && layout.is_ref.first() == Some(&true) {
                    layout.field_disps.first().map(|&d| d as usize)
                } else {
                    None
                };
                // Compact object: 8-byte reference slots at the per-class oop-map
                // offsets. An aligned single-word 8-byte pointer load cannot tear,
                // so (like the reference-array branch above) no stripe lock is
                // needed even though this runs concurrently with mutators.
                for &off in &layout.ref_disps {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    if hidden_disp == Some(off) {
                        continue;
                    }
                    // SAFETY: `off` is within the object (capped above).
                    let slot_ptr = unsafe { obj_ptr.add(off) };
                    let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if markable_old_object(ref_ptr, object_starts)
                            && self.bitmap.try_mark(ref_ptr as usize)
                        {
                            push(ref_ptr);
                        }
                    }
                }
            });
            if scanned.is_none() {
                tracing::warn!(
                    "concurrent mark: compact object at {:p} (class_id={}) has no \
                     resolvable layout; marking all old-gen objects for this cycle",
                    obj_ptr,
                    header.class_id.as_u32(),
                );
                return false;
            }
        } else {
            // Object: 16-byte Value slots.
            //
            // gc-concmark MEDIUM fix — torn-read data race. This loop runs on
            // the marker thread *concurrently* with mutators (Phase 2
            // `concurrent_mark` is the whole point of this module), so a
            // mutator may be mid-store into the very slot we read here. A
            // `Value` is 16 bytes — wider than any stable atomic on x86-64 —
            // so a plain `ptr::read::<Value>` can splice the tag word of one
            // store with the payload word of another (or with the pre-store
            // bytes). The resulting `Value::Object(Some(..))` would carry a
            // garbage pointer that we then feed to `old_gen.contains` /
            // `try_mark` / `queue.push` and ultimately dereference as an
            // `ObjectHeader` in the next `scan_object` — a memory-safety hole
            // reachable purely from concurrent application activity (and plain
            // UB under the Rust memory model: a non-atomic read racing a
            // non-atomic write).
            //
            // Mirror exactly how the rest of the heap reads a 16-byte slot
            // safely under concurrency: `Heap::get_field_volatile`
            // (heap.rs:540) takes the per-slot stripe lock from
            // `collector::volatile_stripe_lock(obj_ref, index)` so the
            // 16-byte `Value` appears either fully-old or fully-new, never
            // torn. We acquire the SAME stripe lock here, keyed on the same
            // `(object, slot index)`, so this read serializes against every
            // mutator store routed through the volatile field-access helpers
            // (the lock's acquire is the edge; the per-slot `SeqCst` fences
            // that used to sit around the read were dropped by gen r4w3/oldgen3
            // — see the loop). The
            // 8-byte reference-array path above needs no lock: an aligned
            // 8-byte pointer load/store is single-word and cannot tear.
            //
            // SAFETY: `obj_ptr` was popped from the mark queue, where it was
            // validated by `old_gen.contains` before being enqueued, so it is
            // a non-null, 8-byte-aligned, live old-gen object address — the
            // precondition for `ObjectRef::from_raw`. We use the resulting
            // `ObjectRef` only as a stripe-lock key (its address is hashed),
            // never to mutate the object.
            let obj_ref = unsafe { cratonvm_types::ObjectRef::from_raw(obj_ptr) };
            // Bound the walk by the extent this function ALREADY VALIDATED,
            // not by a fresh read of the header.
            //
            // `total_size` came from `concurrent_mark_object_size`, which reads
            // a `ConcurrentMarkHeaderSnapshot` and cross-validates it, and the
            // guard above then required
            // `old_gen.contains(obj_ptr + total_size - 1)` — the object's last
            // byte is inside the old generation. Re-reading `header.num_slots()`
            // here discards that: it is a SECOND read of a header this module
            // explicitly treats as racy (the snapshot reader exists precisely
            // because the header can be torn or garbage), so the count that was
            // validated and the count that is walked need not be the same
            // number. If the second read is the larger one, this loop visits
            // slots past the extent `old_gen.contains` approved — which is the
            // "can a reader visit slot n of an object whose real slot count is
            // below n" question that
            // `internal/fixed-bugs/hib-orm-json-xml-function-tests-segfault-g1-zgc-FIXED-20260901.md`
            // §0.5 item 2 asks of exactly this code.
            //
            // Deriving the count from `total_size` closes the window by
            // construction: the same arithmetic that was validated
            // (`HEADER_SIZE + num_slots * SLOT_SIZE`) is inverted here, so the
            // walk cannot outrun the bytes that were checked. The `1 << 24`
            // plausibility clamp still applies — it is enforced inside
            // `concurrent_mark_object_size`, which returns `None` (and so
            // returns early above) for anything larger.
            let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
            // gen r5w1/refs5: slot 0 is `Reference.referent`; a `Reference` in
            // this cycle's skip set starts at slot 1 (see `set_reference_skip`).
            let first_slot = usize::from(self.referent_slot_hidden(obj_ptr));
            for slot_idx in first_slot..num_slots {
                // Serialize the 16-byte read against striped mutator writes so
                // we never observe a torn (tag, payload) pair. Held only for
                // the duration of this single slot read.
                let _stripe = crate::collector::volatile_stripe_lock(obj_ref, slot_idx);
                // gen r4w3/oldgen3 (item 4 of
                // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`):
                // there used to be a `fence(SeqCst)` here and another after the
                // read, on every slot. Neither ordered anything this reader
                // uses. Against `set_field_volatile` writers the stripe lock is
                // the edge (its acquire makes the writer's stores, made under
                // the same lock, visible to the read below). Against JIT writers,
                // which take no lock, a reader-side fence pairs with nothing —
                // the two-word atomic read plus the discriminant screen is what
                // makes a torn cell harmless, and SATB, not the order of this
                // read, is what makes a racing overwrite harmless (the old value
                // is logged and marked at remark). Two full barriers per 16-byte
                // slot, primitive slots included, for no guarantee.
                // SAFETY: slot_idx < num_slots, offset is within the allocated object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // Read the 16-byte slot as two atomic words. The stripe lock
                // serializes this against `set_field_volatile` writers, but
                // JIT-compiled field stores write the slot directly and take no
                // stripe lock, so the atomic read is what makes *that* pairing
                // well-defined (see `read_value_atomic` / `write_value_atomic`).
                // SAFETY: slot_ptr points to a valid, aligned Value slot.
                // Screen the discriminant before the bytes become a `Value`.
                // `read_value_atomic` transmutes two words unconditionally, so a
                // cell holding two heap pointers (a swept-and-reused slot) became
                // a `Value` with an out-of-range tag — UB the moment it exists,
                // and a garbage `ObjectRef` we would push onto the mark queue and
                // later dereference as an `ObjectHeader`. `heap::read_slot`,
                // `g1::get_field` and `zgc::get_field` were moved onto this guard
                // for exactly that reason; the marker was not. (gen r4/mark
                // 2026-09-23: this used to add "and it is a G1/ZGC-only path".
                // It is not — `ConcurrentMarker` is the GENERATIONAL old-gen
                // marker, driven from `maybe_concurrent_gc`; G1 has its own in
                // `g1_concurrent.rs` and shares only `ConcurrentGcState` and
                // `concurrent_mark_object_size` with this file.) Corrupt cells decode to `Value::Object(None)`,
                // which this loop skips, and are counted by the cell census.
                let value = unsafe {
                    crate::heap::read_value_cell_checked(
                        slot_ptr as *const Value,
                        "concurrent_mark::scan_object",
                    )
                };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if markable_old_object(ref_ptr, object_starts)
                        && self.bitmap.try_mark(ref_ptr as usize)
                    {
                        push(ref_ptr);
                    }
                }
            }
        }
        // gen r5w3/unload7 — the class-loader side-table edges, when this
        // cycle may unload classes (`ClassUnloadTables`). One acquire load
        // otherwise. A reference array's header carries its component's class
        // id, so a `C[]` keeps `C`'s loader, as an array klass does in HotSpot
        // (the same lookup `old_gen_gc`'s BFS makes).
        if self.class_unload_armed.load(Ordering::Acquire) {
            self.push_class_unload_edges(obj_ptr, header.class_id.as_u32(), object_starts, push);
        }
        true
    }

    /// Run all four phases of a concurrent GC cycle.
    ///
    /// This is a convenience method for testing. In production, the phases
    /// are coordinated by the GC barrier with STW pauses at phase 1 and 3.
    ///
    /// Returns (objects_marked, objects_swept).
    pub fn full_cycle(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &mut OldGen,
    ) -> (usize, usize) {
        let initial = self.initial_mark(roots, old_gen);
        let concurrent = self.concurrent_mark(old_gen);
        let remark = self.remark(stw, roots, old_gen);
        let swept = self.concurrent_sweep(old_gen);
        (initial + concurrent + remark, swept)
    }
}

// ---------------------------------------------------------------------------
// gen r4w6/oldpin6 (2026-09-24): the remark-to-sweep hand-off over a walk gap.
// `docs/internal/gc/gengc-r4w5-oldcompact5-concurrent-remark-refuses-its-sweep-over-a-walk-gap-FIXED-20260929.md`.
// Kept in its own block so the rest of this file (lane `concsvc6`) merges
// cleanly; the only hunks inside `remark_with_reference_processing` are the
// three marked `gen r4w6/oldpin6`.
// ---------------------------------------------------------------------------

/// Remarks that authorised a sweep over a desynced walk through
/// `ConcurrentMarker::remark_walk_gap_plan` (the counterpart of
/// `CONC_MARK_WALK_DESYNC_ABORTS`, which now counts only the refusals).
pub static CONC_MARK_WALK_GAP_RECOVERIES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Tests only: the flags are read once per process, so a test turns the
    /// opt-in on for its own thread here.
    static FORCE_CONC_WALK_GAP_RECOVERY: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn forced_conc_walk_gap_recovery() -> bool {
    FORCE_CONC_WALK_GAP_RECOVERY.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn forced_conc_walk_gap_recovery() -> bool {
    false
}

/// `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` (opt-in), or the test override.
fn conc_walk_gap_recovery_enabled() -> bool {
    forced_conc_walk_gap_recovery() || crate::gc_flags().conc_walk_gap_recovery
}

/// What a remark over a desynced walk needs in order to sweep anyway: a
/// fresh grid of the objects the walk DID cover, the byte ranges it could not
/// (offsets, `OldGen::walk_objects_with_gaps`), and the object-start set of
/// the grid.
struct RemarkWalkGapPlan {
    walked: Vec<(*mut u8, usize)>,
    gaps: Vec<(usize, usize)>,
    starts: Arc<OldGenObjectStarts>,
}

impl ConcurrentMarker {
    /// gen r4w6/oldpin6 — at a remark whose cycle saw a desynced walk, the
    /// plan that lets it sweep instead of refusing, or `None` (refuse, as
    /// before). Opt-in (`CRATONVM_GC_CONC_WALK_GAP_RECOVERY`).
    ///
    /// The refusal existed because a desynced walk anywhere in the cycle
    /// drops marks: a push to an object behind the break fails
    /// `markable_old_object`, so that object is neither marked nor traced,
    /// and an eligible object reachable only through it keeps a clear bit.
    /// The stop-the-world sweep recovers from the same anomaly by treating
    /// the unwalked bytes as a conservative root range
    /// (`gen_heap::seed_old_gen_walk_gaps`). The concurrent remark can do the
    /// same, plus one more step for the marks the CONCURRENT phase dropped:
    ///
    /// 1. **A fresh walk, inside the pause.** Its gaps must account exactly
    ///    for the bytes it missed (`walked + gap == used`, the stop-the-world
    ///    arm's condition) and its objects must ascend; otherwise refuse. Its
    ///    start set replaces the cached one, so the eligibility snapshot is
    ///    narrowed to the objects walked NOW (gap bytes are never eligible,
    ///    hence never swept) and every push below is judged against the
    ///    layout the sweep will see.
    /// 2. **The gap words as marks** ([`Self::seed_remark_walk_gaps`]): every
    ///    aligned word in a gap (and both narrow halves under compressed
    ///    references) that names a walked object — base or interior — is
    ///    marked and queued. The concurrent marker follows no owner-keyed side
    ///    table for ANY owner (whatever keeps those referents alive for a
    ///    walked owner — the remark's roots — keeps them for an owner in a
    ///    gap), so the heap words are the whole of what a gap can name here.
    /// 3. **A full rescan of every marked object**, forced by raising the
    ///    queue's overflow flag before `drain_closure`: a dropped push had a
    ///    MARKED referrer (or a root, re-scanned at remark), so re-scanning
    ///    every marked object against the fresh start set re-pushes every
    ///    referent a desynced walk refused, and the drain traces it. This is
    ///    the path the overflow fallback already trusts for dropped pushes,
    ///    and it is O(old gen) inside the pause — paid only on a cycle that
    ///    desynced, which before this reclaimed nothing at all.
    ///
    /// With the remark's roots, the SATB log and the young→old roots, the
    /// final bitmap then holds every walked object reachable from the roots,
    /// and nothing in a gap is freed.
    fn remark_walk_gap_plan(&self, old_gen: &OldGen) -> Option<RemarkWalkGapPlan> {
        if !conc_walk_gap_recovery_enabled() {
            return None;
        }
        let (walked, gaps) = old_gen.walk_objects_with_gaps();
        let walked_bytes: usize = walked.iter().map(|&(_, size)| size).sum();
        let gap_bytes: usize = gaps.iter().map(|&(s, e)| e.saturating_sub(s)).sum();
        if walked_bytes.checked_add(gap_bytes) != Some(old_gen.used()) {
            return None;
        }
        if !walked.windows(2).all(|w| (w[0].0 as usize) < (w[1].0 as usize)) {
            return None;
        }
        let starts = Arc::new(OldGenObjectStarts::build_from(old_gen, &walked));
        Some(RemarkWalkGapPlan { walked, gaps, starts })
    }

    /// Step 2 of [`Self::remark_walk_gap_plan`]: mark and queue every walked
    /// object a gap word names, and raise the overflow flag so the remark's
    /// `drain_closure` rescans every marked object (step 3). Returns how many
    /// objects the gap words marked. Called inside the remark pause, under the
    /// old-gen lock the remark holds.
    fn seed_remark_walk_gaps(&self, old_gen: &OldGen, plan: &RemarkWalkGapPlan) -> usize {
        let walked = &plan.walked;
        // The walked object containing `cand` (base or interior), if any.
        let resolve = |cand: usize| -> Option<usize> {
            let i = walked.partition_point(|&(p, _)| (p as usize) <= cand);
            let (p, size) = *walked.get(i.checked_sub(1)?)?;
            let b = p as usize;
            (cand < b.saturating_add(size)).then_some(b)
        };
        let mut marked = 0usize;
        let mut mark = |cand: usize| {
            if let Some(b) = resolve(cand) {
                if self.bitmap.try_mark(b) {
                    self.queue.push(b as *mut u8);
                    marked += 1;
                }
            }
        };
        let base = old_gen.base_ptr() as usize;
        let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();
        for &(start, end) in &plan.gaps {
            let mut a = base + ((start + 7) & !7);
            let hi = base + end;
            while a + 8 <= hi {
                // SAFETY: `[a, a + 8)` lies inside `[base + start, base + end)`,
                // a gap `walk_objects_with_gaps` reported: the tail of an
                // ALLOCATED region (regions are the spans between free
                // blocks), so it is below `readable_end` and committed. `a` is
                // 8-aligned (`base` is, and the offset is rounded up). The
                // world is stopped and the old-gen lock is held, so nothing
                // writes these bytes concurrently. No header is read: the
                // word is only compared against the walked grid.
                let w = unsafe { std::ptr::read(a as *const u64) };
                if w != 0 {
                    mark(w as usize);
                    if narrow {
                        for half in [w as u32, (w >> 32) as u32] {
                            if half != 0 {
                                mark(cratonvm_types::narrow_oop::decode(half) as usize);
                            }
                        }
                    }
                }
                a += 8;
            }
        }
        // Step 3: the dropped pushes' referrers are marked; rescan them all.
        self.queue.overflowed.store(true, Ordering::Relaxed);
        CONC_MARK_WALK_GAP_RECOVERIES.fetch_add(1, Ordering::Relaxed);
        marked
    }
}

/// Torn/garbage-header gate shared by the Generational marker's `scan_object`
/// and (G1MARK-8) G1's `concurrent_mark_step`: reads the header field-by-field
/// with unaligned loads and cross-validates kind tag, element tag, gc-flag
/// universe and size arithmetic. `None` means "do not trust this header".
pub(crate) fn concurrent_mark_object_size(header: *const ObjectHeader) -> Option<usize> {
    let snapshot = ConcurrentMarkHeaderSnapshot::read(header);
    match snapshot.kind_tag {
        tag if tag == ObjectKind::Array as u8 => {
            let element_type = array_element_type_from_tag(snapshot.element_tag)?;
            let data_size = array_data_size(snapshot.array_length() as usize, element_type).ok()?;
            ARRAY_DATA_OFFSET.checked_add(data_size)
        }
        tag if tag == ObjectKind::Object as u8 => {
            // Every DEFINED flag, and `GC_FLAG_HEADER` is one of them as of
            // 2026-09-08. Omitting it here does not merely weaken the screen —
            // it inverts it: the flag is set on every object every allocator
            // publishes, so an incomplete `known_flags` rejects the sizing of
            // EVERY plain object, the concurrent marker skips them all and
            // falls back to "mark all old-gen objects for this cycle", and G1
            // stops unloading classes (caught by `RClassUnloadSweep` in the
            // regression suite, and by this file's own
            // `concurrent_mark_object_size_rejects_inconsistent_object_header`).
            let known_flags = GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_COMPACT | GC_FLAG_HEADER;
            if snapshot.gc_flags & !known_flags != 0 {
                return None;
            }
            if snapshot.gc_flags & GC_FLAG_COMPACT != 0 {
                return snapshot.compact_total_size();
            }
            if snapshot.num_slots() > (1 << 24) {
                return None;
            }
            let fields_size = (snapshot.num_slots() as usize).checked_mul(SLOT_SIZE)?;
            HEADER_SIZE.checked_add(fields_size)
        }
        _ => None,
    }
}

/// The set of old-generation object starts, as a bitmap.
///
/// # Why this is not a `HashSet<usize>` any more (gengc-mark2 2026-09-20)
///
/// It was one, built by `walk_objects().map(..).collect()` at the top of
/// `initial_mark`, `concurrent_mark` and `remark` — three full constructions
/// per cycle, two of them inside a stop-the-world pause. Per call that cost a
/// `Vec<(*mut u8, usize)>` of the whole generation, a `std::collections::
/// HashSet<usize>` at ~48 bytes and one SipHash per entry, and then one
/// SipHash per membership test — and [`markable_old_object`] is called once
/// per reference SLOT scanned, not once per object.
///
/// `young_mark::ObjectStartBits` already exists for exactly this problem on
/// the young side, where the identical `FxHashSet` design measured 49 % of
/// whole-process time on bt18 at `-Xmx8g`. This reuses it rather than
/// inventing a second one: one bit per 8 bytes is 1/64th of the old
/// generation, `contains` becomes a bounds check, a shift and a mask, and the
/// remark-time narrowing becomes a word-wise AND
/// ([`ObjectStartBits::retain_intersection`]).
///
/// # The `unrepresentable` escape hatch
///
/// `ObjectStartBits` indexes by `(addr - base) >> 3` and refuses an address
/// that is outside its span or not 8-byte aligned RELATIVE TO ITS BASE.
/// `OldGen`'s storage is a `Vec<u8>`, whose base carries no alignment
/// guarantee, while every object start is absolutely 8-aligned (`alloc`
/// aligns the ABSOLUTE `block_addr`, not the offset). Anchoring the bitmap at
/// `base & !7` makes the two agree for every 8-aligned address in the
/// generation, which is every object start `walk_objects` can yield.
///
/// `unrepresentable` is the proof rather than the assumption: any start the
/// bitmap declined is kept in a `HashSet` and consulted on every lookup, so
/// this type answers EXACTLY what the old `HashSet` answered no matter what
/// the allocator does. It is expected to stay empty; a non-empty one costs
/// only the old behaviour for those few addresses.
pub(crate) struct OldGenObjectStarts {
    bits: crate::young_mark::ObjectStartBits,
    unrepresentable: HashSet<usize>,
    /// gen r4w2/concmark — the walk accounted for every allocated byte
    /// (`sum of object sizes == OldGen::used()`), i.e. no `scan_region` broke
    /// early. The same test `OldGen::compact` uses (GCAUD-9) before it trusts
    /// a walk enough to slide over it.
    walk_complete: bool,
}

impl OldGenObjectStarts {
    /// Walk `old_gen` and record every allocated object's start.
    ///
    /// # Sizing
    ///
    /// The bitmap is anchored at the generation's storage base (rounded DOWN
    /// to an 8-byte boundary, so that `addr - base` is 8-aligned for every
    /// 8-aligned `addr`, whatever alignment the backing `Vec<u8>` happened to
    /// get) and spans only as far as the HIGHEST object start the walk found.
    ///
    /// Spanning the whole `capacity()` instead would be a regression for a
    /// large but sparsely-occupied old generation — 32 MB of bitmap to
    /// describe a handful of objects, three times per cycle, where the
    /// `HashSet` this replaces cost a few hundred bytes. Sizing to the
    /// occupied prefix keeps the win monotone: the bitmap is never larger
    /// than 1/64th of the bytes actually in use.
    ///
    /// The base is FIXED and the span only GROWS while a cycle is open (the
    /// old generation gains objects through promotion; anything that removes
    /// one bumps `reclaim_epoch` and the cycle is abandoned). That is exactly
    /// the precondition `ObjectStartBits::retain_intersection` requires of the
    /// initial-mark snapshot against the remark walk.
    fn build(old_gen: &OldGen) -> Self {
        let objects = old_gen.walk_objects();
        Self::build_from(old_gen, &objects)
    }

    /// [`Self::build`] over a walk the caller already has, so one walk can
    /// feed more than one set (gen r4w2/concmark: `initial_mark` seeds the
    /// slice cache from the same walk as the eligibility snapshot). `objects`
    /// must be `old_gen.walk_objects()` taken under the same lock hold.
    fn build_from(old_gen: &OldGen, objects: &[(*mut u8, usize)]) -> Self {
        let (lo, _hi) = old_gen.extent();
        let base = lo & !7usize;
        let walked_bytes: usize = objects.iter().map(|&(_, size)| size).sum();
        let walk_complete = walked_bytes == old_gen.used();
        // Do not assume `walk_objects` is ordered — fold for the maximum.
        let highest = objects.iter().map(|(ptr, _)| *ptr as usize).max();
        let span = match highest {
            // `+ 8` so the highest start's own bit is inside the span.
            Some(top) => top.saturating_sub(base).saturating_add(8),
            None => 0,
        };
        let bits = crate::young_mark::ObjectStartBits::new(base, span);
        let mut unrepresentable = HashSet::new();
        for &(ptr, _size) in objects {
            let addr = ptr as usize;
            if !bits.insert(addr) {
                unrepresentable.insert(addr);
            }
        }
        Self {
            bits,
            unrepresentable,
            walk_complete,
        }
    }

    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        self.bits.contains(addr) || self.unrepresentable.contains(&addr)
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.bits.is_empty() && self.unrepresentable.is_empty()
    }

    /// Number of recorded starts. Diagnostics, logging and tests.
    pub(crate) fn len(&self) -> usize {
        self.bits.len() + self.unrepresentable.len()
    }

    /// Whether the walk this set came from covered every allocated byte (see
    /// the field). `false` means some allocated objects are missing.
    #[inline]
    pub(crate) fn walk_complete(&self) -> bool {
        self.walk_complete
    }

    /// Keep only the starts also present in `other`.
    ///
    /// Returns `false` when the two bitmaps no longer agree on their geometry
    /// — a different base, or a `self` that outspans `other` — in which case
    /// NOTHING has changed and the caller must fail closed. Either can only
    /// happen if the old generation's storage moved, or its occupied prefix
    /// shrank, between the two walks; both bump `reclaim_epoch`, which already
    /// makes the cycle abandon, so this is a second, independent guard on the
    /// same condition rather than a new failure mode.
    #[must_use]
    fn retain_intersection(&mut self, other: &OldGenObjectStarts) -> bool {
        if !self.bits.retain_intersection(&other.bits) {
            return false;
        }
        self.unrepresentable.retain(|a| other.contains(*a));
        true
    }
}

fn old_gen_object_starts(old_gen: &OldGen) -> OldGenObjectStarts {
    OldGenObjectStarts::build(old_gen)
}

fn markable_old_object(ptr: *mut u8, object_starts: &OldGenObjectStarts) -> bool {
    !ptr.is_null() && object_starts.contains(ptr as usize)
}

/// gen r5w6/conc10 — does a mark bitmap built over `[bitmap_base,
/// bitmap_base + bitmap_size)` still cover the old generation now at
/// `[old_base, old_base + old_capacity)`? Same base, and a capacity no larger
/// than the span the bitmap was built for (a capacity that SHRANK is still
/// covered). See the remark's fail-closed check in `remark_begin`.
#[inline]
fn bitmap_covers_generation(
    bitmap_base: usize,
    bitmap_size: usize,
    old_base: usize,
    old_capacity: usize,
) -> bool {
    bitmap_base == old_base && old_capacity <= bitmap_size
}

struct ConcurrentMarkHeaderSnapshot {
    class_id: u32,
    kind_tag: u8,
    element_tag: u8,
    shape: u32,
    gc_flags: u8,
}

impl ConcurrentMarkHeaderSnapshot {
    fn read(header: *const ObjectHeader) -> Self {
        unsafe {
            Self {
                class_id: std::ptr::addr_of!((*header).class_id)
                    .read_unaligned()
                    .as_u32(),
                // Raw tags, still without forming a typed enum: they now
                // come out of the mark word rather than out of two header
                // bytes, but the reason for taking them raw is unchanged --
                // this snapshots possibly-corrupt memory, and an
                // out-of-range discriminant must survive to be rejected
                // rather than being UB at the point of the read.
                kind_tag: ObjectHeader::kind_tag((*header).mark_word.load(Ordering::Relaxed)),
                element_tag: ObjectHeader::element_type_tag(
                    (*header).mark_word.load(Ordering::Relaxed),
                ),
                shape: (*header).raw_shape(),
                gc_flags: (*header).gc_flags(),
            }
        }
    }

    #[inline]
    fn array_length(&self) -> u32 {
        if self.kind_tag == ObjectKind::Array as u8 {
            self.shape
        } else {
            0
        }
    }

    #[inline]
    fn num_slots(&self) -> u32 {
        self.shape
    }

    /// A compact instance's total size, from its class's current layout (its
    /// 8-byte header has no shape word; `shape` here is its first field).
    #[inline]
    fn compact_total_size(&self) -> Option<usize> {
        let field_count = cratonvm_types::compact_field_count(self.class_id)?;
        // Borrowing accessor: reads one `u32` and drops the handle, so there is
        // no reason to pay an `Arc` clone/drop for it.
        cratonvm_types::with_class_layout(self.class_id, field_count, |layout| {
            layout.total_size as usize
        })
    }
}

fn array_element_type_from_tag(tag: u8) -> Option<ArrayElementType> {
    match tag {
        tag if tag == ArrayElementType::Reference as u8 => Some(ArrayElementType::Reference),
        tag if tag == ArrayElementType::Boolean as u8 => Some(ArrayElementType::Boolean),
        tag if tag == ArrayElementType::Char as u8 => Some(ArrayElementType::Char),
        tag if tag == ArrayElementType::Float as u8 => Some(ArrayElementType::Float),
        tag if tag == ArrayElementType::Double as u8 => Some(ArrayElementType::Double),
        tag if tag == ArrayElementType::Byte as u8 => Some(ArrayElementType::Byte),
        tag if tag == ArrayElementType::Short as u8 => Some(ArrayElementType::Short),
        tag if tag == ArrayElementType::Int as u8 => Some(ArrayElementType::Int),
        tag if tag == ArrayElementType::Long as u8 => Some(ArrayElementType::Long),
        _ => None,
    }
}

impl std::fmt::Debug for ConcurrentMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrentMarker")
            .field("phase", &self.state.phase())
            .field("bitmap", &self.bitmap)
            .field("queue_len", &self.queue.len())
            .field("satb_queue", &self.satb_queue)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// The STW witness these tests stand in for.
    ///
    /// SAFETY: a unit test has one mutator thread -- itself -- so the
    /// safepoint precondition `StopTheWorldToken::new` demands holds
    /// vacuously. This mirrors the helper `g1.rs` and `g1_concurrent.rs`
    /// already use for the same reason.
    fn stw() -> crate::collector::StopTheWorldToken {
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    fn make_old_gen_with_object(num_slots: u32) -> (OldGen, *mut u8) {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + num_slots as usize * SLOT_SIZE;
        let ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let header = &mut *(ptr as *mut ObjectHeader);
            header.class_id = ClassId::new(1);
            header.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            header.set_num_slots(num_slots);
            header.set_gc_age(0);
            header.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        }
        (og, ptr)
    }

    #[test]
    fn concurrent_mark_object_size_rejects_inconsistent_object_header() {
        let mut header = ObjectHeader::new(
            ClassId::new(240),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            2,
        );
        assert_eq!(
            concurrent_mark_object_size(&header),
            Some(HEADER_SIZE + 2 * SLOT_SIZE)
        );

        header.set_num_slots((1 << 24) + 1);
        assert_eq!(concurrent_mark_object_size(&header), None);
    }

    #[test]
    fn initial_mark_rejects_interior_old_gen_pointer() {
        let (og, obj_ptr) = make_old_gen_with_object(2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let interior = unsafe { obj_ptr.add(8) };

        assert_eq!(marker.initial_mark(&[interior], &og), 0);
        assert!(!marker.bitmap.is_marked(interior as usize));
        assert!(!marker.bitmap.is_marked(obj_ptr as usize));
    }

    #[test]
    fn initial_mark_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
    }

    #[test]
    fn concurrent_mark_follows_references() {
        let mut og = OldGen::new(65536);

        // Allocate object A (2 slots)
        let size_a = HEADER_SIZE + 2 * SLOT_SIZE;
        let ptr_a = og.alloc(size_a, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(2);
            h.set_gc_flags(0x01);
        }

        // Allocate object B (1 slot, no refs)
        let size_b = HEADER_SIZE + SLOT_SIZE;
        let ptr_b = og.alloc(size_b, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // A.field[0] = ref to B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            let obj_b = ObjectRef::from_raw(ptr_b);
            std::ptr::write(slot, Value::Object(Some(obj_b)));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // Both A and B should be marked
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(scanned >= 1); // At least B was scanned via A
    }

    #[test]
    fn sweep_frees_unmarked() {
        let mut og = OldGen::new(65536);

        // Allocate two objects
        let size = HEADER_SIZE + SLOT_SIZE;
        let live_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(live_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let dead_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(dead_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Only mark live_ptr as a root (dead_ptr is unreachable)
        let (_marked, swept) = marker.full_cycle(&stw(), &[live_ptr], &mut og);
        assert_eq!(swept, 1); // dead_ptr should be freed
        assert!(og.used() < used_before);
    }

    /// GCAUD-4 — the concurrent sweep must not act on an address-keyed
    /// snapshot another old-gen collection has invalidated.
    ///
    /// `concurrent_sweep` is the one phase of the cycle that runs outside a
    /// stop-the-world. Its liveness test is "the address existed at remark
    /// (`sweep_eligible`) AND its bit is clear (`bitmap`)" — two tables keyed
    /// on a bare old-gen address. Between remark and the sweep's lock
    /// acquisition, another thread's young GC can run `old_gen_gc`: the
    /// in-place arm hands blocks back to the free list, and the very next
    /// promotion re-issues those addresses to NEW, fully live objects. Such an
    /// object satisfies BOTH halves of the test — it inherited a dead object's
    /// address, so it is "eligible", and its bit is clear because the bit
    /// describes its predecessor — and the sweep frees it while it is live.
    ///
    /// This test drives exactly that sequence: remark, then free a dead block
    /// and let the allocator re-issue the same address to a live object. The
    /// sweep must reclaim nothing.
    #[test]
    fn concurrent_sweep_refuses_a_snapshot_invalidated_by_another_old_gen_collection() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let init = |ptr: *mut u8, cid: u32| {
            // SAFETY: `ptr` is a live `size`-byte old-gen block.
            unsafe {
                let h = &mut *(ptr as *mut ObjectHeader);
                h.class_id = ClassId::new(cid);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
                h.set_num_slots(1);
                h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
            }
        };

        let live = og.alloc(size, 8).unwrap();
        init(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init(dead, 2);
        // A third block, recycled below. It is unreachable at remark, so its
        // address enters `sweep_eligible` with its bit clear.
        let recycled = og.alloc(size, 8).unwrap();
        init(recycled, 3);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[live], &og);

        // --- another old-gen collection interleaves here -------------------
        // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(recycled, size) };
        let resurrected = og
            .alloc(size, 8)
            .expect("the just-freed block must be reusable");
        assert_eq!(
            resurrected, recycled,
            "precondition: the allocator must re-issue the freed address, which \
             is what makes the snapshot's address ambiguous",
        );
        init(resurrected, 4);
        // -------------------------------------------------------------------

        let used_before_sweep = og.used();
        let aborts_before = marker.state.census().sweep_epoch_aborts;
        let swept = marker.concurrent_sweep(&mut og);

        assert_eq!(
            swept, 0,
            "a sweep whose address-keyed snapshot was invalidated must reclaim \
             nothing — one of the eligible addresses now names a LIVE object",
        );
        assert!(
            marker.state.census().sweep_epoch_aborts > aborts_before,
            "the abandoned sweep must be counted, not silent",
        );
        assert_eq!(og.used(), used_before_sweep);
        assert!(
            og.is_allocated_addr(resurrected),
            "the resurrected (live) object must still be allocated after the sweep",
        );
        assert!(
            og.is_allocated_addr(dead),
            "and nothing else may be reclaimed on the abandoned path either",
        );
    }

    /// Shared object initialiser for the TAMS tests below.
    fn init_old_object(ptr: *mut u8, cid: u32) {
        // SAFETY: `ptr` is a live `HEADER_SIZE + SLOT_SIZE` old-gen block.
        unsafe {
            let h = &mut *(ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(cid);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            h.set_num_slots(1);
            h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        }
    }

    /// gengc-mark 2026-09-20 — TAMS must be anchored at INITIAL MARK.
    ///
    /// An object that enters the old generation DURING the concurrent trace
    /// (a young GC promoting a survivor, or a direct large-object allocation)
    /// cannot be discovered by this cycle: the trace may already have scanned
    /// and blackened every object that now points at it, the SATB pre-barrier
    /// logs only OLD slot values, and the remark root rescan sees it only if
    /// something outside the old gen still references it. With the snapshot
    /// taken at remark it was nevertheless "eligible", so the sweep freed a
    /// live object. Anchored at initial mark, it is ineligible by construction
    /// and is simply collected one cycle later.
    #[test]
    fn an_object_promoted_during_the_concurrent_phase_is_not_swept() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 2);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);

        // --- a promotion interleaves with the concurrent trace --------------
        let promoted = og.alloc(size, 8).unwrap();
        init_old_object(promoted, 3);
        // --------------------------------------------------------------------

        marker.remark(&stw(), &[live], &og);
        assert!(
            !marker.bitmap.is_marked(promoted as usize),
            "precondition: nothing in this cycle can have marked the promoted \
             object — that is the whole point of the test",
        );

        let swept = marker.concurrent_sweep(&mut og);
        assert!(
            og.is_allocated_addr(promoted),
            "an object that entered the old gen after initial mark is implicitly \
             live for this cycle and must survive the sweep",
        );
        assert_eq!(
            swept, 1,
            "only the object that was already dead at initial mark may be freed",
        );
        assert!(og.is_allocated_addr(live));
        assert!(!og.is_allocated_addr(dead));
    }

    /// gengc-mark 2026-09-20 — a cycle whose remark never ran must free
    /// nothing, even though its eligibility snapshot is now populated from
    /// initial mark onwards.
    ///
    /// This is the safety property the old arrangement got for free ("empty
    /// snapshot ⇒ remark never ran ⇒ free nothing") and that moving the
    /// snapshot earlier would have silently removed. It is now an explicit
    /// gate: `sweep_eligible_epoch` is stamped by `remark` and by nothing else.
    /// Without remark the bitmap is not final and the SATB log was never
    /// drained, so an unmarked object is not evidence of anything.
    #[test]
    fn a_sweep_without_a_remark_frees_nothing() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let unreached = og.alloc(size, 8).unwrap();
        init_old_object(unreached, 2);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);
        // No remark: model the driver losing the remark STW race.

        let used_before = og.used();
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(
            swept, 0,
            "an unauthorised snapshot must not license any reclamation",
        );
        assert_eq!(og.used(), used_before);
        assert!(og.is_allocated_addr(unreached));
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        // And `abort_cycle` -- the path the driver actually takes -- leaves the
        // same state, so a later sweep cannot consume the stale snapshot.
        let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker2.initial_mark(&[live], &og);
        marker2.abort_cycle();
        assert_eq!(marker2.concurrent_sweep(&mut og), 0);
        assert!(og.is_allocated_addr(unreached));
    }

    /// gengc-mark 2026-09-20 — GCAUD-4 now covers the whole cycle, not its tail.
    ///
    /// The epoch is stamped at initial mark, so a `free` (or `compact`) by
    /// another old-gen collector ANYWHERE in the cycle invalidates the
    /// address-keyed snapshot, and remark must fail closed rather than hand the
    /// sweep a set of addresses that no longer name the objects they named.
    #[test]
    fn a_reclaim_between_initial_mark_and_remark_abandons_the_cycle() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 2);
        let recycled = og.alloc(size, 8).unwrap();
        init_old_object(recycled, 3);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);

        // --- another old-gen collection interleaves -------------------------
        // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(recycled, size) };
        let resurrected = og
            .alloc(size, 8)
            .expect("the just-freed block must be reusable");
        assert_eq!(
            resurrected, recycled,
            "precondition: the allocator must re-issue the freed address",
        );
        init_old_object(resurrected, 4);
        // --------------------------------------------------------------------

        marker.remark(&stw(), &[live], &og);
        let swept = marker.concurrent_sweep(&mut og);

        assert_eq!(
            swept, 0,
            "a cycle whose snapshot was invalidated mid-flight must reclaim nothing",
        );
        assert!(og.is_allocated_addr(dead), "nothing may be reclaimed");
        assert!(og.is_allocated_addr(resurrected));
    }

    /// gengc-mark 2026-09-20 — the two SATB gates must never disagree in the
    /// unsafe direction.
    ///
    /// The generational `satb_barrier` checks the PHASE
    /// (`ConcurrentGcState::is_marking_active`); the log it writes into checks
    /// the QUEUE (`SatbQueue::is_active`). G1 asserts
    /// `is_marking_active() => satb_queue.is_active()`; `remark` used to break
    /// it for the width of its final closure, during which a mutator's
    /// pre-barrier entry went into a per-thread bucket nobody would drain
    /// again. Check the invariant at every phase boundary of a full cycle.
    ///
    /// gc-common w5-e: the barrier now tests the queue itself, so this is no
    /// longer the barrier's safety argument; it stays as the invariant the
    /// phase's other readers (the VM's root builders) rely on.
    #[test]
    fn the_phase_gate_is_never_wider_than_the_satb_gate() {
        let (mut og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let check = |marker: &ConcurrentMarker, where_: &str| {
            assert!(
                !marker.state.is_marking_active() || marker.satb_queue.is_active(),
                "{where_}: the mutator barrier is armed but its queue is not — \
                 an entry logged here would be stranded in a per-thread bucket",
            );
        };

        check(&marker, "idle");
        marker.initial_mark(&[obj_ptr], &og);
        check(&marker, "after initial_mark");
        marker.concurrent_mark(&og);
        check(&marker, "after concurrent_mark");
        marker.remark(&stw(), &[obj_ptr], &og);
        check(&marker, "after remark");
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
        marker.concurrent_sweep(&mut og);
        check(&marker, "after sweep");

        // An aborted cycle must leave the same agreement.
        let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker2.initial_mark(&[obj_ptr], &og);
        marker2.abort_cycle();
        check(&marker2, "after abort_cycle");
    }

    #[test]
    fn satb_prevents_lost_object() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A (root)
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // Object B (initially referenced by A)
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // A.field[0] = B initially
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Phase 1: initial mark
        marker.initial_mark(&[ptr_a], &og);

        // Simulate concurrent mutation: A.field[0] = null
        // The SATB barrier should log the OLD value (ptr_b).
        marker.satb_queue.flush(vec![ptr_b as usize]);

        // Now break the reference
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(None));
        }

        // Phase 2: concurrent mark (won't find B via A anymore). gen
        // r4w3/oldgen3: Phase 2 now drains the SATB log itself (see
        // `concurrent_mark_budget`), so B is discovered HERE rather than at
        // remark — the property is that B is marked before the sweep, not
        // which phase found it.
        marker.concurrent_mark(&og);
        assert!(
            marker.bitmap.is_marked(ptr_b as usize),
            "Phase 2 must drain the SATB log and mark B"
        );
        assert!(
            marker.satb_queue.is_empty(),
            "the log was drained, not left for remark"
        );

        // Phase 3: remark — B stays marked.
        let _discovered = marker.remark(&stw(), &[ptr_a], &og);
        assert!(marker.bitmap.is_marked(ptr_b as usize)); // B is live through the SATB snapshot

        // Phase 4: sweep — B should NOT be freed
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 0); // Both A and B are live
    }

    // gc-concmark HIGH regression — the SATB barrier must stay ACTIVE
    // across the remark closure, so a live old-gen reference overwritten
    // by a mutator DURING remark (after the snapshot drain, before sweep)
    // is still logged, marked, and NOT swept.
    //
    // Before the fix, `remark` called `deactivate_and_drain()` at the very
    // top, flipping the gate to INACTIVE before the closure was computed.
    // An SATB entry flushed after that point (modelling a write the barrier
    // would have logged had it still been active) was never captured by the
    // mark phase, leaving B's bit clear so the sweep freed a live object.
    #[test]
    fn satb_active_through_remark_closure() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A (root, no live refs to B by remark time).
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // Object B — only ever reachable via the SATB log of an overwrite.
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Phases 1–2: A is the only root; B is not reachable from A.
        marker.initial_mark(&[ptr_a], &og);
        marker.concurrent_mark(&og);

        // The barrier must still be active going into remark — that is the
        // invariant whose violation caused the bug.
        assert!(marker.satb_queue.is_active());

        // Model a mutator overwriting a live reference to B *during* the
        // concurrent window: the pre-barrier logs B's old address. With the
        // fix, `remark` drains this WITHOUT deactivating and marks B; the
        // final quiescing drain then closes the gate.
        marker.satb_queue.flush(vec![ptr_b as usize]);

        let discovered = marker.remark(&stw(), &[ptr_a], &og);
        assert!(discovered > 0, "B must be discovered from the SATB log");
        assert!(
            marker.bitmap.is_marked(ptr_b as usize),
            "B must be marked — the SATB-logged live ref was not lost"
        );
        // Gate must be off once the closure is final.
        assert!(!marker.satb_queue.is_active());

        // Sweep must keep B (it is live via the SATB snapshot).
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 0, "neither A nor B may be freed");
    }

    #[test]
    fn phase_transitions() {
        let marker = ConcurrentMarker::new(0x0, 1024);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        let og = OldGen::new(1024);
        marker.initial_mark(&[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        // Phase doesn't change after concurrent mark — stays ConcurrentMark

        marker.remark(&stw(), &[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn initial_mark_multiple_roots() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let ptrs: Vec<*mut u8> = (0..5)
            .map(|i| {
                let p = og.alloc(size, 8).unwrap();
                unsafe {
                    let h = &mut *(p as *mut ObjectHeader);
                    h.class_id = ClassId::new(i + 1);
                    h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                    h.set_num_slots(1);
                    h.set_gc_flags(0x01);
                }
                p
            })
            .collect();

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let count = marker.initial_mark(&ptrs, &og);

        assert_eq!(count, 5);
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
        assert_eq!(marker.queue.len(), 5);
    }

    #[test]
    fn initial_mark_skips_null_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let roots: Vec<*mut u8> = vec![std::ptr::null_mut(), obj_ptr, std::ptr::null_mut()];
        let count = marker.initial_mark(&roots, &og);

        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
    }

    #[test]
    fn initial_mark_deduplicates_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Same root listed twice — should only mark once
        let count = marker.initial_mark(&[obj_ptr, obj_ptr], &og);
        assert_eq!(count, 1);
        assert_eq!(marker.queue.len(), 1);
    }

    #[test]
    fn empty_heap_marking() {
        let og = OldGen::new(4096);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[], &og);
        assert_eq!(count, 0);

        let scanned = marker.concurrent_mark(&og);
        assert_eq!(scanned, 0);

        let discovered = marker.remark(&stw(), &[], &og);
        assert_eq!(discovered, 0);
    }

    #[test]
    fn single_object_full_cycle() {
        let (mut og, obj_ptr) = make_old_gen_with_object(0);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let (marked, swept) = marker.full_cycle(&stw(), &[obj_ptr], &mut og);
        // The single object is a root, so it should survive
        assert!(marked >= 1);
        assert_eq!(swept, 0);
    }

    #[test]
    fn mark_queue_push_pop_ordering() {
        let q = MarkQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        q.push(0x300 as *mut u8);
        assert_eq!(q.len(), 3);

        // gen r4w2/concmark: the SET popped, not the order. These three
        // addresses all used to land in shard 0, which is what made the pops
        // look FIFO; `shard_for` now spreads them, and `pop` visits shards
        // round-robin, so only per-shard order is FIFO. Nothing in the marker
        // depends on cross-shard order.
        let mut popped: Vec<usize> = (0..3).map(|_| q.pop().unwrap() as usize).collect();
        popped.sort_unstable();
        assert_eq!(popped, vec![0x100, 0x200, 0x300]);
        assert!(q.pop().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn mark_queue_push_batch() {
        let q = MarkQueue::new();
        let ptrs: Vec<*mut u8> = (1..=4).map(|i| (i * 0x100) as *mut u8).collect();

        q.push_batch(&ptrs);
        assert_eq!(q.len(), 4);

        // The set, not the order — see `mark_queue_push_pop_ordering`.
        let mut popped: Vec<usize> = (0..4).map(|_| q.pop().unwrap() as usize).collect();
        popped.sort_unstable();
        let mut expected: Vec<usize> = ptrs.iter().map(|p| *p as usize).collect();
        expected.sort_unstable();
        assert_eq!(popped, expected);
        assert!(q.pop().is_none());
    }

    /// gen r4w2/concmark — 16-byte-strided object starts (the legacy object
    /// shape) must reach every shard. The old low-bits `shard_for` put them all
    /// on the even shards, halving both the sharding and the effective
    /// overflow cap.
    #[test]
    fn shard_for_spreads_sixteen_byte_strides_over_every_shard() {
        let mut seen = [0usize; MARK_QUEUE_SHARDS];
        for i in 0..4096usize {
            let ptr = (0x1000_0000usize + i * 16) as *mut u8;
            let s = MarkQueue::shard_for(ptr);
            assert!(s < MARK_QUEUE_SHARDS, "shard index out of range: {s}");
            seen[s] += 1;
        }
        for (s, &n) in seen.iter().enumerate() {
            assert!(
                n > 0,
                "shard {s} never used by a 16-byte stride: {seen:?} — the odd \
                 shards being empty is the regression this test exists for",
            );
        }
    }

    #[test]
    fn mark_queue_clear() {
        let q = MarkQueue::new();
        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        assert_eq!(q.len(), 2);

        q.clear();
        assert!(q.is_empty());
        assert!(q.pop().is_none());
    }

    #[test]
    fn mark_queue_concurrent_push_pop() {
        use std::sync::Arc;

        let q = Arc::new(MarkQueue::new());
        let mut handles = Vec::new();

        // 4 threads each push 100 items
        for t in 0..4u64 {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100u64 {
                    q.push((t * 1000 + i) as *mut u8);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(q.len(), 400);

        // Drain all
        let mut count = 0;
        while q.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn concurrent_gc_state_is_marking_active() {
        let state = ConcurrentGcState::new();
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::InitialMark);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Remark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Idle);
        assert!(!state.is_marking_active());
    }

    #[test]
    fn phase_from_u8_invalid_defaults_to_idle() {
        assert_eq!(ConcurrentGcPhase::from(255), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(5), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(100), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn phase_from_u8_all_valid() {
        assert_eq!(ConcurrentGcPhase::from(0), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(1), ConcurrentGcPhase::InitialMark);
        assert_eq!(
            ConcurrentGcPhase::from(2),
            ConcurrentGcPhase::ConcurrentMark
        );
        assert_eq!(ConcurrentGcPhase::from(3), ConcurrentGcPhase::Remark);
        assert_eq!(
            ConcurrentGcPhase::from(4),
            ConcurrentGcPhase::ConcurrentSweep
        );
    }

    #[test]
    fn large_object_graph_marking() {
        let mut og = OldGen::new(1 << 20); // 1 MB
        let size = HEADER_SIZE + SLOT_SIZE;

        // Build a chain: obj[0] -> obj[1] -> ... -> obj[N-1]
        let n = 50;
        let mut ptrs: Vec<*mut u8> = Vec::new();
        for i in 0..n {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i as u32 + 1);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
            ptrs.push(p);
        }

        // Wire up chain references: ptrs[i].slot[0] = ptrs[i+1]
        for i in 0..n - 1 {
            unsafe {
                let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
            }
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // Only root is the first object
        marker.initial_mark(&[ptrs[0]], &og);
        marker.concurrent_mark(&og);

        // All objects in the chain should be marked
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
    }

    #[test]
    fn marking_graph_with_cycle() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Create A -> B -> C -> A (cycle)
        let ptr_a = og.alloc(size, 8).unwrap();
        let ptr_b = og.alloc(size, 8).unwrap();
        let ptr_c = og.alloc(size, 8).unwrap();

        for (p, id) in [(ptr_a, 1u32), (ptr_b, 2), (ptr_c, 3)] {
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(id);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
        }

        // A -> B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }
        // B -> C
        unsafe {
            let slot = ptr_b.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_c))));
        }
        // C -> A (back edge creating cycle)
        unsafe {
            let slot = ptr_c.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_a))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // All three should be marked despite the cycle
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(marker.bitmap.is_marked(ptr_c as usize));
        assert!(scanned >= 2); // A scanned in initial_mark's queue, B and C via concurrent
    }

    #[test]
    fn full_cycle_phase_sequence() {
        let (mut og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Before cycle: Idle
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

        marker.remark(&stw(), &[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());

        marker.concurrent_sweep(&mut og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn sweep_all_unreachable() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Allocate 3 objects, none rooted
        for i in 0..3 {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i + 1);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let (_marked, swept) = marker.full_cycle(&stw(), &[], &mut og);

        assert_eq!(swept, 3);
        assert!(og.used() < used_before);
    }

    // gc-concmark MEDIUM regression — the concurrent-mark object-slot read
    // must serialize against striped mutator writes so it never observes a
    // torn (tag, payload) `Value`. Here a writer thread continuously flips a
    // reference slot between `Object(Some(b))` and `Object(None)` while
    // holding the SAME per-slot stripe lock that `scan_object` now takes; the
    // marker scans the object in a tight loop on another thread. Without the
    // stripe lock in `scan_object`, a torn read could splice the non-null tag
    // of one store with the (null) payload of another and feed a bogus
    // pointer into `old_gen.contains` / `try_mark` — corrupting the heap or
    // crashing. With the lock, every read sees a fully-old or fully-new value,
    // so the test runs to completion and only ever marks the real target `b`.
    #[test]
    fn scan_object_serializes_against_striped_writer() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A — single reference slot, the contended one.
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }
        // Object B — the only legitimate target A's slot can point to.
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let marker = Arc::new(ConcurrentMarker::new(og.base_ptr() as usize, og.capacity()));
        // The marker must be in the concurrent-mark phase so its bitmap is live.
        marker.initial_mark(&[ptr_a], &og);

        let stop = Arc::new(AtomicBool::new(false));
        let a_addr = ptr_a as usize;
        let b_addr = ptr_b as usize;
        let obj_a = unsafe { ObjectRef::from_raw(ptr_a) };
        let obj_b = unsafe { ObjectRef::from_raw(ptr_b) };

        // Writer: flip A.slot[0] between Some(B) and None under the stripe
        // lock, exactly as the volatile field-access helpers do.
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let slot = (a_addr + HEADER_SIZE) as *mut Value;
                let mut toggle = false;
                while !stop.load(Ordering::Relaxed) {
                    let _g = crate::collector::volatile_stripe_lock(obj_a, 0);
                    std::sync::atomic::fence(Ordering::SeqCst);
                    let v = if toggle {
                        Value::Object(Some(obj_b))
                    } else {
                        Value::Object(None)
                    };
                    // SAFETY: slot is A's single in-bounds reference field.
                    unsafe { std::ptr::write(slot, v) };
                    std::sync::atomic::fence(Ordering::SeqCst);
                    toggle = !toggle;
                }
            })
        };

        // Reader: scan A many times concurrently with the writer.
        let object_starts = old_gen_object_starts(&og);
        for _ in 0..50_000 {
            marker.scan_object(a_addr as *mut u8, &og, &object_starts);
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // A must still be marked; B must be marked iff it was observed as a
        // live target — either is legal, but no THIRD address may ever have
        // been marked (a torn read would have produced a garbage pointer that
        // either failed `contains` or, worse, aliased some other object).
        assert!(marker.bitmap.is_marked(a_addr));
        // Sanity: the only addresses that can possibly be marked are A and B.
        // (B is the sole non-null value the writer ever stores.)
        let _ = b_addr; // referenced for clarity; marking B is permitted, not required.
    }

    /// fork6 GC_STRESS fix — `with_shared` must adopt the caller's SATB queue
    /// + phase state (the heap-attached instances the write barrier reaches),
    /// and a pre-barrier log flushed into that SHARED queue must be marked by
    /// remark. With the old per-cycle private queue this plumbing did not
    /// exist and the logged target was swept while live.
    #[test]
    fn with_shared_marker_marks_satb_entries_from_shared_queue() {
        // One object, reachable ONLY via the (simulated) overwritten ref.
        let mut og2 = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let live = og2.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(live as *mut ObjectHeader);
            h.class_id = ClassId::new(7);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let satb = Arc::new(SatbQueue::new());
        let state = Arc::new(ConcurrentGcState::new());
        let marker = ConcurrentMarker::with_shared(
            og2.base_ptr() as usize,
            og2.capacity(),
            satb.clone(),
            state.clone(),
        );
        // The marker must hold the SAME instances (not copies).
        assert!(Arc::ptr_eq(&marker.satb_queue, &satb));
        assert!(Arc::ptr_eq(&marker.state, &state));

        // Initial mark with NO roots: `live` is invisible to the trace.
        marker.initial_mark(&[], &og2);
        assert!(
            satb.is_active(),
            "initial_mark must activate the shared queue"
        );
        assert!(state.is_marking_active());

        // Simulate the write barrier on another code path: a mutator
        // overwrote the only reference to `live` during concurrent mark and
        // the pre-barrier logged the old value into the SHARED queue.
        crate::satb::satb_thread_local_log(&satb, live as usize);
        crate::satb::flush_thread_satb_buffer(&satb);

        marker.concurrent_mark(&og2);
        marker.remark(&stw(), &[], &og2);

        assert!(
            marker.bitmap.is_marked(live as usize),
            "remark must mark targets logged into the SHARED SATB queue"
        );
    }

    /// fork6 GC_STRESS fix — a cycle whose remark STW could not be acquired
    /// must be abortable: `abort_cycle` deactivates the (shared) SATB barrier
    /// and returns the phase to Idle so the write barrier stops logging and
    /// the next trigger starts fresh. The caller skips the sweep entirely.
    #[test]
    fn abort_cycle_deactivates_barrier_and_resets_phase() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        marker.initial_mark(&[ptr], &og);
        assert!(marker.satb_queue.is_active());
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

        marker.abort_cycle();
        assert!(
            !marker.satb_queue.is_active(),
            "abort must deactivate the barrier"
        );
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    }

    // ------------------------------------------------------------------
    // gengc-mark2 2026-09-20
    // ------------------------------------------------------------------

    /// `gengc-mark-old-gen-object-starts-is-a-hashset-per-phase-FIXED-20260923`.
    ///
    /// The bitmap must answer EXACTLY what the `HashSet<usize>` it replaced
    /// answered — the same membership at every address in (and around) the
    /// generation, and the same cardinality. Mirrors
    /// `young_mark::object_start_bits_match_a_hash_set_exactly`, which is the
    /// in-repo precedent this change reuses.
    #[test]
    fn old_gen_object_starts_bitmap_matches_a_hash_set_exactly() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let mut allocated: Vec<*mut u8> = Vec::new();
        for i in 0..64u32 {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, i + 1);
            allocated.push(p);
        }
        // Free a few so the walk has gaps to skip: that is what makes the
        // "starts" set different from "every 8-byte slot in the generation",
        // and it is the case a bitmap could get wrong.
        for i in [5usize, 17, 40] {
            // SAFETY: each is a live block of exactly `size` bytes from this
            // generation, freed once.
            unsafe { og.free(allocated[i], size) };
        }
        let reference: HashSet<usize> = og
            .walk_objects()
            .into_iter()
            .map(|(p, _)| p as usize)
            .collect();

        let bits = old_gen_object_starts(&og);
        assert_eq!(
            bits.len(),
            reference.len(),
            "bitmap cardinality must equal the HashSet's"
        );
        assert!(!bits.is_empty());

        // Every 8-byte slot in the generation, plus a margin either side, must
        // get the same verdict from both.
        // Walk 8-ALIGNED addresses: every object start is absolutely
        // 8-aligned, so a loop anchored on an unaligned base would step past
        // all of them and the comparison would be vacuous.
        let (lo, hi) = og.extent();
        let mut addr = (lo & !7usize).saturating_sub(64);
        while addr < hi + 64 {
            assert_eq!(
                bits.contains(addr),
                reference.contains(&addr),
                "bitmap and HashSet disagree at {addr:#x}"
            );
            addr += 8;
        }
        // And an interior (non-start) address of a real object is not a start.
        assert!(!bits.contains(allocated[0] as usize + 8));
        // A freed object's address is no longer a start, in both.
        assert!(!bits.contains(allocated[5] as usize));
    }

    /// An empty generation yields an empty, zero-length set — the condition
    /// `concurrent_sweep` uses as its "free nothing" exit.
    #[test]
    fn old_gen_object_starts_on_an_empty_generation_is_empty() {
        let og = OldGen::new(65536);
        let bits = old_gen_object_starts(&og);
        assert!(bits.is_empty());
        assert_eq!(bits.len(), 0);
        assert!(!bits.contains(og.base_ptr() as usize));
    }

    /// Remark narrows the initial-mark snapshot by intersecting it with the
    /// current walk. The word-wise AND must produce exactly what
    /// `HashSet::retain` produced.
    #[test]
    fn object_starts_intersection_keeps_exactly_the_common_starts() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(size, 8).unwrap();
        init_old_object(a, 1);
        let b = og.alloc(size, 8).unwrap();
        init_old_object(b, 2);

        let mut at_initial_mark = old_gen_object_starts(&og);
        assert!(at_initial_mark.contains(a as usize));
        assert!(at_initial_mark.contains(b as usize));

        // `b` goes away and `c` arrives. `c` lands either in `b`'s freed block
        // or above it — never below, because nothing is free below `a` — so
        // the later walk spans at least as far as the earlier one, which is
        // the precondition `retain_intersection` requires.
        // SAFETY: `b` is a live block of exactly `size` bytes from this gen.
        unsafe { og.free(b, size) };
        let c = og.alloc(size, 8).unwrap();
        init_old_object(c, 3);
        let at_remark = old_gen_object_starts(&og);

        assert!(
            at_initial_mark.retain_intersection(&at_remark),
            "the two walks share a base and the later one is no shorter"
        );
        assert!(
            at_initial_mark.contains(a as usize),
            "`a` was present at both walks and must survive the narrowing"
        );
        // ORCHESTRATOR CORRECTION, 2026-09-20 — this assertion used to be
        // unconditional (`!contains(c)`), which contradicted the very next
        // block's acknowledgement that `c` may land on `b`'s address. It does:
        // `b` is freed and `c` requests the SAME size, so the size-segregated
        // free list hands back exactly `b`'s block on the usual path, and the
        // test failed on its own stated edge case rather than on a defect.
        //
        // Both outcomes are correct behaviour and the test now says which is
        // which, because the distinction is the whole point of the epoch check
        // living somewhere else.
        if std::ptr::eq(c, b) {
            // The free list reused the block. That ADDRESS was genuinely
            // present at both walks, so a word-wise AND keeps it — and must.
            // An address-keyed intersection cannot tell the dead `b` from the
            // live `c` that replaced it, and is not the mechanism that is
            // supposed to: `reclaim_epoch` is (see `release_unused_tail`'s
            // address-identity stamp). Asserting absence here would be
            // asserting that the intersection does a job it deliberately
            // delegates.
            assert!(
                at_initial_mark.contains(c as usize),
                "an address live at both walks must survive the narrowing, \
                 whichever object owned it"
            );
        } else {
            assert!(
                !at_initial_mark.contains(c as usize),
                "`c` did not exist at initial mark and must not be added by it"
            );
            assert!(
                !at_initial_mark.contains(b as usize),
                "`b` was freed before the remark walk and must be narrowed out"
            );
        }
    }

    /// `gengc-mark-markqueue-has-no-termination-detection-FIXED-20260923`.
    ///
    /// The property the plain `pop` loop cannot give: with N markers, every
    /// node of a graph is scanned EXACTLY once and no marker leaves while a
    /// peer still has children to push. The graph is a chain, so at any
    /// instant at most one node is available — a worker that terminates on
    /// "the queue looked empty" loses the rest of the chain, which is the
    /// failure this primitive exists to prevent.
    #[test]
    fn parallel_drain_visits_every_node_exactly_once() {
        // Long enough that the "peer is mid-scan with an empty queue" window
        // is hit many times per run, short enough that eight workers parking
        // and waking on every link stays a fast unit test.
        const NODES: usize = 1024;
        for &workers in &[1usize, 2, 4, 8] {
            let queue = MarkQueue::new();
            // Node k is the address `(k + 1) * 8`; scanning node k pushes
            // node k + 1. Addresses are never dereferenced here.
            let visits: Vec<AtomicUsize> = (0..NODES).map(|_| AtomicUsize::new(0)).collect();
            queue.push(8 as *mut u8);
            queue.begin_drain(workers);

            let run = || {
                let _w = queue.worker();
                while let Some(ptr) = queue.pop_or_terminate() {
                    let k = (ptr as usize / 8) - 1;
                    visits[k].fetch_add(1, Ordering::Relaxed);
                    // Make the "peer is inside a scan with the queue empty"
                    // window wide enough to be hit.
                    std::thread::yield_now();
                    if k + 1 < NODES {
                        queue.push(((k + 2) * 8) as *mut u8);
                    }
                }
            };

            std::thread::scope(|s| {
                for _ in 1..workers {
                    s.spawn(&run);
                }
                run();
            });

            assert!(
                queue.drain_is_done(),
                "{workers} worker(s): the drain must end by declaring completion, \
                 not by a worker guessing the queue is empty"
            );
            for (k, v) in visits.iter().enumerate() {
                assert_eq!(
                    v.load(Ordering::Relaxed),
                    1,
                    "{workers} worker(s): node {k} was scanned {} times, not once",
                    v.load(Ordering::Relaxed)
                );
            }
            assert!(queue.is_empty());
        }
    }

    /// The race the chain test makes likely, made deterministic: one marker is
    /// held inside its "scan" while the queue is empty, and the other markers
    /// must not declare the closure complete until it has pushed its child.
    #[test]
    fn a_marker_may_not_terminate_while_a_peer_is_mid_scan() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Barrier;

        let queue = MarkQueue::new();
        let scanning = Barrier::new(2);
        let child_seen = AtomicBool::new(false);
        queue.push(0x100 as *mut u8);
        queue.begin_drain(2);

        std::thread::scope(|s| {
            // Worker A pops 0x100, then parks INSIDE the scan until B has had
            // every chance to observe an empty queue, and only then pushes.
            s.spawn(|| {
                let _w = queue.worker();
                let first = queue.pop_or_terminate().expect("seed must be popped");
                assert_eq!(first as usize, 0x100);
                scanning.wait();
                std::thread::yield_now();
                queue.push(0x200 as *mut u8);
                while let Some(p) = queue.pop_or_terminate() {
                    if p as usize == 0x200 {
                        child_seen.store(true, Ordering::Relaxed);
                    }
                }
            });
            // Worker B sees an empty queue while A holds the only node.
            let _w = queue.worker();
            scanning.wait();
            while let Some(p) = queue.pop_or_terminate() {
                if p as usize == 0x200 {
                    child_seen.store(true, Ordering::Relaxed);
                }
            }
        });

        assert!(
            child_seen.load(Ordering::Relaxed),
            "a child pushed while a peer observed an empty queue was never scanned — \
             this is exactly the premature-termination bug"
        );
    }

    /// A worker that unwinds must not park its peers forever: `MarkWorker`'s
    /// `Drop` decrements the live count on the unwind path too.
    #[test]
    fn a_panicking_marker_releases_its_peers() {
        let queue = std::sync::Arc::new(MarkQueue::new());
        queue.begin_drain(2);

        let panicker = {
            let q = queue.clone();
            std::thread::spawn(move || {
                let _w = q.worker();
                panic!("marker died mid-scan");
            })
        };
        // The survivor must reach termination rather than parking forever.
        let survivor = {
            let q = queue.clone();
            std::thread::spawn(move || {
                let _w = q.worker();
                while q.pop_or_terminate().is_some() {}
            })
        };
        assert!(panicker.join().is_err());
        survivor
            .join()
            .expect("the surviving marker must terminate, not hang");
    }

    /// `gengc-mark-concurrent-mark-holds-the-old-gen-lock-FIXED-20260923`.
    ///
    /// Slicing Phase 2 must reach the same fixed point as the unbounded run:
    /// the same bitmap, the same total scan count, and a `true` completion
    /// flag exactly once.
    #[test]
    fn a_sliced_concurrent_mark_reaches_the_same_fixed_point() {
        /// Build a chain of `n` old-gen objects, each pointing at the next,
        /// and return `(old_gen, root)`.
        fn chain(n: usize) -> (OldGen, *mut u8) {
            let mut og = OldGen::new(1 << 20);
            let size = HEADER_SIZE + SLOT_SIZE;
            let mut ptrs = Vec::new();
            for i in 0..n {
                let p = og.alloc(size, 8).unwrap();
                init_old_object(p, i as u32 + 1);
                ptrs.push(p);
            }
            for i in 0..n - 1 {
                // SAFETY: slot 0 of a 1-slot object allocated just above.
                unsafe {
                    let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                    slot.write(Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
                }
            }
            (og, ptrs[0])
        }

        const N: usize = 200;

        let (og, root) = chain(N);
        let whole = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        whole.initial_mark(&[root], &og);
        let whole_scanned = whole.concurrent_mark(&og);

        let (og2, root2) = chain(N);
        let sliced = ConcurrentMarker::new(og2.base_ptr() as usize, og2.capacity());
        sliced.initial_mark(&[root2], &og2);
        let mut sliced_scanned = 0usize;
        let mut slices = 0usize;
        loop {
            let (n, done) = sliced.concurrent_mark_budget(&og2, 7);
            sliced_scanned += n;
            slices += 1;
            assert!(slices < N + 10, "slicing must terminate");
            if done {
                break;
            }
        }

        assert!(
            slices > 1,
            "a budget of 7 over a 200-node chain must take many slices, took {slices}"
        );
        assert_eq!(
            sliced_scanned, whole_scanned,
            "the sliced mark must scan the same number of objects as the whole-phase mark"
        );
        assert_eq!(
            sliced.bitmap.marked_count(),
            whole.bitmap.marked_count(),
            "the sliced mark must reach the same bitmap"
        );
        assert_eq!(whole.bitmap.marked_count(), N);
    }

    /// A zero budget must still make progress rather than spinning forever.
    #[test]
    fn a_zero_budget_slice_still_scans_one_object() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr], &og);
        let (n, done) = marker.concurrent_mark_budget(&og, 0);
        assert_eq!(n, 1);
        assert!(done);
    }

    /// gen r4/mark (2026-09-23) — a slice must not keep tracing a cycle whose
    /// old-gen layout moved under it.
    ///
    /// Once `reclaim_epoch` has moved since initial mark, `remark` is certain
    /// to fail the cycle closed, and every queued address may name a recycled
    /// block. The slice drops the queue and reports the phase complete instead
    /// of scanning them.
    #[test]
    fn a_slice_stops_tracing_once_the_epoch_moved() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let root = og.alloc(size, 8).unwrap();
        init_old_object(root, 1);
        let victim = og.alloc(size, 8).unwrap();
        init_old_object(victim, 2);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], &og);
        assert!(!marker.queue.is_empty(), "precondition: the root is queued");

        // Another old-gen collection interleaves between two slices.
        // SAFETY: `(victim, size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(victim, size) };

        let (scanned, done) = marker.concurrent_mark_budget(&og, 1_000);
        assert_eq!(scanned, 0, "nothing may be traced for a dead cycle");
        assert!(done);
        assert!(marker.queue.is_empty(), "the stale queue must be dropped");

        // Control: with the epoch intact the same slice traces normally.
        let (og2, ptr2) = make_old_gen_with_object(1);
        let marker2 = ConcurrentMarker::new(og2.base_ptr() as usize, og2.capacity());
        marker2.initial_mark(&[ptr2], &og2);
        assert_eq!(marker2.concurrent_mark_budget(&og2, 1_000), (1, true));
    }

    /// gen r4/mark (2026-09-23) — `abort_cycle` now closes the PHASE gate
    /// before the QUEUE gate (the order `remark` was fixed to on 2026-09-20).
    /// The observable post-conditions must not have moved.
    #[test]
    fn abort_cycle_post_conditions_are_unchanged_by_the_reordering() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr], &og);
        assert!(marker.state.is_marking_active());
        assert!(marker.satb_queue.is_active());
        let drops_before = marker.satb_queue.late_log_drops();
        marker.abort_cycle();
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
        assert!(!marker.satb_queue.is_active());
        assert!(marker.queue.is_empty());
        assert_eq!(marker.satb_queue.late_log_drops(), drops_before);
    }

    // -----------------------------------------------------------------------
    // gen r4w2/concmark (2026-09-23)
    // -----------------------------------------------------------------------

    /// `n` one-slot old-gen objects, each pointing at the next.
    fn r4w2_chain(n: usize) -> (OldGen, Vec<*mut u8>) {
        let mut og = OldGen::new(1 << 20);
        let size = HEADER_SIZE + SLOT_SIZE;
        let mut ptrs = Vec::new();
        for i in 0..n {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, i as u32 + 1);
            ptrs.push(p);
        }
        for i in 0..n.saturating_sub(1) {
            r4w2_link(ptrs[i], ptrs[i + 1]);
        }
        (og, ptrs)
    }

    /// `from.slot0 = to`.
    fn r4w2_link(from: *mut u8, to: *mut u8) {
        // SAFETY: `from` is a live one-slot object allocated by the test and
        // `to` a live object start; slot 0 is inside `from`.
        unsafe {
            let slot = from.add(HEADER_SIZE) as *mut Value;
            slot.write(Value::Object(Some(ObjectRef::from_raw(to))));
        }
    }

    fn r4w2_cached_walk(marker: &ConcurrentMarker) -> Option<*const OldGenObjectStarts> {
        marker
            .starts_cache
            .lock()
            .as_ref()
            .map(|c| Arc::as_ptr(&c.starts))
    }

    /// Two drivers on the SAME shared state: only one may own a cycle, and the
    /// state becomes openable again only once the owner has finished.
    #[test]
    fn only_one_driver_can_own_a_cycle_on_shared_state() {
        let (mut og, ptr) = make_old_gen_with_object(1);
        let satb = Arc::new(SatbQueue::new());
        let state = Arc::new(ConcurrentGcState::new());
        let base = og.base_ptr() as usize;
        let cap = og.capacity();
        let a = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
        let b = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());

        let owned = a.try_open_cycle().expect("an idle state must be openable");
        assert_eq!(state.phase(), ConcurrentGcPhase::InitialMark);
        assert!(
            b.try_open_cycle().is_none(),
            "a second driver must not open a cycle over an open one"
        );

        a.initial_mark(&[ptr], &og);
        assert!(b.try_open_cycle().is_none(), "nor while it marks");
        a.concurrent_mark(&og);
        a.remark(&stw(), &[ptr], &og);
        assert!(b.try_open_cycle().is_none(), "nor between remark and sweep");
        a.concurrent_sweep(&mut og);
        owned.complete();
        assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
        assert!(!satb.is_active());

        let reopened = b
            .try_open_cycle()
            .expect("a finished cycle frees the state");
        drop(reopened);
        assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
    }

    /// `finish_cycle` must not clobber a cycle another driver opened after this
    /// one's sweep had already handed the state back.
    #[test]
    fn finish_cycle_cannot_reopen_the_next_drivers_cycle() {
        let (mut og, ptr) = make_old_gen_with_object(1);
        let satb = Arc::new(SatbQueue::new());
        let state = Arc::new(ConcurrentGcState::new());
        let base = og.base_ptr() as usize;
        let cap = og.capacity();
        let a = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
        let b = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());
        let c = ConcurrentMarker::with_shared(base, cap, satb.clone(), state.clone());

        let owned_a = a.try_open_cycle().unwrap();
        a.initial_mark(&[ptr], &og);
        a.concurrent_mark(&og);
        a.remark(&stw(), &[ptr], &og);
        a.concurrent_sweep(&mut og);
        // The sweep handed the state back: B opens before A's driver returns.
        let owned_b = b
            .try_open_cycle()
            .expect("the sweep's Idle is the hand-off");
        // A's late completion must leave B's cycle alone...
        owned_a.complete();
        assert_eq!(state.phase(), ConcurrentGcPhase::InitialMark);
        // ...so a third driver still cannot open a concurrent one.
        assert!(c.try_open_cycle().is_none());
        drop(owned_b);
        assert_eq!(state.phase(), ConcurrentGcPhase::Idle);
    }

    /// Every early exit of the driver drops the ownership; after an initial
    /// mark that must take the barrier down, not just the phase.
    #[test]
    fn dropping_an_open_cycle_abandons_it() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Lost initial-mark pause: nothing ran, the phase just goes back.
        drop(marker.try_open_cycle().unwrap());
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
        assert!(!marker.satb_queue.is_active());

        // Lost remark (or a stale epoch) after initial mark: full abort.
        let owned = marker.try_open_cycle().unwrap();
        marker.initial_mark(&[ptr], &og);
        assert!(marker.state.is_marking_active());
        assert!(marker.satb_queue.is_active());
        owned.abort();
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
        assert!(!marker.satb_queue.is_active());
        assert!(marker.queue.is_empty());
        assert!(
            r4w2_cached_walk(&marker).is_none(),
            "abort drops the cached walk"
        );
        assert!(marker.try_open_cycle().is_some());
    }

    /// The slice cache: reused while `free_list_seq` stands still, rebuilt the
    /// moment an allocation moves it — and the rebuilt set is what makes an
    /// object allocated between two slices markable.
    #[test]
    fn slices_reuse_the_walk_until_an_allocation_moves_the_free_list() {
        const N: usize = 40;
        let (mut og, nodes) = r4w2_chain(N);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // The re-walking arm: `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` is default
        // on since gce (2026-09-29) and decided at the initial mark; `=0`
        // restores the per-slice walk this test pins (the TAMS arm is
        // `gcd_d9e_phase2_walks_are_counted_and_the_tams_snapshot_skips_them`).
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_CONC_MARK_TAMS_STARTS", Some("0"))],
            || marker.initial_mark(&[nodes[0]], &og),
        );
        let seeded = r4w2_cached_walk(&marker).expect("initial_mark seeds the cache");

        let (n1, done1) = marker.concurrent_mark_budget(&og, 5);
        assert_eq!(n1, 5);
        assert!(!done1);
        assert_eq!(
            r4w2_cached_walk(&marker),
            Some(seeded),
            "nothing was allocated since initial mark, so the slice must reuse its walk"
        );
        let (_, done2) = marker.concurrent_mark_budget(&og, 5);
        assert!(!done2);
        assert_eq!(r4w2_cached_walk(&marker), Some(seeded));

        // A "promotion" between two slices, stored into a node the trace has
        // not reached yet.
        let x = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        init_old_object(x, 999);
        r4w2_link(nodes[N - 1], x);

        let mut slices = 0;
        loop {
            let (_, done) = marker.concurrent_mark_budget(&og, 5);
            slices += 1;
            assert!(slices < N, "slicing must terminate");
            if done {
                break;
            }
        }
        assert_ne!(
            r4w2_cached_walk(&marker),
            Some(seeded),
            "the allocation moved free_list_seq, so the next slice had to re-walk"
        );
        assert!(
            marker.bitmap.is_marked(x as usize),
            "an object allocated between slices must be markable in a later slice"
        );
        assert_eq!(marker.bitmap.marked_count(), N + 1);
    }

    /// `gengc-r4-mark-walk-desync-tail-is-untraced-FIXED-20260923` for the concurrent
    /// cycle. Layout `A, C, B(corrupt), X` with `A → X → C`: the walk stops at
    /// B, so X is not an object start, is never traced, and C — walked,
    /// eligible, reachable only through X — has a clear bit. The sweep used to
    /// free C while X still pointed at it. It must now reclaim nothing.
    #[test]
    fn a_desynced_walk_refuses_the_sweep() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(size, 8).unwrap();
        init_old_object(a, 1);
        let c = og.alloc(size, 8).unwrap();
        init_old_object(c, 2);
        let b = og.alloc(size, 8).unwrap();
        init_old_object(b, 3);
        let x = og.alloc(size, 8).unwrap();
        init_old_object(x, 4);
        r4w2_link(a, x);
        r4w2_link(x, c);
        // SAFETY: the kind-tag byte of a live allocation; any `u8` may be
        // written there, and 0xFF is not a valid `ObjectKind`.
        unsafe { std::ptr::write(b.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };

        let aborts_before = CONC_MARK_WALK_DESYNC_ABORTS.load(Ordering::Relaxed);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[a], &og);
        marker.concurrent_mark(&og);
        assert!(
            !marker.bitmap.is_marked(c as usize),
            "precondition: X is not an object start, so C is never reached"
        );
        marker.remark(&stw(), &[a], &og);
        let swept = marker.concurrent_sweep(&mut og);

        assert_eq!(swept, 0, "a cycle whose walk desynced must reclaim nothing");
        assert!(
            og.is_allocated_addr(c),
            "C is live through X and must survive"
        );
        assert!(
            CONC_MARK_WALK_DESYNC_ABORTS.load(Ordering::Relaxed) > aborts_before,
            "the refusal must be counted"
        );
    }

    /// gen r4w6/oldpin6 — the same layout with the opt-in walk-gap recovery
    /// (`CRATONVM_GC_CONC_WALK_GAP_RECOVERY`, forced for this thread), plus a
    /// dead walked object E: `A, C, E, B(corrupt), X` with `A → X → C`. The
    /// remark seeds the gap's words (X's slot names C), so C survives; E is
    /// walked, eligible and unreachable, so it is freed; nothing in the gap
    /// (B, X) is freed. The cycle reclaims instead of refusing.
    #[test]
    fn a_desynced_walk_sweeps_through_the_gap_when_recovery_is_on() {
        FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(true));
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(size, 8).unwrap();
        init_old_object(a, 1);
        let c = og.alloc(size, 8).unwrap();
        init_old_object(c, 2);
        let e = og.alloc(size, 8).unwrap();
        init_old_object(e, 5);
        let b = og.alloc(size, 8).unwrap();
        init_old_object(b, 3);
        let x = og.alloc(size, 8).unwrap();
        init_old_object(x, 4);
        r4w2_link(a, x);
        r4w2_link(x, c);
        // SAFETY: the kind-tag byte of a live allocation; 0xFF is not a valid
        // `ObjectKind`, so the walk stops at B.
        unsafe { std::ptr::write(b.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8) };
        let (walked, gaps) = og.walk_objects_with_gaps();
        assert_eq!(walked.len(), 3, "precondition: A, C, E are walked");
        assert_eq!(gaps.len(), 1, "precondition: one gap, B and X");

        let recoveries_before = CONC_MARK_WALK_GAP_RECOVERIES.load(Ordering::Relaxed);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[a], &og);
        marker.concurrent_mark(&og);
        assert!(!marker.bitmap.is_marked(c as usize), "precondition: C is not reached");
        marker.remark(&stw(), &[a], &og);
        assert!(marker.bitmap.is_marked(c as usize), "the gap word naming C marked it");
        let swept = marker.concurrent_sweep(&mut og);
        FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(false));

        assert_eq!(swept, 1, "exactly the dead walked object E is reclaimed");
        assert!(!og.is_allocated_addr(e), "E is freed");
        for (name, p) in [("A", a), ("C", c), ("B", b), ("X", x)] {
            assert!(og.is_allocated_addr(p), "{name} must survive");
        }
        assert!(CONC_MARK_WALK_GAP_RECOVERIES.load(Ordering::Relaxed) > recoveries_before);
    }

    /// gen r4w6/oldpin6 — the plan's gates: on a healthy generation (walked
    /// bytes equal `used`, no gaps) a plan exists and names no gap; with the
    /// opt-in off there is no plan at all, so the remark refuses as before.
    #[test]
    fn the_walk_gap_plan_requires_the_gaps_to_account_for_used() {
        FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(true));
        let (og, _obj) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // A healthy generation: a plan exists (no gaps, walked == used).
        let plan = marker.remark_walk_gap_plan(&og).expect("walked == used");
        assert!(plan.gaps.is_empty());
        FORCE_CONC_WALK_GAP_RECOVERY.with(|c| c.set(false));
        // With the override off (and the flag unset in tests) there is none.
        if !crate::gc_flags().conc_walk_gap_recovery {
            assert!(marker.remark_walk_gap_plan(&og).is_none());
        }
    }

    /// A remark that refuses the sweep (the epoch moved) does no marking at
    /// all, and still leaves the post-conditions of a completed remark.
    #[test]
    fn a_refused_remark_skips_both_closures() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let root = og.alloc(size, 8).unwrap();
        init_old_object(root, 1);
        let logged = og.alloc(size, 8).unwrap();
        init_old_object(logged, 2);
        let victim = og.alloc(size, 8).unwrap();
        init_old_object(victim, 3);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);
        marker.satb_queue.flush(vec![logged as usize]);
        // SAFETY: exactly the pair `alloc` handed out.
        unsafe { og.free(victim, size) };
        assert!(marker.cycle_is_stale(&og));

        let discovered = marker.remark(&stw(), &[root], &og);
        assert_eq!(discovered, 0, "a refused remark must not trace anything");
        assert!(
            !marker.bitmap.is_marked(logged as usize),
            "the SATB entry is irrelevant to a cycle that will not sweep"
        );
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
        assert!(marker.satb_queue.is_empty());
        assert!(marker.queue.is_empty());
        assert_eq!(marker.concurrent_sweep(&mut og), 0);
    }

    /// Step 1 of
    /// `gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`:
    /// the callback sees the cycle's verdict and what it keeps is marked, with
    /// its closure, before the sweep.
    #[test]
    fn remark_reference_processing_keeps_what_the_callback_returns() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let root = og.alloc(size, 8).unwrap();
        init_old_object(root, 1);
        let referent = og.alloc(size, 8).unwrap();
        init_old_object(referent, 2);
        let child = og.alloc(size, 8).unwrap();
        init_old_object(child, 3);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 4);
        r4w2_link(referent, child);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);

        let mut calls = 0;
        let mut process = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
            calls += 1;
            assert!(is_marked(root as usize), "the root is marked");
            assert!(!is_marked(referent as usize), "the referent is not (yet)");
            assert!(!is_marked(dead as usize));
            // Outside the old generation: not this cycle's to judge.
            assert!(is_marked(0x10), "a non-eligible address must read as live");
            vec![referent as usize]
        };
        let process_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut process;
        marker.remark_with_reference_processing(&stw(), &[root], &og, Some(process_dyn));
        assert_eq!(calls, 1);
        assert!(marker.bitmap.is_marked(referent as usize));
        assert!(
            marker.bitmap.is_marked(child as usize),
            "the kept object's closure must be marked too"
        );

        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 1, "only the object nothing kept may be freed");
        assert!(og.is_allocated_addr(referent));
        assert!(og.is_allocated_addr(child));
        assert!(!og.is_allocated_addr(dead));
    }

    /// gc-common w36-d — the concurrent half of
    /// `common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928`: a
    /// published finalizer candidate that died old is RETAINED by the remark
    /// with its closure (the sweep frees neither) and recorded, once, for the
    /// next stop-the-world collection to report; a live candidate is not
    /// recorded; an object nobody published is still swept.
    #[test]
    fn w36d_remark_retains_dead_finalizer_candidates_for_the_next_pause() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let root = og.alloc(size, 8).unwrap();
        init_old_object(root, 1);
        let live_fin = og.alloc(size, 8).unwrap();
        init_old_object(live_fin, 2);
        let dead_fin = og.alloc(size, 8).unwrap();
        init_old_object(dead_fin, 3);
        let child = og.alloc(size, 8).unwrap();
        init_old_object(child, 4);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 5);
        r4w2_link(root, live_fin);
        r4w2_link(dead_fin, child);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // A duplicate must not matter.
        marker.state.publish_finalizer_candidates(vec![
            dead_fin as usize,
            live_fin as usize,
            dead_fin as usize,
        ]);
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[root], &og);
        assert!(marker.bitmap.is_marked(dead_fin as usize), "the dead finalizable is kept");
        assert!(marker.bitmap.is_marked(child as usize), "with its closure");
        assert!(!marker.bitmap.is_marked(dead as usize));
        assert_eq!(
            marker.state.take_remark_retained_finalizers(),
            vec![dead_fin as usize],
            "only the dead candidate is handed to the next pause"
        );
        assert!(marker.state.take_remark_retained_finalizers().is_empty(), "drained");

        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 1, "only the unpublished garbage is freed");
        assert!(og.is_allocated_addr(dead_fin));
        assert!(og.is_allocated_addr(child));
        assert!(og.is_allocated_addr(live_fin));
        assert!(!og.is_allocated_addr(dead));
    }

    /// gen r5w5/conc9 — a remark that runs reference processing records the
    /// finalizer retention's closure: the retained finalizable and what only
    /// it reaches, never an object the strong closure had already marked. The
    /// bitmap and the hand-off are the unrecorded retention's.
    #[test]
    fn r5w5_a_recording_retention_reports_only_what_it_marked() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let alloc = |og: &mut OldGen, cid: u32| {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, cid);
            p
        };
        let root = alloc(&mut og, 1);
        let live_fin = alloc(&mut og, 2);
        let shared = alloc(&mut og, 3);
        let dead_fin = alloc(&mut og, 4);
        let child = alloc(&mut og, 5);
        let dead = alloc(&mut og, 6);
        // root -> live_fin -> shared; dead_fin -> child -> shared.
        r4w2_link(root, live_fin);
        r4w2_link(live_fin, shared);
        r4w2_link(dead_fin, child);
        r4w2_link(child, shared);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.record_finalizer_retention_closure();
        marker
            .state
            .publish_finalizer_candidates(vec![dead_fin as usize, live_fin as usize]);
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[root], &og);

        let mut closure = marker.take_finalizer_retention_closure();
        closure.sort_unstable();
        let mut want = vec![dead_fin as usize, child as usize];
        want.sort_unstable();
        assert_eq!(closure, want, "the retained finalizable and its private closure only");
        assert!(marker.take_finalizer_retention_closure().is_empty(), "taken once");
        assert!(marker.bitmap.is_marked(dead_fin as usize));
        assert!(marker.bitmap.is_marked(child as usize));
        assert!(marker.bitmap.is_marked(shared as usize));
        assert!(!marker.bitmap.is_marked(dead as usize));
        assert_eq!(
            marker.state.take_remark_retained_finalizers(),
            vec![dead_fin as usize]
        );
        assert_eq!(marker.concurrent_sweep(&mut og), 1, "only the garbage is freed");
    }

    /// gen r5w5/conc9 — without the request the retention records nothing
    /// (the default remark, byte for byte).
    #[test]
    fn r5w5_an_unrecorded_retention_records_nothing() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let root = og.alloc(size, 8).unwrap();
        init_old_object(root, 1);
        let dead_fin = og.alloc(size, 8).unwrap();
        init_old_object(dead_fin, 2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker
            .state
            .publish_finalizer_candidates(vec![dead_fin as usize]);
        marker.initial_mark(&[root], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[root], &og);
        assert!(marker.bitmap.is_marked(dead_fin as usize));
        assert!(marker.take_finalizer_retention_closure().is_empty());
    }

    // -----------------------------------------------------------------
    // gen r4w3/oldgen3 (2026-09-23)
    // -----------------------------------------------------------------

    /// `n` one-slot objects; every third one (0, 3, 6, ...) is live through a
    /// chain from object 0, the rest are garbage.
    fn r4w3_layout(n: usize) -> (OldGen, Vec<*mut u8>) {
        let mut og = OldGen::new(1 << 20);
        let size = HEADER_SIZE + SLOT_SIZE;
        let ptrs: Vec<*mut u8> = (0..n)
            .map(|i| {
                let p = og.alloc(size, 8).unwrap();
                init_old_object(p, i as u32 + 1);
                p
            })
            .collect();
        let live: Vec<usize> = (0..n).step_by(3).collect();
        for w in live.windows(2) {
            r4w2_link(ptrs[w[0]], ptrs[w[1]]);
        }
        (og, ptrs)
    }

    /// Initial mark from object 0, trace, remark: the sweep is authorised.
    fn r4w3_marked(og: &OldGen, root: *mut u8) -> ConcurrentMarker {
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], og);
        marker.concurrent_mark(og);
        marker.remark(&stw(), &[root], og);
        marker
    }

    /// Sweep in slices of `budget` until done; returns (freed, slices).
    fn r4w3_sweep_in_slices(
        marker: &ConcurrentMarker,
        og: &mut OldGen,
        budget: usize,
    ) -> (usize, usize) {
        let (mut freed, mut slices) = (0usize, 0usize);
        loop {
            let (n, done) = marker.concurrent_sweep_budget(og, budget);
            freed += n;
            slices += 1;
            assert!(slices < 10_000, "a sliced sweep must make progress");
            if done {
                return (freed, slices);
            }
        }
    }

    /// `gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`:
    /// a sweep in slices of any size frees exactly the set a whole sweep
    /// frees, leaves the same generation behind, and tells the old gen's
    /// trigger about ONE collection either way.
    #[test]
    fn a_sweep_in_slices_frees_exactly_what_a_whole_sweep_frees() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let run = |budget: Option<usize>| {
            let (mut og, ptrs) = r4w3_layout(40);
            // A hole before the cycle, so the generation has two regions.
            // SAFETY: exactly the pair `alloc` handed out.
            unsafe { og.free(ptrs[1], size) };
            let marker = r4w3_marked(&og, ptrs[0]);
            let (freed, slices) = match budget {
                None => (marker.concurrent_sweep(&mut og), 1),
                Some(b) => r4w3_sweep_in_slices(&marker, &mut og, b),
            };
            assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
            let base = og.base_ptr() as usize;
            let walk: Vec<(usize, usize)> = og
                .walk_objects()
                .into_iter()
                .map(|(p, s)| (p as usize - base, s))
                .collect();
            (
                freed,
                slices,
                walk,
                og.trigger_stats(),
                marker.state.census(),
            )
        };
        let whole = run(None);
        // 40 objects: 14 live (0, 3, ..., 39), one freed before the cycle.
        assert_eq!(whole.0, 40 - 14 - 1);
        assert_eq!(whole.2.len(), 14);
        assert_eq!(whole.3.concurrent_collections, 1);
        assert_eq!(whole.3.freed_bytes, (25 * size) as u64);
        for budget in [1usize, 3, 7] {
            let sliced = run(Some(budget));
            assert_eq!(sliced.0, whole.0, "budget {budget}: freed");
            assert_eq!(sliced.2, whole.2, "budget {budget}: surviving layout");
            assert_eq!(sliced.3.freed_bytes, whole.3.freed_bytes, "budget {budget}");
            assert_eq!(sliced.3.concurrent_collections, 1, "budget {budget}");
            assert!(
                sliced.1 > 1,
                "budget {budget}: the sweep was actually sliced"
            );
            assert_eq!(sliced.4.sweep_slices as usize, sliced.1, "budget {budget}");
        }
    }

    /// TAMS across slices: an object allocated BETWEEN two sweep slices —
    /// above the resume point (into a hole that was free at remark) or below
    /// it (into a run the sweep itself just freed) — is never freed.
    #[test]
    fn an_object_allocated_between_sweep_slices_is_not_freed() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let (mut og, ptrs) = r4w3_layout(30);
        // A hole at the far end, free at remark.
        // SAFETY: exactly the pair `alloc` handed out.
        unsafe { og.free(ptrs[29], size) };
        let marker = r4w3_marked(&og, ptrs[0]);

        let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
        assert!(!done);
        assert_eq!(
            first, 2,
            "objects 1 and 2 were the garbage among the first four"
        );

        // A promotion lands in the far hole (the one exact fit for its size).
        let high = og.alloc(size, 8).unwrap();
        init_old_object(high, 99);
        assert_eq!(high, ptrs[29], "allocated above the resume point");
        // And a larger one into the run the first slice freed (objects 1-2).
        let low = og.alloc(2 * size, 8).unwrap();
        // SAFETY: a fresh `2 * size` block: one object with three slots.
        unsafe {
            let h = &mut *(low as *mut ObjectHeader);
            h.class_id = ClassId::new(98);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            h.set_num_slots(3);
            h.set_gc_flags(0x01);
        }
        assert_eq!(low, ptrs[1], "allocated below the resume point");

        let (rest, _) = r4w3_sweep_in_slices(&marker, &mut og, 4);
        // 30 objects: 10 live, one freed before the cycle, two in slice one.
        assert_eq!(first + rest, 30 - 10 - 1);
        assert!(
            og.is_allocated_addr(high),
            "never eligible: its block was free at remark"
        );
        assert!(
            og.is_allocated_addr(low),
            "below the resume point: never revisited"
        );
        let walk = og.walk_objects();
        assert!(walk.iter().any(|&(p, _)| p == high) && walk.iter().any(|&(p, _)| p == low));
        assert_eq!(
            walk.len(),
            10 + 2,
            "the live chain plus the two new objects"
        );
    }

    /// GCAUD-4 between slices: another collector freeing old-gen storage stops
    /// the sweep at the next slice — reclaiming nothing more — and a sweep
    /// that had already freed something is still reported as a collection.
    #[test]
    fn a_foreign_free_between_sweep_slices_stops_the_sweep() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let (mut og, ptrs) = r4w3_layout(30);
        let marker = r4w3_marked(&og, ptrs[0]);
        let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
        assert!(!done && first == 2);
        let stops = marker.state.census().sweep_epoch_stops;

        // Another collector (a young pause's in-place old sweep) frees garbage
        // above the resume point.
        // SAFETY: exactly the pair `alloc` handed out; object 20 is garbage.
        unsafe { og.free(ptrs[20], size) };
        assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
        assert_eq!(marker.state.census().sweep_epoch_stops, stops + 1);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
        assert!(
            og.is_allocated_addr(ptrs[26]),
            "garbage beyond the stop is left for the next cycle"
        );
        let s = og.trigger_stats();
        assert_eq!(s.concurrent_collections, 1);
        assert_eq!(s.freed_bytes, (2 * size) as u64);
        // Nothing left to resume.
        assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
    }

    /// gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-window`):
    /// `concurrent_sweep_budget_into` reports exactly the spans it freed —
    /// every freed object inside one, no survivor inside any, ascending and
    /// disjoint — and its results are those of the plain form.
    #[test]
    fn a_sweep_slice_reports_the_spans_it_freed() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let (mut og, ptrs) = r4w3_layout(30);
        let marker = r4w3_marked(&og, ptrs[0]);
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut freed = 0usize;
        let mut slices = 0usize;
        loop {
            let (n, done) = marker.concurrent_sweep_budget_into(&mut og, 4, Some(&mut spans));
            freed += n;
            slices += 1;
            assert!(slices < 10_000);
            if done {
                break;
            }
        }
        // 30 objects, 10 live (0, 3, ..., 27).
        assert_eq!(freed, 20);
        let in_spans = |a: usize| {
            spans
                .iter()
                .any(|&(start, len)| a >= start && a < start + len)
        };
        for (i, &p) in ptrs.iter().enumerate() {
            assert_eq!(
                in_spans(p as usize),
                i % 3 != 0,
                "object {i}: reported iff freed"
            );
        }
        let bytes: usize = spans.iter().map(|&(_, len)| len).sum();
        assert_eq!(bytes, 20 * size);
        for w in spans.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "ascending and disjoint");
        }
        // The plain form is the `None` case: same results on the same layout.
        let (mut og2, ptrs2) = r4w3_layout(30);
        let marker2 = r4w3_marked(&og2, ptrs2[0]);
        assert_eq!(r4w3_sweep_in_slices(&marker2, &mut og2, 4).0, freed);
    }

    /// Item 2 of `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`:
    /// an overflowed mark queue is repaired in Phase-2 slices, not only inside
    /// the remark pause. Staged by marking an object WITHOUT queueing it (what
    /// a dropped push leaves) and raising the flag.
    #[test]
    fn an_overflowed_mark_queue_is_rescanned_in_phase_two_slices() {
        let (og, ptrs) = r4w2_chain(4);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptrs[0]], &og);
        // 0 is marked; 1 is marked but its push was "dropped".
        marker.queue.clear();
        assert!(marker.bitmap.try_mark(ptrs[1] as usize));
        marker.queue.overflowed.store(true, Ordering::Relaxed);

        let mut slices = 0;
        loop {
            let (_, done) = marker.concurrent_mark_budget(&og, 1);
            slices += 1;
            assert!(slices < 100, "the rescan must make progress");
            if done {
                break;
            }
        }
        assert!(slices > 1, "the rescan ran in slices");
        for (i, &p) in ptrs.iter().enumerate() {
            assert!(
                marker.bitmap.is_marked(p as usize),
                "object {i} must be marked"
            );
        }
        assert!(
            !marker.queue.has_overflowed(),
            "a completed pass leaves the flag clear"
        );
        assert_eq!(marker.state.census().overflow_rescan_passes, 1);
        // So remark has no rescan left to do.
        marker.remark(&stw(), &[ptrs[0]], &og);
        assert!(marker.bitmap.is_marked(ptrs[3] as usize));
    }

    /// A rescan pass still part-way when remark runs (no production driver
    /// does that, but a caller may) must not be mistaken for a finished one:
    /// remark re-arms the overflow flag and rescans inside the pause.
    #[test]
    fn remark_finishes_an_overflow_rescan_left_part_way() {
        let (og, ptrs) = r4w2_chain(4);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptrs[0]], &og);
        marker.queue.clear();
        assert!(marker.bitmap.try_mark(ptrs[1] as usize));
        marker.queue.overflowed.store(true, Ordering::Relaxed);

        // One slice: the pass starts (clearing the flag) and stops after one
        // object, before it reaches object 1.
        let (_, done) = marker.concurrent_mark_budget(&og, 1);
        assert!(!done);
        assert!(!marker.queue.has_overflowed());
        assert!(
            !marker.bitmap.is_marked(ptrs[2] as usize),
            "precondition: not reached yet"
        );

        marker.remark(&stw(), &[ptrs[0]], &og);
        for (i, &p) in ptrs.iter().enumerate() {
            assert!(
                marker.bitmap.is_marked(p as usize),
                "object {i} must be marked by remark"
            );
        }
    }

    /// `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md`'s first
    /// step: the census of initial marks that froze a thread mid-JIT.
    #[test]
    fn the_initial_mark_takeover_census_counts_pauses_and_frozen_threads() {
        let state = ConcurrentGcState::new();
        state.note_initial_mark_takeover(0);
        state.note_initial_mark_takeover(3);
        state.note_initial_mark_takeover(1);
        let c = state.census();
        assert_eq!(
            (
                c.initial_marks,
                c.initial_marks_with_takeover,
                c.frozen_threads
            ),
            (3, 2, 4)
        );
    }

    /// `docs/internal/gc-common-round-20260923/common-g-gen-satb-queue-unbounded-during-concurrent-mark-FIXED-20260923.md`:
    /// Phase 2 drains the SATB log while it is active, so remark does not
    /// inherit every entry logged over the whole cycle; entries logged AFTER
    /// the drain are still remark's.
    #[test]
    fn phase_two_drains_the_satb_log_and_remark_takes_the_rest() {
        let (og, ptrs) = r4w3_layout(9);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptrs[0]], &og);
        // Two garbage objects "overwritten" during the trace.
        marker
            .satb_queue
            .flush(vec![ptrs[1] as usize, ptrs[2] as usize]);
        marker.concurrent_mark(&og);
        assert!(marker.bitmap.is_marked(ptrs[1] as usize));
        assert!(marker.bitmap.is_marked(ptrs[2] as usize));
        assert!(marker.satb_queue.is_empty());
        assert_eq!(marker.state.census().satb_drained_in_mark, 2);

        // One more after the trace: remark's.
        marker.satb_queue.flush(vec![ptrs[4] as usize]);
        let discovered = marker.remark(&stw(), &[ptrs[0]], &og);
        assert!(discovered >= 1);
        assert!(marker.bitmap.is_marked(ptrs[4] as usize));
        assert_eq!(marker.state.census().satb_drained_in_mark, 2);
    }

    // -----------------------------------------------------------------
    // gen r4w4/concmark4 (2026-09-24)
    // -----------------------------------------------------------------

    /// `from.slot0 = null` — the frozen thread's (or any) plain overwrite.
    fn r4w4_unlink(from: *mut u8) {
        // SAFETY: `from` is a live one-slot object allocated by the test.
        unsafe {
            let slot = from.add(HEADER_SIZE) as *mut Value;
            slot.write(Value::Object(None));
        }
    }

    /// The initiating occupancy: below the STW floor, `C/32` under it with no
    /// measured growth, lower by `5/4 · G` as the measured growth rises,
    /// never under 20 %, 45 % before the first cycle, and a fixed percentage
    /// clamped to `[20, 75]`.
    #[test]
    fn the_concurrent_start_threshold_sits_below_the_stw_floor_and_adapts() {
        let cap = 1_000_000usize;
        let floor = cap * 75 / 100;
        let min = cap * 20 / 100;
        let t = ConcurrentStartPolicy::threshold;
        assert_eq!(t(cap, None, None), cap * 45 / 100);
        assert_eq!(t(cap, Some(0), None), floor - cap / 32);
        assert_eq!(
            t(cap, Some(100_000), None),
            floor - (100_000 + 25_000 + cap / 32)
        );
        assert_eq!(t(cap, Some(u64::MAX), None), min);
        assert_eq!(t(cap, Some(0), Some(45)), cap * 45 / 100);
        assert_eq!(t(cap, None, Some(5)), min);
        assert_eq!(t(cap, None, Some(99)), floor);
        let mut prev = usize::MAX;
        for g in (0..=cap as u64).step_by(7_919) {
            let x = t(cap, Some(g), None);
            assert!(x <= floor && x >= min, "g={g}: {x}");
            assert!(x <= prev, "monotone in the predicted growth");
            prev = x;
        }
        assert_eq!(t(0, Some(5), None), 0);
    }

    /// A cycle is due at the threshold; over a live set a previous
    /// collection could not bring under it, only after growth of
    /// `max(C/32, (F − after)/2)`.
    #[test]
    fn the_concurrent_start_waits_for_growth_over_a_live_set_above_the_threshold() {
        let cap = 1_000_000usize;
        let t = 450_000usize;
        let due = ConcurrentStartPolicy::start_due;
        assert!(!due(cap, 449_999, None, t));
        assert!(due(cap, 450_000, None, t));
        assert!(
            due(cap, 450_000, Some(300_000), t),
            "left below T: due at T"
        );
        // Left at 600 k: max(31 250, (750 k − 600 k) / 2 = 75 k).
        assert!(!due(cap, 674_999, Some(600_000), t));
        assert!(due(cap, 675_000, Some(600_000), t));
        // Left above the STW floor: C/32 — before the STW arm's C/16 step.
        assert!(!due(cap, 831_249, Some(800_000), t));
        assert!(due(cap, 831_250, Some(800_000), t));
        assert!(!due(0, 0, None, 0), "no generation");
        assert!(!due(cap, 0, None, 0), "an empty generation");
    }

    /// Only an occupancy verdict defers, only below the ceiling, and never
    /// with an allocation failure outstanding — even one the verdict itself
    /// reports as `Occupancy`.
    #[test]
    fn the_stw_collection_defers_only_to_occupancy_below_the_ceiling_without_failures() {
        let d = ConcurrentStartPolicy::stw_defers;
        let cap = 1000usize;
        assert!(d(cap, 800, MajorTrigger::Occupancy, 0));
        assert!(d(cap, 800, MajorTrigger::WouldSuppress, 0));
        assert!(!d(cap, 900, MajorTrigger::Occupancy, 0), "the 90 % ceiling");
        assert!(
            !d(cap, 800, MajorTrigger::Occupancy, 1),
            "a failure never waits"
        );
        assert!(!d(cap, 800, MajorTrigger::AllocationFailure, 1));
        assert!(!d(cap, 800, MajorTrigger::Requested, 0));
        assert!(
            !d(cap, 800, MajorTrigger::Suppressed, 0),
            "it would not run anyway"
        );
    }

    /// gcd d9e — `CRATONVM_GEN_CONC_PRECEDENCE`'s deferral: the ceiling is
    /// gone, nothing else changes (a failure, a request or a non-running
    /// verdict never defers); `false` is exactly `stw_defers`. Every running
    /// verdict that does not defer has one cause.
    #[test]
    fn gcd_d9e_the_ceiling_is_optional_and_every_non_deferral_has_one_cause() {
        let d = ConcurrentStartPolicy::stw_defers_with;
        let cap = 1000usize;
        for used in [0, 800, 899, 900, 950, 1000] {
            for verdict in [
                MajorTrigger::Occupancy,
                MajorTrigger::WouldSuppress,
                MajorTrigger::AllocationFailure,
                MajorTrigger::Requested,
                MajorTrigger::Suppressed,
                MajorTrigger::NotDue,
            ] {
                for failures in [0u64, 1] {
                    assert_eq!(
                        d(cap, used, verdict, failures, false),
                        ConcurrentStartPolicy::stw_defers(cap, used, verdict, failures),
                        "used={used} {verdict:?} failures={failures}"
                    );
                }
            }
        }
        assert!(d(cap, 900, MajorTrigger::Occupancy, 0, true), "no ceiling");
        assert!(d(cap, 1000, MajorTrigger::WouldSuppress, 0, true));
        assert!(
            !d(cap, 950, MajorTrigger::Occupancy, 1, true),
            "a failure still runs"
        );
        assert!(!d(cap, 800, MajorTrigger::AllocationFailure, 1, true));
        assert!(!d(cap, 800, MajorTrigger::Requested, 0, true));
        assert_eq!(
            ConcurrentStartPolicy::preempt_cause(0),
            StwPreemptCause::Ceiling
        );
        assert_eq!(
            ConcurrentStartPolicy::preempt_cause(3),
            StwPreemptCause::AllocationFailure
        );
    }

    /// gcd d9e — with a measured prediction, a generation left at or above
    /// the threshold re-opens after `C/32` of growth, not half the room to the
    /// floor; below the threshold, and with no previous collection, the two
    /// rules agree.
    #[test]
    fn gcd_d9e_a_measured_start_is_not_held_past_the_threshold_by_the_hysteresis() {
        let cap = 1_000_000usize;
        let t = 450_000usize;
        let old = ConcurrentStartPolicy::start_due;
        let new = ConcurrentStartPolicy::start_due_measured;
        // Left at 600 k: the old rule waits for (750 k - 600 k) / 2 = 75 k,
        // the new one for C/32 = 31 250.
        assert!(!old(cap, 640_000, Some(600_000), t));
        assert!(!new(cap, 631_249, Some(600_000), t));
        assert!(new(cap, 631_250, Some(600_000), t));
        assert!(old(cap, 675_000, Some(600_000), t));
        // Where the two agree.
        for (used, after) in [
            (449_999, None),
            (450_000, None),
            (450_000, Some(300_000)),
            (831_249, Some(800_000)),
            (831_250, Some(800_000)),
            (0, None),
        ] {
            assert_eq!(
                old(cap, used, after, t),
                new(cap, used, after, t),
                "used={used} after={after:?}"
            );
        }
        assert!(!new(0, 0, None, 0), "no generation");
    }

    /// gcd d9e — `ConcurrentGcState::concurrent_start_due` takes the measured
    /// rule only under `CRATONVM_GEN_CONC_PRECEDENCE` AND once a cycle has
    /// been measured; before the first measurement, and with the flag off
    /// (`=0`; default on since gce, 2026-09-29), it is the gen r4w4 rule.
    #[test]
    fn gcd_d9e_the_precedence_start_applies_only_to_a_measured_prediction() {
        let cap = 1_000_000usize;
        let on = [("CRATONVM_GEN_CONC_PRECEDENCE", Some("1"))];
        let off = [("CRATONVM_GEN_CONC_PRECEDENCE", Some("0"))];
        let state = ConcurrentGcState::new();
        // Unmeasured: T = 45 %; left at 600 k, 640 k is not due either way.
        let due = |s: &ConcurrentGcState| s.concurrent_start_due(cap, 640_000, Some(600_000));
        let due_without_flag =
            |s: &ConcurrentGcState| cratonvm_types::flags::with_thread_overrides(&off, || due(s));
        let due_with_flag =
            |s: &ConcurrentGcState| cratonvm_types::flags::with_thread_overrides(&on, || due(s));
        assert!(!due_without_flag(&state));
        assert!(!due_with_flag(&state));
        // One measured cycle that grew 100 k: T = 750 k - (125 k + 31 250)
        // = 593 750 <= 600 k, so the generation was left at or above T.
        state.note_cycle_open(0, 0);
        state.note_cycle_end(100_000, 600_000, cap, 0);
        assert_eq!(state.concurrent_start_threshold(cap), 593_750);
        assert!(!due_without_flag(&state), "flag off: waits for (750 k - 600 k) / 2");
        assert!(due_with_flag(&state), "precedence: 40 k >= C/32");
    }

    /// gcd d9e — `[GC] conc_cycles:` says how every claimed cycle ended: a
    /// claim whose initial-mark pause was lost, an opened cycle abandoned in
    /// Phase 2, and a completed one whose marking and sweep were timed.
    /// (`conccyc_started` counts the DRIVER's initial marks,
    /// `note_initial_mark_takeover`, which a bare marker never calls.)
    ///
    /// Run with both d9/e switches at `=0` (default on since gce,
    /// 2026-09-29): the line's tail states them, and this pins the off arm.
    #[test]
    fn gcd_d9e_the_cycle_census_says_how_every_cycle_ended() {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_GEN_CONC_PRECEDENCE", Some("0")),
                ("CRATONVM_GEN_CONC_MARK_TAMS_STARTS", Some("0")),
            ],
            gcd_d9e_the_cycle_census_says_how_every_cycle_ended_body,
        );
    }

    fn gcd_d9e_the_cycle_census_says_how_every_cycle_ended_body() {
        let (mut og, ptrs) = r4w3_layout(6);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        drop(marker.try_open_cycle().expect("no cycle is open"));
        {
            let cycle = marker.try_open_cycle().expect("no cycle is open");
            marker.initial_mark(&[ptrs[0]], &og);
            drop(cycle);
        }
        {
            let cycle = marker.try_open_cycle().expect("no cycle is open");
            marker.initial_mark(&[ptrs[0]], &og);
            marker.concurrent_mark(&og);
            marker.remark(&stw(), &[ptrs[0]], &og);
            let freed = marker.concurrent_sweep(&mut og);
            assert_eq!(freed, 4, "6 objects, 0 and 3 live");
            cycle.complete();
        }
        let c = marker.state.census();
        assert_eq!((c.open_lost, c.abandoned), (1, 1));
        assert_eq!((c.swept_nothing, c.cycles_completed), (0, 1));
        let line = marker.state.cycle_census_line();
        let head = concat!(
            "[GC] conc_cycles: conccyc_started=0 conccyc_completed=1 ",
            "conccyc_open_lost=1 conccyc_abandoned=1 conccyc_swept_nothing=0 ",
            "conccyc_preempted=0 ",
        );
        assert!(line.starts_with(head), "{line}");
        assert!(line.contains(" conccyc_mark_ms_total="), "{line}");
        assert!(
            line.ends_with(" conccyc_precedence=false conccyc_tams_starts=false"),
            "{line}"
        );
        assert!(
            marker.state.driver_census_line().ends_with(line.as_str()),
            "the driver's summary ends with it"
        );
    }

    /// gcd d9e — Phase-2 object-start walks: by default a promotion between
    /// the initial mark and a slice costs that slice one whole-generation walk
    /// (counted); under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` the slices judge
    /// against the initial mark's snapshot and walk nothing. Either way the
    /// snapshot's live chain is marked, and the post-snapshot object — never
    /// marked, never eligible — survives the sweep.
    #[test]
    fn gcd_d9e_phase2_walks_are_counted_and_the_tams_snapshot_skips_them() {
        let size = HEADER_SIZE + SLOT_SIZE;
        for tams in [false, true] {
            let (mut og, ptrs) = r4w3_layout(9);
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            let flag = [(
                "CRATONVM_GEN_CONC_MARK_TAMS_STARTS",
                // Default on since gce (2026-09-29): the walking arm is `=0`.
                if tams { Some("1") } else { Some("0") },
            )];
            // The flag is decided at the initial mark, once per cycle.
            cratonvm_types::flags::with_thread_overrides(&flag, || {
                marker.initial_mark(&[ptrs[0]], &og);
            });
            // A "promotion" after the initial mark: `free_list_seq` moves.
            let promoted = og.alloc(size, 8).unwrap();
            init_old_object(promoted, 500);
            let mut slices = 0;
            while !marker.concurrent_mark_budget(&og, 1).1 {
                slices += 1;
                assert!(slices < 1000, "the slices make progress");
            }
            let c = marker.state.census();
            if tams {
                assert_eq!(c.phase2_start_walks, 0, "the snapshot needs no walk");
                assert!(c.phase2_tams_slices >= 1);
            } else {
                assert_eq!(c.phase2_start_walks, 1, "the promotion forced one walk");
                assert_eq!(c.phase2_tams_slices, 0);
            }
            marker.remark(&stw(), &[ptrs[0]], &og);
            for (i, &p) in ptrs.iter().enumerate() {
                assert_eq!(
                    marker.bitmap.is_marked(p as usize),
                    i % 3 == 0,
                    "object {i} (tams={tams})"
                );
            }
            assert_eq!(marker.concurrent_sweep(&mut og), 6, "tams={tams}");
            assert!(
                og.walk_objects().iter().any(|&(p, _)| p == promoted),
                "the post-snapshot object survives (tams={tams})"
            );
        }
    }

    /// The hysteresis default is one match arm per policy (OFF under `Legacy`,
    /// ON under `ConcurrentFirst` since gen r4w5), an explicit setting wins,
    /// and the opt-out selects `Legacy`.
    #[test]
    fn the_old_gen_policy_defaults_are_stated_once() {
        assert!(!OldGenPolicy::Legacy.hysteresis(None));
        assert!(OldGenPolicy::ConcurrentFirst.hysteresis(None));
        assert!(OldGenPolicy::ConcurrentFirst.hysteresis(Some(true)));
        assert!(OldGenPolicy::Legacy.hysteresis(Some(true)));
        assert!(!OldGenPolicy::ConcurrentFirst.hysteresis(Some(false)));
        assert_eq!(OldGenPolicy::from_opt_out(true), OldGenPolicy::Legacy);
        assert_eq!(
            OldGenPolicy::from_opt_out(false),
            OldGenPolicy::ConcurrentFirst
        );
    }

    /// A cycle measures the old-gen growth between its initial mark and its
    /// sweep's end (a promotion landing mid-cycle here), feeds it to the start
    /// policy and reports itself; the next threshold is the formula's.
    #[test]
    fn a_completed_cycle_feeds_the_start_policy_its_growth_and_a_report() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let (mut og, ptrs) = r4w3_layout(30);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let used_at_open = og.used();
        marker.initial_mark(&[ptrs[0]], &og);
        assert!(marker.state.cycle_in_progress());
        // Four "promotions" during the trace.
        let before = og.used();
        for i in 0..4u32 {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, 100 + i);
        }
        let grown = (og.used() - before) as u64;
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[ptrs[0]], &og);
        let freed = marker.concurrent_sweep(&mut og);
        assert_eq!(freed, 20, "30 objects, every third live");
        assert!(!marker.state.cycle_in_progress());

        let r = marker
            .state
            .last_cycle_report()
            .expect("a completed cycle reports");
        assert_eq!(r.cycle, 1);
        assert_eq!(r.used_at_open, used_at_open);
        assert_eq!(r.used_at_end, og.used());
        assert_eq!(r.freed_bytes, 20 * size);
        assert_eq!(r.growth_during, grown);
        assert_eq!(marker.state.census().cycles_completed, 1);
        assert_eq!(marker.state.policy_estimates().0, Some(grown));
        assert_eq!(
            r.next_threshold,
            ConcurrentStartPolicy::threshold(og.capacity(), Some(grown), None)
        );
        assert!(r.xlog_line().starts_with("GC(c1) Concurrent Mark Cycle "));
        assert!(r.verbose_line().contains("concyc_growth_during="));
    }

    /// A cycle that is abandoned feeds nothing; one that is pre-empted by an
    /// STW collection widens the prediction ONCE, and its own end does not
    /// average the widening away; the next cycle averages normally.
    #[test]
    fn a_preempted_cycle_widens_the_prediction_once_and_an_abandoned_one_feeds_nothing() {
        let (og, ptrs) = r4w3_layout(6);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptrs[0]], &og);
        marker.abort_cycle();
        assert_eq!(marker.state.policy_estimates().0, None);
        assert_eq!(marker.state.last_cycle_report(), None);

        let state = ConcurrentGcState::new();
        state.note_cycle_open(1_000, 500);
        // 10 k so far: max(2 · 10 k, C/8 = 125 k).
        state.note_stw_preempted(11_000, 1_000_000);
        assert_eq!(state.policy_estimates().0, Some(125_000));
        state.note_stw_preempted(901_000, 1_000_000);
        assert_eq!(state.policy_estimates().0, Some(125_000), "once per cycle");
        assert_eq!(state.census().stw_preempted, 2);
        state.note_cycle_end(21_000, 600, 1_000_000, 0);
        assert_eq!(state.policy_estimates().0, Some(125_000));
        assert_eq!(
            state.last_cycle_report().map(|r| r.growth_during),
            Some(20_000)
        );
        state.note_cycle_open(0, 0);
        state.note_cycle_end(25_000, 0, 1_000_000, 0);
        assert_eq!(
            state.policy_estimates().0,
            Some(75_000),
            "(125 k + 25 k) / 2"
        );
        assert_eq!(state.census().cycles_completed, 2);
    }

    /// `docs/internal/gc/gengc-r4w2-concmark-jit-gate-takeover-window-FIXED-20260928.md`, staged.
    ///
    /// `O.f == A` at the snapshot; `O` is held only by a thread the takeover
    /// FROZE between its gate test and `O.f = null`. After the pause: the
    /// trace blackens `P`; the frozen store lands UNLOGGED; a second mutator
    /// stores `A` into black `P` (logging `P`'s old value, null). `A` is
    /// reachable and was reachable at the snapshot.
    ///
    /// Plain `initial_mark` (the hazard): `A` is never marked and is swept.
    /// `initial_mark_with_frozen`: `O` is scanned inside the pause, `A` is
    /// grey before the unlogged store, and survives.
    #[test]
    fn a_frozen_threads_unlogged_store_cannot_hide_a_snapshot_value() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let run = |eager: bool| -> bool {
            let mut og = OldGen::new(1 << 20);
            let mut obj = || {
                let p = og.alloc(size, 8).unwrap();
                init_old_object(p, 7);
                p
            };
            let (o, a, p) = (obj(), obj(), obj());
            r4w2_link(o, a);
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            // The pause: `P` is an ordinary root; `O` is in the frozen
            // thread's registers (the plain arm models "O is scanned only
            // after the frozen store", which is what the race produces).
            if eager {
                marker.initial_mark_with_frozen(&[p], &[o], &og);
            } else {
                marker.initial_mark(&[p], &og);
            }
            marker.concurrent_mark(&og); // `P` is black, `P.f == null`.
            r4w4_unlink(o); // the frozen store, unlogged
            r4w2_link(p, a); // into black `P`; its old value (null) logs nothing
            marker.remark(&stw(), &[p], &og);
            let a_marked = marker.bitmap.is_marked(a as usize);
            marker.concurrent_sweep(&mut og);
            a_marked
        };
        assert!(
            !run(false),
            "the staged race must reproduce the hazard without the fix"
        );
        assert!(run(true), "the eager scan keeps the snapshot value");
    }

    /// The eager scan is counted, skips non-object addresses, and is switched
    /// off by its kill switch.
    #[test]
    fn the_frozen_eager_scan_counts_old_objects_only_and_has_a_kill_switch() {
        let (og, ptrs) = r4w2_chain(4);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let bogus = 0x10usize as *mut u8;
        marker.initial_mark_with_frozen(&[], &[ptrs[1], bogus, std::ptr::null_mut()], &og);
        assert_eq!(marker.state.census().frozen_objects_scanned, 1);
        assert!(marker.bitmap.is_marked(ptrs[1] as usize));
        assert!(
            marker.bitmap.is_marked(ptrs[2] as usize),
            "its child is grey"
        );
        marker.abort_cycle();

        let off = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN", Some("1"))],
            || off.initial_mark_with_frozen(&[], &[ptrs[1]], &og),
        );
        assert_eq!(off.state.census().frozen_objects_scanned, 0);
        assert!(!off.bitmap.is_marked(ptrs[2] as usize));
        off.abort_cycle();
    }

    /// The marker-local stack reaches the same fixed point as the queue-only
    /// drain, whole and in one-object slices (which spill every leftover back
    /// to the queue between slices).
    #[test]
    fn the_local_mark_stack_marks_exactly_what_the_queue_marks() {
        // 60 one-slot objects, every third live on a chain from object 0.
        let marked_set = |no_local: bool, budget: usize| -> (Vec<bool>, u64) {
            let (og, ptrs) = r4w3_layout(60);
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            let edits: &[(&str, Option<&str>)] = if no_local {
                &[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", Some("1"))]
            } else {
                &[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", None)]
            };
            cratonvm_types::flags::with_thread_overrides(edits, || {
                marker.initial_mark(&[ptrs[0]], &og);
                let mut slices = 0;
                while !marker.concurrent_mark_budget(&og, budget).1 {
                    slices += 1;
                    assert!(slices < 10_000);
                }
                marker.remark(&stw(), &[ptrs[0]], &og);
            });
            let set = ptrs
                .iter()
                .map(|&p| marker.bitmap.is_marked(p as usize))
                .collect();
            (set, marker.state.census().mark_local_spills)
        };
        let (queue_only, _) = marked_set(true, usize::MAX);
        assert_eq!(queue_only.iter().filter(|&&m| m).count(), 20);
        let (local_whole, _) = marked_set(false, usize::MAX);
        assert_eq!(local_whole, queue_only);
        let (local_sliced, spills) = marked_set(false, 1);
        assert_eq!(local_sliced, queue_only);
        assert!(spills > 0, "one-object slices spill the next grey object");
    }

    // -----------------------------------------------------------------
    // gen r4w5/concmark5 (2026-09-24)
    // -----------------------------------------------------------------

    /// The periodic re-check backs off exponentially after cycles that opened
    /// and failed, and the back-off is capped.
    #[test]
    fn the_service_back_off_is_exponential_and_capped() {
        assert_eq!(service_backoff_skips(0), 0);
        assert_eq!(service_backoff_skips(1), 1);
        assert_eq!(service_backoff_skips(2), 3);
        assert_eq!(service_backoff_skips(6), 63);
        assert_eq!(service_backoff_skips(7), 63);
        assert_eq!(service_backoff_skips(u32::MAX), 63);
    }

    /// gce e1/c — the inline start request (no service): armed only while
    /// idle, unlatched and above the hint; latched once per crossing; taken
    /// exactly once; never armed with a service attached.
    #[test]
    fn gce_e1c_inline_start_request_latches_once_and_is_taken_once() {
        let state = ConcurrentGcState::new();
        // The hint starts at 0: the first poll is a full check.
        assert!(state.inline_growth_poll_armed(0));
        assert!(!state.inline_start_requested());
        assert!(!state.take_inline_start_request(), "nothing latched yet");
        assert!(state.latch_inline_start_request(), "the first latch sets it");
        assert!(!state.latch_inline_start_request(), "a second is a no-op");
        assert!(state.inline_start_requested());
        assert!(
            !state.inline_growth_poll_armed(usize::MAX),
            "a latched request disarms the poll (no second full check)",
        );
        assert!(state.take_inline_start_request());
        assert!(!state.take_inline_start_request(), "taken exactly once");
        // gce e2/c: counted once each, and printed on `[GC] conc_cycles:`.
        let c = state.census();
        assert_eq!((c.inline_start_latched, c.inline_start_taken), (1, 1));
        assert!(state
            .cycle_census_line()
            .contains(" conccyc_inline_start_latched=1 conccyc_inline_start_taken=1 "));
        assert!(state.inline_growth_poll_armed(usize::MAX));
        // Below the published hint: not armed.
        state.service.start_hint.store(1 << 20, Ordering::Relaxed);
        assert!(!state.inline_growth_poll_armed((1 << 20) - 1));
        assert!(state.inline_growth_poll_armed(1 << 20));
        // A cycle open: not armed.
        assert!(state.try_open_cycle());
        assert!(!state.inline_growth_poll_armed(usize::MAX));
        state.set_phase(ConcurrentGcPhase::Idle);
        // A service attached: the service arm owns the poll.
        let _attached = state.attach_service_thread().expect("no service yet");
        assert!(!state.inline_growth_poll_armed(usize::MAX));
    }

    /// With no service attached every caller runs the cycle itself (the
    /// pre-2026-09-24 behaviour); with one attached, a due cycle is handed to
    /// it from any OTHER thread and never from the service itself; the
    /// service is one per VM, and dropping its attachment detaches it.
    #[test]
    fn a_due_cycle_is_handed_to_an_attached_service_and_never_to_itself() {
        let state = ConcurrentGcState::new();
        assert!(!state.hand_off_to_service(), "no service: run inline");
        assert!(!state.on_service_thread());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let st = &state;
        std::thread::scope(|s| {
            let service = s.spawn(move || {
                let _attached = st.attach_service_thread().expect("the first attach");
                assert!(st.on_service_thread());
                assert!(!st.hand_off_to_service(), "the service runs its own cycles");
                ready_tx.send(()).expect("main is waiting");
                let wake = st.service_wait(std::time::Duration::from_secs(30));
                go_rx.recv().expect("main releases the service");
                wake
            });
            ready_rx.recv().expect("the service attached");
            assert!(st.service_attached());
            assert!(!st.on_service_thread());
            assert!(st.attach_service_thread().is_none(), "one service per VM");
            assert!(st.hand_off_to_service(), "a mutator hands the cycle off");
            go_tx.send(()).expect("the service is waiting");
            let wake = service.join().expect("the service thread");
            assert_eq!(wake, ConcurrentServiceWake::HandedOff);
        });
        assert!(
            !state.service_attached(),
            "the attachment's drop detached it"
        );
        assert!(!state.hand_off_to_service(), "detached: inline again");
        let c = state.census();
        assert_eq!(c.service_handoffs, 1);
        assert_eq!(c.service_wakes_requested, 1);
    }

    /// gen r5w5/conc9 — a service told to stop is still attached until its
    /// loop returns, and its next wait answers `Shutdown` whatever is pending;
    /// a hand-off in that window used to be accepted and then dropped by the
    /// detach. It is refused now, so the caller runs the cycle itself.
    #[test]
    fn r5w5_a_stopping_service_refuses_hand_offs() {
        let state = ConcurrentGcState::new();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let st = &state;
        std::thread::scope(|s| {
            let service = s.spawn(move || {
                let _attached = st.attach_service_thread().expect("the first attach");
                ready_tx.send(()).expect("main is waiting");
                go_rx.recv().expect("main releases the service");
                st.service_wait(std::time::Duration::from_secs(30))
            });
            ready_rx.recv().expect("the service attached");
            state.shutdown_service();
            assert!(state.service_attached(), "still attached until its loop returns");
            assert!(
                !state.hand_off_to_service(),
                "a stopping service accepts no hand-off"
            );
            go_tx.send(()).expect("the service is waiting");
            assert_eq!(
                service.join().expect("the service thread"),
                ConcurrentServiceWake::Shutdown
            );
        });
        assert_eq!(state.census().service_handoffs, 0);
    }

    /// The direct-old-allocation poll's pre-check, and the growth signal: a
    /// request posted while the service is busy is returned by its next wait
    /// at once, and shutdown is permanent.
    #[test]
    fn a_growth_signal_wakes_the_service_and_the_pre_check_gates_it() {
        let state = ConcurrentGcState::new();
        assert!(!state.service_growth_poll_armed(usize::MAX), "no service");
        assert!(!state.signal_old_gen_growth(), "no service to wake");
        let attached = state.attach_service_thread().expect("attach");
        assert!(
            state.service_growth_poll_armed(0),
            "an unknown threshold arms it"
        );
        let capacity = 1_000_000;
        let t = state.concurrent_start_threshold(capacity);
        assert_eq!(t, capacity * CONC_START_INITIAL_PCT / 100);
        assert!(
            !state.service_growth_poll_armed(t - 1),
            "below the published start"
        );
        assert!(state.service_growth_poll_armed(t));
        state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        assert!(!state.service_growth_poll_armed(t), "a cycle is open");
        state.set_phase(ConcurrentGcPhase::Idle);

        assert!(state.signal_old_gen_growth());
        assert!(!state.service_growth_poll_armed(t), "a request is pending");
        assert_eq!(
            state.service_wait(std::time::Duration::from_secs(30)),
            ConcurrentServiceWake::OldGenGrowth
        );
        assert!(state.service_growth_poll_armed(t), "consumed");
        assert_eq!(
            state.service_wait(std::time::Duration::from_millis(1)),
            ConcurrentServiceWake::Periodic
        );
        state.shutdown_service();
        assert_eq!(
            state.service_wait(std::time::Duration::from_secs(30)),
            ConcurrentServiceWake::Shutdown
        );
        drop(attached);
        assert!(
            state.attach_service_thread().is_none(),
            "shutdown is permanent"
        );
        let c = state.census();
        assert_eq!(c.service_growth_signals, 1);
        assert_eq!(c.service_wakes_requested, 1);
        assert_eq!(c.service_wakes_periodic, 1);
    }

    /// Fake VM hooks for `serve`: the cycle's outcome is scripted through
    /// the census the real driver moves (`initial_marks`, `cycles_completed`).
    struct ScriptedService<'s> {
        state: &'s ConcurrentGcState,
        /// One entry per `run_cycle`: does the cycle open, does it complete.
        script: Vec<(bool, bool)>,
        log: Vec<&'static str>,
        idle: bool,
    }

    impl ConcurrentServiceHooks for ScriptedService<'_> {
        fn enter_idle(&mut self) {
            assert!(!self.idle, "enter_idle twice");
            self.idle = true;
            self.log.push("idle");
        }
        fn leave_idle(&mut self) {
            assert!(self.idle, "leave_idle while running");
            self.idle = false;
            self.log.push("run");
        }
        fn cycle_due(&mut self) -> bool {
            assert!(self.idle, "the periodic check is asked while idle");
            self.log.push("due?");
            true
        }
        fn run_cycle(&mut self) {
            assert!(!self.idle, "a cycle runs as a counted mutator");
            let (opens, completes) = self.script.remove(0);
            if opens {
                self.state.note_initial_mark_takeover(0);
                self.state.note_cycle_open(0, 0);
            }
            if completes {
                self.state.note_cycle_end(10, 0, 1_000_000, 0);
            } else if opens {
                self.state.note_cycle_dropped();
            }
            if self.script.is_empty() {
                self.state.shutdown_service();
            }
        }
    }

    /// The service loop: periodic wakes ask the trigger while idle, a cycle
    /// runs as a counted mutator, a cycle that opened and failed backs the
    /// periodic check off, a completed one resets it, and shutdown returns
    /// with the thread idle and the service detached.
    #[test]
    fn the_service_loop_runs_backs_off_and_shuts_down() {
        let state = ConcurrentGcState::new();
        let mut hooks = ScriptedService {
            state: &state,
            // A failed cycle (opens, never completes), then a completed one.
            script: vec![(true, false), (true, true)],
            log: Vec::new(),
            idle: false,
        };
        assert!(state.serve(std::time::Duration::from_millis(1), &mut hooks));
        assert!(hooks.idle, "serve returns with the thread idle");
        assert_eq!(
            hooks.log,
            ["idle", "due?", "run", "idle", "due?", "run", "idle"],
            "one periodic wake is skipped between the two cycles"
        );
        let c = state.census();
        assert_eq!(c.service_cycle_attempts, 2);
        assert_eq!(c.service_cycles_completed, 1);
        assert_eq!(c.service_periodic_due, 2);
        assert_eq!(c.service_backoff_skips, 1);
        assert!(!state.service_attached());

        // A second `serve` on a VM whose service already runs is refused.
        let other = ConcurrentGcState::new();
        let _held = other.attach_service_thread().expect("attach");
        let mut idle_hooks = ScriptedService {
            state: &other,
            script: Vec::new(),
            log: Vec::new(),
            idle: false,
        };
        assert!(!other.serve(std::time::Duration::from_millis(1), &mut idle_hooks));
        assert!(idle_hooks.log.is_empty());
    }

    /// Fake hooks whose periodic trigger check says "due" and, in the same
    /// breath, stops the service: the stop lands between the wake and the
    /// cycle (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`).
    struct StopWhileDue<'s> {
        state: &'s ConcurrentGcState,
        log: Vec<&'static str>,
    }

    impl ConcurrentServiceHooks for StopWhileDue<'_> {
        fn enter_idle(&mut self) {
            self.log.push("idle");
        }
        fn leave_idle(&mut self) {
            self.log.push("run");
        }
        fn cycle_due(&mut self) -> bool {
            self.log.push("due?");
            self.state.shutdown_service();
            true
        }
        fn run_cycle(&mut self) {
            self.log.push("cycle");
        }
    }

    /// gcd d1/c: a service told to stop after its wake does not leave idle and
    /// starts no cycle; it returns idle and detached.
    #[test]
    fn gcd_d1c_a_stop_after_the_wake_starts_no_cycle() {
        let state = ConcurrentGcState::new();
        assert!(!state.service_shutting_down());
        let mut hooks = StopWhileDue {
            state: &state,
            log: Vec::new(),
        };
        assert!(state.serve(std::time::Duration::from_millis(1), &mut hooks));
        assert_eq!(hooks.log, ["idle", "due?"]);
        assert!(state.service_shutting_down());
        assert!(!state.service_attached());
        assert_eq!(state.census().service_cycle_attempts, 0);
    }

    /// The count-based cadence census: each decision resolves the previous
    /// one against the generation's STW collection count and freed bytes.
    #[test]
    fn the_major_cadence_census_counts_runs_back_to_back_and_low_yield() {
        let cap = 1_000;
        let mut m = MajorCadence::default();
        // #1 decides to run; its major frees 10 bytes (1 %).
        m.note_decision(0, 0, cap, false, true);
        // #2 resolves #1 and runs again; its major frees 200 (20 %).
        m.note_decision(1, 10, cap, false, true);
        assert_eq!(m.census.majors, 1);
        assert_eq!(
            m.census.back_to_back, 0,
            "the first major has no predecessor"
        );
        assert_eq!(m.census.low_yield_5pct, 1);
        // #3 resolves #2 (back to back, not low-yield) and does not run.
        m.note_decision(2, 210, cap, false, false);
        assert_eq!(m.census.majors, 2);
        assert_eq!(m.census.back_to_back, 1);
        assert_eq!(m.census.low_yield_5pct, 1);
        // #4 is requested; #3 ran nothing, so #4 cannot be back to back.
        m.note_decision(2, 210, cap, true, true);
        // Resolved against a count that moved: exactly 5 % is not low-yield.
        let c = m.resolved(3, 260).0;
        assert_eq!(c.young_decisions, 4);
        assert_eq!(c.majors, 3);
        assert_eq!(c.requested_majors, 1);
        assert_eq!(c.back_to_back, 1);
        assert_eq!(c.low_yield_5pct, 1);
        // A decision whose major never ran (the count did not move) is not one.
        assert_eq!(m.resolved(2, 210).0.majors, 2);
        assert_eq!(c.majors_per_100_young_tenths(), 750);
        let line = c.line();
        assert!(line.starts_with("[GC] major_cadence: majcad_young_decisions=4 majcad_majors=3 "));
        assert!(line.contains(" majcad_per_100_young=75.0 "), "{line}");

        // Through the state: a concurrent sweep's frees are not the major's.
        let state = ConcurrentGcState::new();
        state.note_major_decision(0, 0, cap, false, true);
        state.note_concurrent_freed(500); // a concurrent sweep, in between
        let c = state.major_cadence(1, 500 + 30);
        assert_eq!(c.majors, 1);
        assert_eq!(c.low_yield_5pct, 1, "the major itself freed 30 bytes (3 %)");
        assert_eq!(
            MajorCadenceCensus::default().majors_per_100_young_tenths(),
            0
        );
    }

    /// The no-op remark reference-processing hook: on by default since gce ve2
    /// (2026-09-29; `=0` kills it); on, it is
    /// called once per authorised remark, and the bitmap is exactly the
    /// unhooked remark's.
    #[test]
    fn the_remark_reference_processing_hook_is_a_counted_no_op() {
        let run = |hook: bool| -> (Vec<bool>, u64) {
            let (og, ptrs) = r4w3_layout(30);
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            let edits: &[(&str, Option<&str>)] = if hook {
                &[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", Some("1"))]
            } else {
                &[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", Some("0"))]
            };
            cratonvm_types::flags::with_thread_overrides(edits, || {
                marker.initial_mark(&[ptrs[0]], &og);
                marker.concurrent_mark(&og);
                marker.remark(&stw(), &[ptrs[0]], &og);
            });
            let set = ptrs
                .iter()
                .map(|&p| marker.bitmap.is_marked(p as usize))
                .collect();
            (set, marker.state.census().remark_refproc_hook_calls)
        };
        let (plain, calls_off) = run(false);
        let (hooked, calls_on) = run(true);
        assert_eq!(calls_off, 0);
        assert_eq!(calls_on, 1);
        assert_eq!(hooked, plain, "the hook changes nothing the sweep reads");
        assert_eq!(plain.iter().filter(|&&m| m).count(), 10);
    }

    // -----------------------------------------------------------------
    // gen r4w6/concsvc6 (2026-09-24)
    // -----------------------------------------------------------------

    /// The trigger-to-start census: the FIRST "due" answer while idle is
    /// remembered, the next initial mark turns it into one sample, a "not
    /// due" answer forgets it, and an answer given while a cycle is claimed
    /// or open is ignored.
    #[test]
    fn the_trigger_to_start_latency_is_sampled_once_per_owed_start() {
        let state = ConcurrentGcState::new();
        // No verdict recorded: an initial mark takes no sample.
        state.note_cycle_open(0, 0);
        state.note_cycle_dropped();
        assert_eq!(state.census().trigger_to_start_samples, 0);

        state.note_start_verdict(true);
        std::thread::sleep(std::time::Duration::from_millis(3));
        state.note_start_verdict(true); // later "due" answers keep the first instant
        state.note_cycle_open(0, 0);
        state.note_cycle_dropped();
        let c = state.census();
        assert_eq!(c.trigger_to_start_samples, 1);
        assert!(
            c.trigger_to_start_max_us >= 2_000,
            "measured from the first due answer: {}us",
            c.trigger_to_start_max_us
        );
        assert_eq!(c.trigger_to_start_total_us, c.trigger_to_start_max_us);

        // Due, then not due: nothing is owed any more.
        state.note_start_verdict(true);
        state.note_start_verdict(false);
        state.note_cycle_open(0, 0);
        state.note_cycle_dropped();
        assert_eq!(state.census().trigger_to_start_samples, 1);

        // An answer while a cycle is open says nothing about the next start.
        state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        state.note_start_verdict(true);
        state.set_phase(ConcurrentGcPhase::Idle);
        state.note_cycle_open(0, 0);
        state.note_cycle_dropped();
        assert_eq!(state.census().trigger_to_start_samples, 1);

        // Through the policy: `concurrent_start_due` records its own verdict.
        let capacity = 1_000_000;
        assert!(state.concurrent_start_due(capacity, capacity / 2, None));
        state.note_cycle_open(0, 0);
        state.note_cycle_dropped();
        assert_eq!(state.census().trigger_to_start_samples, 2);

        let line = state.driver_census_line();
        assert!(line.contains(" concdrv_trigger_to_start_n=2 "), "{line}");
        assert!(line.contains(" concdrv_cycles_started=0 "), "{line}");
        assert!(line.contains(" concdrv_cycles_completed=0 "), "{line}");
        assert!(line.contains(" concdrv_trigger_to_start_avg_us="), "{line}");
        assert!(line.contains(" concdrv_stw_frag_preempted=0 "), "{line}");
        assert!(line.contains(" concdrv_satb_outside_old_gen=0 "), "{line}");
    }

    /// The cadence census splits the confirmed majors by cause: the two
    /// "concurrent cycle too slow" halves and the rest, which together with
    /// the requested ones partition `majors`.
    #[test]
    fn the_major_cadence_census_splits_the_majors_by_cause() {
        let cap = 1_000;
        let mut m = MajorCadence::default();
        let causes = [
            MajorCause::FallbackCycleOpen,
            MajorCause::FallbackStartLate,
            MajorCause::FallbackStartLate,
            MajorCause::Occupancy,
            MajorCause::AllocationFailure,
            MajorCause::Fragmentation,
            MajorCause::Requested,
        ];
        let mut n = 0u64;
        for cause in causes {
            m.note_decision_caused(n, 0, cap, Some(cause));
            n += 1; // each decision's major ran
        }
        // One decision that did not run resolves the last one.
        m.note_decision_caused(n, 0, cap, None);
        let c = m.resolved(n, 0).0;
        assert_eq!(c.majors, 7);
        assert_eq!(c.fallback_cycle_open, 1);
        assert_eq!(c.fallback_start_late, 2);
        assert_eq!(c.fallback_conc_too_slow(), 3);
        assert_eq!(c.other_occupancy, 1);
        assert_eq!(c.other_alloc_failure, 1);
        assert_eq!(c.other_fragmentation, 1);
        assert_eq!(c.requested_majors, 1);
        assert_eq!(
            c.fallback_conc_too_slow()
                + c.other_occupancy
                + c.other_alloc_failure
                + c.other_fragmentation
                + c.requested_majors,
            c.majors,
            "the causes partition the majors"
        );
        assert!(MajorCause::FallbackStartLate.is_concurrent_fallback());
        assert!(!MajorCause::Fragmentation.is_concurrent_fallback());
        let line = c.line();
        assert!(line.starts_with("[GC] major_cadence: majcad_young_decisions=8 majcad_majors=7 "));
        assert!(line.contains(" majcad_fallback_conc_too_slow=3 "), "{line}");
        assert!(line.contains(" majcad_fallback_cycle_open=1 "), "{line}");
        assert!(line.contains(" majcad_fallback_start_late=2 "), "{line}");
        assert!(line.contains(" majcad_other=4 "), "{line}");
        assert!(line.ends_with(" majcad_other_fragmentation=1"), "{line}");

        // The wave-5 entry point still counts a non-requested run as occupancy.
        let mut w5 = MajorCadence::default();
        w5.note_decision(0, 0, cap, false, true);
        assert_eq!(w5.resolved(1, 0).0.other_occupancy, 1);
    }

    /// Phase 2 counts the SATB entries it drained from outside the old
    /// generation (the young old-values the barrier-side filter would drop),
    /// and marks exactly what it marked before.
    #[test]
    fn phase_two_counts_satb_entries_outside_the_old_generation() {
        let (og, ptrs) = r4w3_layout(9);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptrs[0]], &og);
        // Not old-gen storage: a Rust heap block stands in for a young object.
        let outside = vec![0u64; 4];
        let outside_addr = outside.as_ptr() as usize;
        assert!(!og.contains(outside_addr as *const u8));
        marker
            .satb_queue
            .flush(vec![ptrs[1] as usize, outside_addr, ptrs[2] as usize]);
        marker.concurrent_mark(&og);
        assert!(marker.bitmap.is_marked(ptrs[1] as usize));
        assert!(marker.bitmap.is_marked(ptrs[2] as usize));
        let c = marker.state.census();
        assert_eq!(c.satb_drained_in_mark, 3);
        assert_eq!(c.satb_outside_old_gen, 1);
        drop(outside);
    }

    // -----------------------------------------------------------------------
    // gen r5w1/refs5 (2026-09-26) — remark-time reference processing
    // -----------------------------------------------------------------------

    /// A legacy two-slot old-gen object (a stand-in `Reference`: slot 0 the
    /// referent, slot 1 an ordinary field such as `queue`).
    fn r5w1_two_slot(og: &mut OldGen, cid: u32) -> *mut u8 {
        let p = og.alloc(HEADER_SIZE + 2 * SLOT_SIZE, 8).unwrap();
        // SAFETY: a live `HEADER_SIZE + 2 * SLOT_SIZE` block from `alloc`.
        unsafe {
            let h = &mut *(p as *mut ObjectHeader);
            h.class_id = ClassId::new(cid);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            h.set_num_slots(2);
            h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
            (p.add(HEADER_SIZE) as *mut Value).write(Value::Object(None));
            (p.add(HEADER_SIZE + SLOT_SIZE) as *mut Value).write(Value::Object(None));
        }
        p
    }

    /// `from.slot[idx] = to`.
    fn r5w1_store(from: *mut u8, idx: usize, to: *mut u8) {
        // SAFETY: `from` is a live object with more than `idx` slots.
        unsafe {
            let slot = from.add(HEADER_SIZE + idx * SLOT_SIZE) as *mut Value;
            slot.write(Value::Object(Some(ObjectRef::from_raw(to))));
        }
    }

    /// Half 1: a `Reference` in the skip set does not trace its slot 0 — the
    /// referent reads UNMARKED at the seam while slot 1 is traced — and the
    /// ONE-COMMIT RULE is visible: a callback that neither clears nor keeps the
    /// referent lets the sweep free it (the use-after-free half 1 alone would
    /// be), while one that keeps it (a policy-kept soft referent) saves it and
    /// its closure. Without the skip the referent is simply marked.
    #[test]
    fn r5w1_a_hidden_referent_is_unmarked_at_the_seam_and_kept_only_if_returned() {
        let build = || {
            let mut og = OldGen::new(65536);
            let reference = r5w1_two_slot(&mut og, 1);
            let referent = r5w1_two_slot(&mut og, 2);
            let referent_child = r5w1_two_slot(&mut og, 3);
            let queue = r5w1_two_slot(&mut og, 4);
            r5w1_store(reference, 0, referent);
            r5w1_store(reference, 1, queue);
            r5w1_store(referent, 1, referent_child);
            (og, reference, referent, referent_child, queue)
        };

        for keep in [false, true] {
            let (mut og, reference, referent, child, queue) = build();
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            // A young (out-of-range) address is not an old-gen skip entry.
            assert_eq!(marker.set_reference_skip(&[reference as usize, 0x10]), 1);
            marker.initial_mark(&[reference], &og);
            marker.concurrent_mark(&og);
            assert!(marker.bitmap.is_marked(queue as usize), "slot 1 is traced");
            assert!(!marker.bitmap.is_marked(referent as usize), "slot 0 is hidden");

            let mut calls = 0;
            let mut process = |is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
                calls += 1;
                assert!(is_marked(reference as usize));
                assert!(!is_marked(referent as usize), "an untainted verdict");
                if keep {
                    vec![referent as usize]
                } else {
                    Vec::new()
                }
            };
            let process_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut process;
            marker.remark_with_reference_processing(&stw(), &[reference], &og, Some(process_dyn));
            assert_eq!(calls, 1);
            let swept = marker.concurrent_sweep(&mut og);
            assert!(!marker.reference_skip_armed.load(Ordering::Relaxed), "disarmed by the sweep");
            if keep {
                assert_eq!(swept, 0);
                assert!(og.is_allocated_addr(referent), "kept by the callback");
                assert!(og.is_allocated_addr(child), "with its closure");
            } else {
                // The staged use-after-free: the Reference still names it.
                assert_eq!(swept, 2, "referent and its child are freed");
                assert!(!og.is_allocated_addr(referent));
            }
            assert_eq!(marker.state.census().reference_skip_published, 1);
        }

        // No skip set: the referent is traced through slot 0, as before.
        let (og, reference, referent, _child, _queue) = build();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[reference], &og);
        marker.concurrent_mark(&og);
        assert!(marker.bitmap.is_marked(referent as usize));
        assert_eq!(marker.state.census().reference_skip_published, 0);
    }

    /// The split remark is the one-lock remark: `remark_begin` + the seam's
    /// predicate + `remark_finish` mark exactly what
    /// `remark_with_reference_processing` marks, and a refused remark's second
    /// half does nothing.
    #[test]
    fn r5w1_the_split_remark_marks_what_the_one_lock_remark_marks() {
        let run = |split: bool| {
            let mut og = OldGen::new(65536);
            let root = r5w1_two_slot(&mut og, 1);
            let a = r5w1_two_slot(&mut og, 2);
            let b = r5w1_two_slot(&mut og, 3);
            let dead = r5w1_two_slot(&mut og, 4);
            r5w1_store(a, 0, b);
            let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
            marker.initial_mark(&[root], &og);
            marker.concurrent_mark(&og);
            if split {
                let progress = marker.remark_begin(&stw(), &[root], &og);
                assert!(progress.sweep_authorised());
                assert!(!marker.remark_is_marked(a as usize));
                assert!(marker.remark_is_marked(0x10), "not eligible reads live");
                marker.remark_finish(&stw(), progress, Some(&[a as usize][..]), &og);
            } else {
                let mut keep_a = |_: &dyn Fn(usize) -> bool| -> Vec<usize> { vec![a as usize] };
                let dyn_keep: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut keep_a;
                marker.remark_with_reference_processing(&stw(), &[root], &og, Some(dyn_keep));
            }
            assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
            assert!(!marker.satb_queue.is_active());
            let marked = [root, a, b, dead].map(|p| marker.bitmap.is_marked(p as usize));
            (marked, marker.concurrent_sweep(&mut og))
        };
        assert_eq!(run(true), run(false));
        assert_eq!(run(true), ([true, true, true, false], 1));

        // Refused (stale epoch): the begin half establishes the post-conditions
        // and the finish half is a no-op.
        let mut og = OldGen::new(65536);
        let root = r5w1_two_slot(&mut og, 1);
        let victim = r5w1_two_slot(&mut og, 2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[root], &og);
        // SAFETY: exactly the pair `alloc` handed out.
        unsafe { og.free(victim, HEADER_SIZE + 2 * SLOT_SIZE) };
        let progress = marker.remark_begin(&stw(), &[root], &og);
        assert!(!progress.sweep_authorised());
        assert_eq!(marker.remark_finish(&stw(), progress, Some(&[root as usize][..]), &og), 0);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
        assert_eq!(marker.state.census().sweep_epoch_aborts, 1, "counted per VM");
    }

    /// The census the orchestrator's "no cycle ever starts" finding needs:
    /// every start verdict is counted, and the due ones separately.
    #[test]
    fn r5w1_the_start_trigger_census_counts_every_verdict() {
        let state = ConcurrentGcState::new();
        state.note_start_verdict(false);
        state.note_start_verdict(true);
        state.note_start_verdict(true);
        let c = state.census();
        assert_eq!((c.start_verdicts, c.start_verdicts_due), (3, 2));
        let line = state.driver_census_line();
        assert!(line.contains(" concdrv_start_asked=3 concdrv_start_due=2"), "{line}");
        state.note_remark_refproc_retired(5);
        let c = state.census();
        assert_eq!((c.remark_refproc_hook_calls, c.remark_refproc_retired), (1, 5));
    }

    // --- gen r5w3/unload7 ----------------------------------------------------

    fn r5w3_alloc(og: &mut OldGen, cid: u32) -> *mut u8 {
        let p = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        init_old_object(p, cid);
        p
    }

    fn r5w3_rows(rows: Vec<(usize, Vec<usize>)>) -> Option<rustc_hash::FxHashMap<usize, Vec<usize>>> {
        Some(rows.into_iter().collect())
    }

    /// With the side tables armed, the concurrent trace follows the three
    /// class-loader edges a stop-the-world major follows (`old_gen_gc`'s BFS):
    /// instance → its class's loader (`loader_pin`), loader → its mirrors
    /// (`mirror_pin`) and its deferred roots (`metadata_pin`). Without them it
    /// follows none, which is why every deferral used to be vetoed.
    #[test]
    fn r5w3_the_class_unload_tables_add_the_loader_mirror_and_metadata_edges() {
        let mut og = OldGen::new(65536);
        let instance = r5w3_alloc(&mut og, 7);
        let loader = r5w3_alloc(&mut og, 8);
        let mirror = r5w3_alloc(&mut og, 9);
        let statik = r5w3_alloc(&mut og, 10);
        let unrelated = r5w3_alloc(&mut og, 11);
        let tables = || ClassUnloadTables {
            loaders: Some([(7u32, loader as usize)].into_iter().collect()),
            mirrors: r5w3_rows(vec![(loader as usize, vec![mirror as usize])]),
            metadata: r5w3_rows(vec![(loader as usize, vec![statik as usize])]),
        };

        let plain = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        plain.initial_mark_with_frozen(&[instance], &[], &og);
        plain.concurrent_mark(&og);
        assert!(plain.bitmap.is_marked(instance as usize));
        for p in [loader, mirror, statik] {
            assert!(!plain.bitmap.is_marked(p as usize), "unarmed: no side-table edge");
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.set_class_unload_tables(tables());
        assert!(marker.class_unload_armed());
        marker.initial_mark_with_frozen(&[instance], &[], &og);
        marker.concurrent_mark(&og);
        for p in [instance, loader, mirror, statik] {
            assert!(marker.bitmap.is_marked(p as usize), "{p:p} is reachable through the tables");
        }
        assert!(!marker.bitmap.is_marked(unrelated as usize));
        assert_eq!(marker.state.census().class_unload_edges, 3);

        marker.remark(&stw(), &[instance], &og);
        assert!(!marker.class_unload_armed(), "released once the bitmap is final");
        assert_eq!(marker.concurrent_sweep(&mut og), 1, "only the unrelated object dies");
        assert!(!og.is_allocated_addr(unrelated));
        for p in [instance, loader, mirror, statik] {
            assert!(og.is_allocated_addr(p));
        }
    }

    /// The rows of an owner the trace cannot judge are marked without a scan:
    /// a YOUNG owner's (not sweep-eligible; the trace never scans young) at
    /// the initial mark, and an owner already BLACK when the remark's own
    /// capture adds a row for it. An old owner nothing reached keeps its rows
    /// unmarked, so its deferred objects die with it.
    #[test]
    fn r5w3_rows_of_owners_the_trace_cannot_judge_are_marked_without_a_scan() {
        let mut og = OldGen::new(65536);
        let root = r5w3_alloc(&mut og, 1);
        let young_owned = r5w3_alloc(&mut og, 2);
        let late = r5w3_alloc(&mut og, 3);
        let dead_owner = r5w3_alloc(&mut og, 4);
        let dead_owned = r5w3_alloc(&mut og, 5);
        // Outside the old generation: never sweep-eligible, as a young loader.
        let young_owner = 0x10usize;

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.set_class_unload_tables(ClassUnloadTables {
            loaders: None,
            mirrors: None,
            metadata: r5w3_rows(vec![
                (young_owner, vec![young_owned as usize]),
                (dead_owner as usize, vec![dead_owned as usize]),
            ]),
        });
        marker.initial_mark_with_frozen(&[root], &[], &og);
        assert!(marker.bitmap.is_marked(young_owned as usize), "marked in the pause");
        assert!(!marker.bitmap.is_marked(dead_owned as usize));
        marker.concurrent_mark(&og);

        // The remark's capture: the (black) root now owns `late`.
        marker.add_class_unload_tables(ClassUnloadTables {
            loaders: None,
            mirrors: r5w3_rows(vec![(root as usize, vec![late as usize])]),
            metadata: None,
        });
        marker.remark(&stw(), &[root], &og);
        assert!(marker.bitmap.is_marked(late as usize), "a black owner's new row");
        assert!(!marker.bitmap.is_marked(dead_owner as usize));
        assert!(!marker.bitmap.is_marked(dead_owned as usize));
        assert_eq!(marker.concurrent_sweep(&mut og), 2, "the dead owner and its row");
    }

    /// A later capture is a union: rows of either capture survive, a value is
    /// never listed twice, and an empty capture arms nothing.
    #[test]
    fn r5w3_class_unload_tables_absorb_a_later_capture() {
        let mut t = ClassUnloadTables {
            loaders: Some([(1u32, 0x100usize)].into_iter().collect()),
            mirrors: r5w3_rows(vec![(0x100, vec![0x200])]),
            metadata: None,
        };
        t.absorb(ClassUnloadTables {
            loaders: Some([(1u32, 0x180usize), (2, 0x300)].into_iter().collect()),
            mirrors: r5w3_rows(vec![(0x100, vec![0x200, 0x210])]),
            metadata: r5w3_rows(vec![(0x300, vec![0x400])]),
        });
        assert_eq!(t.loader_of(1), Some(0x180), "the later row wins");
        assert_eq!(t.loader_of(2), Some(0x300));
        assert_eq!(t.mirrors.as_ref().unwrap()[&0x100], vec![0x200, 0x210]);
        assert_eq!(t.metadata.as_ref().unwrap()[&0x300], vec![0x400]);
        assert_eq!(t.owner_rows().count(), 2);

        assert!(ClassUnloadTables::default().is_empty());
        let marker = ConcurrentMarker::new(0x1000, 0x1000);
        marker.set_class_unload_tables(ClassUnloadTables::default());
        assert!(!marker.class_unload_armed());
        marker.add_class_unload_tables(ClassUnloadTables::default());
        assert!(!marker.class_unload_armed());
    }

    /// The per-door start census: each visit's furthest step, per door, on a
    /// line of its own after the driver line.
    #[test]
    fn r5w3_the_door_census_counts_each_step_per_door() {
        let state = ConcurrentGcState::new();
        state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Asked);
        state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Asked);
        state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Due);
        state.note_door(GenConcDoor::AllocFail, GenConcDoorStep::Opened);
        state.note_door(GenConcDoor::Service, GenConcDoorStep::Asked);
        let c = state.door_census();
        assert_eq!(c[GenConcDoor::AllocFail as usize], [2, 1, 1]);
        assert_eq!(c[GenConcDoor::Service as usize], [1, 0, 0]);
        assert_eq!(c[GenConcDoor::MaybeGc as usize], [0, 0, 0]);
        let line = state.door_census_line();
        assert!(
            line.starts_with("[GC] conc_doors: concdoor_maybe_gc_asked=0 "),
            "{line}"
        );
        assert!(
            line.contains(
                " concdoor_alloc_fail_asked=2 concdoor_alloc_fail_due=1 \
                 concdoor_alloc_fail_opened=1 "
            ),
            "{line}"
        );
        assert!(line.contains(" concdoor_service_asked=1 "), "{line}");
        let driver = state.driver_census_line();
        let mut lines = driver.lines();
        assert!(lines
            .next()
            .is_some_and(|l| l.starts_with("[GC] conc_driver: concdrv_service_enabled=")));
        assert_eq!(lines.next(), Some(line.as_str()));
    }

    // -----------------------------------------------------------------
    // gen r5w4/conc8 (2026-09-26) — the retained-layout census
    // (`gengc-r5w3-unload7-retained-layouts-are-never-released`)
    // -----------------------------------------------------------------

    /// The state's bookkeeping: pending ids are deduplicated, a census moves
    /// exactly the released ones to releasable (and counts itself), and the
    /// driver drains them once.
    #[test]
    fn r5w4_a_census_moves_released_layouts_from_pending_to_releasable() {
        let state = ConcurrentGcState::new();
        assert!(!state.has_retained_layouts());
        state.note_layouts_retained(&[]);
        assert!(!state.has_retained_layouts(), "an empty retention arms nothing");
        state.note_layouts_retained(&[7, 3]);
        state.note_layouts_retained(&[3, 9]);
        assert_eq!(state.retained_layouts_pending(), vec![3, 7, 9]);
        state.complete_layout_census(vec![9, 3]);
        assert_eq!(state.retained_layouts_pending(), vec![7]);
        let mut released = state.take_releasable_layouts();
        released.sort_unstable();
        assert_eq!(released, vec![3, 9]);
        assert!(state.take_releasable_layouts().is_empty(), "drained");
        state.complete_layout_census(Vec::new());
        state.note_layouts_released(2);
        let c = state.census();
        assert_eq!(c.class_unload_layout_censuses, 2, "an empty census still counts");
        assert_eq!(c.class_unload_layouts_released, 2);
        assert!(state.has_retained_layouts());
    }

    /// gcd d2/g: the stop-the-world census releases every pending id no
    /// surviving object has, keeps the others, and is counted apart from the
    /// concurrent census.
    #[test]
    fn gcd_d2g_a_stw_census_releases_the_pending_ids_no_object_has() {
        let state = ConcurrentGcState::new();
        state.note_layouts_retained(&[4, 5, 6]);
        let survivors: rustc_hash::FxHashSet<u32> = [5u32, 77].into_iter().collect();
        assert_eq!(state.complete_stw_layout_census(&survivors), 2);
        assert_eq!(state.retained_layouts_pending(), vec![5], "an object of 5 is left");
        let mut released = state.take_releasable_layouts();
        released.sort_unstable();
        assert_eq!(released, vec![4, 6]);
        assert_eq!(state.complete_stw_layout_census(&survivors), 0);
        let c = state.census();
        assert_eq!(c.class_unload_stw_layout_censuses, 2);
        assert_eq!(c.class_unload_layout_censuses, 0, "the concurrent count is its own");
    }

    /// A census whose baseline no longer matches the process's walk-break
    /// count saw a walk that skipped bytes: it releases nothing.
    #[test]
    fn r5w4_a_census_across_a_walk_break_releases_nothing() {
        let census = LayoutCensus {
            candidates: [1u32, 2].into_iter().collect(),
            survivors: rustc_hash::FxHashSet::default(),
            breaks_at_start: walk_break_hits().wrapping_add(1),
        };
        assert_eq!(census.released(), None);
    }

    /// A complete sliced sweep: a candidate with a surviving instance stays
    /// pending, a candidate whose only instance was garbage and one with no
    /// instance at all are released. `r4w3_layout` gives object `i` class id
    /// `i + 1`, and objects 0, 3, 6, ... are live. The walk-break count is
    /// process-wide, so a desync test running in parallel can make one attempt
    /// inconclusive (which is the census failing SAFE); the scenario is
    /// retried on a fresh generation until one attempt is conclusive.
    #[test]
    fn r5w4_a_complete_sweep_releases_the_layouts_it_found_no_survivor_of() {
        for _attempt in 0..32 {
            let (mut og, ptrs) = r4w3_layout(30);
            let marker = r4w3_marked(&og, ptrs[0]);
            marker.state.note_layouts_retained(&[1, 2, 1000]);
            marker.set_layout_census(vec![1, 2, 1000]);
            let (freed, _) = r4w3_sweep_in_slices(&marker, &mut og, 4);
            assert_eq!(freed, 20, "the census changes nothing the sweep frees");
            if marker.state.census().class_unload_layout_censuses == 0 {
                // Another test's walk broke meanwhile: inconclusive, nothing
                // released. Check that, then try again.
                assert!(marker.state.take_releasable_layouts().is_empty());
                assert_eq!(marker.state.retained_layouts_pending(), vec![1, 2, 1000]);
                continue;
            }
            let mut released = marker.state.take_releasable_layouts();
            released.sort_unstable();
            assert_eq!(released, vec![2, 1000]);
            assert_eq!(
                marker.state.retained_layouts_pending(),
                vec![1],
                "object 0 (class 1) survives: its layout stays"
            );
            return;
        }
        panic!("no conclusive census in 32 attempts");
    }

    /// A sweep stopped between slices by another collector's free did not see
    /// the whole generation: its census is dropped, nothing is released.
    #[test]
    fn r5w4_an_epoch_stopped_sweep_releases_nothing() {
        let size = HEADER_SIZE + SLOT_SIZE;
        let (mut og, ptrs) = r4w3_layout(30);
        let marker = r4w3_marked(&og, ptrs[0]);
        marker.state.note_layouts_retained(&[2, 1000]);
        marker.set_layout_census(vec![2, 1000]);
        let (first, done) = marker.concurrent_sweep_budget(&mut og, 4);
        assert!(!done && first == 2);
        // SAFETY: exactly the pair `alloc` handed out; object 20 is garbage.
        unsafe { og.free(ptrs[20], size) };
        assert_eq!(marker.concurrent_sweep_budget(&mut og, 4), (0, true));
        assert_eq!(marker.state.census().class_unload_layout_censuses, 0);
        assert!(marker.state.take_releasable_layouts().is_empty());
        assert_eq!(marker.state.retained_layouts_pending(), vec![2, 1000]);
    }

    /// gen r5w6/conc10 — the coverage rule of the remark's fail-closed check,
    /// pure: same base and a capacity the bitmap spans (a shrunk capacity is
    /// still covered); a grown or moved generation is not.
    #[test]
    fn r5w6_bitmap_covers_generation_rule() {
        assert!(bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x4000));
        assert!(bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x2000));
        assert!(!bitmap_covers_generation(0x1000, 0x4000, 0x1000, 0x4008));
        assert!(!bitmap_covers_generation(0x1000, 0x4000, 0x2000, 0x4000));
    }

    /// gen r5w6/conc10 (old9's request) — a remark whose mark bitmap was sized
    /// for a SMALLER old generation than the one it remarks (an in-place
    /// growth between the marker's construction and the pause) refuses the
    /// sweep, so nothing in the unmarkable span is freed; the same cycle over
    /// a covering bitmap is authorised.
    #[test]
    fn r5w6_a_remark_over_a_grown_generation_refuses_its_sweep() {
        let (mut og, ptr) = make_old_gen_with_object(1);
        let base = og.base_ptr() as usize;
        let cap = og.capacity();

        // The bitmap as it would have been sized before a growth by one word.
        let short = ConcurrentMarker::new(base, cap - 8);
        short.initial_mark(&[ptr], &og);
        short.concurrent_mark(&og);
        let progress = short.remark_begin(&stw(), &[ptr], &og);
        assert!(
            !progress.sweep_authorised(),
            "a bitmap that does not span the generation must not authorise a sweep"
        );
        short.remark_finish(&stw(), progress, None, &og);
        assert_eq!(short.concurrent_sweep(&mut og), 0, "a refused remark frees nothing");
        assert!(og.is_allocated_addr(ptr));

        // Control: the covering bitmap authorises the same cycle.
        let covering = ConcurrentMarker::new(base, cap);
        covering.initial_mark(&[ptr], &og);
        covering.concurrent_mark(&og);
        let progress = covering.remark_begin(&stw(), &[ptr], &og);
        assert!(progress.sweep_authorised());
        covering.remark_finish(&stw(), progress, None, &og);
        assert_eq!(covering.concurrent_sweep(&mut og), 0, "the rooted object is kept");
        assert!(og.is_allocated_addr(ptr));
    }
}
