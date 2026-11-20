// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
use crate::satb::SatbQueue;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::Value;

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
    pub census: CycleCensusCells,
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
    pub service: ServiceCells,
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
    /// Per-VM poll sync hook invoked on `inline_start` transitions so the VM
    /// barrier can synchronize its page-form poll without process-global state.
    poll_sync_hook: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

/// gen r5w6/conc10 — what the concurrent pauses' young→old seeding did under
/// `CRATONVM_GEN_Y2O_LIVE_SEED` (`GenerationalHeap::collect_young_to_old_seeds`),
/// and how often a young collection's root scan deferred a young user-loader
/// mirror under `CRATONVM_GEN_YOUNG_MIRROR_DEFER`. Printed as the
/// `[GC] conc_y2o:` line ([`ConcurrentGcState::y2o_census_line`]) only while
/// either flag is on.
#[derive(Default)]
pub struct Y2oSeedCells {
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
    /// gch h3/mj — g4og step 0 (`CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS`,
    /// `crate::gen_heap::Y2oOldHolderCensus`): initial marks censused, how
    /// many of those the root-only marking fell back on, and the summed
    /// distinct seeded old refs / those only old holders' young children
    /// name / their bytes. Printed as `[GC] conc_y2o_holders:`.
    holder_census_pauses: AtomicU64,
    holder_census_fallbacks: AtomicU64,
    holder_census_seeded: AtomicU64,
    holder_census_via_old_holders: AtomicU64,
    holder_census_via_old_holders_bytes: AtomicU64,
}

/// gen r5w4/conc8 — see [`ConcurrentGcState::note_layouts_retained`].
#[derive(Debug, Default)]
pub struct RetainedLayouts {
    /// Class ids whose layout a remark put back and nothing has released yet.
    /// Sorted, no duplicates.
    pending: Vec<u32>,
    /// Ids a COMPLETE census found no instance of, waiting for the driver to
    /// unregister them ([`ConcurrentGcState::take_releasable_layouts`]).
    releasable: Vec<u32>,
}

/// gc-common w36-d — see [`ConcurrentGcState::publish_finalizer_candidates`].
#[derive(Default)]
pub struct FinalizerChannel {
    /// The OLD-generation addresses of every registered finalizable, as the
    /// last stop-the-world collection left them. Sorted.
    candidates: Vec<usize>,
    /// Candidates a remark found dead and RETAINED, not yet reported.
    retained: Vec<usize>,
}

