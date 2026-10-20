// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object heap, GC coordination and the VM-wide GC root tables.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.mem.<field>`.

use crate::memory::vm_heap::{G1ConfigOverrides, GcBackend, VmHeap};
use crate::runtime::lock_order::{LockLevel, OrderedPlMutex, OrderedPlRwLock};
use crate::threading::gc_barrier::GcBarrier;
use crate::types::{ObjectRef, Value};
use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// Object heap, GC coordination and the VM-wide GC root tables.
pub struct HeapRealm {
    /// Allocation and root-publishing diagnostics belong to this VM.
    pub diagnostics: AllocationDiagnostics,
    pub helper_panics: crate::jit::helper_guard::HelperPanicState,
    /// Object/array heap — generational GC with young + old gen.
    pub heap: VmHeap,

    /// fork6 GC_STRESS fix — SATB queue shared between the heap's write
    /// barrier (`satb_barrier`, attached via `enable_concurrent_gc` at
    /// construction) and every concurrent old-gen cycle's marker
    /// (`ConcurrentMarker::with_shared` in `maybe_concurrent_gc`). Without
    /// this shared instance the barrier logs nowhere and remark drains
    /// nothing — the concurrent mark had no write barrier.
    pub concurrent_satb: std::sync::Arc<cratonvm_gc::SatbQueue>,
    /// Concurrent old-gen cycle phase state, shared with every cycle's marker
    /// (`ConcurrentMarker::with_shared`) and read by the root builders
    /// (`generational_concurrent_mark_open`). The heap's `satb_barrier` gates
    /// on `concurrent_satb`, not on this (gc-common w5-e).
    pub concurrent_gc_state: std::sync::Arc<cratonvm_gc::ConcurrentGcState>,

    /// T10 — pool for reusing operand stack Vec<u64> allocations.
    pub operand_stack_pool: crate::runtime::alloc_fastpath::VecPool<u64>,

    /// T10 — pool for reusing tag Vec<u8> allocations.
    pub tag_pool: crate::runtime::alloc_fastpath::VecPool<u8>,

    /// Interned string pool: maps Rust strings to Java String ObjectRefs.
    /// Used by `ldc` string constants and `String.intern()`.
    /// T10.9.B: FxHashMap. (gcg g2/w: the T10.9.B premise -- "keys come from
    /// `ldc` constant-pool strings and internal `String.intern()` calls, not
    /// untrusted runtime input" -- does not hold: a program's own
    /// `String.intern()` of request-derived text puts untrusted keys here, and
    /// FxHash is not collision-resistant. See
    /// `docs/internal/proposals/gc/gcg-g2w-proposal-string-table-keyed-by-the-string-20261001.md`.)
    /// GC disposition: a weak root on Generational by default
    /// (`CRATONVM_GC_WEAK_STRING_TABLE`, `=0` makes it strong), a strong root on
    /// G1 and ZGC (`crate::memory::string_table`).
    pub string_pool: RwLock<FxHashMap<String, ObjectRef>>,

    /// The pool's GC disposition and `String.intern()`'s miss rule, per VM:
    /// `CRATONVM_GC_WEAK_STRING_TABLE` (weak on Generational) and
    /// `CRATONVM_STRING_INTERN_RETURNS_RECEIVER`, both default on (the weak
    /// table since gcg g6/rs, the receiver rule since gcg g4/rs; `=0` kills
    /// each), plus the sweep
    /// census. See `crate::memory::string_table` (gcg g2/w).
    pub string_table: crate::memory::string_table::StringTableState,

    /// gcg g4/rs: reference discovery at allocation (opt-in, per VM). See
    /// `crate::memory::ref_discovery`.
    pub ref_discovery: crate::memory::ref_discovery::RefDiscoveryState,

    /// Pre-allocated singleton `java.lang.OutOfMemoryError`, thrown when the
    /// heap is too full to even materialize a fresh exception object (the
    /// OOM-during-OOM case — see `runtime::exceptions::ensure_singleton_oom`,
    /// which fills this once before user `main`). Kept alive permanently by the
    /// GC root scan (`memory::roots`). `None` until pre-allocated; the OOM-throw
    /// sites fall back to their prior behaviour while it is empty.
    pub singleton_oom: RwLock<Option<ObjectRef>>,

    /// The preallocated `OutOfMemoryError`s for the messages other than `Java
    /// heap space` (which stays [`Self::singleton_oom`]): HotSpot's
    /// `Universe::out_of_memory_error_array_size` / `_metaspace`. Filled by
    /// `runtime::exceptions::ensure_singleton_oom` only under the opt-in
    /// `CRATONVM_GC_PREALLOCATED_OOME_KINDS`; empty otherwise, and every
    /// fallback then throws the singleton as before. Rooted by
    /// `memory::roots` step 8c and remapped by `memory::gc` step 6c together
    /// with the singleton. gcd d2/j (2026-09-27), item 4 of
    /// `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`.
    pub preallocated_oome: RwLock<PreallocatedOome>,

    /// B-J: permanent GC-root registry for `java.lang.invoke.VarHandle` objects.
    /// VarHandles are long-lived singletons stored in `static final` fields
    /// (e.g. `ConcurrentLinkedDeque.NEXT`) and used for lock-free CAS. They were
    /// not being traced as live, so a moving GC left their static holder slots
    /// pointing at a zeroed (all-zero header) location → `VarHandle.set` /
    /// `compareAndSet` misdispatched onto `java/lang/Object` and the heap
    /// appeared corrupt (kafka consumer-suite crashes — see B-J). Registering
    /// each VarHandle here gets it copied into the GC pointer-map, which lets
    /// the existing static-field remap fix the holder slot. Keyed by identity
    /// hash (stable across moves); values are remapped in `update_all_roots`.
    ///
    /// gc-common w29-d (`common-w28b-remaining-identity-hash-keyed-side-tables`,
    /// rank 2): a bucket per hash, never a single slot. Two live objects of
    /// one VM can share an identity hash, and a second registration used to
    /// OVERWRITE the first, unrooting it. See [`VarHandleRoots`].
    pub var_handle_roots: RwLock<VarHandleRoots>,

    /// GC barrier for stop-the-world coordination across threads.
    pub gc_barrier: GcBarrier,

    /// Reference processor for weak/soft/phantom reference tracking during GC.
    ///
    /// L7 in the global lock hierarchy — see [`crate::runtime::lock_order`].
    /// [`OrderedPlMutex`] is a drop-in for `parking_lot::Mutex` that asserts the
    /// descending acquisition order on every `.lock()`.
    pub ref_processor: OrderedPlMutex<cratonvm_gc::ReferenceProcessor>,

    /// Finalizer thread queue — objects with `finalize()` overrides are enqueued
    /// here when the GC determines they are unreachable.  The VM drains this
    /// queue and invokes each object's `finalize()` method (JLS §12.6).
    pub finalizer_thread: cratonvm_gc::reference::FinalizerThread,

