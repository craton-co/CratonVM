//! zt t21/zc2: the sweep out of the pause (`CRATONVM_ZGC_CONCURRENT_SWEEP`,
//! **opt-in**; on under the concurrent profile `CRATONVM_ZGC_CONCURRENT=1`,
//! `=0` kills it there). Stage 1d of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # The split
//!
//! [`ZgcRealHeap::collect_garbage`] is one stop-the-world pause that runs every
//! phase in order. Under this switch a cycle that qualifies
//! ([`ZgcRealHeap::concurrent_sweep_refusal`]) stops the pause at the sweep:
//!
//! 1. **In the pause** (HotSpot's Pause Mark End): everything up to the sweep,
//!    unchanged -- the mark (or the concurrent mark's close), finalizer
//!    resurrection, the heap-side reference pass -- then
//!    [`ZgcRealHeap::defer_sweep_and_close_pause`]: the low free list is TAKEN
//!    (the complement recomputes every byte of it), the low cursor is recorded,
//!    the object-start registry is pruned against the final mark bits (one
//!    `fetch_and` per 512 arena bytes, the bulk prune the bitmap sweep already
//!    does), the monitor table is pruned from the registry key probe, and the
//!    pause-side bookkeeping that needs no sweep total runs (latches, the
//!    trigger mask, the cycle number). The sweep's inputs -- the cycle's
//!    registry snapshot and its configuration -- are parked in
//!    [`ZConcSweepState::job`].
//! 2. **After the release** (a concurrent phase, HotSpot's Concurrent Mark
//!    Free / Concurrent Process Non-Strong References slot): the complement
//!    walk over the frozen mark bitmap ([`ZgcRealHeap::sweep_bitmap_complement`]),
//!    the install of its spans under the arena lock, and the **after-sweep
//!    hook** -- every consumer of the sweep's totals that used to sit inside
//!    the pause: the `allocated` gauge, the re-arm floor, the reclaimed-nothing
//!    latch, the fragmentation sample, the adaptive window, the pause-target
//!    budget, the metrics recorder, the mark-bit clear, and the `[GC]
//!    zgc-real:` / `zgc-pause:` / `zgc-reclaim:` lines (HotSpot prints its
//!    cycle summary at the cycle's end, after the concurrent phases).
//!    The runner is the collecting thread once the world is released
//!    (`run_collection_pause`), before the VM prints the `-Xlog:gc` line and
//!    emits the JFR / JMX events, so those report the swept heap.
//!
//! The slide still runs inside the pause, on the cycles that do not defer
//! (concurrent relocation is stage 3 and needs the colored load barrier).
//!
//! # Invariants (each one is a test below or a pin in the VM)
//!
//! * **Nothing is freed while reachable.** The walk reads the mark bits the
//!   pause finished (resurrected finalizables and kept soft referents
//!   included); nothing sets a mark bit between the pause and the install,
//!   because no cycle can open while a sweep is pending (`start_concurrent_mark`
//!   and `collect_garbage` both finish it first; `allocate_black_if_marking`
//!   and the load barrier's mark arm are gated on `mark_active`).
//! * **Allocation never hands out memory the sweep has not proven free.** The
//!   pause takes the low free list, so a window allocation bumps above the
//!   pause's low cursor or takes a large-object hole that was free BEFORE the
//!   pause; the dead objects of this cycle are on no list until the install.
//!   An allocation that cannot be served finishes the sweep and retries
//!   (`alloc_raw`), the allocation-stall shape.
//! * **The registry is exact from the pause on.** The prune runs in the pause,
//!   so no heap walker, conservative scan or `is_object_address` ever sees a
//!   dead base after the release; a window allocation registers above the
//!   cursor and is never touched by the walk (the walk reads the pause's
//!   snapshot). Unsizable dead large objects are re-registered at the install,
//!   as the bitmap sweep does.
//! * **Weak referents, finalizers, JNI weak globals, class unloading** are
//!   decided in the pause exactly as before (the VM's reference round,
//!   `sweep_weak_global_refs_with` and the reconcile watch read the same mark
//!   bits and the pruned registry); the deferred half frees memory only.
//! * **`System.gc()` returns after the whole cycle.** The door that collected
//!   runs the sweep before it returns, and every other `System.gc()` caller
//!   waits for a pending one (`force_full_collection`).
//! * **The runner runs no Java and waits for no safepoint**: it takes the job
//!   lock, the arena lock and the heap's own counters, nothing a stopped
//!   thread can hold. A pause requested meanwhile waits for it, as it waits for
//!   any thread in VM code.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::{
    director, ZCollectionCpuTotals, ZCycleLaps, ZObjectStartsSnapshot, ZPhaseClock, ZSweepCfg,
    ZSweepShard, ZgcRealHeap,
};
use crate::collector::MonitorCleanup;
use crate::gc::{GcResult, GcStats};

/// `CRATONVM_ZGC_CONCURRENT_SWEEP` (zt t21/zc2, **opt-in**; on under
/// `CRATONVM_ZGC_CONCURRENT=1`, whose `=0` kills it there): a qualifying
/// whole-heap cycle leaves its sweep out of the pause (module doc). Read once
/// per heap, at construction ([`ZConcSweepState::new`]).
pub(crate) fn zgc_concurrent_sweep_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_CONCURRENT_SWEEP") {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => super::zgc_concurrent_profile_enabled(),
    }
}

/// Deferred cycles in a row while the slide is enabled, before one cycle
/// sweeps in the pause so the selective slide gate
/// (`CRATONVM_ZGC_SELECTIVE_RELOCATE`) can look at the heap again. A deferred
/// cycle never slides (the slide reads the post-sweep registry and the
/// sweep's size hints); without this bound a heap that defers every cycle
/// would never compact.
pub(crate) const ZGC_CSWEEP_MAX_DEFERRED_RUN: usize = 4;

/// How long an allocation that found a sweep running waits for it before it
/// fails as before (the VM then collects, and the collection finishes the
/// sweep first). A bound, not a target.
const ZGC_CSWEEP_ALLOC_WAIT_MS: u64 = 2_000;

/// Why a cycle swept in the pause under the switch (`csw_last_refusal=`).
pub(crate) mod refusal {
    /// No runner was armed: the collection was not run by
    /// `run_collection_pause` (a test, an embedder's direct call).
    pub(crate) const NO_RUNNER: u8 = 1;
    /// Generational mode or a young cycle (the young complement keeps its
    /// in-pause arm; out of scope).
    pub(crate) const GENERATIONAL: u8 = 2;
    /// The bitmap complement cannot run: no side mark bits, the hash registry,
    /// the bitmap sweep switched off, or a per-object sweep switch asked for.
    pub(crate) const NOT_BITMAP: u8 = 3;
    /// The monitor cleanup wants the full dead list (no key probe).
    pub(crate) const WANTS_DEAD: u8 = 4;
    /// A frozen peer's published TLAB tail, an incomplete TLAB retire, or a
    /// non-empty off-grid spill set: the walk would need state the window
    /// does not keep.
    pub(crate) const TAILS: u8 = 5;
    /// An allocation failed or the arena is short of headroom: the space is
    /// needed now.
    pub(crate) const URGENT: u8 = 6;
    /// [`super::ZGC_CSWEEP_MAX_DEFERRED_RUN`] deferred cycles in a row with the
    /// slide enabled: this one sweeps in the pause and may slide.
    pub(crate) const SLIDE_DUE: u8 = 7;
    /// A diagnostic that needs the in-pause sweep (`CRATONVM_DBG_ZGC_CORPSE`,
    /// `CRATONVM_ZGC_LAZY_TLAB_STARTS`).
    pub(crate) const DIAGNOSTIC: u8 = 8;
}

/// Who ran a deferred sweep (`csw_by_*=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZSweepRunner {
    /// The collecting thread, right after it released the world.
    Initiator,
    /// An allocation that could not be served without the swept space.
    Allocation,
    /// The next collection, before it touches the arena.
    CycleStart,
    /// The next concurrent mark start, before it reuses the mark bits.
    MarkStart,
    /// A `System.gc()` caller that took part in another thread's pause.
    Waiter,
    /// zt t23/x19 (page item 28d): the VM's exit, before it prints the heap
    /// it ends with ([`ZgcRealHeap::finish_pending_sweep_at_exit`]).
    Exit,
}

impl ZSweepRunner {
    fn token(self) -> &'static str {
        match self {
            ZSweepRunner::Initiator => "initiator",
            ZSweepRunner::Allocation => "allocation",
            ZSweepRunner::CycleStart => "cycle-start",
            ZSweepRunner::MarkStart => "mark-start",
            ZSweepRunner::Waiter => "waiter",
            ZSweepRunner::Exit => "exit",
        }
    }
}

/// `CRATONVM_ZGC_EXIT_FINISHES_SWEEP` (zt t23/x19, page item 28d; default ON,
/// `=0` kill; acts only when a deferred sweep is pending, i.e. under
/// `CRATONVM_ZGC_CONCURRENT_SWEEP`): the VM's exit finishes a pending sweep
/// (or waits for the thread running it) before it reads the heap it reports
/// -- the `-Xlog:gc+exit` heap lines, the `[GC]` summary, the end-of-run
/// metrics row. Read once per exit, only when a sweep is pending.
fn zgc_exit_finishes_sweep_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_EXIT_FINISHES_SWEEP")
}