/// gen r4w3/oldgen3 — the atomic cells behind [`ConcurrentCycleCensus`].
#[derive(Default)]
pub struct CycleCensusCells {
    initial_marks: AtomicU64,
    initial_marks_with_takeover: AtomicU64,
    frozen_threads: AtomicU64,
    pub sweep_slices: AtomicU64,
    pub sweep_epoch_stops: AtomicU64,
    pub overflow_rescan_passes: AtomicU64,
    pub satb_drained_in_mark: AtomicU64,
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
    pub satb_outside_old_gen: AtomicU64,
    // --- gen r5w1/refs5 ----------------------------------------------------
    pub sweep_epoch_aborts: AtomicU64,
    pub reference_skip_published: AtomicU64,
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
    // --- gcg g2/r ----------------------------------------------------------
    sweep_reference_row_prunes: AtomicU64,
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
    pub phase2_start_walks: AtomicU64,
    pub phase2_walk_us: AtomicU64,
    pub phase2_tams_slices: AtomicU64,
    /// gce e2/c — inline start requests (no service): latched by a direct
    /// old-gen allocation, and taken by the VM's allocation slow path.
    inline_start_latched: AtomicU64,
    inline_start_taken: AtomicU64,
    /// gcj j2/og2 (C2) — see [`ConcurrentCycleCensus::bitmap_cover_refusals`].
    pub bitmap_cover_refusals: AtomicU64,
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
    /// `docs/feature-designs/jit-ir-optimizations.md` asks for.
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
    /// gcg g2/r (2026-10-01) — reference-processor walks the concurrent sweep
    /// made to drop those rows
    /// ([`ConcurrentGcState::note_sweep_reference_row_prunes`]), summed: about
    /// one per freeing slice by default, about one per pause boundary plus one
    /// with `CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH`.
    pub sweep_reference_row_prunes: u64,
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
    /// gcj j2/og2 (C2 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`):
    /// remarks that refused their sweep because the old generation had grown
    /// (or moved) past the span the cycle's mark bitmap was built for
    /// (`bitmap_covers_generation`). Each is a whole traced cycle wasted; with
    /// the bitmap spanning the growth limit (the default since gcj j2/og2) it
    /// stays 0 unless `CRATONVM_GC_CONC_BITMAP_GROWTH_SPAN=0`.
    pub bitmap_cover_refusals: u64,
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
            poll_sync_hook: std::sync::OnceLock::new(),
        }
    }

    /// Install this VM's per-instance poll sync hook (called once at VM init).
    pub fn set_poll_sync_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let _ = self.poll_sync_hook.set(hook);
    }

    /// gch h3/mj — g4og step 0: the shutdown `[GC] conc_y2o_holders:` line,
    /// only under `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS` (a default run's
    /// summary gains no line). `conc_y2o_old_via_old_holders` is the page's
    /// key: summed over initial marks, the distinct old objects the live seed
    /// took that NO root-reached young object names -- an upper bound on what
    /// a true-root initial mark would free one cycle earlier.
    pub fn y2o_holder_census_line(&self) -> Option<String> {
        if !y2o_old_holder_census_enabled() {
            return None;
        }
        let c = &self.y2o;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Some(format!(
            "[GC] conc_y2o_holders: census_initial_marks={} census_fallbacks={} \
             conc_y2o_old_seeded={} conc_y2o_old_via_old_holders={} \
             conc_y2o_old_via_old_holders_bytes={}",
            r(&c.holder_census_pauses),
            r(&c.holder_census_fallbacks),
            r(&c.holder_census_seeded),
            r(&c.holder_census_via_old_holders),
            r(&c.holder_census_via_old_holders_bytes),
        ))
    }

    /// gcd d1/c (2026-09-27) — one remark's search for the YOUNG objects only a
    /// retained dead finalizable reaches (`GenerationalHeap::young_reached_only_through`):
    /// `Some(n)` found `n`, `None` fell back and reported nothing. Relaxed adds.
    pub fn note_y2o_finalizer_only_young(&self, found: Option<usize>) {
        let c = &self.y2o;
        c.finalizer_only_passes.fetch_add(1, Ordering::Relaxed);
        match found {
            Some(n) => {
                c.finalizer_only_young
                    .fetch_add(n as u64, Ordering::Relaxed);
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
    /// `CRATONVM_GEN_Y2O_LIVE_SEED` or `CRATONVM_GEN_YOUNG_MIRROR_DEFER` is on.
    /// (gch h3/mj: both are default ON since the gce close, so a default
    /// summary DOES print it; only `=0` on both drops it.) Appended by
    /// [`Self::driver_census_line`].
    pub fn y2o_census_line(&self) -> Option<String> {
        if !y2o_live_seed_enabled()
            && !true
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
    pub fn complete_layout_census(&self, mut released: Vec<u32>) {
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

    /// gci i17/fz (i16/fr's residual, `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`):
    /// take `addrs` (POST-collection addresses, any order) back out of the
    /// published candidates, in the same pause that published them, so the
    /// next remark never retains them. The VM's stop-the-world probe hands its
    /// collection extra resurrection candidates -- the referents of
    /// `Reference`s only a dead finalizable object reaches -- that are not
    /// finalizable: published, a remark that runs before the next
    /// stop-the-world pause retained a dead one (and its subgraph) for that
    /// cycle, and a weak reference to it was not cleared at that remark. The
    /// retained list is untouched (it is drained by the same pause, before
    /// the publication).
    pub fn retract_finalizer_candidates(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        let mut out: Vec<usize> = addrs.to_vec();
        out.sort_unstable();
        let mut channel = self.finalizers.lock();
        channel
            .candidates
            .retain(|a| out.binary_search(a).is_err());
    }

    /// Record candidates a remark found dead and retained.
    ///
    /// gcj j3/og3 (C7 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`):
    /// without duplicates. A dead finalizable that no stop-the-world collection
    /// drained yet is re-retained by every later remark (it must be: the
    /// re-trace is what keeps it and its subgraph alive until it is reported),
    /// and each one appended it again, so the list grew by the whole retained
    /// set per cycle. First occurrence kept, in order, so the report order of
    /// the next pause is unchanged.
    pub fn note_remark_retained_finalizers(&self, addrs: &[usize]) {
        if addrs.is_empty() {
            return;
        }
        let mut channel = self.finalizers.lock();
        let mut seen: rustc_hash::FxHashSet<usize> = channel.retained.iter().copied().collect();
        for &a in addrs {
            if seen.insert(a) {
                channel.retained.push(a);
            }
        }
    }

    /// Take (drain) the retained candidates, for the next stop-the-world
    /// collection to report. The caller must drop any that are no longer
    /// registered finalizables.
    pub fn take_remark_retained_finalizers(&self) -> Vec<usize> {
        std::mem::take(&mut self.finalizers.lock().retained)
    }

    /// gcj j4/rf (`gcj-j1rf-gen-reference-and-finalizer-review-residuals`
    /// item 2, `CRATONVM_GEN_CONC_REMARK_SOFT_BEFORE_FINAL`): take back out of
    /// the retained list the candidates the remark's reference round found
    /// SOFTLY reachable after all. The retention runs on the strong-only
    /// bitmap, before the soft phase; a policy-kept `SoftReference` that
    /// reaches a retained candidate keeps it alive, and HotSpot processes soft
    /// references before final ones, so that candidate is not finalized at this
    /// cycle (it stays marked, and registered). Returns how many were taken.
    pub fn retract_remark_retained_finalizers(
        &self,
        addrs: &rustc_hash::FxHashSet<usize>,
    ) -> usize {
        if addrs.is_empty() {
            return 0;
        }
        let mut channel = self.finalizers.lock();
        let before = channel.retained.len();
        channel.retained.retain(|a| !addrs.contains(a));
        before - channel.retained.len()
    }

    /// gen r4w3/oldgen3 — record one initial-mark pause that OPENED a cycle
    /// (called by the driver after `ConcurrentMarker::initial_mark` ran), and
    /// how many threads `stw_take_over_and_wait` froze for it.
    ///
    /// A frozen thread may be stopped between a compiled reference store's
    /// SATB gate test (which read "clear" before this pause armed it) and the
    /// store itself; when it resumes it completes the store WITHOUT logging
    /// the old value. That is the hazard
    /// `docs/feature-designs/jit-ir-optimizations.md` describes,
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
            sweep_reference_row_prunes: r(&c.sweep_reference_row_prunes),
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
            bitmap_cover_refusals: r(&c.bitmap_cover_refusals),
        }
    }

    /// gci i2/wr — concurrent sweep slices begun over this VM's life, for the
    /// native address-memo epoch (`NativeContext::gc_address_epoch`,
    /// `gci-i1wr-generational-concurrent-sweep-does-not-move-the-address-memo-epoch`).
    ///
    /// [`ConcurrentMarker::concurrent_sweep_budget_into`] bumps the counter
    /// under the old-gen guard BEFORE its slice frees anything, so a block the
    /// slice frees can be re-issued (by an allocation that takes the same
    /// guard afterwards) only once the bump is visible to every thread that
    /// learns the new object's address. Monotonic, never reset; stays 0 on G1
    /// and ZGC (the Generational marker is the only writer). One relaxed load.
    pub fn sweep_slices_begun(&self) -> u64 {
        self.census.sweep_slices.load(Ordering::Relaxed)
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
             conccyc_bitmap_cover_refusals={} \
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
            c.bitmap_cover_refusals,
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

    pub fn note_class_unload_edges(&self, n: u64) {
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

    /// gcg g2/r (2026-10-01, `gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries`)
    /// — a concurrent sweep walked the reference processor `n` times to drop
    /// the rows of the `Reference`s it freed (one walk per freeing slice on
    /// the default path; one per pause boundary, cap or sweep end with the
    /// batch). Printed as `concdrv_sweep_reference_row_prunes`. One relaxed
    /// add per sweep; nothing for `0`.
    pub fn note_sweep_reference_row_prunes(&self, n: u64) {
        if n == 0 {
            return;
        }
        self.census
            .sweep_reference_row_prunes
            .fetch_add(n, Ordering::Relaxed);
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

    /// Whether the PHASE is a marking one (`ConcurrentMark` or `Remark`).
    ///
    /// gcj j2/og2 (C8): not the barrier's gate. The SATB barriers test
    /// `SatbQueue::is_active()` (see [`Self::set_phase`]), so this answer says
    /// nothing about whether a store is logged. Doc only; shared with G1,
    /// behaviour unchanged.
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
    pub fn cas_quiet_phase(&self, from: ConcurrentGcPhase, to: ConcurrentGcPhase) -> bool {
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
/// Page: `docs/feature-designs/generative-gc-architecture.md`.
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

/// gci i13/cm (2026-10-13, item 2 of
/// `gci-i13cm-generational-concurrent-cycle-under-the-user-flips-review-findings-FIXED-20261016`) —
/// `CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP` (default on, `runtime_flag_default_on`;
/// token `gen-conc-preempt-growth-net-of-sweep`): a STW collection that
/// pre-empts an open cycle IN ITS SWEEP widens the start policy's predicted
/// growth by the growth the cycle really saw, `allocated_total + (bytes the
/// sweep freed so far) - baseline`, instead of the legacy
/// `allocated_total - baseline`, which reads low by the sweep's frees (they
/// leave `used()` at once and reach `OldGenTriggerStats::freed_bytes` only at
/// the sweep's end) and saturates to 0 when the sweep has freed more than the
/// cycle has promoted. Off: the widening is byte for byte the legacy figure.
/// The `conccyc_preempt_growth_max_pct` census reads the net figure either
/// way. Read once per pre-emption (rare).
pub const GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP_FLAG: &str =
    "CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP";

/// Is [`GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP_FLAG`] on?
pub fn gen_conc_preempt_growth_net_of_sweep_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on(GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP_FLAG)
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
        // gcf f5/c: the conc8 step term (`CRATONVM_GC_CONC_START_STEP_BUFFER`,
        // gcf f1/c, `threshold_with_step`) was DELETED after vf3 c3-3: its
        // control fired and it lowered nothing.
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
pub struct CycleBaseline {
    at: std::time::Instant,
    allocated_total: u64,
    used: usize,
}

/// gen r4w4/concmark4 — the adaptive start policy's memory, behind
/// `ConcurrentGcState::policy`.
#[derive(Debug, Default)]
pub struct StartPolicyState {
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
    /// gci i13/cm item 2 — bytes the open cycle's sweep has freed SO FAR.
    /// `OldGen::free` takes them off `used()` at once, but
    /// `OldGenTriggerStats::freed_bytes` learns them only at the sweep's end
    /// (`OldGen::note_concurrent_collection_end`), so in between
    /// [`old_gen_allocated_total`] reads LOW by exactly this much. Written by
    /// every sweep slice that freed something and cleared when the cycle's
    /// baseline goes ([`ConcurrentGcState::note_cycle_end`] /
    /// `note_cycle_dropped` / `note_cycle_open`), all under the old-gen lock.
    sweep_freed: u64,
}

/// α = ½ exponential moving average in integers.
#[inline]
pub fn ewma_half(prev: Option<u64>, sample: u64) -> u64 {
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
        p.sweep_freed = 0;
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
        p.sweep_freed = 0;
    }

    /// gci i13/cm item 2 — the open cycle's sweep has freed `freed_so_far`
    /// bytes in total (see `StartPolicyState::sweep_freed`). Called by each
    /// sweep slice that freed something, under the old-gen lock; takes the
    /// policy lock (leaf), so at most once per slice, never per object.
    pub fn note_sweep_freed_so_far(&self, freed_so_far: usize) {
        self.policy.lock().sweep_freed = freed_so_far as u64;
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
        self.end_cycle(allocated_total, used, capacity, freed_bytes, true);
    }

    /// gci i13/cm item 7 — the open cycle's sweep was STOPPED between two
    /// slices by a stop-the-world collection (`reclaim_epoch` moved). The
    /// start policy is fed exactly as [`Self::note_cycle_end`] feeds it (the
    /// growth sample, unless a pre-emption already widened it; the duration;
    /// the start hint), so no decision moves; but the cycle did not finish its
    /// sweep, so it is counted `abandoned` (`conccyc_abandoned`), not
    /// `cycles_completed`, and records no report -- the driver prints no
    /// `Concurrent Mark Cycle` line for it. The census identity
    /// `started = completed + abandoned + swept_nothing (+ 1 open)` holds.
    pub fn note_cycle_stopped(&self, allocated_total: u64, used: usize, capacity: usize) {
        self.end_cycle(allocated_total, used, capacity, 0, false);
    }

    /// [`Self::note_cycle_end`] (`completed`) and [`Self::note_cycle_stopped`].
    fn end_cycle(
        &self,
        allocated_total: u64,
        used: usize,
        capacity: usize,
        freed_bytes: usize,
        completed: bool,
    ) {
        let mut p = self.policy.lock();
        // gci i13/cm item 2: the sweep's frees are in `allocated_total` now
        // (`note_concurrent_collection_end` / `note_concurrent_sweep_stopped`
        // ran first, same old-gen hold).
        p.sweep_freed = 0;
        // No baseline: a sweep over a snapshot no `initial_mark` on this state
        // took (a hand-built test marker). Nothing to sample or report.
        let Some(base) = p.open.take() else {
            return;
        };
        let n = if completed {
            self.census
                .cycles_completed
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1)
        } else {
            self.census.abandoned.fetch_add(1, Ordering::Relaxed);
            0
        };
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
        let next_threshold = self.policy_threshold(&p, capacity);
        // gen r4w5/concmark5: the growth poll's hint follows the new threshold.
        self.service
            .start_hint
            .store(next_threshold, Ordering::Relaxed);
        // gci i13/cm item 7: a stopped cycle leaves the last report alone.
        if !completed {
            return;
        }
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
        // gci i13/cm item 2: a pre-emption that lands in the SWEEP phase reads
        // `allocated_total` low by what the sweep has freed so far (see
        // `StartPolicyState::sweep_freed`), so the growth it saw is
        // `allocated_total + sweep_freed - baseline`. The census below always
        // reads the net figure (diagnostic only); the widening reads it only
        // under `CRATONVM_GEN_CONC_PREEMPT_GROWTH_NET_OF_SWEEP` (opt-in: it moves
        // the next start), else the legacy figure, byte for byte as before.
        let sweep_freed = p.sweep_freed;
        let (so_far, so_far_net) = p.open.map_or((0, 0), |b| {
            (
                allocated_total.saturating_sub(b.allocated_total),
                allocated_total
                    .saturating_add(sweep_freed)
                    .saturating_sub(b.allocated_total),
            )
        });
        let so_far = if gen_conc_preempt_growth_net_of_sweep_enabled() {
            so_far_net
        } else {
            so_far
        };
        // gcd d9/e: the growth the cycle had seen when it was pre-empted, for
        // `[GC] conc_cycles: conccyc_preempt_growth_max_pct` (every
        // pre-emption, not only the one that widens the prediction).
        if capacity > 0 {
            let permille = so_far_net.saturating_mul(1000) / capacity as u64;
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
        let hint = self.policy_threshold(&p, capacity);
        self.service.start_hint.store(hint, Ordering::Relaxed);
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

    /// The initiating occupancy for the policy state `p` (its lock held by
    /// the caller): [`ConcurrentStartPolicy::threshold`] over the growth
    /// prediction and `CRATONVM_GC_CONC_START_PERCENT`. (gcf f5/c: the step
    /// term of the deleted `CRATONVM_GC_CONC_START_STEP_BUFFER` is gone.)
    fn policy_threshold(&self, p: &StartPolicyState, capacity: usize) -> usize {
        ConcurrentStartPolicy::threshold(
            capacity,
            p.growth_ewma,
            crate::gc_flags().conc_start_percent,
        )
    }

    /// gen r4w4/concmark4 — the initiating occupancy now, in bytes.
    pub fn concurrent_start_threshold(&self, capacity: usize) -> usize {
        let t = {
            let p = self.policy.lock();
            self.policy_threshold(&p, capacity)
        };
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
    pub fn note_frozen_objects_scanned(&self, n: u64) {
        self.census
            .frozen_objects_scanned
            .fetch_add(n, Ordering::Relaxed);
    }

    /// gen r4w4/concmark4 — grey objects the local stack spilled.
    pub fn note_mark_local_spills(&self, n: u64) {
        if n != 0 {
            self.census
                .mark_local_spills
                .fetch_add(n, Ordering::Relaxed);
        }
    }
}

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
pub struct ServiceSlot {
    /// The attached service thread, if any.
    thread: Option<std::thread::ThreadId>,
    /// The first unserved request since the service last woke.
    pending: Option<ConcurrentServiceWake>,
    /// Set once by [`ConcurrentGcState::shutdown_service`]; never cleared.
    shutdown: bool,
}

/// gen r4w5/concmark5 — see [`ConcurrentGcState::serve`].
#[derive(Default)]
pub struct ServiceCells {
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
    pub start_hint: AtomicUsize,
    /// gce e1/c — with NO service attached, the direct-old-allocation poll
    /// latches a start request here instead of waking a service; the VM runs
    /// the driver inline at the requesting thread's next allocation slow path
    /// ([`ConcurrentGcState::take_inline_start_request`]). See
    /// `gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924`
    /// item 1.
    inline_start: AtomicBool,
    /// gcf f1/c — `CRATONVM_GEN_CONC_INLINE_START`, read once per VM
    /// (`0` unread, `1` off, `2` on): the direct-old-allocation poll used to
    /// read the flag on EVERY direct old allocation above the start hint with
    /// the flag off (the default), under the old-gen lock.
    inline_start_flag: AtomicU8,
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

    /// gcf f1/c — is `CRATONVM_GEN_CONC_INLINE_START` on for this VM? Read
    /// once and cached on this state (per VM, not a process global); one
    /// relaxed load afterwards.
    #[inline]
    pub fn inline_start_enabled(&self) -> bool {
        match self.service.inline_start_flag.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = true;
                self.service
                    .inline_start_flag
                    .store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// gce e1/c — a direct old-gen allocation found the cycle due and no
    /// service is attached: latch the request for the VM's allocation slow
    /// path (it cannot run here: the caller holds the old-gen lock and is not
    /// at a point where the thread may take part in a pause). Returns `true`
    /// if this call set the latch.
    pub fn latch_inline_start_request(&self) -> bool {
        let set = !self.service.inline_start.swap(true, Ordering::SeqCst);
        if set {
            // gce e2/c: `[GC] conc_cycles: conccyc_inline_start_latched`.
            self.census
                .inline_start_latched
                .fetch_add(1, Ordering::Relaxed);
            if let Some(hook) = self.poll_sync_hook.get() {
                hook();
            }
        }
        set
    }

    /// gce e1/c — is an inline start request latched? One `SeqCst` load: the
    /// VM's per-allocation poll reads this before it pays the `swap` of
    /// [`Self::take_inline_start_request`].
    #[inline]
    pub fn inline_start_requested(&self) -> bool {
        self.service.inline_start.load(Ordering::SeqCst)
    }

    /// gcg g4/oo: the address of the inline-start latch byte (stable: the
    /// state lives in an `Arc`), for a compiled poll that tests it beside the
    /// STW flag under `CRATONVM_GEN_CONC_OWED_POLL`.
    pub fn inline_start_flag_addr(&self) -> usize {
        &self.service.inline_start as *const AtomicBool as usize
    }

    /// gce e1/c — consume the latched request (`true` once per latch). The
    /// caller then runs its driver (`maybe_concurrent_gc_at`), which re-asks
    /// the trigger, so a request gone stale (another door's cycle, a STW
    /// major in between) opens nothing.
    pub fn take_inline_start_request(&self) -> bool {
        let taken = self.service.inline_start.load(Ordering::SeqCst)
            && self.service.inline_start.swap(false, Ordering::SeqCst);
        if taken {
            // gce e2/c: `[GC] conc_cycles: conccyc_inline_start_taken`.
            self.census
                .inline_start_taken
                .fetch_add(1, Ordering::Relaxed);
            if let Some(hook) = self.poll_sync_hook.get() {
                hook();
            }
        }
        taken
    }

    /// A direct old-gen allocation found the cycle due: wake the service.
    /// `false` if no service is attached, or it is stopping.
    pub fn signal_old_gen_growth(&self) -> bool {
        let mut slot = self.service.slot.lock();
        if slot.thread.is_none() {
            return false;
        }
        // gci i1/og: as `hand_off_to_service` (gen r5w5/conc9): a stopping
        // service answers `Shutdown` before it reads `pending`, and its
        // detach clears it, so a signal posted now is dropped; do not count
        // it as sent (`concdrv_service_growth_signals`).
        if slot.shutdown {
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
            let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Idle);
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
    /// (`docs/feature-designs/generative-gc-architecture.md`).
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
    pub fn note_remark_refproc_hook(&self) {
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
        // gch h3/mj: g4og step 0, under its own opt-in.
        if let Some(holders) = self.y2o_holder_census_line() {
            text.push('\n');
            text.push_str(&holders);
        }
        // gcd d2/g: under `CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, the young
        // sweep's precise-root exemptions (process-wide counts; this is the
        // one generational summary printer this lane can reach).
        if let Some(prp) = PRECISE_ROOT_PROMOTE_CENSUS_HOOK.get().and_then(|f| f()) {
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
             concdrv_sweep_reference_rows_dropped={} \
             concdrv_sweep_reference_row_prunes={}",
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
            c.sweep_reference_row_prunes,
        )
    }
}

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
pub struct MajorCadence {
    pub census: MajorCadenceCensus,
    pending: Option<PendingMajor>,
    /// The previous resolved young decision ran a major.
    prev_ran: bool,
}

impl MajorCadence {
    /// The census with the pending decision (if any) resolved against the
    /// generation's STW counters NOW, and whether that decision's major ran.
    /// Pure: the shutdown line uses it without consuming anything.
    pub fn resolved(&self, stw_collections: u64, stw_freed: u64) -> (MajorCadenceCensus, bool) {
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
    pub fn note_decision(
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
    pub fn note_decision_caused(
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

/// Torn/garbage-header gate shared by the Generational marker's `scan_object`
/// and (G1MARK-8) G1's `concurrent_mark_step`: reads the header field-by-field
/// with unaligned loads and cross-validates kind tag, element tag, gc-flag
/// universe and size arithmetic. `None` means "do not trust this header".
pub fn concurrent_mark_object_size(header: *const ObjectHeader) -> Option<usize> {
    let snapshot = ConcurrentMarkHeaderSnapshot::read(header);
    concurrent_mark_snapshot_size(&snapshot)
}

pub fn concurrent_mark_snapshot_size(snapshot: &ConcurrentMarkHeaderSnapshot) -> Option<usize> {
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
            if !snapshot.object_flags_valid() {
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

pub struct ConcurrentMarkHeaderSnapshot {
    pub class_id: u32,
    pub kind_tag: u8,
    pub element_tag: u8,
    shape: u32,
    pub gc_flags: u8,
}

impl ConcurrentMarkHeaderSnapshot {
    pub fn read(header: *const ObjectHeader) -> Self {
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
    pub fn array_length(&self) -> u32 {
        if self.kind_tag == ObjectKind::Array as u8 {
            self.shape
        } else {
            0
        }
    }

    #[inline]
    pub fn num_slots(&self) -> u32 {
        self.shape
    }

    /// A compact instance's total size, from its class's current layout (its
    /// 8-byte header has no shape word; `shape` here is its first field).
    #[inline]
    fn compact_total_size(&self) -> Option<usize> {
        self.with_compact_layout(|layout| layout.total_size as usize)
    }

    pub fn object_flags_valid(&self) -> bool {
        let known_flags = GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_COMPACT | GC_FLAG_HEADER;
        self.gc_flags & !known_flags == 0
    }

    pub fn with_compact_layout<R>(
        &self,
        f: impl FnOnce(&cratonvm_types::CompactLayout) -> R,
    ) -> Option<R> {
        let field_count = cratonvm_types::compact_field_count(self.class_id)?;
        // Borrow rather than cloning an Arc; the caller can validate the
        // extent and visit its slots against this one registry answer.
        cratonvm_types::with_class_layout(self.class_id, field_count, f)
    }
}

pub fn array_element_type_from_tag(tag: u8) -> Option<ArrayElementType> {
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



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MajorTrigger {
    NotDue,
    Requested,
    Occupancy,
    AllocationFailure,
    Suppressed,
    WouldSuppress,
}

impl MajorTrigger {
    #[inline]
    pub fn runs(self) -> bool {
        !matches!(self, MajorTrigger::NotDue | MajorTrigger::Suppressed)
    }
}
pub static WALK_DESYNC_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static SCAN_REGION_BREAK_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub const Y2O_LIVE_SEED_FLAG: &str = "CRATONVM_GEN_Y2O_LIVE_SEED";
pub const Y2O_OLD_HOLDER_CENSUS_FLAG: &str = "CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS";
pub fn y2o_live_seed_enabled() -> bool {
    true
}
pub fn y2o_old_holder_census_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on(Y2O_OLD_HOLDER_CENSUS_FLAG)
}
static PRECISE_ROOT_PROMOTE_CENSUS_HOOK: std::sync::OnceLock<fn() -> Option<String>> =
    std::sync::OnceLock::new();
pub fn install_precise_root_promote_census_hook(f: fn() -> Option<String>) {
    let _ = PRECISE_ROOT_PROMOTE_CENSUS_HOOK.set(f);
}

/// Why a `live_only` request fell back to the all-young enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Y2oFallback {
    /// The young grid walk met an anomaly (or stopped early), so the grid the
    /// conservative roots are resolved against is not proved complete.
    YoungWalkAnomaly,
    /// The old-generation walk did not cover `used()`, so the full old→young
    /// scan could miss a holder.
    OldWalkIncomplete,
    /// A candidate resolved to no young object but names a side-table or
    /// overlay owner (a zero-headed owner the grid steps over).
    UnresolvedOwnerCandidate,
}

/// The seeds, and what produced them.
#[derive(Debug, Default)]
pub struct YoungToOldSeeds {
    /// Old-generation addresses named by the seeding young objects.
    pub old_refs: Vec<usize>,
    /// Class ids of the seeding young objects (the concurrent class unload's
    /// `young_instance_loaders` input).
    pub class_ids: FxHashSet<u32>,
    /// `true` when the seeds came from the live young set.
    pub from_live_set: bool,
    /// Set when a `live_only` request fell back.
    pub fallback: Option<Y2oFallback>,
    /// Young objects enumerated.
    pub young_objects: usize,
    /// Young objects that seeded (every enumerated one on the all-young path).
    pub seeding_objects: usize,
    /// Old referents NOT seeded because their slot 0 was hidden.
    pub hidden_referents_skipped: usize,
    /// `false` when the all-young walk stepped over an unparseable stretch:
    /// its words were seeded (conservatively), but the classes of the objects
    /// inside it are NOT in [`Self::class_ids`], so a caller turning classes
    /// into live loaders must keep every loader instead. Always `true` on the
    /// live path (its grid is complete or it falls back).
    pub young_walk_complete: bool,
    /// gch h3/mj: g4og's step-0 census, only on a live-path INITIAL MARK
    /// under `CRATONVM_GEN_CONC_Y2O_OLD_HOLDER_CENSUS`. `Some(Err(_))` when
    /// the census's own (root-only) marking fell back. Never changes a seed.
    pub old_holder_census: Option<Result<Y2oOldHolderCensus, Y2oFallback>>,
}

impl ConcurrentGcState {
    /// gen r5w6/conc10 — one concurrent pause's young→old seeding under
    /// `CRATONVM_GEN_Y2O_LIVE_SEED` (see [`Y2oSeedCells`]). Relaxed adds.
    pub fn note_y2o_seeds(
        &self,
        seeds: &YoungToOldSeeds,
        hidden_young_refs: usize,
    ) {
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
        // gch h3/mj: g4og step 0 (only an initial mark under the census
        // switch fills it).
        match &seeds.old_holder_census {
            Some(Ok(census)) => {
                c.holder_census_pauses.fetch_add(1, Ordering::Relaxed);
                c.holder_census_seeded
                    .fetch_add(census.seeded as u64, Ordering::Relaxed);
                c.holder_census_via_old_holders
                    .fetch_add(census.via_old_holders as u64, Ordering::Relaxed);
                c.holder_census_via_old_holders_bytes
                    .fetch_add(census.via_old_holders_bytes as u64, Ordering::Relaxed);
            }
            Some(Err(_)) => {
                c.holder_census_pauses.fetch_add(1, Ordering::Relaxed);
                c.holder_census_fallbacks.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
    }

}

/// gch h3/mj -- the size of g4og's prize at one initial mark: of the distinct
/// old objects the live young seed named, how many NO root-reached young
/// object names (only young objects an OLD holder reaches do), and their
/// bytes (the direct referents' own sizes, not what they reach).
///
/// An UPPER bound on what the true-root initial mark (g4og item 1) frees one
/// cycle earlier: a live old holder's young child is re-found by the remark's
/// marked-holder fixed point (item 3), so only the share behind DEAD old
/// holders is the prize. If even this bound is small, g4og is not worth its
/// four-lane change.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Y2oOldHolderCensus {
    /// Distinct old refs the live seed took.
    pub seeded: usize,
    /// ... of which no young object reached from the roots (and the
    /// old-OWNER side-table seeds) names.
    pub via_old_holders: usize,
    /// The walked sizes of those `via_old_holders` objects.
    pub via_old_holders_bytes: usize,
}


use rustc_hash::FxHashSet;