    /// Cleaner thread queue — Cleaner actions are enqueued when the associated
    /// referent becomes unreachable.  The VM drains and executes them.
    pub cleaner_thread: cratonvm_gc::reference::CleanerThread,

    /// A collection at the next `maybe_gc` (an ordinary, `Allocation Failure`
    /// one). Raised by the native String path before it allocates, and by
    /// `jcmd GC.run` beside `AllocLadderState::dcmd_gc_requested`, which turns
    /// that one into the full `Diagnostic Command` collection (gci i2/oo).
    /// `System.gc()` never sets it: it collects on the calling thread.
    pub gc_requested: std::sync::atomic::AtomicBool,

    /// `-verbose:gc`, as it is NOW: seeded from `VmConfig::verbose_gc` and
    /// switched by `MemoryMXBean.setVerbose` (`NativeSystemAccess::set_verbose_gc`).
    /// The per-pause `[GC] pause:` lines read this, not the immutable launch
    /// config (gc-common w18-g).
    pub verbose_gc: std::sync::atomic::AtomicBool,

    /// A native array allocation crossed the occupancy threshold. The next
    /// native-call boundary collects after pinning and remapping its arguments.
    pub native_array_gc_requested: std::sync::atomic::AtomicBool,

    /// T19.3.G1 — number of TLAB refills across all threads since VM start.
    ///
    /// Incremented inside `tlab_alloc_object` in the interpreter each
    /// time a thread exhausts its current TLAB and requests a new
    /// buffer from the shared arena. Visible to `--verbose:gc` and
    /// used by the allocation-storm regression tests to prove that
    /// the adaptive sizer is keeping refill pressure below the
    /// Quarkus static-init baseline (~330 Hz on the old 64 KB TLAB).
    pub tlab_refill_count: std::sync::atomic::AtomicU64,

    /// T19.3.G1 — number of minor/major GC cycles completed.
    ///
    /// Incremented by the interpreter's `maybe_gc` / `maybe_gc_forced`
    /// helpers after a collection finishes. Used by the
    /// allocation-storm regression tests to assert that GC frequency
    /// stays below the 0.2 Hz target under synthetic 25 MB/s load.
    ///
    /// It is also the ONE GC-id sequence (`GC(n)`, JFR `gcId`, `[GC] pause:
    /// gc=n`), so it advances for more than collections: a G1 concurrent
    /// cycle takes an id at its initial mark on every run, a Generational
    /// cycle takes one while a JFR recording runs, and a G1 full collection
    /// reported as one (`CRATONVM_G1_FULL_GC_ONE_REPORT`) takes ONE for all
    /// its pauses (gci i12/ge; `gc_events.rs`). Read it as "at least one GC
    /// event happened", never as an exact collection count.
    pub gc_cycle_count: std::sync::atomic::AtomicU64,

    /// Sum of every collection's sealed duration, nanoseconds, as the GC event
    /// plumbing measures it (`gc_events.rs::gc_event_finish`) --
    /// backend-independent. `getCollectionTime()` reads it where the collector
    /// keeps no pause sum of its own (ZGC). gc-common w6-f
    /// (`handoff-w6f-zgc-collection-time-from-the-pause-events`).
    pub gc_pause_ns_total: std::sync::atomic::AtomicU64,

    /// Completed full collections of the `System.gc()` and metaspace doors
    /// (`GcDoor::SystemGc`, `GcDoor::MetadataThreshold`), bumped by
    /// `run_collection_pause` inside the pause, before the release. A
    /// `System.gc()` that lost its STW request reads it to coalesce with a
    /// sibling `System.gc()` that completed after the call began (HotSpot's
    /// `VM_GC_Operation::skip_operation`), instead of running another full
    /// collection. Per VM: another VM's `System.gc()` satisfies nothing here.
    /// gc-common w7-g (`common-e-small-findings` item 7).
    pub system_gc_collections: std::sync::atomic::AtomicU64,

    /// ZGC's JMX beans (`ZGC Minor/Major Cycles/Pauses`, the `ZGC Young
    /// Generation` / `ZGC Old Generation` pools) and
    /// their GC-notification queue, booked by the common GC event plumbing
    /// (`gc_events.rs`). Read through `HeapRealm::backend_gc_beans` only;
    /// unused on the other backends (G1 keeps its own in `G1State`,
    /// Generational in its heap). Here, not in the heap, because
    /// `VmHeap::Zgc` holds an `Arc<ZgcRealHeap>` the dispatcher cannot
    /// extend. gc-common w7-f.
    pub zgc_gc_beans: std::sync::Arc<cratonvm_gc::gc_metrics::BackendGcBeans>,

    /// Consecutive allocation-failure GCs that freed almost nothing (post-GC
    /// heap still ≥98% full). When this reaches the GC-overhead limit
    /// (`runtime::interpreter::gc_overhead_limit_exceeded`), the allocation
    /// paths surface a catchable `OutOfMemoryError` (the pre-allocated
    /// `singleton_oom`) instead of spinning in an O(n²) GC death-spiral on a
    /// heap that is full of live (retained) objects. Reset to 0 by any
    /// productive forced GC. Mirrors HotSpot's `UseGCOverheadLimit`.
    pub gc_unproductive_streak: std::sync::atomic::AtomicU32,

    /// T19.3.G1 — total bytes allocated across all TLAB and slow-path heap
    /// allocations since VM start. TLAB observations are coalesced per thread
    /// and flushed at 64 KiB or before a refill/wedge decision, rather than
    /// making this shared cache line bounce for every object.
    ///
    /// Sampled by diagnostics and written to `--verbose:gc` after
    /// each cycle so operators can compute allocation rate.
    pub bytes_allocated_total: std::sync::atomic::AtomicU64,

    /// Every Java thread's PUBLISHED allocation total, for this VM only: what
    /// `ThreadMXBean.getTotalThreadAllocatedBytes` sums (the reader adds the
    /// calling thread's unpublished bytes, `Tlab::vm_thread_unpublished_bytes`).
    /// Fed by each thread's TLAB retires, and by its non-TLAB allocations in
    /// 64 KiB batches, through the pointer
    /// `Tlab::attach_vm_thread_allocation_total` installs at the TLAB refill
    /// (and `ensure_vm_thread_allocation_total` at the non-TLAB sites).
    /// gc-common w6-c: it replaced the process static
    /// `tlab::PROCESS_ALLOCATED_BYTES`, so VM A's total no longer grows while
    /// A is idle and VM B allocates, and threads of different VMs no longer
    /// share its cache line.
    pub thread_allocated_total: std::sync::atomic::AtomicU64,

    /// The JIT refill wedge breaker's and the refill-time young trigger's
    /// counters (`runtime/interpreter/gc_and_alloc.rs`), per VM: another
    /// VM's refills must not re-arm this VM's `needs_gc` consult or fire its
    /// forced collection. Until gc-common w6-c these were four process
    /// statics.
    pub tlab_wedge: TlabWedgeState,