/// zt t32 (R6, 6b): `spans` (`(offset, len)`, any order) minus every
/// `[lo, hi)` of `holes` (ascending, disjoint); the pieces left, ascending.
pub(super) fn clip_spans_outside(
    spans: &[(usize, usize)],
    holes: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    let mut sorted: Vec<(usize, usize)> = spans.to_vec();
    sorted.sort_unstable();
    let mut out = Vec::with_capacity(sorted.len());
    for (off, len) in sorted {
        let (mut lo, hi) = (off, off + len);
        let first = holes.partition_point(|&(_, h_hi)| h_hi <= lo);
        for &(h_lo, h_hi) in &holes[first..] {
            if h_lo >= hi {
                break;
            }
            if h_lo > lo {
                out.push((lo, h_lo - lo));
            }
            lo = lo.max(h_hi);
            if lo >= hi {
                break;
            }
        }
        if lo < hi {
            out.push((lo, hi - lo));
        }
    }
    out
}

/// One cycle's deferred sweep: the walk's inputs and the after-sweep hook's
/// context, parked by the pause.
pub(super) struct ZDeferredSweep {
    registered: ZObjectStartsSnapshot,
    cfg: ZSweepCfg,
    workers: usize,
    registered_count: usize,
    /// The true low bump cursor (arena offset) when the pause took the list.
    low_cursor_at_pause: usize,
    /// Bytes on the low free list the pause took (the complement lists them
    /// again; `bytes_freed` is what the cycle added).
    low_list_taken: usize,
    /// `allocated` when the pause ended: the hook moves the gauge by
    /// `allocated_at_pause - republished`, keeping the window's allocations.
    allocated_at_pause: usize,
    /// `recharge_published_tails` of the pause.
    recharged: usize,
    cycle: usize,
    laps: ZCycleLaps,
    mark_kind: &'static str,
    bytes_in_use_before: usize,
    trigger_mask: usize,
    /// The pause's wall time, when its clock ran.
    pause_ns: Option<u64>,
    /// When the pause ended (the sweep's wait starts here).
    released_at: std::time::Instant,
    /// zt t32 (R6, 6b): the extents of the regions this pause planned to
    /// relocate (arena offsets, ascending): the install lists no free span
    /// inside them (the release frees them whole). Empty with no plan.
    crl_extents: Vec<(usize, usize)>,
}

/// The per-heap state of the concurrent sweep (inside the boxed
/// `ZgcCounters`, so `ZgcRealHeap`'s size ratchet is untouched).
pub(crate) struct ZConcSweepState {
    /// `CRATONVM_ZGC_CONCURRENT_SWEEP` on this heap (construction seed).
    enabled: AtomicBool,
    /// The VM's collecting door will run a deferred sweep after its release
    /// (`run_collection_pause`); taken by the collection.
    runner_armed: AtomicBool,
    /// zt t22/x18: deferred cycles whose collecting door has not yet asked
    /// [`ZgcRealHeap::run_pending_sweep_after_release`]. Counted at the
    /// deferral and spent by that door, WHOEVER ran the sweep: an allocation
    /// (the VM's own post-collection allocations inside the pause included),
    /// a `System.gc()` waiter or the next cycle may have run it first, and the
    /// door still owes the swept figures to its events and its productivity
    /// verdict.
    initiator_owed: AtomicUsize,
    /// A deferred sweep has not installed yet. `needs_gc` answers no while it
    /// is set: the cycle it belongs to has not ended.
    pending: AtomicBool,
    /// The deferred walk is running (the oversize-array screen refuses: its
    /// free-list and skip-region checks describe the pause, not the window).
    walking: AtomicBool,
    /// The parked sweep. Held by the runner for the whole sweep, so a second
    /// runner waits and then finds it gone.
    job: Mutex<Option<Box<ZDeferredSweep>>>,
    /// Deferred cycles in a row while the slide was enabled.
    deferred_run: AtomicUsize,
    /// `republished` of the last installed deferred sweep (the live bytes the
    /// cycle left, without the window's allocations).
    last_live: AtomicUsize,
    /// Engagement, cumulative.
    deferred: AtomicUsize,
    refused: AtomicUsize,
    last_refusal: AtomicU8,
    by_initiator: AtomicUsize,
    by_allocation: AtomicUsize,
    by_cycle_start: AtomicUsize,
    by_mark_start: AtomicUsize,
    by_waiter: AtomicUsize,
    /// zt t23/x19: sweeps the VM's exit ran ([`ZSweepRunner::Exit`]).
    by_exit: AtomicUsize,
    /// Allocations that found the swept space missing and retried after the
    /// sweep finished (the allocation-stall count of this mode).
    alloc_retries: AtomicUsize,
    /// Microseconds, cumulative: the deferred cycles' pauses, the walks, the
    /// installs + hooks, and the time from release to the walk's start.
    pause_us: AtomicU64,
    walk_us: AtomicU64,
    install_us: AtomicU64,
    wait_us: AtomicU64,
}

/// The `[GC] zgc-concurrent-sweep:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZConcSweepReport {
    pub enabled: bool,
    pub deferred: usize,
    pub refused: usize,
    pub last_refusal: u8,
    pub by_initiator: usize,
    pub by_allocation: usize,
    pub by_cycle_start: usize,
    pub by_mark_start: usize,
    pub by_waiter: usize,
    pub by_exit: usize,
    pub alloc_retries: usize,
    pub pause_us: u64,
    pub walk_us: u64,
    pub install_us: u64,
    pub wait_us: u64,
    pub pending: bool,
}

impl ZConcSweepState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            runner_armed: AtomicBool::new(false),
            initiator_owed: AtomicUsize::new(0),
            pending: AtomicBool::new(false),
            walking: AtomicBool::new(false),
            job: Mutex::new(None),
            deferred_run: AtomicUsize::new(0),
            last_live: AtomicUsize::new(0),
            deferred: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            last_refusal: AtomicU8::new(0),
            by_initiator: AtomicUsize::new(0),
            by_allocation: AtomicUsize::new(0),
            by_cycle_start: AtomicUsize::new(0),
            by_mark_start: AtomicUsize::new(0),
            by_waiter: AtomicUsize::new(0),
            by_exit: AtomicUsize::new(0),
            alloc_retries: AtomicUsize::new(0),
            pause_us: AtomicU64::new(0),
            walk_us: AtomicU64::new(0),
            install_us: AtomicU64::new(0),
            wait_us: AtomicU64::new(0),
        }
    }

    /// Is the switch on for this heap? One relaxed load.
    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Is a deferred sweep waiting for its install? One relaxed load: the
    /// `needs_gc` poll's whole cost on the default path.
    #[inline]
    pub(crate) fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    /// The deferred walk is running (read by the oversize-array screen).
    #[inline]
    pub(crate) fn walking(&self) -> bool {
        self.walking.load(Ordering::Relaxed)
    }
}

/// Everything [`ZgcRealHeap::defer_sweep_and_close_pause`] needs from
/// `collect_garbage`'s frame. A struct, so the call site names each piece.
pub(super) struct ZDeferredPause<'a> {
    pub(super) registered: ZObjectStartsSnapshot,
    pub(super) cfg: ZSweepCfg,
    pub(super) registered_count: usize,
    pub(super) prune_keys: Option<Vec<usize>>,
    pub(super) gc_started: Option<std::time::Instant>,
    pub(super) laps: ZCycleLaps,
    pub(super) clock: ZPhaseClock,
    pub(super) mark_kind: &'static str,
    pub(super) bytes_in_use_before: usize,
    pub(super) pace_near_full: bool,
    pub(super) pace_window: usize,
    pub(super) pause_cpu: &'a super::mark::ZThreadCpuSpan,
    pub(super) cpu_before: Option<ZCollectionCpuTotals>,
    /// zt t32 (R6, 6b, `CRATONVM_ZGC_CONCURRENT_RELOCATE`): the collection's
    /// roots when this pause may plan a concurrent relocation (admitted and
    /// unrefused by the caller); `None` otherwise.
    pub(super) relocate_roots: Option<&'a mut [cratonvm_types::ObjectRef]>,
}