    /// What the previous allocation-failure collection left behind, for the
    /// mutator-progress half of the GC-overhead limit
    /// (`runtime::interpreter::note_gc_productivity`). Per VM.
    /// gen r4w5/thrash5 (2026-09-24).
    pub gc_progress: GcProgressMarks,

    /// The allocation-failure ladder's per-VM state: the futile-young-cycle
    /// backoff of the object doors and the `jit_alloc_oom` fail-closed
    /// counter (`runtime::interpreter::gc_and_alloc`). gcd d2/j (2026-09-27).
    pub alloc_ladder: AllocLadderState,

    /// zr z1/br: the ZGC concurrent mark start's take-over cover
    /// (`CRATONVM_ZGC_SATB_TAKEOVER_COVER`). Per VM; see
    /// [`ZgcSatbWindowState`].
    pub zgc_satb_window: ZgcSatbWindowState,

    /// gci i45/sc: this VM's TLAB sizing -- `-XX:TLABSize`, `-XX:MinTLABSize`,
    /// `-XX:±UseTLAB`, `-XX:±ResizeTLAB`, `-XX:TLABWasteTargetPercent`,
    /// `-XX:TLABRefillWasteFraction`, `-XX:TLABAllocationWeight`
    /// (`VmConfig::tlab_flags`), built once by `vm_init.rs`
    /// `gci_i45_sc_tlab_sizing` (`CRATONVM_TLAB_HOTSPOT_FLAGS`). Read per TLAB
    /// refill by `tlab_alloc_shaped_inner`. `Copy`, immutable, per VM.
    pub tlab_sizing: cratonvm_gc::tlab::TlabSizing,
}

/// See [`HeapRealm::zgc_satb_window`] and
/// `gc_and_alloc::zgc_satb_window_refuses` (zr z1/br,
/// `docs/internal/fixed-bugs/zgc-zr1br-takeover-straddles-the-satb-gate-FIXED-20261017.md`).
///
/// A ZGC mark start whose take-over froze a peer in compiled code does not
/// open its concurrent cycle: the peer may sit between a reference store's
/// SATB gate test (which read "idle") and the store, or between an inline
/// allocation's `blocked` test and its start-bit store, and it resumes after
/// the barrier is armed. The latch keeps the trigger from re-requesting a
/// take-over pause on every later occupancy poll until the next collection.
#[derive(Default)]
pub struct ZgcSatbWindowState {
    /// `collection_count() + 1` when the last refusal latched; `0` = none.
    /// The latch holds while the collection count is unchanged.
    pub refused_at: std::sync::atomic::AtomicU64,
    /// Mark starts refused by the cover since the VM started.
    pub refusals: std::sync::atomic::AtomicU64,
}

impl HeapRealm {
    /// gci i2/wr — the epoch every address-keyed memo (the native ones through
    /// `NativeContext::gc_address_epoch`, the JIT helpers' directly) keys on:
    /// it moves whenever an object may have moved or a block may have been
    /// freed for re-use. `VmHeap::address_epoch` (collections; G1 `cleanup`
    /// too) plus the Generational concurrent sweep's slice count
    /// (`ConcurrentGcState::sweep_slices_begun`, 0 on G1 / ZGC): the sweep
    /// frees old blocks between pauses and books no collection
    /// (`gci-i1wr-generational-concurrent-sweep-does-not-move-the-address-memo-epoch`).
    /// Both terms are monotonic, so the sum never repeats a value.
    #[inline]
    pub fn address_epoch(&self) -> u64 {
        self.heap
            .address_epoch()
            .wrapping_add(self.concurrent_gc_state.sweep_slices_begun())
    }
}

/// See [`HeapRealm::alloc_ladder`]. Heuristic state, `Relaxed` throughout: a
/// lost update between two racing doors costs at most one extra (or one
/// skipped) forced young cycle.
#[derive(Default)]
pub struct AllocLadderState {
    /// Consecutive allocation-door forced young cycles after which young still
    /// could not serve the object that missed (`0`: the backoff is idle). Set
    /// by `note_forced_young_cycle_outcome`, read by
    /// `futile_young_backoff_skips` (`gc_and_alloc.rs`).
    pub futile_young_streak: AtomicU32,
    /// [`HeapRealm::bytes_allocated_total`] at the last futile verdict: the
    /// backoff re-arms the forced cycle after a quantum of allocation since.
    pub futile_young_alloc_mark: std::sync::atomic::AtomicU64,
    /// Door entries whose forced young cycle the backoff skipped since the
    /// last verdict: the entry-counted re-arm, for a door whose allocation
    /// does not reach the byte counter.
    pub futile_young_skipped_since: std::sync::atomic::AtomicU64,
    /// Census: futile verdicts over the VM's life.
    pub futile_young_verdicts: std::sync::atomic::AtomicU64,
    /// Census: forced young cycles the backoff skipped over the VM's life.
    pub futile_young_skips: std::sync::atomic::AtomicU64,
    /// `jit_alloc_oom` reached with no JIT thread to publish the
    /// `OutOfMemoryError` on (item 2 of
    /// `gengc-r5w1-oom5-oom-path-review-residuals-FIXED-20260928`): the compiled
    /// caller then takes its exception exit with nothing pending. By
    /// construction unreachable; counted, and the first one per VM printed.
    pub jit_alloc_oom_without_thread: std::sync::atomic::AtomicU64,
    /// zr z3/lt: ZGC boxing-helper refusals that went on to the collect-and-
    /// retry rung (`jit::helpers::jit_box_cold_alloc`); printed on
    /// `[GC] zgc-jit-box:` (`gc_and_alloc::zgc_jit_box_line`).
    pub jit_box_collects: std::sync::atomic::AtomicU64,
    /// zr z3/lt: ... and those still refused after it, handed to the native
    /// funnel (which raises the `OutOfMemoryError`).
    pub jit_box_funnel_fallthroughs: std::sync::atomic::AtomicU64,
    /// zr z4/ab: post-collection referent restores this VM refused
    /// (`gc_and_alloc::referent_restore_refusals`); was a process static.
    /// Kept here, with the other per-VM collection-path counters, so the
    /// realm literal in `vm_init` needs no new line.
    pub referent_restore_refusals: std::sync::atomic::AtomicU64,
    /// gcd d4/j: a heap `OutOfMemoryError` was raised and the native funnel
    /// has not collected for it yet. Set at the chokepoint every heap-OOME
    /// door passes (`maybe_dump_heap_on_oom_for`); read by the native funnel,
    /// which then collects before its callback (`native_call_owes_oome_major`).
    ///
    /// gcd d5/q: one packed word, `0` = nothing owed, else the RAISING
    /// thread's key (`ThreadId + 1`) above two bits of remaining funnel
    /// payments (`gc_and_alloc::oome_debt_decision`). Only the raising thread
    /// pays -- a JDK daemon's native call used to take the debt at a random
    /// moment, before the program had dropped anything -- and a payment that
    /// left the heap still full keeps one more for the call after the drop.
    pub oome_native_debt: std::sync::atomic::AtomicU64,
    /// gce e2/o: graced attempts the latched GC-overhead exit has spent in the
    /// current latched episode (`gc_and_alloc::latched_overhead_grace`).
    /// Cleared whenever the streak resets.
    pub latched_grace_used: AtomicU32,
    /// gcf f2/o: this VM's GC-overhead limit in unproductive forced cycles
    /// (`0` = none), resolved on first use from `CRATONVM_GC_OVERHEAD_LIMIT`,
    /// `VmConfig::use_gc_overhead_limit` and
    /// `CRATONVM_GC_OVERHEAD_LIMIT_HOTSPOT`
    /// (`gc_and_alloc::gc_overhead_limit_cycles`). Per VM: it used to be a
    /// process `static OnceLock`.
    pub overhead_limit_cycles: OnceLock<u32>,
    /// gcf f3/o: the `oome_debt_key` of the thread whose native retry runs on
    /// the latched overhead exit's grace (`0` = none pending): set by
    /// `gc_and_alloc::note_graced_native_retry` at the
    /// `NativeContext::reclaim_before_alloc_retry` door, counted and cleared by
    /// that thread's next successful native allocation
    /// (`gc_and_alloc::note_native_alloc_succeeded`).
    pub graced_native_retry: std::sync::atomic::AtomicU64,
    /// gcf f4/o (`gcd-d10o-proposal-oome-ladder-census-on-every-collector`,
    /// step 2): per allocation-failure door, `[entered, allocated]` -- how
    /// often the door's ladder ran and how often it ended with the
    /// allocation (the factory door's second slot is "retry granted": the
    /// factory itself makes the retry). Every collector; printed as
    /// `[GC] oome_ladder_doors:` (`gc_and_alloc::oome_ladder_doors_line`).
    /// Indexed by `gc_and_alloc::LadderDoor`.
    pub door_census: [[std::sync::atomic::AtomicU64; 2]; 5],
    /// gcf f4/o: `CRATONVM_LAMBDA_PROXY_TLAB`, read once per VM
    /// (`invokedynamic::lambda_proxy_tlab_enabled`).
    pub lambda_proxy_tlab: OnceLock<bool>,
    /// gcg g1/o: VM-raised `OutOfMemoryError`s built with a stack trace of
    /// their own under `CRATONVM_GC_PREALLOCATED_OOME_IMMUTABLE`, up to
    /// HotSpot's `PreallocatedOutOfMemoryErrorCount` (4); after that the
    /// message's traceless default is thrown
    /// (`exceptions::hotspot_pool_spent_default`).
    pub traced_oomes: AtomicU32,
    /// gch h1/jp (g4-oo hunk 5.3, `gcg-g1o-embedded-vms-never-preallocate-the-oome`):
    /// `Vm::invoke` has tried the lazy `OutOfMemoryError` preallocation once
    /// (`exceptions::ensure_singleton_oom`), for an embedder that skips
    /// `cratonvm_embed::preallocate_out_of_memory_error`. Once per VM: a VM
    /// whose `OutOfMemoryError` cannot be built pays one attempt, not one per
    /// call. Set without an attempt by `Vm::own_the_oome_preallocation`.
    pub singleton_oom_attempted: std::sync::atomic::AtomicBool,
    /// gcg g6/oo: `CRATONVM_GC_MIRROR_AT_INIT`, read once per VM
    /// (`vm_util::mirror_at_init_enabled`): it is asked on every class
    /// initialization, where an uncached flag lookup would cost each time.
    pub mirror_at_init: OnceLock<bool>,
    /// gci i2/oo: a `jcmd GC.run` is pending (`VmDiagnosticState::trigger_gc`
    /// raises it beside `HeapRealm::gc_requested`). Taken once, by the next
    /// `maybe_gc` or JIT-guarded TLAB refill, which then runs HotSpot's
    /// `_dcmd_gc_run`: a full collection with cause `Diagnostic Command`
    /// (`gc_and_alloc::take_diagnostic_command_gc_request`). Kept apart from
    /// `gc_requested`, which the native String path also raises for an
    /// ordinary young collection.
    pub dcmd_gc_requested: std::sync::atomic::AtomicBool,
    /// gci i12/ge (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016`
    /// (b)): the G1 full collection whose pauses are being reported as ONE
    /// `Pause Full` (`CRATONVM_G1_FULL_GC_ONE_REPORT`, opt-in), or `None`.
    /// Opened and closed by `gc_events::g1_full_report_begin` / its report's
    /// `finish` around `gc_and_alloc::force_full_collection`; read by the
    /// pauses that thread initiates meanwhile. Per VM. Kept here because this
    /// struct is `Default`-constructed with the realm.
    pub g1_full_report: parking_lot::Mutex<Option<G1FullReportScope>>,
    /// gci i13/oa item 2: remarks that ran the last-ditch soft-clear rule
    /// (condemned every active soft reference), counted by
    /// `gc_and_alloc::remark_reference_rows`. Read before and after
    /// `last_ditch_reclaim`'s marking cycle: an increase proves an armed remark
    /// ran inside it (`CRATONVM_G1_OOME_SKIP_FUTILE_SOFT_RUNG`).
    pub armed_soft_remarks: std::sync::atomic::AtomicU64,
    /// gci i27/ob (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
    /// the G1 young reports predicted `Concurrent Start` whose label waits for
    /// their requester's mark-cycle drive (`gc_events::G1DeferredYoungReport`,
    /// parked by `gc_events::gc_event_finish_by`, emitted by
    /// `gc_events::g1_emit_deferred_young_report`). Empty unless `-Xlog` / JFR
    /// takes collection events on G1; at most one entry per requesting thread.
    pub g1_deferred_young: parking_lot::Mutex<Vec<G1DeferredYoungSlot>>,
    /// gci cs48 (`gci-i19ob-g1-concurrent-start-label-is-a-prediction-FIXED-20261017`):
    /// the last G1 collection pause ran as `Mixed`, so no mark-cycle drive
    /// opens a cycle until the next young-only pause (HotSpot 25's
    /// `G1Policy::decide_on_concurrent_start_pause` starts a cycle only at a
    /// young-only pause). Written inside every G1 collection pause by
    /// `gc_events::gc_event_seal` (`false` under
    /// `CRATONVM_G1_CONCURRENT_START_AT_YOUNG_PAUSE=0`); read by
    /// `gc_and_alloc::g1_start_waits_for_young_only_pause`. Per VM.
    pub g1_conc_start_waits_for_young_pause: std::sync::atomic::AtomicBool,
    /// gci i47w/xl47 (`gci-i47w-j45-g1-system-gc-prints-two-safepoint-lines-FIXED-20261016`):
    /// the OS threads whose G1 full-collection door
    /// (`gc_and_alloc::force_full_collection`) is past its own collection
    /// pause and inside its marking cycle and compaction. Every COLLECTION
    /// pause such a thread initiates there is part of the door's ONE HotSpot
    /// VM operation (`G1CollectFull`, `G1TryInitiateConcMark`,
    /// `CollectForMetadataAllocation`): no `-Xlog:safepoint` line, `gc,jni`
    /// line or `sun.rt.safepoints` count of its own
    /// (`gc_events::g1_door_folds_this_pause`). Pushed and popped by
    /// `gc_events::G1DoorSafepointFold`; empty unless such a door runs. Per VM.
    pub g1_door_safepoint_folds: parking_lot::Mutex<Vec<std::thread::ThreadId>>,
    /// zt t1/nd: the ZGC define-mirror reservation's engagement census
    /// (`NativeContext::try_reserve_class_mirror` /
    /// `adopt_reserved_class_mirror`), indexed by
    /// `vm_object::DefineMirrorCensus`: `[reserved, refused, adopted]`.
    /// `refused` counts every refusal of the fallible door, the one after the
    /// reclaim ladder's collection included. Printed as
    /// `[GC] zgc-define-mirror:` (`vm_object::zgc_define_mirror_line`). Per VM;
    /// ZGC only (the other collectors never reserve).
    pub define_mirror_census: [std::sync::atomic::AtomicU64; 3],
}