impl ZgcRealHeap {
    /// Should THIS cycle leave its sweep out of the pause? `None`: yes. A
    /// refusal names its reason ([`refusal`]). Asked once, at the sweep, with
    /// the switch on; it also takes the runner arm, so an arm never outlives
    /// its collection.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn concurrent_sweep_refusal(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        sweep_floor: usize,
        gen_on: bool,
        want_dead: bool,
        after_alloc_failure: bool,
    ) -> Option<u8> {
        let st = &self.counters.csweep;
        let armed = st.runner_armed.swap(false, Ordering::Relaxed);
        if !armed {
            return Some(refusal::NO_RUNNER);
        }
        if gen_on || sweep_floor != 0 {
            return Some(refusal::GENERATIONAL);
        }
        if self.mark_bits.is_none()
            || registered.words.is_empty()
            || !self.bitmap_sweep_enabled()
            || !cfg.zero_header_only
            || !cfg.merge_dead_runs
        {
            return Some(refusal::NOT_BITMAP);
        }
        if super::zgc_corpse_enabled() || self.counters.lazy_starts.on() {
            return Some(refusal::DIAGNOSTIC);
        }
        if want_dead {
            return Some(refusal::WANTS_DEAD);
        }
        if self.tlab_retire_incomplete.load(Ordering::Acquire)
            || !registered.extra.is_empty()
            || !self.jit_tlab_skip_regions().is_empty()
        {
            return Some(refusal::TAILS);
        }
        if after_alloc_failure
            || self.hard_alloc_failure.load(Ordering::Relaxed)
            || self.headroom_low.load(Ordering::Relaxed)
        {
            return Some(refusal::URGENT);
        }
        if self.relocation_requested()
            && st.deferred_run.load(Ordering::Relaxed) >= ZGC_CSWEEP_MAX_DEFERRED_RUN
        {
            return Some(refusal::SLIDE_DUE);
        }
        None
    }

    /// Book a refusal; a refused cycle sweeps in the pause and ends the run of
    /// deferred cycles (it may slide).
    pub(crate) fn note_concurrent_sweep_refusal(&self, reason: u8) {
        let st = &self.counters.csweep;
        st.refused.fetch_add(1, Ordering::Relaxed);
        st.last_refusal.store(reason, Ordering::Relaxed);
        st.deferred_run.store(0, Ordering::Relaxed);
    }

    /// The pause half of a deferred cycle: everything `collect_garbage` does
    /// from the sweep to its end that needs no sweep total, then the job is
    /// parked. Returns the collection's result; its byte totals are `0`
    /// (unknown until the install; the VM only logs them at debug level).
    ///
    /// # Which hunks of `collect_garbage` this is
    ///
    /// In `collect_garbage`'s order, the deferred cycle runs here: the bitmap
    /// sweep's registry prune (`sweep_bitmap`'s `retain_marked_within`), the
    /// key probe for the monitor prune and the prune itself (the block after
    /// the slide), `prune_relocations`, `recharge_published_tails`, the three
    /// latch clears, the trigger-mask snapshot, the warm-up count and the
    /// pacing census, `prune_huge_extents`, the cycle number,
    /// `forget_empty_registry_blocks`, the sweep lap, the root-filter disarm
    /// and the pause CPU span. Everything else after the sweep moved to
    /// [`Self::run_deferred_sweep`]'s hook; the compaction block does not run
    /// on a deferred cycle.
    pub(super) fn defer_sweep_and_close_pause(
        &self,
        p: ZDeferredPause<'_>,
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        let ZDeferredPause {
            registered,
            mut cfg,
            registered_count,
            prune_keys,
            gc_started,
            mut laps,
            mut clock,
            mark_kind,
            bytes_in_use_before,
            pace_near_full,
            pace_window,
            pause_cpu,
            cpu_before,
            relocate_roots,
        } = p;
        let st = &self.counters.csweep;
        // No slide follows this sweep, so no size hints.
        cfg.record_live_cells = false;
        // zt t23/zc7: zc2's invariant, made a check. `collect_garbage`
        // released the last evacuating slide's quarantine (and, under
        // `CRATONVM_ZGC_REGION_QUARANTINE`, its region cycle) before the
        // defer decision, and a deferred cycle never slides, so nothing is
        // held while the concurrent half rebuilds the low free list over the
        // complement of the live set -- which would otherwise list a held span
        // and re-issue a from-space address a kept cycle still answers.
        debug_assert!(
            self.counters.evac_quarantine.lock().is_empty(),
            "a deferred sweep must not run while evacuated spans are quarantined"
        );
        debug_assert!(
            !self.counters.region_reloc.holds_cycle(),
            "a deferred sweep must not run while a region relocation cycle is kept"
        );

        // (1) THE LOW FREE LIST LEAVES THE ALLOCATOR. The complement recomputes
        // every byte of it (it is the space below the cursor no survivor
        // covers), so nothing is lost; until the install, a window allocation
        // bumps above `low_cursor_at_pause` or takes a large-object hole that
        // was free before this pause.
        let (low_cursor_at_pause, low_list_taken) = {
            let mut arena = self.arena.lock();
            let cursor = arena.used_low_for_compaction();
            let taken = arena.take_low_free_list_for_deferred_sweep();
            (cursor, taken)
        };

        // (2) THE REGISTRY PRUNE, IN THE PAUSE. `sweep_bitmap`'s bulk
        // `starts &= marks`, moved ahead of the walk: from the release on, no
        // dead base is registered, and a window allocation (start bit set, mark
        // bit clear) is never in the range this pass clears.
        if let Some(marks) = self.mark_bits.as_ref() {
            if super::zgc_bitmap_bounds_enabled() {
                self.registry
                    .retain_marked_within(marks, &self.live_bitmap_bounds());
            } else {
                self.registry.retain_marked(marks);
            }
        }

        // (2a) zt t34 (page `zgc-r6-reference-handler-spins-in-reference-queue-enqueue`):
        // THE KEY PROBE of (3), taken now, while the registry is exactly the
        // marked set. The plan below copies its eager set (and, through the
        // map's hook, every root it rewrites), and a copy unregisters the
        // object's OLD base: probed after it, a moved survivor read as dead,
        // (3) pruned its inflated monitor, and the remap had nothing to
        // re-key -- the survivor kept an INFLATED word no index entry named
        // (`monitorexit`: `IllegalMonitorStateException`; the next enter: a
        // fresh monitor). A `ReferenceQueue$Lock` so lost had two owners,
        // and javac's catch-any around the `Reference Handler`'s
        // `ReferenceQueue.enqueue` exit spun on it. The in-pause sweep takes
        // its probe before its plan already (`collect_garbage`).
        let dead_keys: Option<Vec<usize>> = prune_keys.as_ref().map(|keys| {
            let mut found: Vec<usize> = keys
                .iter()
                .copied()
                .filter(|&k| registered.contains(k) && !self.registry.contains(k))
                .collect();
            found.sort_unstable();
            found.dedup();
            found
        });

        // (2b) zt t32 (R6, 6b): THE PLAN, when the caller admitted one. The
        // pruned registry is the live set (`starts & marks`), as the in-pause
        // sweep leaves it; the free list is already out of the allocator, so
        // the plan withholds nothing, and its destinations bump above
        // `low_cursor_at_pause` (the install then never retracts the cursor
        // below them). Every planned survivor's old header stays intact until
        // the release, which is what the walk sizes it by.
        let mut plan_map = cratonvm_types::PointerMap::default();
        let mut planned = 0usize;
        if let Some(roots) = relocate_roots {
            // zt t34: built only when the plan reads the whole live set.
            let live = || -> Vec<usize> {
                if super::zgc_bitmap_bounds_enabled() {
                    self.registry.bases_within(&self.live_bitmap_bounds())
                } else {
                    self.registry.snapshot().bases()
                }
            };
            if let Some((n, map)) = self.concurrent_relocate_start(&live) {
                self.counters.crl.note_deferred_plan();
                planned = n;
                for r in roots.iter_mut() {
                    if let Some(to) = map.get(&(r.as_ptr() as usize)) {
                        // SAFETY: a planned destination this pause allocated
                        // and (through the map's hook) just copied.
                        *r = unsafe { cratonvm_types::ObjectRef::from_raw(*to as *mut u8) };
                    }
                }
                self.counters
                    .compaction_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .objects_relocated
                    .fetch_add(n, Ordering::Relaxed);
                plan_map = map;
            }
        }
        let crl_extents = if planned > 0 {
            self.concurrent_relocate_extents()
        } else {
            Vec::new()
        };

        // (3) THE MONITOR PRUNE, from the key probe (zr z5/sw): a key died this
        // cycle iff the snapshot registered it and the pruned registry does
        // not (probed in (2a), before the plan). Its objects are not freed yet,
        // but they are unreachable, and no new object can reuse their bases
        // before the install.
        {
            let mut dead_guard = self.counters.dead_scratch.lock();
            let dead: &mut Vec<usize> = &mut dead_guard;
            dead.clear();
            if let (Some(keys), Some(found)) = (&prune_keys, dead_keys) {
                dead.extend(found);
                self.counters
                    .sweep_prune_key_cycles
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .sweep_prune_keys_probed
                    .fetch_add(keys.len(), Ordering::Relaxed);
                self.counters
                    .sweep_prune_keys_dead
                    .fetch_add(dead.len(), Ordering::Relaxed);
                monitors.prune_dead_filtered(dead.as_slice());
            } else {
                monitors.prune_dead(dead.as_slice());
            }
            self.counters.fp.after_dead_scratch(dead);
        }
        monitors.remap_after_gc(&plan_map);
        // zt t32 (R6, 6b): the collector-side tables a plan moves, as the
        // in-pause epilogue does (`collect_garbage`).
        if !plan_map.is_empty() {
            self.counters
                .ref_processor
                .lock()
                .update_after_gc(&plan_map);
            let mut resurrected = self.counters.resurrected_finalizers.lock();
            for addr in resurrected.iter_mut() {
                if let Some(&new) = plan_map.get(addr) {
                    *addr = new;
                }
            }
        }

        // (4) The pause-side bookkeeping, in `collect_garbage`'s order.
        self.prune_relocations();
        let recharged = self.recharge_published_tails();
        self.headroom_low.store(false, Ordering::Relaxed);
        self.native_alloc_pressure.store(false, Ordering::Relaxed);
        self.hard_alloc_failure.store(false, Ordering::Relaxed);
        // The re-arm floor is the hook's to compute (it needs the live bytes).
        // Until then no allocation may raise the native-boundary pressure latch
        // (`alloc_raw`'s `after >= gc_rearm`, the TLAB paths' copies), which
        // would collect again at the next native boundary without asking
        // `needs_gc`; and no warm-up or director point may fall due.
        self.gc_rearm.store(usize::MAX, Ordering::Relaxed);
        let trigger_mask = self.counters.trigger_seen.load(Ordering::Relaxed);
        self.counters.trigger_seen.store(0, Ordering::Relaxed);
        self.counters
            .last_cycle_trigger_mask
            .store(trigger_mask, Ordering::Relaxed);
        if trigger_mask & super::TRIGGER_BIT_WARMUP != 0 {
            self.counters
                .warmup_cycles_done
                .fetch_add(1, Ordering::Relaxed);
        }
        self.counters.pacing.record(
            pace_near_full,
            super::pace_kind_for_mask(trigger_mask),
            pace_window,
        );
        self.prune_huge_extents();
        let cycle = self.gc_count.fetch_add(1, Ordering::Relaxed) + 1;
        let _forgotten_blocks = self.forget_empty_registry_blocks();
        // The mark bits stay set until the install clears them.
        self.conc_bits_known_clear.store(false, Ordering::Release);
        laps.sweep_us = clock.lap();
        self.counters.last_sweep_us.store(
            laps.sweep_us.min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        let pause_ns = gc_started.map(|s| s.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        if self.relocation_requested() {
            st.deferred_run.fetch_add(1, Ordering::Relaxed);
        }

        // (5) PARK THE JOB, then publish it. `pending` after the job is in its
        // slot, so a runner that sees the flag finds the job.
        let workers = self.sweep_workers(false);
        let job = ZDeferredSweep {
            registered,
            cfg,
            workers,
            registered_count,
            low_cursor_at_pause,
            low_list_taken,
            allocated_at_pause: self.allocated.load(Ordering::Relaxed),
            recharged,
            cycle,
            laps,
            mark_kind,
            bytes_in_use_before,
            trigger_mask,
            pause_ns,
            released_at: std::time::Instant::now(),
            crl_extents,
        };
        *st.job.lock() = Some(Box::new(job));
        st.deferred.fetch_add(1, Ordering::Relaxed);
        st.initiator_owed.fetch_add(1, Ordering::Relaxed);
        // zt t23/x19: the previous collection's VM reference round is not
        // this cycle's; this cycle's is noted by the door after the
        // collection returns, so a runner that installs before that note
        // (an allocation in the pause's own post-collection work) prints
        // `cp_vm_refs_pause_us=0`, never the previous cycle's figure.
        self.forget_last_vm_reference_round();
        if let Some(ns) = pause_ns {
            st.pause_us.fetch_add(ns / 1_000, Ordering::Relaxed);
        }
        st.pending.store(true, Ordering::Release);

        // (6) The end of `collect_garbage`: disarmed before any mutator runs.
        self.counters.mark_root_filter.disarm();
        self.book_pause_cpu(cycle, pause_cpu, cpu_before, gc_started);
        GcResult {
            stats: GcStats {
                objects_copied: planned,
                bytes_copied: 0,
                bytes_freed: 0,
            },
            pointer_map: plan_map,
        }
    }

    /// The VM's collecting door will run a deferred sweep after it releases
    /// the world (`run_collection_pause`). A no-op with the switch off.
    pub fn arm_concurrent_sweep_runner(&self) {
        let st = &self.counters.csweep;
        if st.enabled() {
            st.runner_armed.store(true, Ordering::Relaxed);
        }
    }

    /// Run the pending sweep on this thread, if there is one; wait for a
    /// runner already at it. `true` when this call ran it. Cheap (one load)
    /// when nothing is pending.
    pub fn finish_pending_sweep(&self, who: ZSweepRunner) -> bool {
        let st = &self.counters.csweep;
        if !st.pending.load(Ordering::Acquire) {
            return false;
        }
        // zt t31/zc11 (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`): the collecting
        // thread's reference round reads the pause's mark bits; no other
        // runner installs before it ends (an allocation waits a bounded time,
        // then fails as before). One load when no round holds the install.
        if who != ZSweepRunner::Initiator {
            let bound = (who == ZSweepRunner::Allocation)
                .then(|| std::time::Duration::from_millis(ZGC_CSWEEP_ALLOC_WAIT_MS));
            if !self.wait_for_sweep_install_hold(bound) {
                return false;
            }
        }
        let mut slot = st.job.lock();
        let Some(job) = slot.take() else {
            // Another runner finished it while this one waited for the lock:
            // it clears `pending` before it lets the lock go. A flag still set
            // here, with the lock held and no job, is a runner that unwound
            // mid-sweep; drop the flag rather than hold `needs_gc` off for good
            // (a job is only ever parked inside a pause, when no runner can
            // hold this lock, so no live job is lost by it).
            if st.pending.load(Ordering::Acquire) {
                st.pending.store(false, Ordering::Release);
            }
            return false;
        };
        // zt t32: the staged referent clear before the walk can free a dead
        // referent (the backstop; the round normally ran it already).
        self.run_staged_referent_clear();
        // Held for the whole sweep: a second runner waits, then finds `None`.
        self.run_deferred_sweep(*job, who);
        drop(slot);
        true
    }

    /// The collecting door's call after its release (`run_collection_pause`):
    /// run the sweep its collection parked, or wait for the runner that took
    /// it, and answer whether the door's collection DEFERRED its sweep -- then
    /// the after-figures the pause sealed describe the unswept heap and the
    /// door re-samples them.
    ///
    /// zt t22/x18: not [`Self::finish_pending_sweep`]'s "this call ran it".
    /// Another runner may have installed the sweep first -- an allocation
    /// that failed in the pause's own post-collection work (the low free list
    /// is taken), a `System.gc()` waiter, the next cycle's start -- and the
    /// door then skipped the re-seal: its `-Xlog:gc` after-figure, JFR heap
    /// summary, JMX collection usage and productivity `live` were the pre-sweep
    /// ones. When this returns, the sweep is installed (a running runner holds
    /// the job lock, and clears `pending` before it lets it go).
    pub fn run_pending_sweep_after_release(&self) -> bool {
        let ran = self.finish_pending_sweep(ZSweepRunner::Initiator);
        let owed = self
            .counters
            .csweep
            .initiator_owed
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        ran || owed
    }

    /// zt t23/x19 (page item 28d; `CRATONVM_ZGC_EXIT_FINISHES_SWEEP`, default
    /// ON, `=0` kill): the VM's exit, before it reads the heap it reports --
    /// the `-Xlog:gc+exit` heap lines (`unified_logging::zgc_exit_heap`), the
    /// `[GC]` summary, the end-of-run metrics row. A process that exits
    /// between a deferring pause and its install (an exit from another thread
    /// while the collecting thread sweeps) reported the unswept `allocated`.
    /// Runs the pending sweep here, or waits for the thread running it (a
    /// running runner holds the job lock and clears `pending` before it lets
    /// it go), so the exit reports the cycle's end, as the collecting thread
    /// would have a moment later. Not a destructor: `ZgcRealHeap`'s `Drop`
    /// never runs in the shipped VM. `true` when this call ran the sweep; one
    /// load when nothing is pending (every run without the opt-in).
    pub fn finish_pending_sweep_at_exit(&self) -> bool {
        if !self.counters.csweep.pending() || !zgc_exit_finishes_sweep_enabled() {
            return false;
        }
        self.finish_pending_sweep(ZSweepRunner::Exit)
    }

    /// `alloc_raw`'s retry door: a sweep is pending, so the arena is short of
    /// the dead objects' space. Finish it (or wait for its runner, bounded)
    /// and tell the caller to retry. Never called with the arena lock held.
    pub(crate) fn finish_pending_sweep_for_allocation(&self) -> bool {
        let st = &self.counters.csweep;
        if !st.pending.load(Ordering::Acquire) {
            return false;
        }
        st.alloc_retries.fetch_add(1, Ordering::Relaxed);
        if self.finish_pending_sweep(ZSweepRunner::Allocation) {
            return true;
        }
        // The runner held the job; `finish_pending_sweep` waited on its lock,
        // so the install is done unless a NEW sweep was parked meanwhile
        // (another cycle ran). Bounded either way.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(ZGC_CSWEEP_ALLOC_WAIT_MS);
        while st.pending.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        true
    }

    /// The live bytes the last installed deferred sweep left (the
    /// productivity verdict's `live`, without the window's allocations).
    /// `None` with the switch off or before the first install.
    pub fn concurrent_sweep_last_live(&self) -> Option<usize> {
        let st = &self.counters.csweep;
        (st.enabled() && st.by_total() > 0).then(|| st.last_live.load(Ordering::Relaxed))
    }

    /// The `[GC] zgc-concurrent-sweep:` summary figures.
    pub fn concurrent_sweep_report(&self) -> ZConcSweepReport {
        let st = &self.counters.csweep;
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        ZConcSweepReport {
            enabled: st.enabled(),
            deferred: l(&st.deferred),
            refused: l(&st.refused),
            last_refusal: st.last_refusal.load(Ordering::Relaxed),
            by_initiator: l(&st.by_initiator),
            by_allocation: l(&st.by_allocation),
            by_cycle_start: l(&st.by_cycle_start),
            by_mark_start: l(&st.by_mark_start),
            by_waiter: l(&st.by_waiter),
            by_exit: l(&st.by_exit),
            alloc_retries: l(&st.alloc_retries),
            pause_us: u(&st.pause_us),
            walk_us: u(&st.walk_us),
            install_us: u(&st.install_us),
            wait_us: u(&st.wait_us),
            pending: st.pending(),
        }
    }

    /// The concurrent half: the walk, the install, and the after-sweep hook.
    /// Runs with the mutators live; the caller holds the job lock.
    fn run_deferred_sweep(&self, job: ZDeferredSweep, who: ZSweepRunner) {
        let st = &self.counters.csweep;
        let started = std::time::Instant::now();
        let wait_us = started
            .saturating_duration_since(job.released_at)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        let ZDeferredSweep {
            registered,
            cfg,
            workers,
            registered_count,
            low_cursor_at_pause,
            low_list_taken,
            allocated_at_pause,
            recharged,
            cycle,
            laps,
            mark_kind,
            bytes_in_use_before,
            trigger_mask,
            pause_ns,
            released_at: _,
            crl_extents,
        } = job;

        // ---- THE WALK -------------------------------------------------------
        st.walking.store(true, Ordering::Relaxed);
        let walked = self.sweep_bitmap_complement(&registered, &cfg, workers);
        st.walking.store(false, Ordering::Relaxed);
        let (mut sh, mut spans, new_cursor) = match walked {
            Some((sh, spans, cursor_abs)) => (sh, spans, cursor_abs - cfg.arena_base),
            None => {
                // Unreachable by the refusal screen (the same four conditions
                // decide both). Fail safe: the per-object sweep frees the dead
                // this cycle can size; the free list the pause took is lost
                // until the next cycle's complement lists it again.
                tracing::warn!(
                    target: "zgc",
                    cycle,
                    "zgc concurrent sweep: the complement declined a cycle the screen admitted; \
                     sweeping per object"
                );
                let mut sh = self.sweep_serial(&registered, 0, &cfg);
                sh.flush();
                (sh, Vec::new(), low_cursor_at_pause)
            }
        };
        // ...AND PUT BACK WHAT THE HIGH ARM REFUSED TO SWEEP: the pause's prune
        // cleared those unmarked bases, as `sweep_bitmap`'s does, and their
        // bytes are on no free list. Empty on every healthy run.
        if !sh.unsizable_high.is_empty() {
            tracing::warn!(
                target: "zgc",
                count = sh.unsizable_high.len(),
                "zgc concurrent sweep: re-registering large-object-end bases the complement \
                 could not size"
            );
            self.registry.insert_all(&sh.unsizable_high);
            sh.unsizable_high.clear();
        }
        sh.dead_in_runs = sh.dead_count;
        sh.flush();
        let walk_done = std::time::Instant::now();

        // zt t32 (R6, 6b): no low free span inside a region the planning pause
        // selected; the release frees each one whole.
        if !crl_extents.is_empty() {
            spans = clip_spans_outside(&spans, &crl_extents);
        }

        // ---- THE INSTALL ----------------------------------------------------
        let dead_runs = sh.spans.len();
        let mut bytes_freed = sh.bytes_freed;
        {
            let mut arena = self.arena.lock();
            // What the window freed onto the low list (retired TLAB tails, all
            // above the pause's cursor) is kept: the rebuild replaces the list.
            let window = arena.take_low_free_blocks();
            let _ = arena.rebuild_low_free_list(&spans);
            let now_free: usize = spans.iter().map(|(_, len)| *len).sum();
            bytes_freed += now_free.saturating_sub(low_list_taken);
            let cursor_now = arena.used_low_for_compaction();
            if window.is_empty() && cursor_now == low_cursor_at_pause {
                // Nothing bumped in the window: the in-pause arm's retraction,
                // byte for byte.
                bytes_freed += arena.retract_cursor_to(new_cursor);
            } else {
                // The cursor moved: the bytes above the last survivor are a
                // free span below the pause's cursor instead.
                if new_cursor < low_cursor_at_pause {
                    // zt t32 (R6, 6b): minus any selected region.
                    let tail = [(new_cursor, low_cursor_at_pause - new_cursor)];
                    for (off, len) in clip_spans_outside(&tail, &crl_extents) {
                        arena.add_free_block(off, len);
                        bytes_freed += len;
                    }
                }
                for (off, len) in window {
                    arena.add_free_block(off, len);
                }
                arena.coalesce_free_list();
            }
            // The large-object end's individual holes, as the in-pause arm.
            for (off, len) in sh.spans.drain(..) {
                arena.add_free_block(off, len);
            }
            arena.coalesce_high();
            let reclaimed_high = arena.retract_high_cursor_into_free_head();
            if reclaimed_high != 0 {
                tracing::debug!(
                    target: "cratonvm::gc",
                    bytes = reclaimed_high,
                    high_cursor = arena.high_cursor(),
                    "zgc concurrent sweep: retracted the large-object cursor into a free head",
                );
            }
            if self.gc_log_enabled.load(Ordering::Relaxed) {
                eprintln!(
                    "[GC] zgc-reclaim: bytes_freed={} free_list_bytes={} \
                     largest_free_block={} cursor={} capacity={} committed={} \
                     registered={} reset_total={}",
                    bytes_freed,
                    arena.free_list_bytes(),
                    arena.largest_free_block(),
                    arena.used(),
                    arena.capacity(),
                    arena.committed_bytes(),
                    registered_count,
                    arena.bytes_reset(),
                );
            }
            let _reclaimed_tail = arena.retract_cursor_into_free_tail();
            if self.counters.fp.stamp_at_sweep_on.load(Ordering::Relaxed) {
                let stamped = arena.stamp_idle_unused();
                self.counters
                    .fp
                    .idle_stamps
                    .fetch_add(stamped, Ordering::Relaxed);
            }
        }
        if sh.region_words_skipped != 0 {
            self.counters
                .sweep_region_words_skipped
                .fetch_add(sh.region_words_skipped, Ordering::Relaxed);
        }
        // EVERY MARK BIT IS CLEAR FROM HERE. Nothing set one since the pause
        // (module doc), so the clear races no claim.
        let _ = self.mark_clear_all();
        self.conc_bits_known_clear.store(true, Ordering::Release);

        // ---- THE AFTER-SWEEP HOOK: the consumers of the sweep's totals --------
        self.after_deferred_sweep(
            &sh,
            &cfg,
            ZAfterSweep {
                cycle,
                laps,
                mark_kind,
                registered_count,
                bytes_freed,
                dead_runs,
                allocated_at_pause,
                recharged,
                bytes_in_use_before,
                trigger_mask,
                pause_ns,
            },
        );

        let done = std::time::Instant::now();
        let walk_us = walk_done
            .duration_since(started)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        let install_us = done
            .duration_since(walk_done)
            .as_micros()
            .min(u64::MAX as u128) as u64;
        st.walk_us.fetch_add(walk_us, Ordering::Relaxed);
        st.install_us.fetch_add(install_us, Ordering::Relaxed);
        st.wait_us.fetch_add(wait_us, Ordering::Relaxed);
        match who {
            ZSweepRunner::Initiator => &st.by_initiator,
            ZSweepRunner::Allocation => &st.by_allocation,
            ZSweepRunner::CycleStart => &st.by_cycle_start,
            ZSweepRunner::MarkStart => &st.by_mark_start,
            ZSweepRunner::Waiter => &st.by_waiter,
            ZSweepRunner::Exit => &st.by_exit,
        }
        .fetch_add(1, Ordering::Relaxed);
        // THE CENSUS (zt t21/zc2 item 4): this cycle's pause beside its
        // concurrent phases, so a row reads the pause shrinking. `--verbose:gc`
        // only, like `zgc-pause:`. The mark-start pause of a concurrent cycle is
        // on its own `[GC] zgc-markstart:` line.
        if self.gc_log_enabled.load(Ordering::Relaxed) {
            eprintln!(
                "[GC] zgc-concurrent-phases: cycle={cycle} cp_markend_pause_us={} \
                 cp_sweep_pause_us={} cp_refs_pause_us={} cp_sweep_wait_us={wait_us} \
                 cp_sweep_conc_us={walk_us} cp_sweep_install_us={install_us} \
                 cp_runner={} cp_mark={mark_kind} cp_vm_refs_pause_us={}",
                pause_ns.map_or(0, |ns| ns / 1_000),
                laps.sweep_us,
                laps.resurrect_us.saturating_add(laps.refs_us),
                who.token(),
                // zt t22/zc3: this cycle's VM reference round (the
                // pre-collection pass, `process_references_after_gc`, the JNI
                // weak sweep), which `cp_refs_pause_us` (the heap's laps) does
                // not see. Noted by the VM door inside the pause, after the
                // collection returned; zt t23/x19: `0` when another runner
                // installed this sweep before that note (`cp_runner=` is
                // then not `initiator`), reset at the deferral.
                self.last_vm_reference_round_us(),
            );
        }
        // LAST: the cycle has ended. A `needs_gc` poll may collect again.
        st.pending.store(false, Ordering::Release);
    }

    /// The consumers of the sweep's totals that `collect_garbage` runs after
    /// its in-pause sweep (from `---- PHASE G bookkeeping` to the log block,
    /// and `record_metrics_for_cycle`), for a deferred cycle: non-generational
    /// and whole-heap by the screen, with no slide.
    fn after_deferred_sweep(&self, sh: &ZSweepShard, cfg: &ZSweepCfg, a: ZAfterSweep) {
        let st = &self.counters.csweep;
        let bytes_copied = sh.bytes_copied;
        let objects_copied = sh.objects_copied;
        let dead_count = sh.dead_count;
        let live_bytes = bytes_copied;
        let bytes_freed = a.bytes_freed;
        if sh.zero_bytes_skipped != 0 {
            self.counters
                .gen_zero_bytes_skipped
                .fetch_add(sh.zero_bytes_skipped, Ordering::Relaxed);
        }
        if cfg.skip_dead_headers {
            let total = self
                .counters
                .sweep_dead_headers_skipped
                .fetch_add(sh.dead_headers_skipped, Ordering::Relaxed)
                + sh.dead_headers_skipped;
            if self.gc_log_enabled.load(Ordering::Relaxed)
                && super::sweep::zgc_sweep_skip_dead_headers_explicit()
            {
                eprintln!(
                    "[GC] zgc-sweep-skip-dead-headers: cycle_skipped={} \
                     total_skipped={total} dead={dead_count} want_dead={}",
                    sh.dead_headers_skipped, cfg.want_dead
                );
            }
        }
        if sh.dead_in_runs != 0 {
            self.counters
                .gen_dead_runs
                .fetch_add(a.dead_runs, Ordering::Relaxed);
            self.counters
                .gen_dead_objects
                .fetch_add(sh.dead_in_runs, Ordering::Relaxed);
        }
        if sh.unsizable != 0 {
            self.counters
                .local
                .unsizable_objects
                .fetch_add(sh.unsizable, Ordering::Relaxed);
            if !self
                .counters
                .local
                .unsizable_warned
                .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    count = sh.unsizable,
                    "zgc sweep: registered object(s) whose header could not be \
                     sized (compact layout unresolvable, or an implausible \
                     legacy num_slots) — retained rather than zeroed and \
                     free-listed. See ZgcRealHeap::alloc_size.",
                );
            }
        }

        // THE GAUGE. The window's allocations were added by their own
        // `fetch_add`s on top of `allocated_at_pause`; move the gauge by the
        // cycle's reclaim only, so it reads `live + window`, and saturate: a
        // decrement in the window (a tail give-back) can never wrap it.
        let republished = live_bytes.saturating_add(a.recharged);
        if a.allocated_at_pause >= republished {
            let drop_by = a.allocated_at_pause - republished;
            let _ = self
                .allocated
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(drop_by))
                });
        } else {
            self.allocated
                .fetch_add(republished - a.allocated_at_pause, Ordering::Relaxed);
        }
        // "Bytes allocated since the last collection" counts the window.
        self.cycle_alloc_watermark
            .store(republished, Ordering::Relaxed);
        let cap = self.heap_capacity();
        let legacy_floor = director::rearm_floor_legacy(cap, live_bytes);
        let near_full_pct = self.counters.near_full_progress_pct.load(Ordering::Relaxed);
        let mut floor = director::rearm_floor(cap, live_bytes, self.gc_threshold, near_full_pct);
        if self.counters.soft_max_paces.load(Ordering::Relaxed) {
            let soft = self.soft_max_capacity();
            if soft < cap && live_bytes < soft {
                floor = floor.min(director::rearm_floor_legacy(soft, live_bytes));
            }
        }
        self.counters
            .gc_rearm_legacy
            .store(republished.saturating_add(legacy_floor), Ordering::Relaxed);
        self.gc_rearm
            .store(republished.saturating_add(floor), Ordering::Relaxed);
        // The window may have raised it against the taken list.
        self.headroom_low.store(false, Ordering::Relaxed);
        self.last_cycle_reclaimed_nothing
            .store(bytes_freed == 0, Ordering::Relaxed);
        self.counters
            .gc_stress_mark
            .store(self.allocated.load(Ordering::Relaxed), Ordering::Relaxed);
        self.recheck_sub_request_spans();
        {
            let arena = self.arena.lock();
            self.sample_frag_gauge(&arena, a.cycle);
        }
        self.refresh_adaptive_conc_start(ZgcRealHeap::monotonic_nanos());
        // ONE budget reading: no slide ran, so both arms of
        // `CRATONVM_ZGC_PAUSE_TARGET_INCLUDES_RELOCATE` read the same pause.
        // zt t24/zc8 (`CRATONVM_ZGC_CONC_CLOSE_FORGETS_PAUSE_BUDGET`, default
        // ON, `=0` kill): none when the cycle was marked concurrently -- its
        // pause walked neither span -- and the budget is dropped instead
        // (`mark_end.rs`, `conc_close_forgets_pause_budget`).
        if !self.conc_close_forgets_pause_budget() {
            if let Some(ns) = a.pause_ns {
                self.refresh_pause_target_budget(ns, live_bytes, bytes_freed);
            }
        }
        // THE LOG BLOCK, at the cycle's end (HotSpot prints its cycle summary
        // after the concurrent phases). `pause_us` is the pause the mutators
        // took; the keys are `collect_garbage`'s, verbatim.
        if let Some(pause_ns) = a
            .pause_ns
            .filter(|_| self.gc_log_enabled.load(Ordering::Relaxed))
        {
            let pause_us = pause_ns / 1_000;
            let cycle = a.cycle;
            let mark = a.mark_kind;
            let l = &a.laps;
            eprintln!(
                "[GC] zgc-real: cycle={cycle} pause_us={pause_us} mark={mark} gen=off \
                 objects_copied={objects_copied} bytes_copied={bytes_copied} \
                 bytes_freed={bytes_freed} occupancy={bytes_copied}/{cap} bytes",
            );
            eprintln!(
                "[GC] zgc-pause: cycle={cycle} total_us={pause_us} \
                 markend_us={} tlab_us={} snapshot_us={} \
                 mark_us={} resurrect_us={} refs_us={} \
                 sweep_us={} registered={} dead={} \
                 alloc_trigger={}/{} \
                 pause_target={}ms/{}/unreachable={}",
                l.markend_us,
                l.tlab_us,
                l.snapshot_us,
                l.mark_us,
                l.resurrect_us,
                l.refs_us,
                l.sweep_us,
                a.registered_count,
                dead_count,
                self.counters.alloc_trigger_fires.load(Ordering::Relaxed),
                self.alloc_trigger_bytes.load(Ordering::Relaxed),
                self.pause_target_ms.load(Ordering::Relaxed),
                self.pause_affordable_span.load(Ordering::Relaxed),
                self.counters
                    .pause_target_unreachable
                    .load(Ordering::Relaxed),
            );
        }
        self.record_metrics_for_cycle(
            a.laps,
            a.bytes_in_use_before,
            live_bytes,
            cap,
            a.trigger_mask,
        );
        st.last_live.store(republished, Ordering::Relaxed);
    }
}