/// See [`AllocLadderState::g1_deferred_young`]: one parked report and the
/// registry thread (`ThreadId.0`) whose drive resolves it. The report's type
/// is private to `runtime::interpreter::gc_events`, hence `Any`.
pub struct G1DeferredYoungSlot {
    pub requester: u64,
    pub report: Box<dyn std::any::Any + Send>,
}

/// See [`AllocLadderState::g1_full_report`]: the open scope of one G1 full
/// collection reported as one (gci i12/ge).
#[derive(Debug)]
pub struct G1FullReportScope {
    /// The OS thread running the full collection: only the pauses IT
    /// initiates are folded into the report (another thread's allocation
    /// pause in between is its own collection).
    pub owner: std::thread::ThreadId,
    /// The report's `GC(n)` / JFR `gcId`, taken by the first pause folded in
    /// (`None`: nothing ran yet; a scope that ends so reports nothing).
    pub gc_id: Option<u64>,
    /// The folded COLLECTION pauses' summed duration, nanoseconds.
    pub pause_ns: u64,
    /// How many collection pauses were folded in.
    pub pauses: u32,
    /// The folded MARKING pauses' summed work, nanoseconds: shared with each
    /// such pause's timer, which adds to it after its release (the timer's
    /// finish has no `SharedVm` to reach this scope through).
    pub marking_ns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The longest single folded pause (collection or marking), nanoseconds:
    /// the report's JFR `longestPause` (`fetch_max` by every folded pause).
    pub longest_ns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Marking pauses (G1 initial mark, remark + cleanup) are folded in too
    /// -- until the full collection's own marking cycle has finished
    /// (`gc_events::g1_full_report_marking_done`); a cycle the compaction
    /// pause's mark drive opens afterwards is an ordinary one.
    pub marking_folded: bool,
    /// gci i25/sg (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
    /// item 16): the summed `-Xlog:gc+ref` / `gc+phases+ref` census of the
    /// reference-processing rounds the folded pauses ran (young, remark,
    /// compaction), printed by the report as ONE block under its id
    /// (`gc_events::g1_full_scope_absorb_ref_census`). `None` while no round
    /// handed one in (always, unless `-Xlog` takes the block).
    pub ref_census: Option<crate::runtime::unified_logging::RefProcCensus>,
}

/// A VM-raised `OutOfMemoryError` message with a preallocated default of its
/// own ([`HeapRealm::preallocated_oome`]). `Java heap space` is not one: it is
/// [`HeapRealm::singleton_oom`], unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomeKind {
    /// `Requested array size exceeds VM limit`
    /// (`Universe::out_of_memory_error_array_size`).
    ArraySize,
    /// `Metaspace` (`Universe::out_of_memory_error_metaspace`).
    Metaspace,
}

impl OomeKind {
    /// Every kind, in slot order.
    pub const ALL: [OomeKind; 2] = [OomeKind::ArraySize, OomeKind::Metaspace];

    /// The Java-visible detail message (HotSpot's text).
    pub fn message(self) -> &'static str {
        match self {
            OomeKind::ArraySize => crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT,
            OomeKind::Metaspace => "Metaspace",
        }
    }

    /// The kind whose message is exactly `message`, if any. A site-detailed
    /// `Java heap space (...)` and every other text answer `None`.
    pub fn for_message(message: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.message() == message)
    }

    fn slot(self) -> usize {
        match self {
            OomeKind::ArraySize => 0,
            OomeKind::Metaspace => 1,
        }
    }
}

/// See [`HeapRealm::preallocated_oome`]: one slot per [`OomeKind`].
#[derive(Default)]
pub struct PreallocatedOome {
    defaults: [Option<ObjectRef>; 2],
    /// zs s9/la: HotSpot's `_preallocated_out_of_memory_error_array` (the
    /// errors `Universe::gen_out_of_memory_error` hands out WITH a fresh
    /// trace, one each, before the traceless default), built at boot on ZGC
    /// (`CRATONVM_ZGC_OOME_TRACED_POOL`). Kept rooted after they are handed
    /// out, so their bytes never return to the free list -- as on HotSpot,
    /// whose few dead objects in a full page are never reused.
    traced: Vec<ObjectRef>,
}

impl PreallocatedOome {
    /// The preallocated default for `kind`, if one was built.
    pub fn get(&self, kind: OomeKind) -> Option<ObjectRef> {
        self.defaults[kind.slot()]
    }

    /// Install `kind`'s default.
    pub fn set(&mut self, kind: OomeKind, obj: ObjectRef) {
        self.defaults[kind.slot()] = Some(obj);
    }

    /// zs s9/la: the `index`-th traced pool error, if it was built.
    pub fn traced(&self, index: usize) -> Option<ObjectRef> {
        self.traced.get(index).copied()
    }

    /// zs s9/la: the traced pool errors built so far.
    pub fn traced_len(&self) -> usize {
        self.traced.len()
    }

    /// zs s9/la: append one traced pool error.
    pub fn push_traced(&mut self, obj: ObjectRef) {
        self.traced.push(obj);
    }

    /// Every built default (and traced pool error), for the root scan
    /// (`memory/roots.rs`, step 8c).
    pub fn values(&self) -> impl Iterator<Item = &ObjectRef> + '_ {
        self.defaults.iter().flatten().chain(self.traced.iter())
    }

    /// Every built default (and traced pool error), for the post-move remap
    /// (`memory/gc.rs`, 6c).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut ObjectRef> + '_ {
        self.defaults
            .iter_mut()
            .flatten()
            .chain(self.traced.iter_mut())
    }
}

/// See [`HeapRealm::gc_progress`]. Written by `note_gc_productivity` at the end
/// of every allocation-failure collection this VM's threads initiate, read by
/// the next one. Heuristic state: `Relaxed` throughout, and a lost update
/// between two racing forced collections costs at most one cycle's verdict.
#[derive(Default)]
pub struct GcProgressMarks {
    /// `false` until the first forced collection has set the three marks
    /// below; the progress half abstains until then.
    pub armed: std::sync::atomic::AtomicBool,
    /// [`HeapRealm::bytes_allocated_total`] after that collection.
    pub bytes_allocated_mark: std::sync::atomic::AtomicU64,
    /// [`HeapRealm::thread_allocated_total`] after that collection.
    pub thread_allocated_mark: std::sync::atomic::AtomicU64,
    /// The live-bytes estimate that collection left (young live + old used).
    pub live_after_mark: std::sync::atomic::AtomicU64,
    /// gci i2/oo (`CRATONVM_GC_OVERHEAD_SEALED_WINDOW`, opt-in): the pause key
    /// (`gc_generation + 1`, sampled inside the pause) of the newest
    /// collection whose verdict was taken, `0` before the first. Held across
    /// one verdict -- the window marks above and the streak -- so two threads'
    /// verdicts never interleave, and an older pause's verdict that arrives
    /// after a newer one is dropped instead of overwriting it
    /// (`gc_and_alloc::note_gc_productivity_for_pause`).
    pub verdict: parking_lot::Mutex<u64>,
}

/// The permanent-root registry behind `NativeContext::register_var_handle_root`
/// / `read_var_handle_root` ([`HeapRealm::var_handle_roots`]).
///
/// gc-common w29-d. The registry used to be `FxHashMap<i32, ObjectRef>` keyed
/// by identity hash, and `register` was a plain `insert`. An identity hash is
/// 32 bits minted from a wrapping per-heap counter, so two LIVE objects of one
/// VM can share one; the second registration then replaced the first. The first
/// object lost its permanent root (a native still caching it read a reclaimed
/// slot after the next collection) and every read of its key answered the
/// second object.
///
/// Now each hash owns a bucket:
///
/// * [`insert`](Self::insert) APPENDS, and is idempotent per object (compared
///   by address, which the collector keeps current in every entry). Nothing is
///   ever displaced, so every registered object stays rooted.
/// * [`get`](Self::get) answers the FIRST object registered under the hash:
///   the answer a key gave before a collider arrived never changes. A collider
///   registered later cannot be told apart by its hash alone, so its reads
///   answer the first object. That residual is why consumers are moving off
///   `read_var_handle_root` to JNI global handles (`add_global_root` /
///   `resolve_global_root`), which are unique per object.
///
/// The method names match the `HashMap` calls the older call sites used
/// (`insert`, `get`), so those still compile.
#[derive(Default)]
pub struct VarHandleRoots {
    buckets: FxHashMap<i32, smallvec::SmallVec<[ObjectRef; 1]>>,
    /// Objects held across every bucket (for the root-count metrics and tests).
    len: usize,
}

impl VarHandleRoots {
    /// Root `obj` under `key` unless this very object is already filed there.
    /// Returns `true` when a new row was added.
    pub fn insert(&mut self, key: i32, obj: ObjectRef) -> bool {
        let bucket = self.buckets.entry(key).or_default();
        if bucket.iter().any(|held| *held == obj) {
            return false;
        }
        bucket.push(obj);
        self.len += 1;
        true
    }

    /// The first object registered under `key` (see the type doc).
    pub fn get(&self, key: &i32) -> Option<&ObjectRef> {
        self.buckets.get(key).and_then(|bucket| bucket.first())
    }

    /// Every object registered under `key`, oldest first.
    pub fn bucket(&self, key: i32) -> &[ObjectRef] {
        self.buckets.get(&key).map(|b| b.as_slice()).unwrap_or(&[])
    }

    /// Number of rooted objects (not buckets).
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every rooted object, for the root scan (`memory/roots.rs`, step 8b).
    pub fn values(&self) -> impl Iterator<Item = &ObjectRef> + '_ {
        self.buckets.values().flat_map(|bucket| bucket.iter())
    }

    /// Every rooted object, for the post-move remap (`memory/gc.rs`, 6b).
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut ObjectRef> + '_ {
        self.buckets
            .values_mut()
            .flat_map(|bucket| bucket.iter_mut())
    }
}

/// See [`HeapRealm::tlab_wedge`]. The field names are the old statics' names,
/// lower-cased (`TLAB_GATE_CONSECUTIVE_FAILS` -> `gate_consecutive_fails`,
/// and so on). All four are rate limiters read and written with `Relaxed`
/// ordering, exactly as the statics were.
#[derive(Default)]
pub struct TlabWedgeState {
    /// Consecutive JIT TLAB-refill gate failures, counted by
    /// `tlab_refill_wedge_break` and reset by every successful refill (never
    /// by a gate pass: see the note in `tlab_alloc_shaped_inner`).
    pub gate_consecutive_fails: std::sync::atomic::AtomicU64,
    /// `bytes_allocated_total` at the last forced wedge break; a second break
    /// re-arms only after `WEDGE_REARM_BYTES` of further allocation. 0 means
    /// no break has fired yet.
    pub last_break_alloc_total: std::sync::atomic::AtomicU64,
    /// Slow-path entries since the last refill-time `needs_gc()` fire (the
    /// crumb-treadmill fix). Entry-counted, not byte-counted: the degraded
    /// modes this guards (per-object allocation, crumb-sized mini-TLABs)
    /// enter the slow path orders of magnitude more often than healthy TLAB
    /// flow, so the counter accelerates exactly when the wedge deepens, and a
    /// bytes-based stamp would freeze (the per-object path does not bump
    /// `bytes_allocated_total`).
    pub slowpath_entries_since_gc: std::sync::atomic::AtomicU64,
    /// Bytes handed out by SUCCESSFUL TLAB refills since the last refill-time
    /// `needs_gc()` fire: the healthy-path re-arm metric, OR-ed with the entry
    /// count above. The history (`gen-gc-minor-pause-20260902`: the trigger
    /// was consulted zero times per cycle on healthy JIT flow) is on
    /// `NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES` in `gc_and_alloc.rs`.
    pub refill_bytes_since_gc: std::sync::atomic::AtomicU64,
    /// zr z3/ja: collection decisions that carried a stamp
    /// (`gc_and_alloc::zgc_gc_request_stamp`), and the requests among them
    /// dropped because a collection completed after the decision
    /// (`CRATONVM_ZGC_DROP_STALE_GC_REQUESTS`). Diagnostic counters, printed by
    /// `zgc_stale_gc_requests_line`. gci i48/zl2: on every collector since the
    /// switch was extended to Generational and G1 (the `zgc_` names are kept).
    pub zgc_stamped_gc_requests: std::sync::atomic::AtomicU64,
    pub zgc_stale_gc_requests_dropped: std::sync::atomic::AtomicU64,
    /// zr s1/ca: the opt-in `jit_newarray` headroom arm's ZGC rate-limit
    /// skips (`gc_and_alloc::zgc_jit_newarray_headroom_rate_limited`).
    pub zgc_newarray_headroom_rate_skips: std::sync::atomic::AtomicU64,
    /// zt t4/ca2: requests the opt-in stamped JIT array failure door dropped
    /// (`gc_and_alloc::maybe_gc_forced_refusal_stamped_pub_at`,
    /// `CRATONVM_ZGC_JIT_FAILURE_DOOR_STAMPED`); also counted in
    /// `zgc_stale_gc_requests_dropped`.
    pub zgc_failure_door_dropped: std::sync::atomic::AtomicU64,
    /// zr z4/rv4: collections SEALED (`run_collection_pause`, bumped with the
    /// world still stopped, before `complete_gc` releases it) -- the epoch a
    /// stale-request stamp is taken against. `gc_cycle_count` is advanced by
    /// `gc_event_finish` AFTER the release, so a mutator released by a pause
    /// could decide on the post-collection heap, stamp the old count, and see
    /// its request dropped when the finishing thread bumped it. The shared
    /// pause door bumps it for every collector's collection (gci i48/zl2).
    pub collections_sealed: std::sync::atomic::AtomicU64,
    /// zr z5/cr: the ZGC refusal census and the futile-refusal rule's state
    /// (`gc_and_alloc::zgc_refusal_*`). Per VM, diagnostic plus one stamp.
    pub zgc_refusal: ZgcRefusalCensus,
    /// gci i28/tl: the `-Xlog:gc+tlab=debug` `TLAB totals` line's per-VM
    /// half -- each live thread's counters at the previous collection and the
    /// line this collection computed at its start
    /// (`unified_logging::stash_tlab_totals` / `log_tlab_totals`). Touched
    /// only while that tag is on.
    pub tlab_totals: cratonvm_gc::tlab::TlabTotalsState,
}