/// The after-sweep hook's inputs from the pause and the install.
struct ZAfterSweep {
    cycle: usize,
    laps: ZCycleLaps,
    mark_kind: &'static str,
    registered_count: usize,
    bytes_freed: usize,
    dead_runs: usize,
    allocated_at_pause: usize,
    recharged: usize,
    bytes_in_use_before: usize,
    trigger_mask: usize,
    pause_ns: Option<u64>,
}

impl ZConcSweepState {
    /// Installed sweeps, by every runner.
    fn by_total(&self) -> usize {
        self.by_initiator.load(Ordering::Relaxed)
            + self.by_allocation.load(Ordering::Relaxed)
            + self.by_cycle_start.load(Ordering::Relaxed)
            + self.by_mark_start.load(Ordering::Relaxed)
            + self.by_waiter.load(Ordering::Relaxed)
            + self.by_exit.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    //! zt t21/zc2: the phase split on a real heap. Each test runs both arms
    //! (the switch on and off) or carries an explicit positive control, so a
    //! split that never engaged cannot pass as "the graph survived".

    use super::*;
    use crate::collector::{GarbageCollector, StopTheWorldToken};
    use crate::heap::ArrayElementType;
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// The VM's monitor table with no monitor in it, as the key probe sees it
    /// (zr z5/sw): it wants the dead, and its candidate keys are none. The
    /// trait's defaults (`wants_dead_addresses` = yes, no key probe) would ask
    /// for the full dead list, which the screen refuses (`WANTS_DEAD`).
    struct NoMonitors;
    impl MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
        fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
            Some(Vec::new())
        }
    }

    /// A 64 MiB heap that holds addresses still (no slide), with the switch
    /// set as `on` says.
    fn test_heap(on: bool) -> ZgcRealHeap {
        let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        // zt t27/gn5 (gn page item 10): single generation -- the mode refuses
        // the deferred sweep (`refusal::GENERATIONAL`; the refusal test turns
        // it on itself); under `CRATONVM_ZGC_GENERATIONAL=1` every test heap
        // starts generational.
        heap.set_generational_enabled(false);
        heap.counters.csweep.enabled.store(on, Ordering::Relaxed);
        heap
    }

    /// A chain of `n` two-slot objects plus `garbage` unreferenced ones.
    fn graph(heap: &ZgcRealHeap, n: usize, garbage: usize) -> (ObjectRef, Vec<usize>, Vec<usize>) {
        let mut chain: Vec<ObjectRef> = Vec::with_capacity(n);
        for _ in 0..n {
            chain.push(heap.alloc_object(ClassId::new(51), 2));
        }
        for i in 0..n - 1 {
            heap.set_field(chain[i], 0, Value::Object(Some(chain[i + 1])));
        }
        let dead: Vec<usize> = (0..garbage)
            .map(|_| heap.alloc_object(ClassId::new(52), 4).as_ptr() as usize)
            .collect();
        let addrs = chain.iter().map(|o| o.as_ptr() as usize).collect();
        (chain[0], addrs, dead)
    }

    fn walk(heap: &ZgcRealHeap, head: ObjectRef) -> Vec<usize> {
        let mut out = vec![head.as_ptr() as usize];
        let mut cur = head;
        while let Value::Object(Some(next)) = heap.get_field(cur, 0) {
            out.push(next.as_ptr() as usize);
            cur = next;
        }
        out
    }

    fn collect(heap: &ZgcRealHeap, head: ObjectRef, armed: bool) {
        if armed {
            heap.arm_concurrent_sweep_runner();
        }
        // SAFETY: single-threaded test; the token stands for the pause.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [head];
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    }

    /// The reader: unset off; the profile turns it on; its own `=0` kills it
    /// inside the profile; alone it is on.
    #[test]
    fn zt21zc2_the_switch_reads_the_profile_and_its_own_kill() {
        let run = |edits: &[(&str, Option<&str>)]| {
            let all: Vec<(&str, Option<&str>)> =
                ["CRATONVM_ZGC_CONCURRENT", "CRATONVM_ZGC_CONCURRENT_SWEEP"]
                    .iter()
                    .map(|&k| {
                        let v = edits.iter().find(|(e, _)| *e == k).and_then(|(_, v)| *v);
                        (k, v)
                    })
                    .collect();
            cratonvm_types::flags::with_thread_overrides(&all, zgc_concurrent_sweep_enabled)
        };
        assert!(!run(&[]), "control: unset is off");
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("1"))]));
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_CONCURRENT_SWEEP", Some("0")),
        ]));
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT_SWEEP", Some("1"))]));
        assert!(!run(&[("CRATONVM_ZGC_CONCURRENT", Some("0"))]));
    }

    /// The split: the pause prunes the registry and frees no byte; the window
    /// allocates only above the pause's cursor; the install frees the dead,
    /// keeps every survivor (the window's included) and moves the gauge by the
    /// reclaim only. Kill arm: the same collection frees in the pause.
    #[test]
    fn zt21zc2_the_pause_frees_nothing_and_the_install_frees_the_dead() {
        for on in [true, false] {
            let heap = test_heap(on);
            let (head, chain, garbage) = graph(&heap, 2_000, 4_000);
            collect(&heap, head, true);
            let st = &heap.counters.csweep;
            assert_eq!(st.pending(), on, "on={on}: deferred only with the switch");
            for &g in &garbage {
                assert!(
                    heap.is_object_address(g).is_none(),
                    "on={on}: the registry is pruned in the pause"
                );
            }
            assert_eq!(walk(&heap, head), chain, "on={on}");
            if !on {
                assert_eq!(heap.concurrent_sweep_report().deferred, 0);
                continue;
            }
            let (cursor_at_pause, base) = {
                let a = heap.arena.lock();
                (a.used_low_for_compaction(), a.base_ptr() as usize)
            };
            assert!(
                !heap.conc_bits_known_clear.load(Ordering::Acquire),
                "the mark bits wait for the install"
            );
            // THE WINDOW: allocations land above the pause's cursor (the list
            // is taken), never on a dead object's bytes.
            let window: Vec<ObjectRef> = (0..64)
                .map(|_| heap.alloc_object(ClassId::new(53), 1))
                .collect();
            for (i, o) in window.iter().enumerate() {
                let a = o.as_ptr() as usize;
                assert!(
                    a >= base + cursor_at_pause,
                    "window object {i} below the cursor"
                );
                heap.set_field(*o, 0, Value::Int(i as i32));
            }
            let at_pause = heap.allocated.load(Ordering::Relaxed);
            assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
            assert!(!st.pending());
            assert!(!heap.finish_pending_sweep(ZSweepRunner::Waiter), "ran once");
            assert!(heap.conc_bits_known_clear.load(Ordering::Acquire));
            assert_eq!(
                walk(&heap, head),
                chain,
                "survivors intact after the install"
            );
            for (i, o) in window.iter().enumerate() {
                assert_eq!(
                    heap.get_field(*o, 0),
                    Value::Int(i as i32),
                    "window object {i}"
                );
            }
            let after = heap.allocated.load(Ordering::Relaxed);
            assert!(
                after < at_pause,
                "the gauge dropped by the reclaim: {after} vs {at_pause}"
            );
            let (free_after, cursor_after) = {
                let a = heap.arena.lock();
                (a.free_list_bytes(), a.used_low_for_compaction())
            };
            assert!(
                free_after > 0 || cursor_after < cursor_at_pause,
                "the dead objects' bytes came back"
            );
            let r = heap.concurrent_sweep_report();
            assert_eq!((r.deferred, r.by_initiator, r.refused), (1, 1, 0));
            assert_eq!(
                heap.concurrent_sweep_last_live(),
                Some(heap.counters.csweep.last_live.load(Ordering::Relaxed))
            );
        }
    }

    /// zt t22/x18: the collecting door re-seals its figures when its cycle
    /// deferred, even if another runner (here an allocation's) installed the
    /// sweep before the door asked; once per deferral. Controls: a cycle
    /// swept in the pause (unarmed) and the kill arm owe the door nothing.
    #[test]
    fn zt22x18_the_door_reseals_after_another_runner_installed_its_sweep() {
        let heap = test_heap(true);
        let (head, chain, _) = graph(&heap, 200, 1_000);
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending(), "fixture: deferred");
        assert!(heap.finish_pending_sweep(ZSweepRunner::Allocation));
        assert!(
            heap.run_pending_sweep_after_release(),
            "the door's cycle deferred: re-seal although the allocation ran it"
        );
        assert!(!heap.run_pending_sweep_after_release(), "once per deferral");
        let r = heap.concurrent_sweep_report();
        assert_eq!((r.by_allocation, r.by_initiator), (1, 0));
        // The door that ran its own sweep answers yes, as before.
        collect(&heap, head, true);
        assert!(heap.run_pending_sweep_after_release());
        assert!(!heap.run_pending_sweep_after_release());
        assert_eq!(heap.concurrent_sweep_report().by_initiator, 1);
        assert_eq!(walk(&heap, head), chain);
        // Controls: in-pause sweeps owe nothing.
        collect(&heap, head, false);
        assert!(
            !heap.run_pending_sweep_after_release(),
            "unarmed: no re-seal"
        );
        let off = test_heap(false);
        let (head, _, _) = graph(&off, 10, 10);
        collect(&off, head, true);
        assert!(
            !off.run_pending_sweep_after_release(),
            "kill arm: no re-seal"
        );
    }

    /// zt t23/x19 (page item 28d): the VM's exit finishes a pending sweep
    /// before it reads the heap it reports, once, booked `csw_by_exit`; the
    /// door that deferred still owes its re-seal. Kill arm
    /// (`CRATONVM_ZGC_EXIT_FINISHES_SWEEP=0`): the sweep stays pending.
    /// Control: nothing pending, nothing run.
    #[test]
    fn zt23x19_the_exit_finishes_a_pending_sweep() {
        const KEY: &str = "CRATONVM_ZGC_EXIT_FINISHES_SWEEP";
        let heap = test_heap(true);
        let (head, chain, _) = graph(&heap, 200, 1_000);
        assert!(
            !heap.finish_pending_sweep_at_exit(),
            "control: nothing pending"
        );
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending(), "fixture: deferred");
        let killed = cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("0"))], || {
            heap.finish_pending_sweep_at_exit()
        });
        assert!(!killed, "kill arm: the exit leaves it");
        assert!(heap.counters.csweep.pending(), "kill arm: still pending");
        let ran = cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
            heap.finish_pending_sweep_at_exit()
        });
        assert!(ran, "the exit ran the sweep");
        assert!(!heap.counters.csweep.pending());
        assert!(!heap.finish_pending_sweep_at_exit(), "once");
        let r = heap.concurrent_sweep_report();
        assert_eq!((r.by_exit, r.by_initiator, r.pending), (1, 0, false));
        assert!(
            heap.run_pending_sweep_after_release(),
            "the deferring door still re-seals"
        );
        assert_eq!(walk(&heap, head), chain);
    }

    /// zt t23/x19 (review of t22/zc3): a deferral forgets the previous
    /// collection's VM reference round, so a runner that installs before the
    /// door notes this cycle's round prints `cp_vm_refs_pause_us=0`, not the
    /// previous figure. Control: a cycle swept in the pause keeps it.
    #[test]
    fn zt23x19_a_deferral_forgets_the_previous_vm_reference_round() {
        let heap = test_heap(true);
        let (head, _, _) = graph(&heap, 50, 100);
        heap.note_vm_reference_round(10, 20, 3);
        assert_eq!(heap.last_vm_reference_round_us(), 33, "fixture");
        collect(&heap, head, false);
        assert_eq!(
            heap.last_vm_reference_round_us(),
            33,
            "control: swept in the pause, nothing forgotten"
        );
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending(), "fixture: deferred");
        assert_eq!(heap.last_vm_reference_round_us(), 0, "forgotten at the deferral");
        heap.note_vm_reference_round(1, 2, 3);
        assert!(heap.run_pending_sweep_after_release());
        assert_eq!(heap.last_vm_reference_round_us(), 6);
    }

    /// zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): a holder names two
    /// `Reference`-shaped rows (hidden from the mark) and a strong copy of the
    /// second one's referent; the first referent dies. Returns the heap after
    /// the collection, the two rows and their referents.
    fn referent_clear_cycle(csweep: bool, after_release: bool) -> (ZgcRealHeap, [ObjectRef; 4]) {
        let heap = test_heap(csweep);
        heap.dbg_set_concurrent_referent_clear(true, after_release);
        let holder = heap.alloc_object(ClassId::new(61), 3);
        let weak_dead = heap.alloc_object(ClassId::new(62), 2);
        let weak_live = heap.alloc_object(ClassId::new(62), 2);
        let dead = heap.alloc_object(ClassId::new(63), 1);
        let live = heap.alloc_object(ClassId::new(63), 1);
        heap.set_field(weak_dead, 0, Value::Object(Some(dead)));
        heap.set_field(weak_live, 0, Value::Object(Some(live)));
        heap.set_field(holder, 0, Value::Object(Some(weak_dead)));
        heap.set_field(holder, 1, Value::Object(Some(weak_live)));
        heap.set_field(holder, 2, Value::Object(Some(live)));
        assert!(heap.concurrent_refs_skip_mode());
        heap.set_stw_vm_reference_skip(vec![
            weak_dead.as_ptr() as usize,
            weak_live.as_ptr() as usize,
        ]);
        collect(&heap, holder, true);
        heap.clear_stw_vm_reference_skip();
        (heap, [weak_dead, weak_live, dead, live])
    }

    /// zt t32 (steps 2b / 2d): a cycle that parks its sweep leaves the dead
    /// referent in its slot, arms the gate (which blocks that read and passes
    /// the live one), and the clear after the release nulls the slot and
    /// disarms; the install then frees the referent. Kill arm: the pause
    /// clears, and no gate is ever armed.
    #[test]
    fn zt32_a_parked_sweep_clears_the_dead_referents_after_the_release() {
        use super::super::concurrent_refs::{
            ZGC_REFERENT_GATE_ARMED, ZGC_REFERENT_GATE_DISARMED, ZGC_REFERENT_GATE_NEVER,
        };
        for after_release in [true, false] {
            let (heap, [weak_dead, weak_live, dead, live]) =
                referent_clear_cycle(true, after_release);
            let (wd, wl, d, l) = (
                weak_dead.as_ptr() as usize,
                weak_live.as_ptr() as usize,
                dead.as_ptr() as usize,
                live.as_ptr() as usize,
            );
            assert!(heap.counters.csweep.pending(), "the sweep is parked");
            assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
            if !after_release {
                let slot = heap.get_field(weak_dead, 0);
                assert_eq!(slot, Value::Object(None), "kill: the pause clears");
                assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_NEVER);
                assert_eq!(heap.run_staged_referent_clear(), 0, "nothing staged");
                continue;
            }
            assert_eq!(
                heap.get_field(weak_dead, 0),
                Value::Object(Some(dead)),
                "the slot still names the dead referent at the release"
            );
            assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_ARMED);
            assert!(heap.referent_read_blocked(wd, d), "dead: blocked");
            assert!(!heap.referent_read_blocked(wl, l), "the live read passes");
            assert!(!heap.referent_read_blocked(l, d), "not a row: passes");
            assert_eq!(heap.run_staged_referent_clear(), 1);
            assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_DISARMED);
            assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
            assert_eq!(heap.run_staged_referent_clear(), 0, "idempotent");
            assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
            assert!(heap.is_object_address(d).is_none(), "the install freed it");
            assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
            let r = heap.concurrent_refs_report();
            assert_eq!(
                (r.deferred_clears, r.after_release_cleared, r.gate_blocked),
                (1, 1, 1)
            );
        }
    }

    /// zt t32 (R6, 6b): the install's clip drops every byte inside a selected
    /// region and keeps the rest, in pieces, ascending.
    #[test]
    fn zt32_the_install_lists_no_span_inside_a_planned_region() {
        let holes = [(100, 200), (300, 400)];
        assert_eq!(
            clip_spans_outside(&[(350, 100), (0, 50), (150, 200)], &holes),
            vec![(0, 50), (200, 100), (400, 50)]
        );
        assert_eq!(clip_spans_outside(&[(100, 100)], &holes), vec![]);
        assert_eq!(
            clip_spans_outside(&[(50, 400)], &holes),
            vec![(50, 50), (200, 100), (400, 50)]
        );
        assert_eq!(clip_spans_outside(&[(10, 5)], &[]), vec![(10, 5)]);
    }

    /// zt t32: a runner of the parked sweep that is not the round (here the
    /// initiator straight away) runs the staged clear before its walk, so no
    /// referent is freed while a slot names it.
    #[test]
    fn zt32_the_sweep_runner_runs_the_staged_clear_first() {
        let (heap, [weak_dead, _, dead, _]) = referent_clear_cycle(true, true);
        assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
        assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
        assert!(heap.is_object_address(dead.as_ptr() as usize).is_none());
        assert_eq!(heap.concurrent_refs_report().after_release_cleared, 1);
    }

    /// zt t32: a cycle that sweeps in its pause (the concurrent sweep off) runs
    /// the staged clear there, before its sweep; no gate is armed.
    #[test]
    fn zt32_a_cycle_that_sweeps_in_its_pause_clears_there() {
        let (heap, [weak_dead, weak_live, dead, live]) = referent_clear_cycle(false, true);
        assert!(!heap.counters.csweep.pending());
        assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
        assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
        assert!(heap.is_object_address(dead.as_ptr() as usize).is_none());
        assert_eq!(
            heap.referent_gate_state(),
            super::super::concurrent_refs::ZGC_REFERENT_GATE_NEVER
        );
        assert_eq!(heap.concurrent_refs_report().deferred_clears, 0);
    }

    /// No runner armed (a direct call, a test elsewhere in the crate): the
    /// cycle sweeps in the pause and the refusal is booked.
    #[test]
    fn zt21zc2_an_unarmed_collection_sweeps_in_the_pause() {
        let heap = test_heap(true);
        let (head, chain, _garbage) = graph(&heap, 100, 100);
        collect(&heap, head, false);
        assert!(!heap.counters.csweep.pending());
        let r = heap.concurrent_sweep_report();
        assert_eq!(
            (r.deferred, r.refused, r.last_refusal),
            (0, 1, refusal::NO_RUNNER)
        );
        assert_eq!(walk(&heap, head), chain);
        // Generational mode refuses too.
        let g = test_heap(true);
        g.set_generational_enabled(true);
        let (head, _, _) = graph(&g, 10, 10);
        collect(&g, head, true);
        assert!(!g.counters.csweep.pending());
        assert_eq!(
            g.concurrent_sweep_report().last_refusal,
            refusal::GENERATIONAL
        );
    }

    /// `needs_gc` holds while a sweep is pending (the cycle has not ended) and
    /// answers again after the install (positive control: the same gauge then
    /// asks for a collection).
    #[test]
    fn zt21zc2_needs_gc_holds_while_the_sweep_is_pending() {
        let heap = test_heap(true);
        heap.counters.warmup_enabled.store(false, Ordering::Relaxed);
        let (head, _, _) = graph(&heap, 100, 1_000);
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending());
        heap.allocated
            .fetch_add(heap.gc_threshold.saturating_mul(2), Ordering::Relaxed);
        assert!(!heap.needs_gc(), "pending: no new cycle");
        assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
        heap.allocated
            .fetch_add(heap.gc_threshold.saturating_mul(2), Ordering::Relaxed);
        assert!(
            heap.needs_gc(),
            "positive control: installed, the gauge collects"
        );
    }

    /// The next collection finishes a pending sweep before it touches the
    /// arena, and a fresh cycle then defers again.
    #[test]
    fn zt21zc2_the_next_collection_finishes_the_pending_sweep_first() {
        let heap = test_heap(true);
        let (head, chain, _) = graph(&heap, 500, 2_000);
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending());
        collect(&heap, head, true);
        let r = heap.concurrent_sweep_report();
        assert_eq!((r.by_cycle_start, r.deferred), (1, 2));
        assert!(
            heap.counters.csweep.pending(),
            "the second cycle deferred too"
        );
        assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
        assert_eq!(walk(&heap, head), chain);
    }

    /// An allocation the window cannot serve finishes the sweep and retries
    /// instead of failing (the allocation stall). Control: the same request
    /// after the install needs no retry.
    #[test]
    fn zt21zc2_an_allocation_short_of_space_finishes_the_sweep_and_retries() {
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.set_relocation_enabled(false);
        // zt t27/gn5b: single generation, as `test_heap` -- the mode refuses
        // the deferred sweep (`refusal::GENERATIONAL`).
        heap.set_generational_enabled(false);
        heap.counters.csweep.enabled.store(true, Ordering::Relaxed);
        let (head, chain, _) = graph(&heap, 10, 0);
        // Fill the arena with garbage arrays, leaving little bump space.
        let mut filled = 0usize;
        while filled < 13 * 1024 * 1024 {
            if heap
                .try_alloc_array(ClassId::new(54), ArrayElementType::Byte, 4096)
                .is_none()
            {
                break;
            }
            filled += 4096;
        }
        // The fill may have raised the urgent latches; this test is about the
        // window, not about an urgent cycle (which sweeps in the pause).
        heap.headroom_low.store(false, Ordering::Relaxed);
        heap.hard_alloc_failure.store(false, Ordering::Relaxed);
        collect(&heap, head, true);
        assert!(heap.counters.csweep.pending());
        let big = heap.try_alloc_array(ClassId::new(55), ArrayElementType::Byte, 6 * 1024 * 1024);
        assert!(big.is_some(), "served after the sweep it finished");
        let r = heap.concurrent_sweep_report();
        assert!(r.alloc_retries >= 1 && r.by_allocation == 1, "{r:?}");
        assert!(!heap.counters.csweep.pending());
        assert_eq!(walk(&heap, head), chain);
    }
}