/// zr z5/cr: forced collections at ZGC's allocation-REFUSAL doors (an
/// allocation failed and its ladder collects), split by how much the mutators
/// allocated since the previous collection (`ZgcRealHeap::bytes_allocated_since_cycle`,
/// which charges a carved TLAB chunk in full, so it never under-reads), the
/// refusals whose forced collection the futile-refusal rule skipped
/// (`CRATONVM_ZGC_REFUSAL_SKIP_FUTILE`), and the opt-in capacity refill gate's
/// engagement (`CRATONVM_ZGC_REFILL_CAPACITY_GATE`). Printed by
/// `gc_and_alloc::zgc_refusal_line` (`[GC] zgc-refusal:`).
#[derive(Default)]
pub struct ZgcRefusalCensus {
    /// Refusal collections run (or joined), every refusal door incl. the
    /// clear-soft rung.
    pub collections: std::sync::atomic::AtomicU64,
    /// Refusal forced collections skipped as futile (the ladder went straight
    /// to its clear-soft rung).
    pub skipped_futile: std::sync::atomic::AtomicU64,
    /// Windows (bytes allocated since the previous collection) of the refusal
    /// collections: < 4 KiB, < 64 KiB, < 1 MiB, >= 1 MiB.
    pub window_lt4k: std::sync::atomic::AtomicU64,
    pub window_lt64k: std::sync::atomic::AtomicU64,
    pub window_lt1m: std::sync::atomic::AtomicU64,
    pub window_ge1m: std::sync::atomic::AtomicU64,
    /// Sum of those windows, bytes.
    pub window_bytes: std::sync::atomic::AtomicU64,
    /// Refusal collections whose previous collection was itself a refusal
    /// collection (no occupancy or System.gc() cycle in between): the crawl.
    pub after_refusal: std::sync::atomic::AtomicU64,
    /// Longest run of consecutive refusal collections with no other
    /// collection in between, and the current run.
    pub streak_max: std::sync::atomic::AtomicU64,
    pub streak_now: std::sync::atomic::AtomicU64,
    /// `collections_sealed` right after the latest refusal collection (0 =
    /// none yet), for `after_refusal` / the streak.
    pub last_seal: std::sync::atomic::AtomicU64,
    /// A skipped refusal owes its ladder's clear-soft rung one collection
    /// (paid even with no soft reference active); lowered by that rung or by
    /// the next refusal collection.
    pub soft_rung_owed: std::sync::atomic::AtomicBool,
    /// Owed collections the clear-soft rung paid with no soft reference to
    /// clear.
    pub owed_paid: std::sync::atomic::AtomicU64,
    /// Native-boundary reliefs on the hard-failure latch alone that the
    /// futile rule skipped (`vm-exec-pressure`).
    pub relief_skipped: std::sync::atomic::AtomicU64,
    /// Native factory refusals whose clear-soft rung was skipped because the
    /// forced collection had made room for the refused request
    /// (`CRATONVM_ZGC_NATIVE_ROOM_SKIPS_SOFT`).
    pub soft_skipped_room: std::sync::atomic::AtomicU64,
    /// zr z8/nr: the thread (`oome_debt_key`) whose native room skip has not
    /// yet been followed by its retry's outcome: `0` = none. One slot; a skip
    /// is taken only when the slot is free, so every live skip is tracked
    /// (`CRATONVM_ZGC_NATIVE_ROOM_SKIP_REPAY`).
    pub soft_skip_pending: std::sync::atomic::AtomicU64,
    /// zr z8/nr: skipped clear-soft rungs paid after the skip's retry failed.
    pub soft_skip_repaid: std::sync::atomic::AtomicU64,
    /// zr z8/nr: heap `OutOfMemoryError`s raised by a thread whose room skip
    /// was still pending (the rung was never paid): the residual this page's
    /// fix closes for the callers that repay.
    pub soft_skip_unpaid: std::sync::atomic::AtomicU64,
    /// zr z11/nd: re-runs of a restartable native after its allocation
    /// refused and a ladder rung collected (`CRATONVM_ZGC_NATIVE_OOM_REPLAY`),
    /// and how many of them returned (no second refusal).
    pub native_replays: std::sync::atomic::AtomicU64,
    pub native_replay_served: std::sync::atomic::AtomicU64,
    /// Opt-in capacity gate: refills it let through that the occupancy gate
    /// would have refused, and how many of those carved a chunk.
    pub gate_passed: std::sync::atomic::AtomicU64,
    pub gate_carved: std::sync::atomic::AtomicU64,
    /// zr zs s1/tp: `CRATONVM_ZGC_REFILL_CAPACITY_GATE`, seeded on first use
    /// per VM: `0` = not read yet, `1` = off, `2` = on. The gate is asked on
    /// every refill the occupancy gate refuses, and it read the variable each
    /// time -- on the default (off) path too.
    pub capacity_gate_seed: std::sync::atomic::AtomicU8,
    /// zs s6/no: refusal collections that asked for every soft reference to
    /// be cleared, HotSpot ZGC's allocation-stall rule (opt-in
    /// `CRATONVM_ZGC_STALL_CLEARS_SOFT`; `stall_soft_clears=`).
    pub stall_soft_clears: std::sync::atomic::AtomicU64,
    /// zs s9/no2: allocation-driven collections on a nearly full heap that
    /// asked every soft reference cleared (opt-in
    /// `CRATONVM_ZGC_NEAR_FULL_SOFT_CLEAR_PERMILLE`; `near_full_soft_clears=`).
    pub near_full_soft_clears: std::sync::atomic::AtomicU64,
    /// zt t4/ch: compiled refills the occupancy gate refused on ZGC (the
    /// gate is `!needs_gc()` there), i.e. allocations sent to the per-object
    /// locked path while a collection was due and not yet taken
    /// (`gate_refused=`). Diagnostic, every run.
    pub gate_refused: std::sync::atomic::AtomicU64,
    /// zt t4/ch (opt-in `CRATONVM_ZGC_REFILL_GATE_COLLECTS`): refused refills
    /// that took the due collection at once (`gate_collects=`), and those whose
    /// request was dropped as stale because another collection had sealed
    /// since the verdict (`gate_collects_dropped=`).
    pub gate_collects: std::sync::atomic::AtomicU64,
    pub gate_collects_dropped: std::sync::atomic::AtomicU64,
    /// zt t6/rv3 (`zgc-zt4x3-*` item 3): refused refills whose request ran
    /// no pause of its own (`Some(false)`: the thread took part in another
    /// thread's pause, or the futile rule skipped it), counted apart from
    /// `gate_collects` (`gate_collects_joined=`).
    pub gate_collects_joined: std::sync::atomic::AtomicU64,
    /// zt t4/ch: `CRATONVM_ZGC_REFILL_GATE_COLLECTS`, seeded on first use per
    /// VM: `0` = not read yet, `1` = off, `2` = on.
    pub gate_collects_seed: std::sync::atomic::AtomicU8,
}

#[cfg(test)]
mod gcd_d2j_preallocated_oome_tests {
    use super::{OomeKind, PreallocatedOome};
    use crate::types::ObjectRef;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// Each kind answers exactly HotSpot's message; `Java heap space` (with
    /// or without a site detail) is the singleton's, not a kind.
    #[test]
    fn a_kind_is_found_by_its_exact_message_only() {
        for kind in OomeKind::ALL {
            assert_eq!(OomeKind::for_message(kind.message()), Some(kind));
        }
        assert_eq!(
            OomeKind::for_message("Requested array size exceeds VM limit"),
            Some(OomeKind::ArraySize)
        );
        assert_eq!(
            OomeKind::for_message("Metaspace"),
            Some(OomeKind::Metaspace)
        );
        for other in [
            "Java heap space",
            "Java heap space (alloc_array length 8)",
            "",
            "metaspace",
        ] {
            assert_eq!(OomeKind::for_message(other), None, "{other:?}");
        }
    }

    /// One slot per kind; the root scan and the remap see every built one.
    #[test]
    fn every_built_default_is_scanned_and_remapped() {
        let mut table = PreallocatedOome::default();
        assert!(table.values().next().is_none());
        assert_eq!(table.get(OomeKind::Metaspace), None);
        table.set(OomeKind::Metaspace, fake(0x2000));
        assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
        assert_eq!(table.get(OomeKind::ArraySize), None);
        table.set(OomeKind::ArraySize, fake(0x1000));
        assert_eq!(table.values().count(), 2);
        for slot in table.values_mut() {
            if *slot == fake(0x1000) {
                *slot = fake(0x4_0000_1000);
            }
        }
        assert_eq!(table.get(OomeKind::ArraySize), Some(fake(0x4_0000_1000)));
        assert_eq!(table.get(OomeKind::Metaspace), Some(fake(0x2000)));
    }
}

#[cfg(test)]
mod var_handle_roots_tests {
    use super::VarHandleRoots;
    use crate::types::ObjectRef;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: non-null, 8-byte aligned, never dereferenced (lesson o).
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// gc-common w29-d: two live objects with ONE identity hash. The second
    /// registration used to overwrite the first (unrooting it); both must stay
    /// rooted and the key's answer must stay the first object.
    #[test]
    fn a_same_hash_second_registration_keeps_the_first_rooted() {
        let mut roots = VarHandleRoots::default();
        let a = fake(0x1_0000_1000);
        let b = fake(0x2_0000_1000);
        let key = 0x1000;
        assert!(roots.insert(key, a));
        assert!(roots.insert(key, b));
        assert_eq!(roots.len(), 2);
        assert_eq!(
            roots.get(&key).copied(),
            Some(a),
            "first registrant keeps the key"
        );
        assert_eq!(roots.bucket(key), &[a, b]);
        let scanned: Vec<ObjectRef> = roots.values().copied().collect();
        assert!(
            scanned.contains(&a) && scanned.contains(&b),
            "both are roots"
        );
    }

    #[test]
    fn re_registering_one_object_is_idempotent() {
        let mut roots = VarHandleRoots::default();
        let a = fake(0x3000);
        assert!(roots.insert(7, a));
        assert!(!roots.insert(7, a));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.bucket(7), &[a]);
    }

    /// The remap walks every bucket member, and a re-registration with the
    /// post-move address still dedups against the remapped row.
    #[test]
    fn remap_reaches_every_bucket_member() {
        let mut roots = VarHandleRoots::default();
        let (a, b) = (fake(0x4000), fake(0x5000));
        roots.insert(9, a);
        roots.insert(9, b);
        let (a2, b2) = (fake(0x4_0000_4000), fake(0x4_0000_5000));
        for slot in roots.values_mut() {
            if *slot == a {
                *slot = a2;
            } else if *slot == b {
                *slot = b2;
            }
        }
        assert_eq!(roots.bucket(9), &[a2, b2]);
        assert!(
            !roots.insert(9, a2),
            "the moved object is still the same row"
        );
        assert_eq!(roots.len(), 2);
        assert!(roots.get(&10).is_none());
        assert!(!roots.is_empty());
    }
}

/// Counters and frame-trace demand isolated from other VMs in the process.
#[derive(Default)]
pub struct AllocationDiagnostics {
    pub frame_trace_wanted: std::sync::atomic::AtomicU32,
    pub rootsnap_calls: std::sync::atomic::AtomicU64,
    pub rootsnap_nanos: std::sync::atomic::AtomicU64,
    pub rootsnap_frames: std::sync::atomic::AtomicU64,
    pub rootsnap_cached_calls: std::sync::atomic::AtomicU64,
    pub rootsnap_reused_frames: std::sync::atomic::AtomicU64,
    pub rootsnap_reused_roots: std::sync::atomic::AtomicU64,
    pub rootsnap_verified: std::sync::atomic::AtomicU64,
    pub rootsnap_misses: std::sync::atomic::AtomicU64,
    pub rootsnap_miss_calls: std::sync::atomic::AtomicU64,
    pub tlab_compact_objects: std::sync::atomic::AtomicU64,
    pub tlab_legacy_objects: std::sync::atomic::AtomicU64,
    pub tlab_compact_bytes_saved: std::sync::atomic::AtomicU64,
}
