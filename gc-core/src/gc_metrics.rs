// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Remembered-set / card-table cost counters, and the per-cycle
//! **collector-decision record**.
//!
//! Two independent observability gaps live here because they answer the same
//! operational question — *what did the collector actually do, and what did it
//! cost?* — and neither can be answered from the source alone.
//!
//! # 1. Card / remembered-set costs
//!
//! The generational write barrier ([`crate::gen_heap::GenerationalHeap::write_barrier`])
//! and the dirty-card scan ([`crate::gen_heap::GenerationalHeap::scan_dirty_cards`])
//! are the whole cross-generational machinery, and before this module nothing
//! counted either of them. "Is the card table paying for itself?" could only be
//! answered by a profiler run, and "how many old→young edges does this workload
//! actually have?" could not be answered at all.
//!
//! ## Hot-path cost, and why the barrier counter is gated
//!
//! [`record_card_mark`] sits on the tail of `write_barrier`, which runs on
//! **every reference store in the VM**. An unconditional
//! `fetch_add(1, Relaxed)` there is a locked read-modify-write on a single
//! process-global cache line: uncontended it is ~5-20 cycles, but under N
//! mutators storing references concurrently it is a guaranteed cache-line
//! ping-pong on a line that has no other reason to be shared. That is a real
//! multi-threaded throughput regression paid by every workload, in exchange for
//! a number almost no run wants.
//!
//! So the barrier counter is gated on `CRATONVM_GC_CARD_METRICS` (default
//! **off**). When off the cost is one `Relaxed` load of an already-resolved
//! `AtomicU8` — a plain `mov` from a line that is read-only-shared and
//! therefore replicated in every core's cache — plus a perfectly-predicted
//! not-taken branch. That is under a cycle in steady state and adds no
//! coherence traffic.
//!
//! Note also **what the gate sees** (corrected 2026-09-23, gen r4/plumbing).
//! This paragraph used to say the JIT emits its own inline post-write barrier
//! (`jit_card_table_info` → a direct byte-store into the card bitmap) that
//! never enters `write_barrier`, so `card_marks_executed` counted *interpreter
//! and native* marks only. By default compiled old-to-young stores call
//! `jit_putfield_object` / `jit_write_barrier`, which reach `write_barrier`
//! like every other store, so `card_marks_executed` is EVERY card mark on this
//! backend.
//!
//! **Except under `CRATONVM_JIT_INLINE_CARD_MARK=1`** (gen r4w4/cards4, default
//! OFF; corrected gen r4w6/review6, 2026-09-24 — this paragraph used to cite an
//! emitter, `emit_inline_card_mark_regs`, that no longer exists, and a
//! hard-`false` `inline_card_mark_available()` that is now the flag). With the
//! flag the x64 JIT emits the post barrier inline
//! (`jit/src/x64/objects.rs::emit_gen_card_barrier`): it checks the card itself
//! and calls the collector's barrier only for a CLEAN card. So under that flag
//! `card_marks_executed` counts compiled code's clean→dirty TRANSITIONS, not its
//! card-marking stores, `duplicate_card_marks_barrier` sees almost none of
//! compiled code's duplicates (the inline check absorbed them), and the ratio
//! of the two is no longer the waste fraction of every mark — only of the
//! interpreter's and natives'. What the barrier counters do not see is
//! the compiled stores the JIT's reference-store gate proves need no card
//! (a receiver without `GC_FLAG_OLD_GEN`, see `JitRefStoreGates::post_skip_mask`)
//! — those skip the call, so `barrier_ref_stores` is the stores that reached
//! the Rust barrier, not all reference stores. The collector-side counters
//! below are complete regardless, because every card ultimately funnels
//! through `take_dirty_cards`.
//!
//! ## Collector-side counters are UNGATED
//!
//! Everything else here is bumped **once per collection** (or once per drain),
//! not per store: cards found dirty at scan, buffered duplicate marks,
//! remembered-set bytes retained, old→young edge count, refinement time. A
//! handful of relaxed adds per GC pause is unmeasurable against a pause, and
//! gating them would make the default report empty — which is the failure mode
//! this whole item exists to prevent. See `tlab-and-card-audit.md`.
//!
//! **One exception, and it is deliberate**:
//! `duplicate_card_marks_barrier` is a *per-store* counter and is therefore
//! gated with `card_marks_executed`, not with the collector-side block. That
//! is not a cost decision so much as an arithmetic one — it is the numerator
//! whose denominator is `card_marks_executed`, and a ratio whose two terms are
//! armed by different flags is not a ratio. Its buffered sibling
//! `duplicate_card_marks_buffered` stays ungated because it *is* per-drain.
//! Keeping them apart is the whole of
//! `gengc-plumbing-duplicate-card-mark-denominator-FIXED-20260923.md`: they were one
//! counter, the sum was divided by a third population, and the result looked
//! like a duplicate rate.
//!
//! # 2. The collector-decision record
//!
//! `docs/GC.md` and `ARCHITECTURE.md` disagree about whether a young collection
//! moves. [`record_collector_decision`] is called from the exact branch in
//! `collect_garbage_inner` that decides, so [`collector_decision_report`] is
//! ground truth rather than a third prose claim. When the cycle fell back to
//! the non-moving sweep it carries the
//! [`crate::gc_quiescence::incomplete_reason`] code that forced it.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Every counter in one cache-adjacent block.
///
/// Process-global in production. Thread-local under `cfg(test)` for the same
/// reason as every counter in [`crate::gc_quiescence`]: the gc unit tests run in
/// parallel threads of one process, and a global tally would let one test's
/// deliberate increments break another's arithmetic assertion.
struct Counters {
    /// Card marks actually executed by the *Rust-side* write barrier. Gated —
    /// see the module header.
    card_marks_executed: AtomicU64,
    /// Reference stores that reached the barrier's cross-generation test.
    /// Gated. `card_marks_executed / barrier_ref_stores` is the barrier's hit
    /// rate: a low ratio means the barrier is mostly paying for nothing.
    barrier_ref_stores: AtomicU64,
    /// Distinct cards handed to the collector by `take_dirty_cards`, summed
    /// over every scan. Ungated.
    cards_found_dirty: AtomicU64,
    /// Buffered offsets that resolved to a card already dirty (`drain_pending`
    /// saw `pending.len()` offsets but only `newly_dirtied` clean→dirty
    /// transitions). Ungated. This is the pure waste in the buffered path.
    ///
    /// **Structurally zero in production since gc-genpause F5.2** moved the
    /// mutator barrier off the buffered pipeline: `drain_pending` drains empty
    /// buffers. It is kept because the pipeline is kept (see
    /// `gengc-oldgen-dead-buffered-card-pipeline-FIXED-20260923.md`) and because a
    /// non-zero value here is exactly the signal that something started
    /// buffering again. It has **no ratio**: its honest denominator would be
    /// `pending.len()`, and a counter recording the size of a queue nothing
    /// fills would be inert instrumentation.
    duplicate_card_marks_buffered: AtomicU64,
    /// Rust-barrier card marks that found the card already dirty
    /// (`CardTable::mark_dirty_lockfree`'s else-arm). **Gated, exactly like
    /// [`Counters::card_marks_executed`]** — same flag, same branch shape — and
    /// that symmetry is what makes the pair divisible:
    /// `GenerationalHeap::write_barrier` calls
    /// [`record_card_mark`] on *both* arms of that branch, so
    /// `duplicate_card_marks_barrier / card_marks_executed` is the fraction of
    /// Rust-barrier marks that were pure waste. Like `card_marks_executed` it
    /// sees every card mark by default; under `CRATONVM_JIT_INLINE_CARD_MARK=1`
    /// compiled code checks the card inline and reaches the barrier only for a
    /// clean one (see the module header, corrected 2026-09-23 and again by gen
    /// r4w6/review6).
    duplicate_card_marks_barrier: AtomicU64,
    /// Bytes of remembered-set metadata currently retained (gauge, not a
    /// total). For Generational this is the card byte-map plus the dirty-index
    /// tracking list plus the undrained pending queue.
    remembered_set_bytes: AtomicU64,
    /// Old→young reference slots discovered by the dirty-card scan, summed
    /// over every scan. Ungated.
    old_to_young_edges: AtomicU64,
    /// G1 post-evacuation CSet verification (audit I-6 / §9 item 2). Objects
    /// the verifier actually walked, summed over every pause; the pauses it
    /// ran in; and the number of dangling-into-freed-CSet references it found.
    ///
    /// These exist so "the remembered set is complete" stops being a review
    /// claim. The verifier used to run only under `debug_assertions` or the
    /// verify flag, i.e. never in a shipping build, which meant the one direct
    /// check of I-6 produced no evidence at all in the configuration anyone
    /// actually runs. `objects` against `live_bytes` is what says how much of
    /// that claim a given run has actually tested.
    cset_verify_objects: AtomicU64,
    cset_verify_pauses: AtomicU64,
    cset_verify_dangling: AtomicU64,
    /// Pauses in which the verifier hit its budget and stopped early, so the
    /// pass covered only part of the heap. Coverage accumulates across pauses
    /// via a rotating start cursor, but a run whose budget is always exhausted
    /// has never verified the whole heap in one pause, and the difference
    /// matters when reading a zero `cset_verify_dangling`.
    cset_verify_truncated: AtomicU64,
    /// LANE W4-B — occupied BYTES the verifier walked, the denominator the
    /// object count could not be.
    ///
    /// `objects` says how much the pass did; it does not say how much of the
    /// heap that was, because the population is a number of bytes and the mean
    /// object size is workload-specific. With this beside it, the census can
    /// report a coverage FRACTION, and a coverage fraction is the only form in
    /// which "the sampler rotates, so coverage accumulates" is a checkable
    /// claim rather than a hope. It was not: measured on `HumongousChurn`, the
    /// flat 4096-object budget covers 0.31 % of a 160 MiB heap per pause and
    /// 0.065 % of a 768 MiB one — a sweep period that grows linearly with the
    /// live set. See `g1::verify_sweep_pauses`.
    cset_verify_bytes: AtomicU64,
    /// LANE W4-B — pauses whose verify pass ran UNBOUNDED (whole heap).
    ///
    /// Not a tuning number; a trap detector. The budget is waived under
    /// `debug_assertions` or `--verbose:gc`, and `--verbose:gc` is also how
    /// most people make this census print — so the ordinary way to look at the
    /// sampler switches the sampler off, and `budget_truncated=0` then reads
    /// as "the budget was never reached" when it means "there was no budget".
    /// One run, two doors: `--verbose:gc` walked 6 547 331 objects with
    /// `budget_truncated=0`; `CRATONVM_GC_STATS=1` walked 24 576 with
    /// `budget_truncated=6`. Without this field those two lines are
    /// indistinguishable in kind.
    cset_verify_unbounded: AtomicU64,
    /// Remembered sets that gave up naming individual source regions and
    /// coarsened to "any region may point into me" (audit §9 item 5). A
    /// non-zero value means some pause after it walked every plausible source
    /// region wholesale, which is correct but is the expensive arm.
    rset_coarsened: AtomicU64,
    /// Humongous spans reclaimed by an evacuation pause rather than by a
    /// concurrent-mark cleanup (`CRATONVM_G1_EAGER_HUMONGOUS`), and the bytes
    /// they held.
    ///
    /// Worth a counter of its own because eager reclaim is the ONLY path that
    /// frees memory outside the collection set. When a humongous object goes
    /// missing, the first question is which of the two reclaimers took it, and
    /// `spans` answers it without a rebuild.
    /// Ten-findings item 1 — spans decided LIVE by this pause's own scan
    /// marks, so their remembered set was never read and their sources never
    /// walked. The engagement counter for the marking half: a run where this
    /// stays 0 while `humongous_eager_walked_sources` climbs is one where the
    /// marking bought nothing and the walk is still doing all the work.
    humongous_eager_marked: AtomicU64,
    /// Source regions the undecided-span walk actually read.
    /// Spans the ROOT/finalizer seed decided (not the scan marking).
    humongous_eager_root_seeded: AtomicU64,
    humongous_eager_walked_sources: AtomicU64,
    humongous_eager_spans: AtomicU64,
    humongous_eager_bytes: AtomicU64,
    /// Pauses that had eager reclaim enabled but declined to run it, because
    /// some precondition (a mark cycle in flight, an evacuation failure, an
    /// aborted region walk) made "unreferenced" untrustworthy. A high ratio
    /// against `humongous_eager_spans` is why a heap is not reclaiming.
    humongous_eager_declined: AtomicU64,
    /// Nanoseconds spent in card refinement (flush + drain + dirty-card scan).
    /// Ungated.
    refinement_nanos: AtomicU64,
    /// Number of refinement passes the nanos above are spread over.
    refinement_passes: AtomicU64,
    /// Objects allocated since startup (gauge, republished from `HeapStats` at
    /// report time).
    ///
    /// gen r4/plumbing (2026-09-23): on the generational backend this is NOT
    /// every object. It is `HeapStats::young_allocations + old_allocations`,
    /// and the young half counts arena SLOW-path allocations only — every
    /// object bump-allocated inside a TLAB (the interpreter's and the JIT's
    /// `new`) is missing — so the `*_per_allocated_object` ratios divide by an
    /// undercount and read HIGH. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    allocated_objects: AtomicU64,
    /// Bytes allocated (gauge). On the generational backend this is the
    /// CURRENT occupancy (`young used + old used`), not a since-startup total
    /// — same page as above.
    allocated_bytes: AtomicU64,
    /// Live bytes as of the last completed cycle (gauge).
    live_bytes: AtomicU64,
    /// gen r5w2/obs6 — bytes ALLOCATED since startup, cumulative and
    /// monotone: the VM's per-VM `HeapRealm::bytes_allocated_total` (every
    /// allocation door feeds it; TLAB-resident bytes at retire granularity),
    /// published at the top of every collection by
    /// [`record_vm_allocation_totals`]. The honest denominator
    /// `allocated_objects` / `allocated_bytes` are not
    /// (`gengc-r4-plumbing-alloc-denominators-are-not-allocation`, step 1).
    /// `0` until a VM publishes.
    allocated_bytes_total: AtomicU64,
    /// Collections triggered due to GPU VRAM pressure (`gpu_coordination`).
    gc_triggered_by_vram_pressure: AtomicU64,
}

impl Counters {
    pub const fn new() -> Self {
        Self {
            card_marks_executed: AtomicU64::new(0),
            barrier_ref_stores: AtomicU64::new(0),
            cards_found_dirty: AtomicU64::new(0),
            duplicate_card_marks_buffered: AtomicU64::new(0),
            duplicate_card_marks_barrier: AtomicU64::new(0),
            remembered_set_bytes: AtomicU64::new(0),
            old_to_young_edges: AtomicU64::new(0),
            cset_verify_objects: AtomicU64::new(0),
            cset_verify_bytes: AtomicU64::new(0),
            cset_verify_unbounded: AtomicU64::new(0),
            cset_verify_pauses: AtomicU64::new(0),
            cset_verify_dangling: AtomicU64::new(0),
            cset_verify_truncated: AtomicU64::new(0),
            rset_coarsened: AtomicU64::new(0),
            humongous_eager_marked: AtomicU64::new(0),
            humongous_eager_root_seeded: AtomicU64::new(0),
            humongous_eager_walked_sources: AtomicU64::new(0),
            humongous_eager_spans: AtomicU64::new(0),
            humongous_eager_bytes: AtomicU64::new(0),
            humongous_eager_declined: AtomicU64::new(0),
            refinement_nanos: AtomicU64::new(0),
            refinement_passes: AtomicU64::new(0),
            allocated_objects: AtomicU64::new(0),
            allocated_bytes: AtomicU64::new(0),
            live_bytes: AtomicU64::new(0),
            allocated_bytes_total: AtomicU64::new(0),
            gc_triggered_by_vram_pressure: AtomicU64::new(0),
        }
    }
}

#[cfg(not(any(test, feature = "test-hooks")))]
static COUNTERS: Counters = Counters::new();

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
fn with_counters<R>(f: impl FnOnce(&Counters) -> R) -> R {
    f(&COUNTERS)
}

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static COUNTERS: Counters = const { Counters::new() };
}

#[cfg(any(test, feature = "test-hooks"))]
#[inline]
fn with_counters<R>(f: impl FnOnce(&Counters) -> R) -> R {
    COUNTERS.with(f)
}

// ---------------------------------------------------------------------------
// Hot-path gate
// ---------------------------------------------------------------------------

const GATE_UNRESOLVED: u8 = 0;
const GATE_OFF: u8 = 1;
const GATE_ON: u8 = 2;

#[cfg(not(any(test, feature = "test-hooks")))]
static HOT_PATH_GATE: AtomicU8 = AtomicU8::new(GATE_UNRESOLVED);

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static HOT_PATH_GATE: AtomicU8 = const { AtomicU8::new(GATE_UNRESOLVED) };
}

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
fn gate_load() -> u8 {
    HOT_PATH_GATE.load(Ordering::Relaxed)
}

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
fn gate_store(v: u8) {
    HOT_PATH_GATE.store(v, Ordering::Relaxed);
}

#[cfg(any(test, feature = "test-hooks"))]
#[inline]
fn gate_load() -> u8 {
    HOT_PATH_GATE.with(|g| g.load(Ordering::Relaxed))
}

#[cfg(any(test, feature = "test-hooks"))]
#[inline]
fn gate_store(v: u8) {
    HOT_PATH_GATE.with(|g| g.store(v, Ordering::Relaxed));
}

/// Whether the per-reference-store barrier counters are armed.
///
/// **This is the predicate that keeps an unconditional atomic increment off a
/// barrier that runs on every reference store.** In the default (off) build it
/// costs one relaxed load of a read-only-shared byte plus a not-taken branch.
/// Opt in with `CRATONVM_GC_CARD_METRICS=1`.
#[inline]
pub fn hot_path_counters_enabled() -> bool {
    match gate_load() {
        GATE_ON => true,
        GATE_OFF => false,
        _ => resolve_hot_path_gate(),
    }
}

#[cold]
fn resolve_hot_path_gate() -> bool {
    // `runtime_flag_on`, not `runtime_var_os(..).is_some()`: under presence
    // semantics `CRATONVM_GC_CARD_METRICS=0` ARMED the counters (gc-common
    // w1-e small finding 3, fixed w2-e). The grouped spelling
    // `CRATONVM_GC=card-metrics` / `-card-metrics` inserts / removes the key
    // and reads the same either way.
    let on = false;
    gate_store(if on { GATE_ON } else { GATE_OFF });
    on
}

/// Arm or disarm the barrier counters programmatically.
///
/// Exists because the typed-flag layer latches on first use, so a test cannot
/// arm the gate by setting the environment variable after startup (the
/// "declared flags latch" trap). Production never calls this — the environment
/// resolves the gate exactly once.
pub fn set_hot_path_counters_enabled(on: bool) {
    gate_store(if on { GATE_ON } else { GATE_OFF });
}

// ---------------------------------------------------------------------------
// Recording — hot path (gated)
// ---------------------------------------------------------------------------

/// Record that the Rust write barrier reached its cross-generation test with a
/// reference-typed value. Gated; see [`hot_path_counters_enabled`].
#[inline]
pub fn record_barrier_ref_store() {
    if hot_path_counters_enabled() {
        with_counters(|c| {
            c.barrier_ref_stores.fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Record one card mark executed by the Rust write barrier. Gated; see
/// [`hot_path_counters_enabled`].
///
/// Sees compiled-code marks too: the JIT's inline card store is never
/// selected (`inline_card_mark_available()` is hard-`false`), so compiled
/// old-to-young stores reach this barrier through `jit_putfield_object` /
/// `jit_write_barrier`. (This doc said the opposite until 2026-09-23.)
#[inline]
pub fn record_card_mark() {
    if hot_path_counters_enabled() {
        with_counters(|c| {
            c.card_marks_executed.fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Record one Rust-barrier card mark that found the card **already dirty**.
///
/// Called from `CardTable::mark_dirty_lockfree`'s else-arm, which is the only
/// place left in the system that can tell a mark which dirtied a clean card
/// from one that hit a dirty card.
///
/// Gated *here*, internally, rather than at the call site — deliberately. This
/// counter's whole value is that it divides by [`record_card_mark`]'s
/// `card_marks_executed`, and that division is only honest while the two are
/// armed and disarmed together. Making the gate part of the recorder rather
/// than a convention the call site has to remember is what keeps the
/// numerator and the denominator on the same clock; see
/// `docs/feature-designs/jit-barrier-lowering.md`
/// for what happened when they were not.
///
/// Cost when disarmed is [`record_card_mark`]'s: one relaxed load of a
/// read-only-shared byte and a perfectly-predicted not-taken branch.
#[inline]
pub fn record_duplicate_card_mark_barrier() {
    if hot_path_counters_enabled() {
        with_counters(|c| {
            c.duplicate_card_marks_barrier
                .fetch_add(1, Ordering::Relaxed);
        });
    }
}

// ---------------------------------------------------------------------------
// Recording — collector side (ungated, once per cycle / per drain)
// ---------------------------------------------------------------------------

/// Record `n` distinct cards handed to a dirty-card scan.
pub fn record_cards_found_dirty(n: u64) {
    with_counters(|c| {
        c.cards_found_dirty.fetch_add(n, Ordering::Relaxed);
    });
}

/// Record `n` **buffered** offsets that landed on a card that was already
/// dirty.
///
/// Called from `CardTable::drain_pending`, which knows both the number of
/// offsets it consumed and the number of clean→dirty transitions it caused.
/// That is its only caller, and it drains empty buffers in production — see
/// [`GcMetricsRaw::duplicate_card_marks_buffered`].
///
/// **Not the barrier's duplicates.** Those go to
/// [`record_duplicate_card_mark_barrier`], which is gated and has a real
/// denominator. The name is kept unchanged because this function is `pub` in a
/// `pub mod` of a library crate.
pub fn record_duplicate_card_marks(n: u64) {
    with_counters(|c| {
        c.duplicate_card_marks_buffered
            .fetch_add(n, Ordering::Relaxed);
    });
}

/// Record `n` old→young reference slots found by a dirty-card scan.
pub fn record_old_to_young_edges(n: u64) {
    with_counters(|c| {
        c.old_to_young_edges.fetch_add(n, Ordering::Relaxed);
    });
}

/// Publish the currently-retained remembered-set metadata size (a **gauge** —
/// the last value wins, it is not accumulated).
pub fn record_remembered_set_bytes(bytes: u64) {
    with_counters(|c| {
        c.remembered_set_bytes.store(bytes, Ordering::Relaxed);
    });
}

/// Record one G1 post-evacuation CSet verification pass.
///
/// `objects` is how many objects the pass actually walked, `dangling` how many
/// references into a freed CSet region with no forwarding entry it found, and
/// `truncated` whether it stopped on its budget rather than on the end of the
/// heap. See the counter docs for why a *coverage* number is the point: a zero
/// `dangling` from a pass that walked 300 objects of a 2 M-object heap is not
/// the same statement as a zero from a full sweep, and before this the two were
/// indistinguishable because neither was published.
pub fn record_g1_cset_verify(
    objects: u64,
    bytes: u64,
    dangling: u64,
    truncated: bool,
    unbounded: bool,
) {
    with_counters(|c| {
        c.cset_verify_objects.fetch_add(objects, Ordering::Relaxed);
        c.cset_verify_bytes.fetch_add(bytes, Ordering::Relaxed);
        c.cset_verify_pauses.fetch_add(1, Ordering::Relaxed);
        c.cset_verify_dangling
            .fetch_add(dangling, Ordering::Relaxed);
        if truncated {
            c.cset_verify_truncated.fetch_add(1, Ordering::Relaxed);
        }
        if unbounded {
            c.cset_verify_unbounded.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Record an evacuation pause's eager humongous reclaim.
///
/// `spans == 0` with `declined == false` is the ordinary "nothing was dead"
/// outcome; `declined == true` means the pause never asked the question.
/// Ten-findings item 1 — record how the pause decided span liveness.
pub fn record_g1_humongous_liveness(marked: u64, seeded_by_roots: u64, walked_sources: u64) {
    with_counters(|c| {
        c.humongous_eager_marked
            .fetch_add(marked, Ordering::Relaxed);
        c.humongous_eager_root_seeded
            .fetch_add(seeded_by_roots, Ordering::Relaxed);
        c.humongous_eager_walked_sources
            .fetch_add(walked_sources, Ordering::Relaxed);
    });
}

pub fn record_g1_eager_humongous(spans: u64, bytes: u64, declined: bool) {
    with_counters(|c| {
        c.humongous_eager_spans.fetch_add(spans, Ordering::Relaxed);
        c.humongous_eager_bytes.fetch_add(bytes, Ordering::Relaxed);
        if declined {
            c.humongous_eager_declined.fetch_add(1, Ordering::Relaxed);
        }
    });
}

/// Record that one remembered set coarsened (audit §9 item 5).
pub fn record_g1_rset_coarsened() {
    with_counters(|c| {
        c.rset_coarsened.fetch_add(1, Ordering::Relaxed);
    });
}

/// Record one refinement pass of `nanos` nanoseconds (card buffer flush +
/// pending drain + dirty-card scan).
pub fn record_refinement(nanos: u64) {
    with_counters(|c| {
        c.refinement_nanos.fetch_add(nanos, Ordering::Relaxed);
        c.refinement_passes.fetch_add(1, Ordering::Relaxed);
    });
}

/// Publish the allocation / occupancy denominators the normalized view divides
/// by (**gauges**). Called by the collector at the end of every cycle, and
/// again by the end-of-run summary so a report taken outside a pause is current.
pub fn record_heap_occupancy(allocated_objects: u64, allocated_bytes: u64, live_bytes: u64) {
    with_counters(|c| {
        c.allocated_objects
            .store(allocated_objects, Ordering::Relaxed);
        c.allocated_bytes.store(allocated_bytes, Ordering::Relaxed);
        c.live_bytes.store(live_bytes, Ordering::Relaxed);
    });
}

/// Publish the VM's cumulative allocation total (**monotone gauge**): bytes
/// allocated since startup across every allocation door, as the VM counts
/// them (`HeapRealm::bytes_allocated_total`). The VM calls this at the top of
/// every collection (`gc_events.rs::gc_event_start`), so an end-of-run report
/// shows the total as of the last collection (publishing once more before
/// `print_gc_summary`, in `vm-cli`, is a recorded follow-up). Rendered as
/// `allocated_bytes_total=` on the `[GC] cards/alloc:` line beside the two
/// legacy denominators.
///
/// Monotone by construction: a smaller value (a second VM in the same
/// process publishing its own, smaller total into these process-wide
/// counters) never lowers it — the figure is then the largest VM's, which is
/// no worse than the legacy gauges' last-writer-wins. gen r5w2/obs6,
/// `gengc-r4-plumbing-alloc-denominators-are-not-allocation` step 1.
pub fn record_vm_allocation_totals(bytes: u64) {
    with_counters(|c| {
        c.allocated_bytes_total.fetch_max(bytes, Ordering::Relaxed);
    });
}

/// Record one collection triggered by GPU VRAM pressure.
pub fn record_vram_pressure_gc() {
    with_counters(|c| {
        c.gc_triggered_by_vram_pressure
            .fetch_add(1, Ordering::Relaxed);
    });
}

/// Reset every counter. Tests only — production counters are monotonic for the
/// life of the process.
///
/// **Every** field of [`Counters`], not a subset — twenty-three as of
/// 2026-09-21, when `duplicate_card_marks` split into a buffered and a barrier
/// half. Until 2026-09-20 this reset eleven of the twenty-two and silently left
/// the other eleven
/// (`cset_verify_*`, `rset_coarsened`, `humongous_eager_*`) carrying whatever
/// an earlier test in the same binary had put there — so a test that called
/// this and then asserted on `gc_metrics_raw()` was asserting test ORDERING for
/// those fields. A partial reset is worse than none: it reads as a clean slate.
/// If a counter is added to `Counters` it must be added here too.
pub fn reset_metrics_for_test() {
    with_counters(|c| {
        c.card_marks_executed.store(0, Ordering::Relaxed);
        c.barrier_ref_stores.store(0, Ordering::Relaxed);
        c.cards_found_dirty.store(0, Ordering::Relaxed);
        c.duplicate_card_marks_buffered.store(0, Ordering::Relaxed);
        c.duplicate_card_marks_barrier.store(0, Ordering::Relaxed);
        c.remembered_set_bytes.store(0, Ordering::Relaxed);
        c.old_to_young_edges.store(0, Ordering::Relaxed);
        c.cset_verify_objects.store(0, Ordering::Relaxed);
        c.cset_verify_pauses.store(0, Ordering::Relaxed);
        c.cset_verify_dangling.store(0, Ordering::Relaxed);
        c.cset_verify_truncated.store(0, Ordering::Relaxed);
        // The 2026-09-20 G1 round added these two and did not add them
        // here. The gap was invisible until the fixture above was
        // widened to move them, which is the whole point of that
        // fixture: a counter `reset` skips turns every later assertion
        // on it into an assertion about test ordering.
        c.cset_verify_bytes.store(0, Ordering::Relaxed);
        c.cset_verify_unbounded.store(0, Ordering::Relaxed);
        c.rset_coarsened.store(0, Ordering::Relaxed);
        c.humongous_eager_marked.store(0, Ordering::Relaxed);
        c.humongous_eager_root_seeded.store(0, Ordering::Relaxed);
        c.humongous_eager_walked_sources.store(0, Ordering::Relaxed);
        c.humongous_eager_spans.store(0, Ordering::Relaxed);
        c.humongous_eager_bytes.store(0, Ordering::Relaxed);
        c.humongous_eager_declined.store(0, Ordering::Relaxed);
        c.refinement_nanos.store(0, Ordering::Relaxed);
        c.refinement_passes.store(0, Ordering::Relaxed);
        c.allocated_objects.store(0, Ordering::Relaxed);
        c.allocated_bytes.store(0, Ordering::Relaxed);
        c.live_bytes.store(0, Ordering::Relaxed);
        c.allocated_bytes_total.store(0, Ordering::Relaxed);
        c.gc_triggered_by_vram_pressure.store(0, Ordering::Relaxed);
    });
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// The raw counter values, before normalization.
///
/// Split out from [`GcMetricsReport`] so the normalization arithmetic is a pure
/// function of plain numbers ([`GcMetricsReport::from_raw`]) and can be tested
/// without touching any global.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcMetricsRaw {
    pub card_marks_executed: u64,
    pub barrier_ref_stores: u64,
    pub cards_found_dirty: u64,
    /// `duplicate_card_marks_buffered + duplicate_card_marks_barrier`.
    ///
    /// A **derived** total, not a counter: nothing increments it, and
    /// [`gc_metrics_raw`] computes it from the two halves below so the three
    /// can never drift apart. Kept, with its value unchanged from before the
    /// 2026-09-21 split, because [`GcMetricsRaw`] is `pub` and re-exported from
    /// `lib.rs` to embedders.
    ///
    /// **Do not divide by anything with this.** Its two halves are on
    /// different clocks — one ungated per-drain, one gated per-store — so any
    /// ratio built on the sum divides a mixed population. Use
    /// [`GcMetricsReport::barrier_duplicate_mark_ratio`], or the halves.
    pub duplicate_card_marks: u64,
    /// Buffered-path duplicates, from `CardTable::drain_pending`. Ungated, and
    /// structurally zero in production since the mutator barrier stopped
    /// buffering — a non-zero value means something started again.
    pub duplicate_card_marks_buffered: u64,
    /// Rust-barrier duplicates, from `CardTable::mark_dirty_lockfree`'s
    /// already-dirty arm. Gated exactly like `card_marks_executed`, which is
    /// its denominator.
    pub duplicate_card_marks_barrier: u64,
    pub remembered_set_bytes: u64,
    pub old_to_young_edges: u64,
    pub cset_verify_objects: u64,
    pub cset_verify_pauses: u64,
    pub cset_verify_dangling: u64,
    pub cset_verify_truncated: u64,
    pub cset_verify_bytes: u64,
    pub cset_verify_unbounded: u64,
    pub rset_coarsened: u64,
    pub humongous_eager_marked: u64,
    pub humongous_eager_root_seeded: u64,
    pub humongous_eager_walked_sources: u64,
    pub humongous_eager_spans: u64,
    pub humongous_eager_bytes: u64,
    pub humongous_eager_declined: u64,
    pub refinement_nanos: u64,
    pub refinement_passes: u64,
    pub allocated_objects: u64,
    pub allocated_bytes: u64,
    pub live_bytes: u64,
    /// gen r5w2/obs6 — cumulative bytes allocated since startup, as the VM
    /// counts them ([`record_vm_allocation_totals`]); `0` until published.
    /// Unlike `allocated_bytes` (an occupancy on the generational backend) it
    /// never falls.
    pub allocated_bytes_total: u64,
    /// Collections triggered due to GPU VRAM pressure.
    pub gc_triggered_by_vram_pressure: u64,
}

/// Card / remembered-set costs, raw and normalized per allocated object and
/// per live byte.
///
/// Every ratio is `0.0` when its denominator is zero — a report taken before
/// the first collection reads as "no cost observed", never as `NaN`/`inf`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GcMetricsReport {
    /// Whether the per-reference-store barrier counters were armed. When
    /// `false`, `card_marks_executed` / `barrier_ref_stores` (and the two
    /// ratios derived from them) are **not measured**, not zero-valued.
    pub hot_path_counters_enabled: bool,
    pub raw: GcMetricsRaw,

    // --- normalized: per allocated object -------------------------------
    /// Rust-side card marks per object allocated. Only meaningful when
    /// `hot_path_counters_enabled`.
    pub card_marks_per_allocated_object: f64,
    /// Dirty cards the collector had to scan, per object allocated. This is the
    /// number that says whether the card table is tracking the workload's
    /// mutation rate or its allocation rate.
    pub dirty_cards_per_allocated_object: f64,
    /// Old→young edges discovered, per object allocated.
    pub old_to_young_edges_per_allocated_object: f64,

    // --- normalized: per MiB allocated (gen r5w5/sizer9) ----------------
    //
    // Step 3 of `gengc-r4-plumbing-alloc-denominators-are-not-allocation`: the
    // three `*_per_allocated_object` ratios above divide by
    // `allocated_objects`, which on the Generational heap counts slow-path
    // hand-outs only (every TLAB and JIT inline allocation is missing), so
    // they read high by the TLAB hit rate. There is no honest object count to
    // re-base them on — counting objects would put a counter on the bump
    // path — so these divide by the VM's cumulative
    // `allocated_bytes_total` instead, in MiB. `0.0` until a VM publishes it.
    // The per-object fields stay (this struct is `pub` and re-exported to
    // embedders) but are no longer rendered.
    /// Rust-side card marks per MiB allocated. Only meaningful when
    /// `hot_path_counters_enabled`.
    pub card_marks_per_alloc_mib: f64,
    /// Dirty cards the collector scanned, per MiB allocated.
    pub dirty_cards_per_alloc_mib: f64,
    /// Old→young edges discovered, per MiB allocated.
    pub old_to_young_edges_per_alloc_mib: f64,

    // --- normalized: per live byte --------------------------------------
    /// Remembered-set metadata bytes retained per live heap byte. The card
    /// table's space overhead, measured rather than assumed.
    pub remembered_set_bytes_per_live_byte: f64,
    /// Refinement nanoseconds per live heap byte.
    pub refinement_nanos_per_live_byte: f64,
    /// Old→young edges per live byte.
    pub old_to_young_edges_per_live_byte: f64,

    // --- structural ratios ----------------------------------------------
    /// Old→young edges per dirty card scanned. **Edge density.** A value near
    /// zero means the scan is walking cards that hold no cross-generational
    /// reference at all — the card granularity is too coarse, or cards are
    /// staying dirty across cycles.
    pub old_to_young_edge_density: f64,
    /// **SUPERSEDED 2026-09-21 by [`Self::barrier_duplicate_mark_ratio`].
    /// Read that one.** This field is frozen, arithmetic and value, and is
    /// kept only because [`GcMetricsReport`] is `pub` and re-exported from
    /// `lib.rs` to embedders (`cratonvm-embed`, `libcratonvm`), so removing a
    /// field is a breaking change to a library crate. It is not marked
    /// `#[deprecated]` because this crate builds under `clippy -D warnings`
    /// and its own `Display` would then have to `allow` its way past the
    /// attribute, which is a worse signal than this paragraph.
    ///
    /// It computes
    /// `duplicate_card_marks / (duplicate_card_marks + cards_found_dirty)`,
    /// and **it is not "the fraction of card marks that hit an already-dirty
    /// card"**, which is what it claimed to be until 2026-09-20. The terms
    /// come from different populations and different clocks:
    ///
    /// * `duplicate_card_marks` is now a *sum* of a per-drain ungated half and
    ///   a per-store gated half (see [`GcMetricsRaw::duplicate_card_marks`]);
    /// * `cards_found_dirty` is `dirty_indices.len()` from the dirty-card
    ///   *scan*, per collection, and it counts cards marked by ANY route —
    ///   the buffered pipeline as well as the direct Rust barrier.
    ///
    /// Its rendered key spells its arithmetic (`dup_over_dup_plus_scanned=`)
    /// so the log line cannot be mistaken for the quantity it is not.
    pub duplicate_mark_ratio: f64,
    /// `duplicate_card_marks_barrier / card_marks_executed` — **the fraction
    /// of Rust-barrier card marks that were pure waste**, and the quantity
    /// `duplicate_mark_ratio` was supposed to be.
    ///
    /// This is the ratio that argues for or against a per-thread last-card
    /// filter in the write barrier, because both terms are the *same* barrier
    /// counted at the *same* gate:
    /// `GenerationalHeap::write_barrier` calls [`record_card_mark`] after
    /// `mark_dirty_lockfree` on both arms of its clean/dirty branch, and the
    /// else-arm additionally calls [`record_duplicate_card_mark_barrier`]. So
    /// `duplicate_card_marks_barrier <= card_marks_executed` holds by
    /// construction, and the ratio is a genuine fraction.
    ///
    /// Only meaningful when `hot_path_counters_enabled` — like
    /// [`Self::barrier_hit_rate`], and for the same reason — which is why it
    /// is rendered on the armed barrier line rather than beside the
    /// collector-side figures.
    ///
    /// Counts every card mark, compiled code's included: the JIT has no
    /// inline card store in use (see the module header — until 2026-09-23
    /// this paragraph said a JIT-warm run's rate was only *sampled* from the
    /// interpreted tail, which described an emitter that never ran).
    ///
    /// **It is a mark-side number, not a scan-side one.** It says how much of
    /// the barrier's work was redundant. It does *not* say how much of the
    /// dirty-card scan was redundant: the card is keyed on the holder's object
    /// start, so one duplicate mark on a large array's header card still
    /// provokes a re-read of every slot in that array. That cost is
    /// `gengc-oldgen-imprecise-card-over-scan-FIXED-20260923.md` and this ratio
    /// under-states it by design.
    pub barrier_duplicate_mark_ratio: f64,
    /// Fraction of reference stores reaching the barrier that produced a card
    /// mark. Only meaningful when `hot_path_counters_enabled`.
    pub barrier_hit_rate: f64,
    /// Mean refinement nanoseconds per pass.
    pub refinement_nanos_per_pass: f64,
}

#[inline]
fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

/// `num` per MiB of `allocated_bytes`, `0.0` when nothing was published
/// (gen r5w5/sizer9; see `GcMetricsReport::dirty_cards_per_alloc_mib`).
#[inline]
fn per_alloc_mib(num: u64, allocated_bytes: u64) -> f64 {
    ratio(num, allocated_bytes) * (1u64 << 20) as f64
}

impl GcMetricsReport {
    /// Normalize a raw counter set. Pure — no globals, no allocation.
    pub fn from_raw(raw: GcMetricsRaw, hot_path_counters_enabled: bool) -> Self {
        Self {
            hot_path_counters_enabled,
            raw,
            card_marks_per_allocated_object: ratio(raw.card_marks_executed, raw.allocated_objects),
            dirty_cards_per_allocated_object: ratio(raw.cards_found_dirty, raw.allocated_objects),
            old_to_young_edges_per_allocated_object: ratio(
                raw.old_to_young_edges,
                raw.allocated_objects,
            ),
            card_marks_per_alloc_mib: per_alloc_mib(
                raw.card_marks_executed,
                raw.allocated_bytes_total,
            ),
            dirty_cards_per_alloc_mib: per_alloc_mib(
                raw.cards_found_dirty,
                raw.allocated_bytes_total,
            ),
            old_to_young_edges_per_alloc_mib: per_alloc_mib(
                raw.old_to_young_edges,
                raw.allocated_bytes_total,
            ),
            remembered_set_bytes_per_live_byte: ratio(raw.remembered_set_bytes, raw.live_bytes),
            refinement_nanos_per_live_byte: ratio(raw.refinement_nanos, raw.live_bytes),
            old_to_young_edges_per_live_byte: ratio(raw.old_to_young_edges, raw.live_bytes),
            old_to_young_edge_density: ratio(raw.old_to_young_edges, raw.cards_found_dirty),
            duplicate_mark_ratio: ratio(
                raw.duplicate_card_marks,
                raw.duplicate_card_marks
                    .saturating_add(raw.cards_found_dirty),
            ),
            barrier_duplicate_mark_ratio: ratio(
                raw.duplicate_card_marks_barrier,
                raw.card_marks_executed,
            ),
            barrier_hit_rate: ratio(raw.card_marks_executed, raw.barrier_ref_stores),
            refinement_nanos_per_pass: ratio(raw.refinement_nanos, raw.refinement_passes),
        }
    }
}

impl std::fmt::Display for GcMetricsReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = &self.raw;
        writeln!(
            f,
            "[GC] cards: dirty_scanned={} duplicate_marks={} dup_buffered={} \
             old_to_young_edges={} \
             rset_bytes={} refinement_ms={:.3} passes={}",
            r.cards_found_dirty,
            r.duplicate_card_marks,
            // The buffered half, broken out. It is ungated and the barrier
            // half is not, so a reader who sees `duplicate_marks` move while
            // the barrier line says NOT ARMED needs to know which half moved.
            // Non-zero here also means the buffered pipeline — dead since
            // gc-genpause F5.2 — has a producer again, which is worth seeing.
            r.duplicate_card_marks_buffered,
            r.old_to_young_edges,
            r.remembered_set_bytes,
            r.refinement_nanos as f64 / 1.0e6,
            r.refinement_passes,
        )?;
        if self.hot_path_counters_enabled {
            writeln!(
                f,
                "[GC] cards: barrier_ref_stores={} card_marks_executed={} hit_rate={:.4} \
                 dup_barrier={} dup_barrier_over_card_marks={:.4} \
                 (Rust-barrier card marks; under CRATONVM_JIT_INLINE_CARD_MARK=1 a compiled \
                 store into an already-dirty card is not counted)",
                r.barrier_ref_stores,
                r.card_marks_executed,
                self.barrier_hit_rate,
                // Rendered HERE, on the armed line, and nowhere else: both
                // terms are gated on the flag this branch tests, so off the
                // armed line the pair is unmeasured rather than zero. The key
                // spells its own arithmetic for the same reason
                // `dup_over_dup_plus_scanned=` does.
                r.duplicate_card_marks_barrier,
                self.barrier_duplicate_mark_ratio,
            )?;
        } else {
            writeln!(
                f,
                "[GC] cards: barrier counters NOT ARMED (set CRATONVM_GC_CARD_METRICS=1) — \
                 card_marks_executed/barrier_hit_rate/dup_barrier are unmeasured, not zero"
            )?;
        }
        // gen r5w5/sizer9 — steps 2 and 3 of
        // `gengc-r4-plumbing-alloc-denominators-are-not-allocation`. The
        // ratios are re-based on the VM's cumulative `allocated_bytes_total`
        // (per MiB) and RENAMED, so an old log's `dirty_cards_per_obj=` cannot
        // be compared with a new one by accident. The object count is rendered
        // under the name of what it counts: slow-path hand-outs (arena
        // allocations outside any TLAB), not objects allocated.
        // `allocated_bytes` is no longer rendered: since step 2 the
        // Generational heap stops publishing its occupancy under that name
        // and G1 never published it, so it is a gauge nobody writes (the
        // field stays in `GcMetricsRaw`, which embedders see).
        writeln!(
            f,
            "[GC] cards/alloc: dirty_cards_per_alloc_mib={:.6} edges_per_alloc_mib={:.6} \
             card_marks_per_alloc_mib={:.6} (allocated_bytes_total={} slow_path_objects={})",
            self.dirty_cards_per_alloc_mib,
            self.old_to_young_edges_per_alloc_mib,
            self.card_marks_per_alloc_mib,
            // gen r5w2/obs6: the cumulative figure (`record_vm_allocation_totals`),
            // the denominator of the three ratios.
            r.allocated_bytes_total,
            r.allocated_objects,
        )?;
        write!(
            f,
            "[GC] cards/live: rset_bytes_per_live_byte={:.6} refine_ns_per_live_byte={:.6} \
             edges_per_live_byte={:.9} edge_density={:.4} \
             dup_over_dup_plus_scanned={:.4} rset_coarsened={} (live_bytes={})",
            self.remembered_set_bytes_per_live_byte,
            self.refinement_nanos_per_live_byte,
            self.old_to_young_edges_per_live_byte,
            self.old_to_young_edge_density,
            // Renamed from `duplicate_ratio=` in round 2. The old key named a
            // quantity this number is not (see
            // `GcMetricsReport::duplicate_mark_ratio`); the new one spells the
            // arithmetic, so a reader cannot take it for a duplicate fraction.
            //
            // It is NOT being renamed back. Round 3 gave the barrier half a
            // real denominator (`dup_barrier_over_card_marks=`, on the armed
            // line above); this key still divides a two-clock sum by a scan
            // count, so the name still has to spell the arithmetic. The field
            // survives for ABI reasons only.
            self.duplicate_mark_ratio,
            // `rset_coarsened` is bumped by `region.rs:251` and, until
            // 2026-09-20, was rendered nowhere — reachable only by a caller
            // that took `gc_metrics_raw()` itself, of which there were none.
            // A remembered set that gave up naming its sources changes what
            // every later pause walks, so a zero here is worth citing.
            r.rset_coarsened,
            r.live_bytes,
        )?;
        if r.gc_triggered_by_vram_pressure > 0 {
            write!(
                f,
                "\n[GC] gpu: vram_pressure_gcs={}",
                r.gc_triggered_by_vram_pressure,
            )?;
        }
        Ok(())
    }
}

/// Snapshot the raw counters.
pub fn gc_metrics_raw() -> GcMetricsRaw {
    with_counters(|c| {
        let dup_buffered = c.duplicate_card_marks_buffered.load(Ordering::Relaxed);
        let dup_barrier = c.duplicate_card_marks_barrier.load(Ordering::Relaxed);
        GcMetricsRaw {
            card_marks_executed: c.card_marks_executed.load(Ordering::Relaxed),
            barrier_ref_stores: c.barrier_ref_stores.load(Ordering::Relaxed),
            cards_found_dirty: c.cards_found_dirty.load(Ordering::Relaxed),
            // Derived, never stored: the two halves below are the
            // counters, this is their sum, so the legacy field cannot
            // drift away from them.
            duplicate_card_marks: dup_buffered.saturating_add(dup_barrier),
            duplicate_card_marks_buffered: dup_buffered,
            duplicate_card_marks_barrier: dup_barrier,
            remembered_set_bytes: c.remembered_set_bytes.load(Ordering::Relaxed),
            old_to_young_edges: c.old_to_young_edges.load(Ordering::Relaxed),
            cset_verify_objects: c.cset_verify_objects.load(Ordering::Relaxed),
            cset_verify_pauses: c.cset_verify_pauses.load(Ordering::Relaxed),
            cset_verify_dangling: c.cset_verify_dangling.load(Ordering::Relaxed),
            cset_verify_truncated: c.cset_verify_truncated.load(Ordering::Relaxed),
            cset_verify_bytes: c.cset_verify_bytes.load(Ordering::Relaxed),
            cset_verify_unbounded: c.cset_verify_unbounded.load(Ordering::Relaxed),
            rset_coarsened: c.rset_coarsened.load(Ordering::Relaxed),
            humongous_eager_marked: c.humongous_eager_marked.load(Ordering::Relaxed),
            humongous_eager_root_seeded: c.humongous_eager_root_seeded.load(Ordering::Relaxed),
            humongous_eager_walked_sources: c
                .humongous_eager_walked_sources
                .load(Ordering::Relaxed),
            humongous_eager_spans: c.humongous_eager_spans.load(Ordering::Relaxed),
            humongous_eager_bytes: c.humongous_eager_bytes.load(Ordering::Relaxed),
            humongous_eager_declined: c.humongous_eager_declined.load(Ordering::Relaxed),
            refinement_nanos: c.refinement_nanos.load(Ordering::Relaxed),
            refinement_passes: c.refinement_passes.load(Ordering::Relaxed),
            allocated_objects: c.allocated_objects.load(Ordering::Relaxed),
            allocated_bytes: c.allocated_bytes.load(Ordering::Relaxed),
            live_bytes: c.live_bytes.load(Ordering::Relaxed),
            allocated_bytes_total: c.allocated_bytes_total.load(Ordering::Relaxed),
            gc_triggered_by_vram_pressure: c.gc_triggered_by_vram_pressure.load(Ordering::Relaxed),
        }
    })
}

/// The card / remembered-set cost report, normalized per allocated object and
/// per live byte.
///
/// Cheap (one relaxed load per counter and some float division); safe to call
/// outside a pause. The counters are not snapshot-consistent with each other —
/// they are observability, not a transaction.
pub fn gc_metrics_report() -> GcMetricsReport {
    GcMetricsReport::from_raw(gc_metrics_raw(), hot_path_counters_enabled())
}

// ---------------------------------------------------------------------------
// Collector decision record
// ---------------------------------------------------------------------------

/// Why the collector took the young path it took.
///
/// Numbering is stable; append new variants and bump [`decision_reason::COUNT`].
pub mod decision_reason {
    /// No collection has been recorded yet.
    pub const UNRECORDED: u8 = 0;
    /// Moving (Cheney) young collection with **no** live JIT frame anywhere —
    /// the collector is fully precise and relocation is unconditionally safe.
    pub const MOVING_NO_JIT_FRAMES: u8 = 1;
    /// Moving (Cheney) young collection **while a JIT frame was live** — every
    /// live compiled frame proved a complete rewritable root map. This is the
    /// case `ARCHITECTURE.md` describes and `docs/GC.md` denies.
    pub const MOVING_WITH_PROVEN_JIT_COVERAGE: u8 = 2;
    /// Moving because `CRATONVM_DBG_FORCE_MOVING` overrode a diversion.
    pub const MOVING_FORCED_BY_DEBUG_FLAG: u8 = 3;
    /// Non-moving because moving-young is switched **OFF** and a live JIT
    /// frame's roots were discovered CONSERVATIVELY, so they cannot be
    /// rewritten — `divert_non_moving`'s legacy `has_conservative_roots &&
    /// !moving_young` rule. The lever is turning moving-young on.
    ///
    /// Until 2026-09-23 this one code covered THREE situations (see
    /// `docs/feature-designs/generative-gc-architecture.md`).
    /// The moving-young-ON case is now
    /// [`NON_MOVING_UNREWRITABLE_CONSERVATIVE_JIT_ROOTS`] and the bare opt-out
    /// with no compiled frame is [`NON_MOVING_YOUNG_COMPACTION_DISABLED`], each
    /// recorded from its own arm in `gen_heap::collect_garbage_inner`; the
    /// `conservative_jit_root_divert_split` counter that approximated the
    /// split from `moving_young_requested` was deleted with it.
    pub const NON_MOVING_CONSERVATIVE_JIT_ROOTS: u8 = 4;
    /// Non-moving: moving-young was requested but this cycle's coverage proof
    /// failed. The [`crate::gc_quiescence::incomplete_reason`] code on the
    /// record names the obligation that failed.
    pub const NON_MOVING_COVERAGE_INCOMPLETE: u8 = 5;
    /// Non-moving: both generations near full, so the moving path could abort
    /// on a promotion failure.
    pub const NON_MOVING_PROMOTION_OOM_RISK: u8 = 6;
    /// Non-moving: an explicit `System.gc()` requested an old-gen-inclusive
    /// cycle, which is routed through the non-moving marker.
    pub const NON_MOVING_EXPLICIT_FULL_GC: u8 = 7;
    /// The backend has no moving young generation at all (ZGC's STW
    /// mark-sweep), and this cycle did not compact either. Recorded for every
    /// ZGC cycle whose slide did not run for a reason other than a GPU veto or
    /// a failed coverage proof — the cost gate, `CRATONVM_ZGC_RELOCATE=0`,
    /// nothing worth sliding. A compacting ZGC cycle is
    /// [`MOVING_BACKEND_COMPACTED`]. Recorded since 2026-09-23 (it had no
    /// recorder before that).
    pub const NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY: u8 = 8;
    /// The backend always evacuates its collection set (G1).
    pub const MOVING_BACKEND_ALWAYS_EVACUATES: u8 = 9;
    /// G1 declined to evacuate anything this pause because this collection's
    /// JIT root set is known to be incomplete, so the collector cannot know
    /// which regions hold an object whose only reference it failed to
    /// enumerate. The [`crate::gc_quiescence::incomplete_reason`] code on the
    /// record names the obligation that failed.
    ///
    /// This is G1's analogue of the generational collector's
    /// [`NON_MOVING_COVERAGE_INCOMPLETE`] diversion — G1 has no non-moving
    /// young sweep to divert *to*, so the fail-safe is an empty collection
    /// set: the pause reclaims nothing and every object stays at its address.
    pub const NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE: u8 = 10;
    /// G1 declined to evacuate anything this pause because a compiled frame was
    /// live and the conservative JIT root publication was EMPTY.
    ///
    /// Distinct from [`NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE`], and the
    /// distinction is the whole point. That one means "the roots were
    /// enumerated but are not rewritable" — the normal state under G1, true on
    /// ~99.9% of pauses, and the reason its refusal is an opt-in lever. This
    /// one means the scan published *nothing at all* while a compiled frame was
    /// running, so `pin_addrs=0` cannot be read as "there are no JIT roots"; it
    /// reads as "the scan found none", which is unknown, not none. An empty
    /// publication and a genuinely reference-free compiled frame are
    /// indistinguishable where the CSet is built, and only one of them is safe
    /// to evacuate. See `G1Collector::empty_jit_publication`.
    pub const NON_MOVING_G1_EMPTY_JIT_PUBLICATION: u8 = 11;
    /// Non-moving: a GPU device was reading or writing the heap arena in
    /// place — a zero-copy upload or a writeback download — and the
    /// collector's bounded wait for that window to close expired. The
    /// generational collector takes its non-moving young sweep; the
    /// ZGC slide and the large-object compactor stand down for the
    /// cycle. See `cratonvm_gc::vm_heap::gpu_relocation_forbidden`.
    pub const NON_MOVING_GPU_CRITICAL: u8 = 12;
    /// G1's twin of [`NON_MOVING_GPU_CRITICAL`]: an empty collection set,
    /// because G1 has no non-moving sweep to divert to.
    pub const NON_MOVING_G1_GPU_CRITICAL: u8 = 13;
    /// The generational young collection **refused to run at all** because the
    /// moving path's exact from-space object-start walk did not complete — a
    /// corrupt GAP filler, an implausible extent, a walk that crossed a
    /// free/TLAB range, or a misaligned object start. The cycle over-retains,
    /// lets the allocation slow paths spill to old gen, and retries on the
    /// next trigger.
    ///
    /// This and [`SKIPPED_YOUNG_RESERVED_TLAB_TAILS`] are the only outcomes in
    /// this table that are neither moving nor non-moving — **nothing
    /// happened** — and [`is_skipped`] is how a reader tells them from the
    /// non-moving arms. They have their own codes because until 2026-09-20 a
    /// refusal moved no counter whatsoever: not `cycles`, not
    /// `coverage_fallbacks`, not `minor_gc_count`. A heap wedged in a refusal
    /// loop and a heap that is simply idle published byte-identical numbers,
    /// so the one shape an operator most needs to see was the one shape the
    /// metrics could not express. Its companion is the refusal FLOOR
    /// (`note_skipped_young_cycle`), which stops the refusal repeating at
    /// every allocation; the floor ends the storm, this makes it visible.
    ///
    /// 2026-09-21: this code used to be spelled `SKIPPED_YOUNG_CYCLE_REFUSED`
    /// and covered BOTH refusal causes. They are split because the two have
    /// nothing in common but their exit: this one says the from-space layout
    /// could not be parsed, which is a heap-integrity question, and the other
    /// says a perfectly parseable arena is still owned by a live mutator,
    /// which is a TLAB-retirement question. One code for both left an operator
    /// with a count and no direction.
    pub const SKIPPED_YOUNG_WALK_INCOMPLETE: u8 = 14;

    /// The generational young collection **refused to run at all** because the
    /// T-3 tripwire fired: a live mutator still had a reserved TLAB tail
    /// published inside the very from-space this cycle was about to evacuate,
    /// swap and reset. The walk itself was fine — the tail is skipped, not
    /// parsed; the hazard is afterwards, when the owner resumes and
    /// bump-allocates from a cursor into memory the collector has handed back.
    ///
    /// Twin of [`SKIPPED_YOUNG_WALK_INCOMPLETE`]: same exit, same over-retain,
    /// same floor, different cause and different fix. A run in which this code
    /// dominates has a TLAB that is not being retired at a safepoint; a run in
    /// which the other dominates has a young arena whose object-start layout
    /// cannot be walked.
    pub const SKIPPED_YOUNG_RESERVED_TLAB_TAILS: u8 = 15;

    /// The generational young collection **refused to run at all** because
    /// to-space could not cover from-space: the Cheney invariant the copy
    /// phase rests on. The collector declines to start a copy it cannot
    /// finish — over-retain for one cycle, let the allocation slow path spill
    /// to old gen, and raise a *catchable* `OutOfMemoryError` through the
    /// normal ladder if memory is genuinely exhausted. Aborting mid-copy is
    /// not recoverable by anyone.
    ///
    /// Only reachable when the semispace equalisation immediately above it was
    /// skipped because to-space was somehow non-empty, so a non-zero count
    /// here is itself the finding — it says the two semispaces have drifted
    /// out of the shape the copy phase assumes, not merely that the heap is
    /// small.
    ///
    /// 2026-09-21: this is the THIRD refusal arm. The 2026-09-20 round closed
    /// the other two and did not see this one, which returns from a different
    /// place with the same all-zero `GcResult`; it moved no counter at all
    /// until this code existed.
    pub const SKIPPED_YOUNG_TO_SPACE_UNDERSIZED: u8 = 16;

    /// A whole-heap backend COMPACTED this cycle: ZGC's stop-the-world slide
    /// ran and relocated objects (its `compaction_cycles` counter moved across
    /// the collection).
    ///
    /// Added 2026-09-23 with the first ZGC caller of
    /// [`super::record_collector_decision`] (`VmHeap::zgc_recording_decision`).
    /// Not [`MOVING_BACKEND_ALWAYS_EVACUATES`]: that one is G1's, and it is
    /// unconditional by construction, where a ZGC cycle decides afresh every
    /// time and may decline — the declines are recorded under
    /// [`NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY`],
    /// [`NON_MOVING_COVERAGE_INCOMPLETE`] or [`NON_MOVING_GPU_CRITICAL`].
    /// "Young" in the record's `young=MOVING` rendering is a misnomer for this
    /// backend: its slide is whole-heap.
    pub const MOVING_BACKEND_COMPACTED: u8 = 17;

    /// Non-moving although moving-young is **ON**: a compiled frame is live and
    /// a conservative JIT scan ran this cycle, so the Cheney copy cannot honour
    /// the pins those un-rewritable roots need (`divert_non_moving`'s
    /// `unrewritable_conservative_jit_roots`, 2026-09-06). The dominant
    /// diversion on a JIT-warm workload; the only lever is
    /// `CRATONVM_GC=-peer-pin-divert` (legacy `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`),
    /// off by default for a soundness reason.
    ///
    /// Split out of [`NON_MOVING_CONSERVATIVE_JIT_ROOTS`] 2026-09-23 (gen
    /// r4w2/youngpolicy), which now names only the legacy moving-young-OFF rule.
    pub const NON_MOVING_UNREWRITABLE_CONSERVATIVE_JIT_ROOTS: u8 = 18;

    /// Non-moving because the operator switched young compaction off
    /// (`CRATONVM_NO_MOVING_YOUNG`, alias `CRATONVM_GC=-moving-young`) and
    /// nothing else about the cycle would have diverted it — no live compiled
    /// frame, no failed coverage proof, no near-full heap, no GPU critical
    /// section, no `System.gc()`. `divert_non_moving`'s TERM 0 alone.
    ///
    /// Recorded as [`NON_MOVING_CONSERVATIVE_JIT_ROOTS`] until 2026-09-23 (and
    /// qualified `/no-moving-young-optout` on the verbose line), which pointed a
    /// reader at a compiled frame that did not exist.
    pub const NON_MOVING_YOUNG_COMPACTION_DISABLED: u8 = 19;

    /// Moving although the conservative-JIT-root term fired: the pinned
    /// in-place young cycle ran (`CRATONVM_GEN_PINNED_YOUNG_COPY`, gen
    /// r4w5/pinned5). The 4 KiB pages the cycle's unrewritable conservative
    /// words land in kept their objects in place; every other survivor was
    /// copied into from-space's free spans or promoted. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    pub const MOVING_PINNED_PAGES: u8 = 20;

    /// Moving although the conservative-JIT-root term fired: the pin ledger
    /// was complete for this pause and named no word overlapping a young
    /// from-space object, so there was nothing to pin and the ordinary Cheney
    /// copy ran. Only reachable with `CRATONVM_GEN_PINNED_YOUNG_COPY` (gen
    /// r4w5/pinned5).
    pub const MOVING_NO_YOUNG_PIN_WORDS: u8 = 21;

    /// Non-moving: the conservative-JIT-root term fired, the pinned young copy
    /// was enabled, and the pages its words pin exceed 1/8 of from-space — the
    /// bound past which leaving pages in place costs more than the sweep (gen
    /// r4w5/pinned5).
    pub const NON_MOVING_PINNED_PAGES_OVER_BOUND: u8 = 22;

    /// Non-moving: the conservative-JIT-root term fired, the pinned young copy
    /// was enabled, and `gc_quiescence::pause_young_pin_words` reported the
    /// pin ledger INCOMPLETE for this pause (a thread's scan did not deposit),
    /// so the pin set is unknown and nothing may move (gen r4w5/pinned5).
    pub const NON_MOVING_PIN_LEDGER_INCOMPLETE: u8 = 23;

    /// One past the highest defined code.
    pub const COUNT: u8 = 24;

    /// Human-readable label.
    pub fn label(code: u8) -> &'static str {
        match code {
            UNRECORDED => "no-collection-recorded",
            MOVING_NO_JIT_FRAMES => "moving-no-jit-frames-live",
            MOVING_WITH_PROVEN_JIT_COVERAGE => "moving-jit-coverage-proven",
            MOVING_FORCED_BY_DEBUG_FLAG => "moving-forced-by-debug-flag",
            NON_MOVING_CONSERVATIVE_JIT_ROOTS => "nonmoving-conservative-jit-roots",
            NON_MOVING_COVERAGE_INCOMPLETE => "nonmoving-coverage-incomplete",
            NON_MOVING_PROMOTION_OOM_RISK => "nonmoving-promotion-oom-risk",
            NON_MOVING_EXPLICIT_FULL_GC => "nonmoving-explicit-full-gc",
            NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY => "nonmoving-backend-has-no-young-copy",
            MOVING_BACKEND_ALWAYS_EVACUATES => "moving-backend-always-evacuates",
            NON_MOVING_G1_ROOT_COVERAGE_INCOMPLETE => "g1-no-evacuation-root-coverage-incomplete",
            NON_MOVING_G1_EMPTY_JIT_PUBLICATION => "g1-no-evacuation-empty-jit-publication",
            NON_MOVING_GPU_CRITICAL => "nonmoving-gpu-critical-section",
            NON_MOVING_G1_GPU_CRITICAL => "g1-no-evacuation-gpu-critical-section",
            SKIPPED_YOUNG_WALK_INCOMPLETE => "skipped-young-walk-incomplete",
            SKIPPED_YOUNG_RESERVED_TLAB_TAILS => "skipped-young-reserved-tlab-tails",
            SKIPPED_YOUNG_TO_SPACE_UNDERSIZED => "skipped-young-to-space-undersized",
            MOVING_BACKEND_COMPACTED => "moving-backend-compacted",
            NON_MOVING_UNREWRITABLE_CONSERVATIVE_JIT_ROOTS => {
                "nonmoving-unrewritable-conservative-jit-roots"
            }
            NON_MOVING_YOUNG_COMPACTION_DISABLED => "nonmoving-young-compaction-disabled",
            MOVING_PINNED_PAGES => "moving-pinned-pages",
            MOVING_NO_YOUNG_PIN_WORDS => "moving-no-young-pin-words",
            NON_MOVING_PINNED_PAGES_OVER_BOUND => "nonmoving-pinned-pages-over-bound",
            NON_MOVING_PIN_LEDGER_INCOMPLETE => "nonmoving-pin-ledger-incomplete",
            _ => "unknown",
        }
    }

    /// Did this reason mean the young collection **did not run at all**?
    ///
    /// Neither moving nor non-moving. Kept beside [`is_moving`] and the labels
    /// for the same reason they are kept together: a refusal that is counted
    /// as a non-moving cycle is a claim that the non-moving sweep ran, and it
    /// did not. Every code answering `true` here answers `false` to
    /// [`is_moving`], and its label starts with `skipped-`.
    pub fn is_skipped(code: u8) -> bool {
        matches!(
            code,
            SKIPPED_YOUNG_WALK_INCOMPLETE
                | SKIPPED_YOUNG_RESERVED_TLAB_TAILS
                | SKIPPED_YOUNG_TO_SPACE_UNDERSIZED
        )
    }

    /// Does this reason describe a MOVING young collection? Kept next to the
    /// labels so the two can never disagree.
    pub fn is_moving(code: u8) -> bool {
        matches!(
            code,
            MOVING_NO_JIT_FRAMES
                | MOVING_WITH_PROVEN_JIT_COVERAGE
                | MOVING_FORCED_BY_DEBUG_FLAG
                | MOVING_BACKEND_ALWAYS_EVACUATES
                | MOVING_BACKEND_COMPACTED
                | MOVING_PINNED_PAGES
                | MOVING_NO_YOUNG_PIN_WORDS
        )
    }
}

/// What the collector decided for one cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectorDecision {
    /// 1-based sequence number of the recorded decision.
    pub sequence: u64,
    /// `"generational"`, `"g1"`, `"zgc"`.
    pub backend: &'static str,
    /// Did the young half of this collection relocate objects?
    pub young_moving: bool,
    /// [`decision_reason`] code.
    pub reason: u8,
    /// When `reason == NON_MOVING_COVERAGE_INCOMPLETE`, the
    /// [`crate::gc_quiescence::incomplete_reason`] code that forced it;
    /// `incomplete_reason::NONE` otherwise.
    pub incomplete_reason: usize,
    /// Was any thread inside a JIT call when the decision was taken?
    pub jit_active: bool,
    /// Was an unregistered compiled frame found on the deciding thread's stack?
    pub unregistered_jit_frame: bool,
    /// Did the published (codegen-authoritative) gate request moving-young?
    pub moving_young_requested: bool,
}

impl std::fmt::Display for CollectorDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[GC] decision #{seq}: backend={backend} young={young} reason={reason}",
            seq = self.sequence,
            backend = self.backend,
            // 2026-09-21: THREE states, not two. A refused cycle ran neither
            // collector, and printing `young=NON-MOVING` beside
            // `reason=skipped-…` asserted that the non-moving sweep ran. It
            // did not: nothing did.
            young = if self.young_moving {
                "MOVING"
            } else if decision_reason::is_skipped(self.reason) {
                "SKIPPED"
            } else {
                "NON-MOVING"
            },
            reason = decision_reason::label(self.reason),
        )?;
        if self.reason == decision_reason::NON_MOVING_COVERAGE_INCOMPLETE {
            write!(
                f,
                " unproven_obligation={}",
                crate::gc_quiescence::incomplete_reason::label(self.incomplete_reason),
            )?;
        }
        write!(
            f,
            " (moving_young_requested={} jit_active={} unregistered_jit_frame={})",
            self.moving_young_requested, self.jit_active, self.unregistered_jit_frame,
        )
    }
}

// The record is a handful of scalars, so it is stored as loose atomics rather
// than behind a lock: the collector writes it inside the pause and readers are
// diagnostics that tolerate a torn read across fields (they cannot tear WITHIN
// one). `sequence` is written last, so a reader that sees sequence == N has
// seen at least the fields written for cycle N or a later one.
struct DecisionSlot {
    sequence: AtomicU64,
    backend: AtomicU8,
    reason: AtomicU8,
    incomplete_reason: AtomicU64,
    flags: AtomicU8,
}

const BACKEND_UNKNOWN: u8 = 0;
const BACKEND_GENERATIONAL: u8 = 1;
const BACKEND_G1: u8 = 2;
const BACKEND_ZGC: u8 = 3;

const FLAG_JIT_ACTIVE: u8 = 0b001;
const FLAG_UNREGISTERED_JIT_FRAME: u8 = 0b010;
const FLAG_MOVING_YOUNG_REQUESTED: u8 = 0b100;

impl DecisionSlot {
    pub const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            backend: AtomicU8::new(BACKEND_UNKNOWN),
            reason: AtomicU8::new(decision_reason::UNRECORDED),
            incomplete_reason: AtomicU64::new(0),
            flags: AtomicU8::new(0),
        }
    }
}

/// Per-reason histogram of collector decisions across the whole run.
///
/// `last_collector_decision` only reports the MOST RECENT cycle, which is
/// exactly the trap that made a corruption look like it belonged to the
/// non-moving sweep when 153 of the run's 154 cycles had actually been
/// moving. "Which collector ran" is a question about the distribution, not
/// the last sample.
#[cfg(not(any(test, feature = "test-hooks")))]
static DECISION_HISTOGRAM: [AtomicU64; decision_reason::COUNT as usize] =
    [const { AtomicU64::new(0) }; decision_reason::COUNT as usize];

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static DECISION_HISTOGRAM: [AtomicU64; decision_reason::COUNT as usize] =
        const { [const { AtomicU64::new(0) }; decision_reason::COUNT as usize] };
}

#[cfg(not(any(test, feature = "test-hooks")))]
fn bump_decision_histogram(reason: u8) {
    if let Some(slot) = DECISION_HISTOGRAM.get(reason as usize) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn bump_decision_histogram(reason: u8) {
    DECISION_HISTOGRAM.with(|h| {
        if let Some(slot) = h.get(reason as usize) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[cfg(not(any(test, feature = "test-hooks")))]
fn read_decision_histogram() -> Vec<(u8, u64)> {
    DECISION_HISTOGRAM
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u8, s.load(Ordering::Relaxed)))
        .collect()
}

#[cfg(any(test, feature = "test-hooks"))]
fn read_decision_histogram() -> Vec<(u8, u64)> {
    DECISION_HISTOGRAM.with(|h| {
        h.iter()
            .enumerate()
            .map(|(i, s)| (i as u8, s.load(Ordering::Relaxed)))
            .collect()
    })
}

/// `(moving_cycles, non_moving_cycles, skipped_cycles)` across the run, plus
/// the per-reason breakdown. The distribution `last_collector_decision` cannot
/// give.
///
/// 2026-09-21: `skipped` is a THIRD term, not a slice of `non_moving`. A
/// refused young cycle ran neither collector — counting it as non-moving says
/// the non-moving sweep reclaimed something, which is the false report this
/// histogram exists to prevent. See [`decision_reason::is_skipped`].
pub fn decision_histogram() -> (u64, u64, u64, Vec<(&'static str, u64)>) {
    let mut moving = 0u64;
    let mut non_moving = 0u64;
    let mut skipped = 0u64;
    let mut rows = Vec::new();
    for (code, n) in read_decision_histogram() {
        if n == 0 || code == decision_reason::UNRECORDED {
            continue;
        }
        if decision_reason::is_moving(code) {
            moving += n;
        } else if decision_reason::is_skipped(code) {
            skipped += n;
        } else {
            non_moving += n;
        }
        rows.push((decision_reason::label(code), n));
    }
    (moving, non_moving, skipped, rows)
}

#[cfg(not(any(test, feature = "test-hooks")))]
static DECISION: DecisionSlot = DecisionSlot::new();

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
fn with_decision<R>(f: impl FnOnce(&DecisionSlot) -> R) -> R {
    f(&DECISION)
}

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static DECISION: DecisionSlot = const { DecisionSlot::new() };
}

#[cfg(any(test, feature = "test-hooks"))]
#[inline]
fn with_decision<R>(f: impl FnOnce(&DecisionSlot) -> R) -> R {
    DECISION.with(f)
}

fn backend_code(name: &str) -> u8 {
    match name {
        "generational" => BACKEND_GENERATIONAL,
        "g1" => BACKEND_G1,
        "zgc" => BACKEND_ZGC,
        _ => BACKEND_UNKNOWN,
    }
}

fn backend_name(code: u8) -> &'static str {
    match code {
        BACKEND_GENERATIONAL => "generational",
        BACKEND_G1 => "g1",
        BACKEND_ZGC => "zgc",
        _ => "unknown",
    }
}

/// Record the young-path decision for the collection that is about to run.
///
/// Called from the branch in `GenerationalHeap::collect_garbage_inner` that
/// actually chooses, so the report cannot drift from the code the way the two
/// architecture documents did. `backend` must be one of `"generational"`,
/// `"g1"`, `"zgc"`.
///
/// `incomplete_reason` is only read back when `reason` is
/// [`decision_reason::NON_MOVING_COVERAGE_INCOMPLETE`]; pass
/// [`crate::gc_quiescence::incomplete_reason::NONE`] otherwise.
pub fn record_collector_decision(backend: &str, reason: u8, incomplete_reason: usize) {
    bump_decision_histogram(reason);
    let mut flags = 0u8;
    if crate::gc_quiescence::is_active() {
        flags |= FLAG_JIT_ACTIVE;
    }
    if crate::gc_quiescence::unregistered_jit_frame_on_stack() {
        flags |= FLAG_UNREGISTERED_JIT_FRAME;
    }
    if crate::gc_quiescence::moving_young_enabled() {
        flags |= FLAG_MOVING_YOUNG_REQUESTED;
    }
    with_decision(|d| {
        d.backend.store(backend_code(backend), Ordering::Relaxed);
        d.reason.store(reason, Ordering::Relaxed);
        d.incomplete_reason
            .store(incomplete_reason as u64, Ordering::Relaxed);
        d.flags.store(flags, Ordering::Relaxed);
        // Written LAST: a reader that observes sequence == N has seen at least
        // the fields belonging to cycle N.
        d.sequence.fetch_add(1, Ordering::Release);
    });
}

/// The decision recorded for the last (or currently-running) cycle, or `None`
/// if no collection has recorded one yet.
pub fn last_collector_decision() -> Option<CollectorDecision> {
    with_decision(|d| {
        let sequence = d.sequence.load(Ordering::Acquire);
        if sequence == 0 {
            return None;
        }
        let flags = d.flags.load(Ordering::Relaxed);
        let reason = d.reason.load(Ordering::Relaxed);
        Some(CollectorDecision {
            sequence,
            backend: backend_name(d.backend.load(Ordering::Relaxed)),
            young_moving: decision_reason::is_moving(reason),
            reason,
            incomplete_reason: d.incomplete_reason.load(Ordering::Relaxed) as usize,
            jit_active: flags & FLAG_JIT_ACTIVE != 0,
            unregistered_jit_frame: flags & FLAG_UNREGISTERED_JIT_FRAME != 0,
            moving_young_requested: flags & FLAG_MOVING_YOUNG_REQUESTED != 0,
        })
    })
}


// ---------------------------------------------------------------------------
// G1 per-cycle decision record
// ---------------------------------------------------------------------------

/// Ways a G1 cycle can be **degraded** — i.e. still correct, but not doing what
/// an undisturbed cycle would do.
///
/// Every flag here corresponds to a fail-safe the collector took rather than to
/// a failure. They are recorded because each one silently changes what the
/// pause reclaims, and none of them was previously visible from outside a
/// debug build: an operator watching G1 fail to reclaim old gen had no way to
/// tell "the mark closure was abandoned this cycle" from "there is no garbage".
///
/// See `g1-audit.md` for which invariant each one protects.
pub mod g1_degraded {
    /// Nothing unusual happened.
    pub const NONE: u32 = 0;
    /// At least one object could not be evacuated (to-space exhausted) and was
    /// self-forwarded in place; its region was KEPT rather than freed
    /// (`G1Collector::free_or_keep_cset`).
    pub const EVACUATION_FAILURE: u32 = 1 << 0;
    /// The same-pause drain of self-forwarded objects gave up (wedge or pass
    /// cap) with live objects still parked in kept regions
    /// (`retry_after_evacuation_failure`). The heap is coherent but not
    /// reclaimed.
    pub const EVACUATION_FAILURE_UNRESOLVED: u32 = 1 << 1;
    /// The mark worklist hit `MARK_WORKLIST_CAP` during this cycle, so some
    /// gray pushes were converted to black-without-scan and a conservative
    /// whole-heap rescan was required.
    pub const MARK_WORKLIST_OVERFLOW: u32 = 1 << 2;
    /// A gray entry with an implausible header was skipped, so the closure may
    /// be incomplete and `cleanup` retained every region this cycle.
    pub const MARK_IMPLAUSIBLE_HEADER: u32 = 1 << 3;
    /// `cleanup` ran with a NON-EMPTY gray set — the transitive closure was
    /// never driven to a fixed point, so a zero-live verdict is not
    /// trustworthy and every region was retained. This is the fail-safe for
    /// the unenforced "drain the worklist before cleanup" precondition (see
    /// `VmHeap::g1_signal_marking_complete`, which skips the remark).
    pub const CLEANUP_CLOSURE_INCOMPLETE: u32 = 1 << 4;
    /// At least one region was excluded from the collection set by a JNI
    /// no-relocation pin (`G1Region::pinned`).
    pub const JNI_PINNED_REGIONS_EXCLUDED: u32 = 1 << 5;
    /// At least one region was excluded from the collection set because it
    /// holds a conservatively-discovered JIT root or a frozen peer's
    /// un-retired TLAB tail (`jit_pinned_region_set`).
    pub const JIT_PINNED_REGIONS_EXCLUDED: u32 = 1 << 6;
    /// The parallel evacuator ran, rather than the single-threaded one.
    ///
    /// Unlike its neighbours this is NOT a fail-safe the collector took, and
    /// since 2026-08-13 it is not an experiment either — parallel evacuation is
    /// the default. It stays in this word because the question it answers is
    /// the one every G1 bug report needs answered first: WHICH evacuator ran.
    /// Reading a cycle record without it is how a parallel-path defect gets
    /// triaged as a serial-path one, which is most of what went wrong with
    /// G1-9.
    pub const PARALLEL_EVACUATOR: u32 = 1 << 7;
    /// The collection set came out empty, so the pause did nothing.
    pub const EMPTY_COLLECTION_SET: u32 = 1 << 8;
    /// The collection set was forced empty because this collection's JIT root
    /// set is known-incomplete: at least one live compiled frame could not be
    /// enumerated, so no region can be proven free of an object whose only
    /// reference the collector never saw. Always accompanies
    /// [`EMPTY_COLLECTION_SET`]; it distinguishes "there was nothing to
    /// collect" from "the collector was not allowed to collect".
    pub const ROOT_COVERAGE_INCOMPLETE: u32 = 1 << 9;
    /// The collection set was forced empty because a compiled frame was live
    /// and the conservative JIT root publication was empty, so no region could
    /// be proven free of an object the scan never saw. Always accompanies
    /// [`EMPTY_COLLECTION_SET`]. Distinct from [`ROOT_COVERAGE_INCOMPLETE`]:
    /// that bit says the roots are not rewritable, this one says there were no
    /// roots to rewrite *and a compiled frame was running*, which is the state
    /// that let a live `StringLatin1.newString` reference get evacuated out
    /// from under a JIT frame (`docs/known-issues/gc/`).
    pub const JIT_PUBLICATION_EMPTY: u32 = 1 << 10;

    /// Every defined bit. A flag outside this mask is a programming error and
    /// is rejected by `record_g1_cycle`'s `debug_assert!`.
    pub const ALL: u32 = EVACUATION_FAILURE
        | EVACUATION_FAILURE_UNRESOLVED
        | MARK_WORKLIST_OVERFLOW
        | MARK_IMPLAUSIBLE_HEADER
        | CLEANUP_CLOSURE_INCOMPLETE
        | JNI_PINNED_REGIONS_EXCLUDED
        | JIT_PINNED_REGIONS_EXCLUDED
        | PARALLEL_EVACUATOR
        | EMPTY_COLLECTION_SET
        | ROOT_COVERAGE_INCOMPLETE
        | JIT_PUBLICATION_EMPTY;

    /// Stable labels, lowest bit first. A new flag cannot be added without a
    /// label — `every_g1_degraded_flag_has_a_label` pins that.
    pub fn labels(bits: u32) -> Vec<&'static str> {
        const TABLE: &[(u32, &str)] = &[
            (EVACUATION_FAILURE, "evacuation-failure-self-forwarded"),
            (
                EVACUATION_FAILURE_UNRESOLVED,
                "evacuation-failure-drain-wedged",
            ),
            (MARK_WORKLIST_OVERFLOW, "mark-worklist-overflow-rescan"),
            (
                MARK_IMPLAUSIBLE_HEADER,
                "mark-implausible-header-retain-all",
            ),
            (
                CLEANUP_CLOSURE_INCOMPLETE,
                "cleanup-closure-incomplete-retain-all",
            ),
            (JNI_PINNED_REGIONS_EXCLUDED, "jni-pinned-regions-excluded"),
            (JIT_PINNED_REGIONS_EXCLUDED, "jit-pinned-regions-excluded"),
            (PARALLEL_EVACUATOR, "parallel-evacuator"),
            (EMPTY_COLLECTION_SET, "empty-collection-set"),
            (
                ROOT_COVERAGE_INCOMPLETE,
                "root-coverage-incomplete-no-evacuation",
            ),
            (JIT_PUBLICATION_EMPTY, "jit-publication-empty-no-evacuation"),
        ];
        TABLE
            .iter()
            .filter(|(bit, _)| bits & bit != 0)
            .map(|(_, name)| *name)
            .collect()
    }
}

/// Which G1 pause shape recorded the facts.
pub mod g1_cycle_kind {
    pub const UNRECORDED: u8 = 0;
    pub const YOUNG: u8 = 1;
    pub const MIXED: u8 = 2;
    pub const KEPT_REGION_DRAIN: u8 = 3;
    pub const CONCURRENT_CLEANUP: u8 = 4;

    pub fn label(code: u8) -> &'static str {
        match code {
            YOUNG => "young",
            MIXED => "mixed",
            KEPT_REGION_DRAIN => "kept-region-drain",
            CONCURRENT_CLEANUP => "concurrent-cleanup",
            _ => "unrecorded",
        }
    }
}

/// What a G1 cycle decided: which regions it took, which it was forced to
/// leave, and which fail-safes fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct G1CycleFacts {
    /// 1-based sequence number of the recorded G1 cycle.
    pub sequence: u64,
    /// [`g1_cycle_kind`] code.
    pub kind: u8,
    /// Young (Eden + Survivor) regions in the collection set.
    pub cset_young: u32,
    /// Old regions in the collection set (mixed pauses only).
    pub cset_old: u32,
    /// Collectable regions kept OUT of the collection set by a pin of either
    /// vocabulary.
    pub regions_pinned_out: u32,
    /// Distinct remembered-set source regions the pause scanned.
    pub rset_sources_scanned: u32,
    /// Bitmask of [`g1_degraded`] flags.
    pub degraded: u32,
}

impl std::fmt::Display for G1CycleFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[GC] g1 cycle #{seq}: kind={kind} cset_young={y} cset_old={o} \
             pinned_out={p} rset_sources={r} degraded=",
            seq = self.sequence,
            kind = g1_cycle_kind::label(self.kind),
            y = self.cset_young,
            o = self.cset_old,
            p = self.regions_pinned_out,
            r = self.rset_sources_scanned,
        )?;
        let labels = g1_degraded::labels(self.degraded);
        if labels.is_empty() {
            write!(f, "none")
        } else {
            write!(f, "{}", labels.join(","))
        }
    }
}

struct G1CycleSlot {
    sequence: AtomicU64,
    kind: AtomicU8,
    cset_young: AtomicU64,
    cset_old: AtomicU64,
    regions_pinned_out: AtomicU64,
    rset_sources_scanned: AtomicU64,
    degraded: AtomicU64,
}

impl G1CycleSlot {
    pub const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            kind: AtomicU8::new(g1_cycle_kind::UNRECORDED),
            cset_young: AtomicU64::new(0),
            cset_old: AtomicU64::new(0),
            regions_pinned_out: AtomicU64::new(0),
            rset_sources_scanned: AtomicU64::new(0),
            degraded: AtomicU64::new(0),
        }
    }
}

#[cfg(not(any(test, feature = "test-hooks")))]
static G1_CYCLE: G1CycleSlot = G1CycleSlot::new();

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
fn with_g1_cycle<R>(f: impl FnOnce(&G1CycleSlot) -> R) -> R {
    f(&G1_CYCLE)
}

#[cfg(any(test, feature = "test-hooks"))]
thread_local! {
    static G1_CYCLE: G1CycleSlot = const { G1CycleSlot::new() };
}

#[cfg(any(test, feature = "test-hooks"))]
#[inline]
fn with_g1_cycle<R>(f: impl FnOnce(&G1CycleSlot) -> R) -> R {
    G1_CYCLE.with(f)
}

// NOTE (2026-09-20): the doc block below belongs to `G1_PAUSES` /
// `G1_PAUSES_COVERAGE_INCOMPLETE`. It used to open with `record_g1_cycle`'s
// own documentation — that function lives ~180 lines further down and had none
// — so `cargo doc` rendered "Record what a G1 pause decided" as the docs of a
// counter, and the function a reader would go looking for was undocumented.
// `record_g1_cycle` now carries its own block at its definition.
/// Total G1 STW collections that reached the dispatch point, and how many of
/// those found this collection's JIT root set incomplete.
///
/// Counted on BOTH sides of the `CRATONVM_G1_NO_COVERAGE_PIN` opt-out, so the
/// arm that reinstates the pre-fix evacuation still reports how often the gate
/// would have fired — see `g1::G1Collector::root_coverage_incomplete_reason`.
///
/// Deliberately NOT folded into the generational
/// [`crate::gc_quiescence::record_moving_young_coverage_fallback`] counters:
/// that function's warn line says the cycle "runs the NON-MOVING sweep", which
/// G1 does not have. Sharing the counter would also merge two different
/// denominators into one number.
static G1_PAUSES: AtomicU64 = AtomicU64::new(0);
static G1_PAUSES_COVERAGE_INCOMPLETE: AtomicU64 = AtomicU64::new(0);

/// Per-reason census for the pauses counted above, indexed by
/// [`crate::gc_quiescence::incomplete_reason`] code.
///
/// The rate alone cannot be acted on. `G1Collector::root_coverage_incomplete_reason`
/// computes WHICH obligation failed and this counter used to be handed only
/// `is_some()`, so the reason was discarded at the one place it was known —
/// and a reader of `incomplete=58 (100.00%)` had no way to tell an
/// unregistered JIT frame from an unpublished bounds table from an OSR shadow.
/// Measured on H2 (`org.h2.test.store.TestMVStoreTool`, 612 compiled frames),
/// G1 reports 100% incomplete where the probes report 0%, and the reason is
/// exactly what that difference needed naming.
//
// NOTE (2026-09-20): the block above documents `G1_COVERAGE_REASONS`, which is
// declared ~40 lines down; the block below documents `eager_decline`. They were
// a single run-on doc comment attached to `eager_decline` alone, so the
// coverage-reason census was documented on the wrong item and `mod
// eager_decline` rendered with someone else's rationale above its own.
/// Why a pause declined to reclaim humongous spans eagerly.
///
/// The `declined_pauses` total says the feature did not run; it never said
/// WHICH gate stopped it, and on H2 the answer decides whether the remembered-
/// set source walk beneath it is buying anything at all. Same shape as
/// `G1_COVERAGE_REASONS`, and for the same reason: a count without a cause
/// cannot direct work.
pub mod eager_decline {
    pub const CENSUS_INCOMPLETE: usize = 0;
    pub const MARKING_ACTIVE: usize = 1;
    pub const GRAY_SET_NON_EMPTY: usize = 2;
    pub const EVACUATION_FAILURE: usize = 3;
    pub const SOURCE_WALK_ABORTED: usize = 4;
    pub const COUNT: usize = 5;

    pub const LABELS: [&str; COUNT] = [
        "census-incomplete",
        "marking-active",
        "gray-set-non-empty",
        "evacuation-failure",
        "source-walk-aborted",
    ];
}

static G1_EAGER_DECLINE_REASONS: [AtomicU64; eager_decline::COUNT] =
    [const { AtomicU64::new(0) }; eager_decline::COUNT];

/// Record which gate stopped an eager humongous reclaim.
pub fn record_g1_eager_decline_reason(code: usize) {
    if let Some(slot) = G1_EAGER_DECLINE_REASONS.get(code) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// `(label, count)` for every decline reason seen at least once.
pub fn g1_eager_decline_counts() -> Vec<(&'static str, u64)> {
    eager_decline::LABELS
        .iter()
        .enumerate()
        .filter_map(|(i, label)| {
            let n = G1_EAGER_DECLINE_REASONS[i].load(Ordering::Relaxed);
            (n > 0).then_some((*label, n))
        })
        .collect()
}

static G1_COVERAGE_REASONS: [AtomicU64; crate::gc_quiescence::incomplete_reason::COUNT] =
    [const { AtomicU64::new(0) }; crate::gc_quiescence::incomplete_reason::COUNT];

/// Count one G1 STW collection and, when its root set was incomplete, which
/// obligation failed. `None` is a complete root set.
pub fn record_g1_pause_coverage_reason(reason: Option<usize>) {
    G1_PAUSES.fetch_add(1, Ordering::Relaxed);
    if let Some(code) = reason {
        G1_PAUSES_COVERAGE_INCOMPLETE.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = G1_COVERAGE_REASONS.get(code) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// (`record_g1_pause_coverage(incomplete: bool)` was DELETED in round 10. Its
// own doc said "It currently has NO caller in the tree -- `g1.rs:21181` uses
// the reason form", and that stayed true: it was the superseded boolean half of
// `record_g1_pause_coverage_reason`, kept "for callers that have only the
// boolean" and never acquiring one. The question it answered -- how many G1
// pauses ran with an incomplete root set -- is answered by the reason form,
// which also says WHICH obligation failed. See
// docs/feature-designs/generative-gc-architecture.md.)

/// The non-zero rows of the per-reason census, as `(label, count)`.
pub fn g1_coverage_reason_counts() -> Vec<(&'static str, u64)> {
    G1_COVERAGE_REASONS
        .iter()
        .enumerate()
        .filter_map(|(code, c)| {
            let n = c.load(Ordering::Relaxed);
            (n > 0).then(|| (crate::gc_quiescence::incomplete_reason::label(code), n))
        })
        .collect()
}

/// `(total G1 pauses, pauses with an incomplete root set)`.
pub fn g1_pause_coverage_counts() -> (u64, u64) {
    (
        G1_PAUSES.load(Ordering::Relaxed),
        G1_PAUSES_COVERAGE_INCOMPLETE.load(Ordering::Relaxed),
    )
}

static G1_PAUSES_EMPTY_JIT_PUBLICATION: AtomicU64 = AtomicU64::new(0);

/// Count one G1 STW collection that ran with a live compiled frame and an EMPTY
/// conservative JIT root publication.
///
/// Recorded on DETECTION, not on refusal, and deliberately not gated on the
/// opt-in `CRATONVM_G1_PIN_EMPTY_PUBLICATION` lever: a pause in this state
/// evacuated against a root set it could not prove, which is worth counting on
/// a normal run precisely because the shipped default does not decline it.
///
/// Expected to be ZERO. G1 no longer skips the conservative JIT scan (see
/// `memory::roots::collect_roots`), so a live compiled frame that publishes no
/// pin means the scan ran over the frame's band and validated nothing in it.
/// Before that fix this counter measured something else entirely — how often
/// precise mode was active, which was every in-JIT pause on the workload it was
/// written for.
pub fn record_g1_pause_empty_jit_publication() {
    G1_PAUSES_EMPTY_JIT_PUBLICATION.fetch_add(1, Ordering::Relaxed);
}

/// Pauses that ran with a live compiled frame and an empty JIT root
/// publication. Denominator is `g1_pause_coverage_counts().0`.
pub fn g1_empty_jit_publication_count() -> u64 {
    G1_PAUSES_EMPTY_JIT_PUBLICATION.load(Ordering::Relaxed)
}

/// Record what a G1 pause (or the concurrent cleanup) decided.
///
/// Called from inside the pause, with the regions lock held, so the numbers
/// describe the collection set that actually ran rather than a later
/// re-derivation. `degraded` is a bitmask of [`g1_degraded`] flags.
pub fn record_g1_cycle(
    kind: u8,
    cset_young: u32,
    cset_old: u32,
    regions_pinned_out: u32,
    rset_sources_scanned: u32,
    degraded: u32,
) {
    debug_assert!(
        (degraded & !g1_degraded::ALL) == 0,
        "record_g1_cycle: degraded mask {degraded:#x} has bits outside g1_degraded::ALL — \
         a new flag was added without extending ALL (and therefore without a label)"
    );
    with_g1_cycle(|s| {
        s.kind.store(kind, Ordering::Relaxed);
        s.cset_young.store(cset_young as u64, Ordering::Relaxed);
        s.cset_old.store(cset_old as u64, Ordering::Relaxed);
        s.regions_pinned_out
            .store(regions_pinned_out as u64, Ordering::Relaxed);
        s.rset_sources_scanned
            .store(rset_sources_scanned as u64, Ordering::Relaxed);
        s.degraded.store(degraded as u64, Ordering::Relaxed);
        // Written LAST, same publication rule as the collector decision slot.
        s.sequence.fetch_add(1, Ordering::Release);
    });
}

/// The facts recorded for the last G1 cycle, or `None` if this process has run
/// none (every non-G1 run).
pub fn last_g1_cycle() -> Option<G1CycleFacts> {
    with_g1_cycle(|s| {
        let sequence = s.sequence.load(Ordering::Acquire);
        if sequence == 0 {
            return None;
        }
        Some(G1CycleFacts {
            sequence,
            kind: s.kind.load(Ordering::Relaxed),
            cset_young: s.cset_young.load(Ordering::Relaxed) as u32,
            cset_old: s.cset_old.load(Ordering::Relaxed) as u32,
            regions_pinned_out: s.regions_pinned_out.load(Ordering::Relaxed) as u32,
            rset_sources_scanned: s.rset_sources_scanned.load(Ordering::Relaxed) as u32,
            degraded: s.degraded.load(Ordering::Relaxed) as u32,
        })
    })
}

// ---------------------------------------------------------------------------
// Per-generation JMX beans (gen r4w2/obs, 2026-09-23)
// ---------------------------------------------------------------------------
//
// What `java.lang.management` reports for the generational heap. Until round 4
// wave 2 every backend answered with ONE collector named `G1 Young Generation`
// whose count was `minor + major`, and two pools (`Eden Space`, `Old Gen`)
// whose usage was the JMM "undefined" sentinel `-1` — although this heap has
// exact per-arena numbers. The generational backend is a copying (or, when it
// diverts, mark-sweeping) young generation over a mark-sweep tenured one, i.e.
// HotSpot's Serial shape — which is also what its JFR events already say
// (`DefNew` / `SerialOld`) — so it now reports Serial's beans:
//
// | bean                | HotSpot Serial        | here                                   |
// |---------------------|-----------------------|----------------------------------------|
// | young collector     | `Copy`                | cycles that did NOT reclaim old gen    |
// | full collector      | `MarkSweepCompact`    | cycles that DID (major or old sweep)   |
// | `Eden Space`        | eden                  | young live bytes above the survivors   |
// | `Survivor Space`    | from-survivor         | the last collection's young survivors  |
// | `Tenured Gen`       | tenured               | the old generation, committed granules |
//
// Micrometer, JConsole and VisualVM key on exactly these names
// (`Copy` -> young, `MarkSweepCompact` -> old; `... Eden Space` / `... Tenured
// Gen` pool classification). G1 and ZGC report HotSpot's own beans for their
// collector shapes since gc-common w7-f — see "G1 and ZGC beans" below.
//
// Survivors of the last young cycle live in the from-space beside new
// allocation (the semi-space pair has no separate survivor space). Until gen
// r5w3/obs7 they were therefore counted in `Eden Space` and `Survivor Space`
// was empty; since then the young live bytes are split at what the last
// collection left alive (`GenerationalHeap::jmx_memory_pools`): those bytes
// are `Survivor Space` (committed = used, carved out of the young commit),
// the rest `Eden Space`, so right after a young collection eden reads ~0 and
// survivor the survivors, as on Serial. The to-space is the copy reserve,
// which HotSpot counts in NO pool (Serial's survivor pool is the FROM
// survivor, the one that holds data) and in neither `totalMemory()` nor
// `maxMemory()`.
//
// gen r5w2/obs6 (2026-09-26): the pools now obey HotSpot's own sums, which a
// program can check (`GenR5W2HeapNumbersProbe.java`): the pools'
// `committed` sum to the heap `MemoryUsage.getCommitted()` and to
// `Runtime.totalMemory()`, their `used` to the heap's `used`, and their
// defined `max` to `Runtime.maxMemory()`. Until then `Survivor Space`
// reported the to-space's capacity as committed and `Tenured Gen` its
// reservation as committed, so the pools summed to `-Xmx` while the heap bean
// and `Runtime` reported committed granules.

/// `Copy` — HotSpot Serial's young collector bean name.
pub const JMX_SERIAL_YOUNG_COLLECTOR: &str = "Copy";
/// `MarkSweepCompact` — HotSpot Serial's full collector bean name.
pub const JMX_SERIAL_OLD_COLLECTOR: &str = "MarkSweepCompact";
/// Serial's eden pool name.
pub const JMX_POOL_EDEN: &str = "Eden Space";
/// Serial's survivor pool name.
pub const JMX_POOL_SURVIVOR: &str = "Survivor Space";
/// Serial's tenured pool name.
pub const JMX_POOL_TENURED: &str = "Tenured Gen";

/// Pools the young collector manages (HotSpot Serial: eden + survivor).
pub const JMX_SERIAL_YOUNG_POOLS: &[&str] = &[JMX_POOL_EDEN, JMX_POOL_SURVIVOR];
/// Pools the full collector manages (HotSpot Serial: all three).
pub const JMX_SERIAL_OLD_POOLS: &[&str] = &[JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED];

/// One `GarbageCollectorMXBean`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JmxCollector {
    pub name: &'static str,
    /// `getCollectionCount()`.
    pub count: u64,
    /// `getCollectionTime()`, milliseconds.
    pub time_ms: u64,
    /// zt t27/lg1 (page `zgc-zt13x10-review-residuals-*` item 9): the same
    /// accumulated pause time in nanoseconds (the perf counters'
    /// `sun.gc.collector.<n>.time` ticks, HotSpot's tick-precision timer);
    /// `time_ms` is its whole milliseconds.
    pub time_ns: u64,
    /// `getMemoryPoolNames()`.
    pub pools: &'static [&'static str],
    /// `GarbageCollectionNotificationInfo.getGcAction()` of this bean's
    /// notifications (gc-common w7-f: per bean, so a backend whose beans say
    /// `end of GC cycle` / `end of GC pause` needs no special case in
    /// [`gc_notifications_between`]).
    pub action: &'static str,
}

/// One heap `MemoryPoolMXBean`'s usage, in bytes. `None` is the JMM's
/// "undefined" (`-1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JmxPoolUsage {
    pub name: &'static str,
    /// `MemoryUsage.getInit()`; `None` when the initial size is not recorded.
    pub init: Option<u64>,
    pub used: u64,
    pub committed: u64,
    pub max: Option<u64>,
    /// `getCollectionUsage().getUsed()` — the pool's occupancy right after the
    /// most recent collection; `None` before the first one.
    pub collection_used: Option<u64>,
    /// `getMemoryManagerNames()`.
    pub managers: &'static [&'static str],
}

/// Split a heap's pause totals into Serial's two collector beans.
///
/// `all` and `old` are `(count, total_us, total_ms_ceiled)` over every
/// completed collection and over the ones that also reclaimed the old
/// generation (`GenerationalHeap::gc_pause_totals` / `gc_old_pause_totals`).
/// A full collection counts under the full collector ONLY, as on HotSpot, so
/// the two counts sum to `all.0`. Saturating: `old` is read after `all`, and a
/// collection completing between the two loads must not wrap.
///
/// # `getCollectionTime()` is HotSpot's arithmetic
///
/// gen r5w3/obs7 (2026-09-26;
/// `gengc-r5w2-obs6-collection-time-ceils-every-pause`): each bean's time is
/// the whole milliseconds of its MICROSECOND pause sum (`total_us / 1000`),
/// as HotSpot's `GCMemoryManager::_accumulated_timer` is converted once from
/// the summed elapsed time. It was the per-collection-ceiled figure (`.2`),
/// which charged every sub-millisecond pause a full millisecond: 1 000 young
/// pauses of 200 µs read 1 000 ms where HotSpot reads 200. The third tuple
/// element is no longer read here (it stays the `VmHeap::collection_time_ms`
/// fallback's figure).
///
/// H2's `Utils.collectGarbage()` (checked against its current source:
/// `while (count == getGarbageCollectionCount()) { runtime.gc(); }`, where
/// that "count" is `sum(getCollectionTime()) / poolCount`, rounded) needs the
/// summed time to MOVE eventually, not on every collection: the microsecond
/// sum grows with every completed collection, so the loop ends once the
/// accumulated pauses cross the next rounding step — exactly as it does on
/// HotSpot, which carries no per-collection guarantee either.
pub fn serial_collectors(all: (u64, u64, u64), old: (u64, u64, u64)) -> [JmxCollector; 2] {
    let old_count = old.0.min(all.0);
    let old_us = old.1.min(all.1);
    [
        JmxCollector {
            name: JMX_SERIAL_YOUNG_COLLECTOR,
            count: all.0 - old_count,
            time_ms: (all.1 - old_us) / 1_000,
            time_ns: (all.1 - old_us).saturating_mul(1_000),
            pools: JMX_SERIAL_YOUNG_POOLS,
            action: GC_ACTION_MINOR,
        },
        JmxCollector {
            name: JMX_SERIAL_OLD_COLLECTOR,
            count: old_count,
            time_ms: old_us / 1_000,
            time_ns: old_us.saturating_mul(1_000),
            pools: JMX_SERIAL_OLD_POOLS,
            action: GC_ACTION_MAJOR,
        },
    ]
}

/// The collectors that manage `pool`, by Serial's mapping.
pub fn serial_pool_managers(pool: &str) -> &'static [&'static str] {
    if pool == JMX_POOL_TENURED {
        &[JMX_SERIAL_OLD_COLLECTOR]
    } else {
        &[JMX_SERIAL_YOUNG_COLLECTOR, JMX_SERIAL_OLD_COLLECTOR]
    }
}

// ---------------------------------------------------------------------------
// G1 and ZGC beans (gc-common w7-f, 2026-09-24)
// ---------------------------------------------------------------------------
//
// Until wave 7 both backends answered ONE collector named `G1 Young
// Generation` (on ZGC too — the default collector) whose count was every
// collection, two pools (`Eden Space`, `Old Gen`) with the JMM "undefined"
// `-1` usage, and NO `GarbageCollectionNotificationInfo` at all
// (`GcNotificationThreadProbe` printed `NO-NOTIFICATIONS` on both). They now
// report HotSpot's beans for their collector shapes:
//
// | backend | collector              | counts                                   | action                        |
// |---------|------------------------|------------------------------------------|-------------------------------|
// | G1      | `G1 Young Generation`  | every collection pause (young and mixed) | `end of minor GC`             |
// | G1      | `G1 Old Generation`    | full collections — 0, except under the opt-in `CRATONVM_G1_FULL_GC_ONE_REPORT` (gci i12/ge: one per `force_full_collection`) | `end of major GC` |
// | G1      | `G1 Concurrent GC`     | the marking cycle's pauses (initial mark, remark + cleanup) | `end of concurrent GC pause` |
// | ZGC     | `ZGC Minor Cycles`     | young cycles of the opt-in generational mode | `end of GC cycle`         |
// | ZGC     | `ZGC Minor Pauses`     | the pause of each of those young cycles  | `end of GC pause`             |
// | ZGC     | `ZGC Major Cycles`     | every whole-heap collection              | `end of GC cycle`             |
// | ZGC     | `ZGC Major Pauses`     | every other stop-the-world pause (whole-heap collections and mark starts) | `end of GC pause` |
//
// Pools: G1 `G1 Eden Space` / `G1 Survivor Space` / `G1 Old Gen` (old =
// Old + humongous regions, as HotSpot), ZGC `ZGC Young Generation` /
// `ZGC Old Generation`, each managed by all four ZGC collectors.
//
// gcd d10/o retirement (2026-10-04): the ZGC names were HotSpot's
// NON-generational ZGC (`ZGC Cycles` / `ZGC Pauses` over `ZHeap`, JDK 15-22),
// on the argument that this ZGC's default mode has no young generation. JDK 23
// removed non-generational ZGC, so no JDK 25 HotSpot answers those names, and
// code written against JDK 25 that looks up `ZGC Major Cycles` or
// `ZGC Old Generation` found nothing. They are now HotSpot 25's four
// collectors and two pools (measured on 25.0.4, `Gcd10oBeanShapeProbe`): a
// whole-heap collection -- every collection of the default mode -- is a Major
// cycle plus its pause, as HotSpot reports a `System.gc()`; a young cycle of
// the opt-in generational mode is a Minor cycle plus its pause. The default
// mode keeps every object in `ZGC Old Generation` (there is no nursery to
// put in the young pool), so `ZGC Young Generation` reads 0 used.
//
// The counts and times are NOT the collectors' own counters. They are the
// common GC event plumbing's (`gc_events.rs`), per VM, one per door pause —
// [`BackendGcBeans::note_collection`] at the collection's sealed end,
// [`BackendGcBeans::note_non_collection_pause`] for a marking pause — so a
// G1 evacuation-failure drain pass (which G1's own `collection_count` books
// as another record inside the same pause) is not a second collection here,
// and every bean's figure is exactly what its notifications say.

/// HotSpot G1's young collector bean (young and mixed pauses).
pub const JMX_G1_YOUNG_COLLECTOR: &str = "G1 Young Generation";
/// HotSpot G1's full collector bean.
pub const JMX_G1_OLD_COLLECTOR: &str = "G1 Old Generation";
/// HotSpot G1's concurrent-cycle pause bean (JDK 20+).
pub const JMX_G1_CONCURRENT_COLLECTOR: &str = "G1 Concurrent GC";
/// HotSpot G1's eden pool.
pub const JMX_POOL_G1_EDEN: &str = "G1 Eden Space";
/// HotSpot G1's survivor pool.
pub const JMX_POOL_G1_SURVIVOR: &str = "G1 Survivor Space";
/// HotSpot G1's old pool (Old and humongous regions).
pub const JMX_POOL_G1_OLD: &str = "G1 Old Gen";
/// HotSpot 25 ZGC's young-cycle bean.
pub const JMX_ZGC_MINOR_CYCLES_COLLECTOR: &str = "ZGC Minor Cycles";
/// HotSpot 25 ZGC's young-pause bean.
pub const JMX_ZGC_MINOR_PAUSES_COLLECTOR: &str = "ZGC Minor Pauses";
/// HotSpot 25 ZGC's whole-heap cycle bean.
pub const JMX_ZGC_MAJOR_CYCLES_COLLECTOR: &str = "ZGC Major Cycles";
/// HotSpot 25 ZGC's whole-heap pause bean.
pub const JMX_ZGC_MAJOR_PAUSES_COLLECTOR: &str = "ZGC Major Pauses";
/// HotSpot 25 ZGC's young pool.
pub const JMX_POOL_ZGC_YOUNG: &str = "ZGC Young Generation";
/// HotSpot 25 ZGC's old pool.
pub const JMX_POOL_ZGC_OLD: &str = "ZGC Old Generation";

/// `getGcAction()` of a G1 remark / cleanup (`G1 Concurrent GC`).
pub const GC_ACTION_CONCURRENT_PAUSE: &str = "end of concurrent GC pause";
/// `getGcAction()` of a `ZGC Minor/Major Cycles` notification.
pub const GC_ACTION_CYCLE: &str = "end of GC cycle";
/// `getGcAction()` of a `ZGC Minor/Major Pauses` notification.
pub const GC_ACTION_PAUSE: &str = "end of GC pause";

/// The most collector beans one backend describes (ZGC's four).
pub const MAX_JMX_COLLECTORS: usize = 4;
/// The most heap pools one backend describes (Serial's and G1's three).
pub const MAX_JMX_POOLS: usize = 3;

const G1_ALL_POOLS: &[&str] = &[JMX_POOL_G1_EDEN, JMX_POOL_G1_SURVIVOR, JMX_POOL_G1_OLD];
const G1_OLD_POOL_ONLY: &[&str] = &[JMX_POOL_G1_OLD];
const ZGC_POOLS: &[&str] = &[JMX_POOL_ZGC_YOUNG, JMX_POOL_ZGC_OLD];
const ZGC_COLLECTORS: &[&str] = &[
    JMX_ZGC_MINOR_CYCLES_COLLECTOR,
    JMX_ZGC_MINOR_PAUSES_COLLECTOR,
    JMX_ZGC_MAJOR_CYCLES_COLLECTOR,
    JMX_ZGC_MAJOR_PAUSES_COLLECTOR,
];
/// zr z2/t2: `getMemoryManagerNames()` of both HotSpot 25 ZGC heap pools, in
/// HotSpot's order (host-observed, `GcfSerialObservabilityProbe`, census
/// vz0): the two cycle managers, then the two pause managers -- HotSpot's
/// `ZServiceability` adds the pools to the cycle managers before the pause
/// managers. The collector BEAN order (`ZGC_COLLECTORS`) is a different list
/// and is unchanged. `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER=0` restores
/// `ZGC_COLLECTORS` here.
const ZGC_POOL_MANAGERS_HOTSPOT: &[&str] = &[
    JMX_ZGC_MINOR_CYCLES_COLLECTOR,
    JMX_ZGC_MAJOR_CYCLES_COLLECTOR,
    JMX_ZGC_MINOR_PAUSES_COLLECTOR,
    JMX_ZGC_MAJOR_PAUSES_COLLECTOR,
];

/// zs s3/ob2: HotSpot's class-metadata pool (`MetaspacePool`), a non-heap
/// `MemoryPoolMXBean` managed by [`JMX_METASPACE_MANAGER`].
pub const JMX_POOL_METASPACE: &str = "Metaspace";
/// zs s3/ob2: HotSpot's `Metaspace Manager`, a plain (non-GC)
/// `MemoryManagerMXBean`.
pub const JMX_METASPACE_MANAGER: &str = "Metaspace Manager";
/// `getMemoryManagerNames()` of [`JMX_POOL_METASPACE`].
pub const METASPACE_POOL_MANAGERS: &[&str] = &[JMX_METASPACE_MANAGER];
/// zt t1/ob: HotSpot's compressed class space pool, managed by
/// [`JMX_METASPACE_MANAGER`] (gci i33/ms states its figures).
pub const JMX_POOL_COMPRESSED_CLASS_SPACE: &str = "Compressed Class Space";
/// zt t1/ob: HotSpot's `CodeCacheManager` (the three code heaps' manager).
pub const JMX_CODE_CACHE_MANAGER: &str = "CodeCacheManager";
/// `getMemoryManagerNames()` of a code heap.
const CODE_HEAP_POOL_MANAGERS: &[&str] = &[JMX_CODE_CACHE_MANAGER];
/// zt t1/ob: HotSpot 25's five non-heap pools with their managers, in
/// `_pools_list` order -- the names of
/// `cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES` (this crate does
/// not depend on that one; a `vm_init.rs` test pins the two lists equal). A
/// ZGC pause record carries an all-zero usage for each
/// ([`BackendGcBeans::unrecord_pause_usage`]).
pub const HOTSPOT_NON_HEAP_POOLS: [(&str, &[&str]); 5] = [
    ("CodeHeap 'non-nmethods'", CODE_HEAP_POOL_MANAGERS),
    ("CodeHeap 'profiled nmethods'", CODE_HEAP_POOL_MANAGERS),
    ("CodeHeap 'non-profiled nmethods'", CODE_HEAP_POOL_MANAGERS),
    (JMX_POOL_METASPACE, METASPACE_POOL_MANAGERS),
    (JMX_POOL_COMPRESSED_CLASS_SPACE, METASPACE_POOL_MANAGERS),
];

/// zt t30/nh: HotSpot 25's non-heap pools with an unsegmented code cache
/// (`cratonvm_native_api::registry::JMX_UNSEGMENTED_NON_HEAP_POOL_NAMES`;
/// a `vm_init.rs` test pins the two lists equal): ONE `CodeCache` pool, then
/// the two metaspace pools.
pub const HOTSPOT_UNSEGMENTED_NON_HEAP_POOLS: [(&str, &[&str]); 3] = [
    (UNSEGMENTED_CODE_CACHE_POOL, CODE_HEAP_POOL_MANAGERS),
    (JMX_POOL_METASPACE, METASPACE_POOL_MANAGERS),
    (JMX_POOL_COMPRESSED_CLASS_SPACE, METASPACE_POOL_MANAGERS),
];

use cratonvm_types::code_cache_layout::{CodeCacheLayout, UNSEGMENTED_CODE_CACHE_POOL};

/// zt t1/ob: one walk's HotSpot-shaped metaspace figures
/// (`vm_init::metaspace_stats`: the model's used total, its 64 KiB-granule
/// committed total and the compressed class space's part), kept with the
/// `Metaspace` memo so a ZGC `GcInfo` carries the `committed` and the
/// `Compressed Class Space` figures the pool beans report
/// (`jmx.rs::non_heap_pool_from`), at no extra walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetaspaceSpaceFigures {
    /// Both spaces' used bytes (`MetaspaceUtils::used_bytes()`).
    pub used: u64,
    /// Both spaces' committed bytes.
    pub committed: u64,
    /// The class space's `(used, committed, reserved)`; `None` is the
    /// no-class-space shape (`CRATONVM_METASPACE_HOTSPOT_SPACES=0`).
    pub class_space: Option<(u64, u64, u64)>,
}



/// `CRATONVM_ZGC_PAUSE_PHASES` -- default ON, `=0` kill. zs s4/ob3. Read
/// once per heap, by both readers: [`ZgcBeanShapeSwitches::from_env`] (the
/// beans book the split) and the ZGC heap (its pause clock records the
/// boundaries, `zgc::jfr_facts::ZJfrFacts`).
pub fn zgc_pause_phases_enabled() -> bool {
    true
}

/// zs s4/ob3: one ZGC stop-the-world collection cut, at its own phase
/// boundaries, into the pauses of a HotSpot 25 ZGC cycle it has a real phase
/// for. HotSpot books every stop-the-world pause of a cycle on the
/// `... Pauses` bean (local 25.0.3, `-Xlog:gc,gc+phases`): a minor cycle has
/// three (`Pause Mark Start`, `Pause Mark End`, `Pause Relocate Start`), a
/// major cycle five (the young collection's three, then the old generation's
/// `Pause Mark End` and `Pause Relocate Start`), a `System.gc()` major eight
/// (a promote-all young collection's three first). This collector does in ONE
/// pause what those pauses bracket, in the same order, so the pause is cut
/// where its own laps end:
///
/// | record | the collection's work in it |
/// |---|---|
/// | `Pause Mark Start` | everything up to the end of the object-start snapshot: the door's preamble, the TLAB retire, the snapshot. Absent when the cycle's mark start was its own pause (a concurrent mark ran), which is already booked. |
/// | `Pause Mark End` | the mark (or the join of a concurrent one) through reference processing |
/// | `Pause Relocate Start` | the sweep, the slide and everything to the end of the pause |
///
/// So the records' durations sum to the pause, and no record covers time the
/// world was not stopped. What it cannot match: a major cycle's old
/// generation pauses and a `System.gc()`'s promote-all young collection are
/// separate marks and relocations on HotSpot; this collector runs one
/// whole-heap mark and one relocation, so it has no phase to give them
/// (`zgc-zs4ob3-major-cycles-book-three-pauses-where-hotspot-books-five-or-eight-CLOSED-20261019.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZgcPauseSplit {
    bounds: [std::time::Instant; 2],
    len: u8,
}

impl ZgcPauseSplit {
    /// The boundaries a collection measured: where its mark start ended
    /// (`None` when its mark start was a pause of its own) and where its mark
    /// end ended.
    pub fn new(
        mark_start_end: Option<std::time::Instant>,
        mark_end_end: std::time::Instant,
    ) -> Self {
        match mark_start_end {
            Some(ms) => Self {
                bounds: [ms, mark_end_end],
                len: 2,
            },
            None => Self {
                bounds: [mark_end_end, mark_end_end],
                len: 1,
            },
        }
    }

    /// The interior boundaries, in order.
    pub fn bounds(&self) -> &[std::time::Instant] {
        &self.bounds[..usize::from(self.len)]
    }

    /// How many pause records the collection is booked as.
    pub fn pauses(&self) -> usize {
        usize::from(self.len) + 1
    }

    /// Do the boundaries lie, in order, inside `[start, end]`? A split that
    /// does not (a boundary left by another collection) is not used: the
    /// collection is then booked as one pause.
    pub fn fits(&self, start: std::time::Instant, end: std::time::Instant) -> bool {
        let mut prev = start;
        for &b in self.bounds() {
            if b < prev || b > end {
                return false;
            }
            prev = b;
        }
        true
    }

    /// The pause spans over `[start, end]`, each boundary clamped into it
    /// (so there are always [`Self::pauses`] spans, contiguous, from `start`
    /// to `end`).
    pub fn spans(
        &self,
        start: std::time::Instant,
        end: std::time::Instant,
    ) -> ([(std::time::Instant, std::time::Instant); 3], usize) {
        let end = end.max(start);
        let mut out = [(start, end); 3];
        let mut prev = start;
        let n = self.pauses();
        for (i, &b) in self.bounds().iter().enumerate() {
            let b = b.clamp(prev, end);
            out[i] = (prev, b);
            prev = b;
        }
        out[n - 1] = (prev, end);
        (out, n)
    }
}

fn span_nanos(span: (std::time::Instant, std::time::Instant)) -> u64 {
    u64::try_from(span.1.saturating_duration_since(span.0).as_nanos()).unwrap_or(u64::MAX)
}

/// zs s2/ob: the ZGC-only JMX shapes of HotSpot 25's `ZServiceability`
/// that [`BackendGcBeans`] follows, each a default-ON switch with a `=0`
/// kill, read once per heap. Every one is `false` on a G1 shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZgcBeanShapeSwitches {
    /// `CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE`: a `ZGC ... Pauses` record
    /// carries all-zero pool usages ([`BackendGcBeans::unrecord_pause_usage`]).
    pub pause_usage_unrecorded: bool,
    /// `CRATONVM_ZGC_POOL_PEAK_AT_GC`: each pool's peak is sampled at both
    /// edges of every collection ([`BackendGcBeans::note_pool_peaks`]).
    pub pool_peaks_at_gc: bool,
    /// `CRATONVM_ZGC_HOTSPOT_POOL_INIT`: `ZGC Young Generation`'s `init` is
    /// `MinHeapSize`, `ZGC Old Generation`'s 0 ([`BackendGcBeans::pools`]).
    pub hotspot_pool_init: bool,
    /// zs s3/ob2 `CRATONVM_ZGC_JMX_METASPACE_POOL`: the non-heap `Metaspace`
    /// pool and its `Metaspace Manager` are listed, and every collection's
    /// `GcInfo` carries a `Metaspace` key
    /// ([`BackendGcBeans::append_metaspace_usage`]).
    pub metaspace_pool: bool,
    /// zs s4/ob3 `CRATONVM_ZGC_PAUSE_PHASES`: a collection is booked on its
    /// `... Pauses` bean as one record per HotSpot pause it has a real phase
    /// for ([`ZgcPauseSplit`], [`BackendGcBeans::note_collection_split`]).
    pub pause_phases: bool,
    /// zs s6/ob5 `CRATONVM_ZGC_METASPACE_FIRST_CYCLES`: a cycle record built
    /// before the `Metaspace` pool was handed out keeps the class-store
    /// census of its edges and gains the `Metaspace` key the first time a
    /// `GcInfo` can name the pool ([`BackendGcBeans::backfill_metaspace`]).
    pub metaspace_first_cycles: bool,
    /// zs s6/ob5 `CRATONVM_ZGC_METASPACE_THRESHOLD`: a `Metaspace` usage
    /// threshold is armed and checked when it is set and at the end of every
    /// collection, as HotSpot 25's `LowMemoryDetector` checks it.
    pub metaspace_threshold: bool,
    /// zs s6/ob5 `CRATONVM_ZGC_METASPACE_PEAK_AT_GC`: the `Metaspace` edge
    /// samples are that pool's peaks too (with [`Self::pool_peaks_at_gc`]).
    pub metaspace_peak_at_gc: bool,
    /// zs s6/ob5 `CRATONVM_ZGC_THRESHOLD_SET_DRAIN`: a usage-threshold
    /// crossing found when the threshold is set requests asynchronous delivery
    /// ([`BackendGcBeans::threshold_set_drain_enabled`]).
    pub threshold_set_drain: bool,
    /// zt t1/ob `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`: ZGC lists HotSpot
    /// 25's five non-heap pools and two plain managers (gci i15/nh's model,
    /// as every other backend lists them), not zs s3/ob2's `Metaspace`-only
    /// set; a pause record carries an all-zero usage for each of the five,
    /// and a cycle record's `Compressed Class Space` key comes from the edge
    /// walk its `Metaspace` key comes from
    /// ([`BackendGcBeans::hotspot_non_heap_pools_enabled`]).
    pub hotspot_non_heap_pools: bool,
    /// zt t3/ob `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`: each of the three
    /// `CodeHeap '...'` pools reports HotSpot's `init`
    /// ([`HOTSPOT_CODE_HEAP_INIT_BYTES`]), so `getNonHeapMemoryUsage().getInit()`
    /// is HotSpot's 7667712, not 2 MiB
    /// ([`BackendGcBeans::hotspot_code_heap_init_bytes`]).
    pub hotspot_code_heap_init: bool,
    /// zt t6/ob4 `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES`: with
    /// [`Self::hotspot_non_heap_pools`], a cycle record carries the three
    /// `CodeHeap '...'` entries sampled at its two edges
    /// ([`BackendGcBeans::append_code_heap_usage`]), as HotSpot's
    /// `GCMemoryManager::gc_begin` / `gc_end` sample every pool; `=0`: they
    /// are rendered from the usage at delivery (`jmx.rs::gc_info_pool_usage`).
    pub code_heap_edges: bool,
}

/// zt t3/ob: HotSpot 25's `init` of each `CodeHeap '...'` pool on x86_64:
/// the pool is built (`MemoryService::add_code_heap_memory_pool`) with the
/// heap's capacity right after `CodeCache::add_heap` committed its first
/// `MIN2(InitialCodeCacheSize, reserved)` -- `InitialCodeCacheSize` is
/// 2496K -- for each of the three heaps of the default segmented code cache.
/// Local 25.0.3 `-XX:+UseZGC`: three pools of 2555904, a non-heap `init` of
/// 7667712 (`Metaspace` and `Compressed Class Space` contribute 0).
pub const HOTSPOT_CODE_HEAP_INIT_BYTES: u64 = 2_555_904;

/// zt t6/ob4: gci i15/nh's `init` of `CodeHeap 'non-profiled nmethods'`
/// (`jmx.rs` `NON_HEAP_INIT_BYTES`, 2 MiB) under
/// `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT=0`; the other two heaps' is 0 there.
const GCI_NON_PROFILED_CODE_HEAP_INIT_BYTES: u64 = 2 * 1024 * 1024;

/// zt t6/ob4: the code heap that carries the JIT's whole code arena
/// (`jmx.rs` `non_heap_pool_from`); the other two read 0 bytes.
const CODE_HEAP_NON_PROFILED: &str = "CodeHeap 'non-profiled nmethods'";

impl ZgcBeanShapeSwitches {
    /// Every switch at its default (ON).
    pub const DEFAULT: Self = Self {
        pause_usage_unrecorded: true,
        pool_peaks_at_gc: true,
        hotspot_pool_init: true,
        metaspace_pool: true,
        pause_phases: true,
        metaspace_first_cycles: true,
        metaspace_threshold: true,
        metaspace_peak_at_gc: true,
        threshold_set_drain: true,
        hotspot_non_heap_pools: true,
        hotspot_code_heap_init: true,
        code_heap_edges: true,
    };
    /// Every switch killed (the pre-zs2 shapes).
    pub const KILLED: Self = Self {
        pause_usage_unrecorded: false,
        pool_peaks_at_gc: false,
        hotspot_pool_init: false,
        metaspace_pool: false,
        pause_phases: false,
        metaspace_first_cycles: false,
        metaspace_threshold: false,
        metaspace_peak_at_gc: false,
        threshold_set_drain: false,
        hotspot_non_heap_pools: false,
        hotspot_code_heap_init: false,
        code_heap_edges: false,
    };

    /// The process's settings (`runtime_flag_default_on` per key).
    pub fn from_env() -> Self {
        let on = cratonvm_types::flags::runtime_flag_default_on;
        Self {
            pause_usage_unrecorded: on("CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE"),
            pool_peaks_at_gc: on("CRATONVM_ZGC_POOL_PEAK_AT_GC"),
            hotspot_pool_init: on("CRATONVM_ZGC_HOTSPOT_POOL_INIT"),
            metaspace_pool: on("CRATONVM_ZGC_JMX_METASPACE_POOL"),
            pause_phases: zgc_pause_phases_enabled(),
            metaspace_first_cycles: on("CRATONVM_ZGC_METASPACE_FIRST_CYCLES"),
            metaspace_threshold: on("CRATONVM_ZGC_METASPACE_THRESHOLD"),
            metaspace_peak_at_gc: on("CRATONVM_ZGC_METASPACE_PEAK_AT_GC"),
            threshold_set_drain: on("CRATONVM_ZGC_THRESHOLD_SET_DRAIN"),
            hotspot_non_heap_pools: on("CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS"),
            hotspot_code_heap_init: on("CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT"),
            code_heap_edges: on("CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES"),
        }
    }
}

/// zt t1/ob, t3/ob: the `Compressed Class Space` usage one walk's figures
/// `f` state, in the pool bean's shape (`jmx.rs::non_heap_pool_from`:
/// `init = 0`, `max` the class space reservation, never below `committed`);
/// `None` in the no-class-space shape.
fn class_space_usage_of(f: &MetaspaceSpaceFigures) -> Option<JmxPoolUsage> {
    let (class_used, class_committed, reserved) = f.class_space?;
    let committed = class_committed.max(class_used);
    Some(JmxPoolUsage {
        name: JMX_POOL_COMPRESSED_CLASS_SPACE,
        init: Some(0),
        used: class_used,
        committed,
        max: Some(reserved.max(committed)),
        collection_used: None,
        managers: METASPACE_POOL_MANAGERS,
    })
}

/// The usage HotSpot reports for a pool a memory manager did not record
/// (`GCStatInfo`'s arrays start as `MemoryUsage()`: init, used, committed and
/// max all 0) -- what a `ZGC ... Pauses` `GcInfo` holds for every pool,
/// because HotSpot 25's `ZServiceabilityPauseTracer` opens its
/// `TraceMemoryManagerStats` with `recordPreGCUsage`, `recordPeakUsage` and
/// `recordPostGCUsage` all false (local 25.0.3, `Zs2ObBeanSurface`:
/// `gcinfo_usage_before=zero gcinfo_usage_after=zero` on `ZGC Major Pauses`).
fn unrecorded_pool_usage(p: &JmxPoolUsage) -> JmxPoolUsage {
    JmxPoolUsage {
        init: Some(0),
        used: 0,
        committed: 0,
        max: Some(0),
        ..p.clone()
    }
}

/// Which HotSpot bean set a [`BackendGcBeans`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendBeanShape {
    /// `G1 Young Generation` / `G1 Old Generation` / `G1 Concurrent GC`.
    G1,
    /// `ZGC Minor Cycles` / `ZGC Minor Pauses` / `ZGC Major Cycles` /
    /// `ZGC Major Pauses` (HotSpot 25's bean order).
    Zgc,
}

impl BackendBeanShape {
    /// The collector bean names, in bean order.
    pub fn collectors(self) -> &'static [&'static str] {
        match self {
            BackendBeanShape::G1 => &[
                JMX_G1_YOUNG_COLLECTOR,
                JMX_G1_OLD_COLLECTOR,
                JMX_G1_CONCURRENT_COLLECTOR,
            ],
            BackendBeanShape::Zgc => ZGC_COLLECTORS,
        }
    }

    /// The heap pool names, in pool order (the order
    /// [`BackendGcBeans::pools`] expects its samples in).
    pub fn pools(self) -> &'static [&'static str] {
        match self {
            BackendBeanShape::G1 => G1_ALL_POOLS,
            BackendBeanShape::Zgc => ZGC_POOLS,
        }
    }

    /// `getMemoryPoolNames()` of collector `i`. HotSpot's G1 young collector
    /// lists `G1 Old Gen` too (a mixed pause collects old regions); its
    /// concurrent bean lists only `G1 Old Gen`.
    fn collector_pools(self, i: usize) -> &'static [&'static str] {
        match (self, i) {
            (BackendBeanShape::G1, 2) => G1_OLD_POOL_ONLY,
            (BackendBeanShape::G1, _) => G1_ALL_POOLS,
            (BackendBeanShape::Zgc, _) => ZGC_POOLS,
        }
    }

    fn collector_action(self, i: usize) -> &'static str {
        match (self, i) {
            (BackendBeanShape::G1, 0) => GC_ACTION_MINOR,
            (BackendBeanShape::G1, 1) => GC_ACTION_MAJOR,
            (BackendBeanShape::G1, _) => GC_ACTION_CONCURRENT_PAUSE,
            (BackendBeanShape::Zgc, 0 | 2) => GC_ACTION_CYCLE,
            (BackendBeanShape::Zgc, _) => GC_ACTION_PAUSE,
        }
    }

    /// `getMemoryManagerNames()` of `pool`: every collector that lists it.
    pub fn pool_managers(self, pool: &str) -> &'static [&'static str] {
        match self {
            BackendBeanShape::G1 if pool == JMX_POOL_G1_OLD => &[
                JMX_G1_YOUNG_COLLECTOR,
                JMX_G1_OLD_COLLECTOR,
                JMX_G1_CONCURRENT_COLLECTOR,
            ],
            BackendBeanShape::G1 => &[JMX_G1_YOUNG_COLLECTOR, JMX_G1_OLD_COLLECTOR],
            BackendBeanShape::Zgc => ZGC_COLLECTORS,
        }
    }

    /// The beans one collection pause of `kind` counts under: G1's young bean
    /// (a mixed pause is a young-collector pause on HotSpot too; this G1 runs
    /// no full collection, so `kind` does not change it), or a ZGC cycle bean
    /// AND its pause bean (a collection is a cycle and a pause): the Minor
    /// pair for a young cycle, the Major pair otherwise.
    fn collection_beans(self, kind: CollectionKind) -> &'static [usize] {
        match (self, kind) {
            (BackendBeanShape::G1, _) => &[0],
            (BackendBeanShape::Zgc, CollectionKind::Minor) => &[0, 1],
            (BackendBeanShape::Zgc, CollectionKind::Major) => &[2, 3],
        }
    }

    /// The bean a pause that collects nothing counts under: `G1 Concurrent
    /// GC`, or `ZGC Major Pauses` (a mark start opens a whole-heap cycle).
    fn non_collection_pause_bean(self) -> usize {
        match self {
            BackendBeanShape::G1 => 2,
            BackendBeanShape::Zgc => 3,
        }
    }
}

/// Which collection a backend pause was, for [`BackendGcBeans::note_collection_of`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionKind {
    /// A young-only cycle (ZGC's opt-in generational mode).
    Minor,
    /// Every other collection: a G1 pause, a whole-heap ZGC cycle.
    Major,
}

/// One heap pool's usage as the dispatcher samples it (`VmHeap`), before
/// [`BackendGcBeans::pools`] names it and adds its managers and collection
/// usage. `None` is the JMM's undefined `-1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JmxPoolSample {
    pub init: Option<u64>,
    pub used: u64,
    pub committed: u64,
    pub max: Option<u64>,
}

impl JmxPoolSample {
    /// `java.lang.management.MemoryUsage`'s constructor invariants —
    /// `used <= committed` and `committed <= max` when `max` is defined — made
    /// true of a sample whose parts were read at slightly different moments
    /// (a region census against a committed-prefix load), rather than handed
    /// to a JDK path that throws `IllegalArgumentException` on them.
    pub fn normalized(self) -> Self {
        let committed = self.committed.max(self.used);
        Self {
            init: self.init,
            used: self.used,
            committed,
            max: self.max.map(|m| m.max(committed)),
        }
    }
}

/// A G1 or ZGC heap's JMX bean state, per heap and therefore per VM (no
/// process global): each bean's count and summed pause time as the common GC
/// event plumbing books them, each pool's usage after the last collection,
/// and the GC-notification queue (`getLastGcInfo()` and listener delivery).
/// The generational backend keeps its own ([`GcNotificationQueue`] on
/// `GenerationalHeap`, counts from its pause totals).
#[derive(Debug)]
pub struct BackendGcBeans {
    shape: BackendBeanShape,
    counts: [AtomicU64; MAX_JMX_COLLECTORS],
    pause_ns: [AtomicU64; MAX_JMX_COLLECTORS],
    /// `getCollectionUsage().getUsed()` per pool, stamped at the sealed end of
    /// every collection.
    collection_used: [AtomicU64; MAX_JMX_POOLS],
    /// A collection has stamped `collection_used` (before: undefined).
    collection_recorded: std::sync::atomic::AtomicBool,
    /// gci i14/mp finding 3: each pool's committed at the end of the last
    /// collection that AFFECTED it (`getCollectionUsage().getCommitted()`),
    /// valid where `collection_stamped` has the pool's bit.
    collection_committed: [AtomicU64; MAX_JMX_POOLS],
    /// gci i14/mp finding 3: bit `i` = pool `i` has been affected by a
    /// collection (HotSpot's `set_last_collection_usage` ran for it).
    collection_stamped: std::sync::atomic::AtomicU8,
    notifications: GcNotificationQueue,
    /// zr z2/t2: `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER` on a ZGC shape (always
    /// `false` on G1), read at construction.
    zgc_hotspot_order: bool,
    /// zs s2/ob: the ZGC-only shapes ([`ZgcBeanShapeSwitches`]); all `false`
    /// on G1.
    zgc_shape: ZgcBeanShapeSwitches,
    /// zs s2/ob: HotSpot's `MinHeapSize` for `ZGC Young Generation`'s `init`
    /// ([`Self::set_zgc_min_capacity`]); `u64::MAX` until set.
    zgc_min_capacity: AtomicU64,
    /// zs s3/ob2: the `Metaspace` pool's collection-edge samples.
    metaspace: MetaspacePoolState,
    /// zt t30/nh: the VM's HotSpot code cache layout
    /// ([`Self::set_code_cache_layout`], `CRATONVM_JMX_CODE_CACHE_HOTSPOT_LAYOUT`);
    /// unset: HotSpot's default (three heaps of 2555904).
    code_cache_layout: std::sync::OnceLock<CodeCacheLayout>,
}

/// zs s3/ob2: what [`BackendGcBeans`] keeps for the non-heap `Metaspace` pool
/// (`CRATONVM_ZGC_JMX_METASPACE_POOL`). The figure itself is the VM's measured
/// class metadata (the same walk the non-heap `MemoryUsage`, the heap summary
/// and the `gc,exit` `Metaspace` line report); this crate only keeps the
/// samples taken at the edges of a collection and the memo that lets a
/// collection reuse the last walk while the loaded-class set is unchanged.
#[derive(Debug)]
struct MetaspacePoolState {
    /// `MemoryImpl.getMemoryPools0` has handed the pool out, so a `GcInfo`
    /// can name it: the collection edges sample only from then on.
    published: std::sync::atomic::AtomicBool,
    /// `-XX:MaxMetaspaceSize` (the pool's `max`); `u64::MAX` = undefined (-1).
    max: AtomicU64,
    /// The last walk: `((loaded classes, unloaded classes), bytes)`.
    memo: parking_lot::Mutex<Option<((u64, u64), u64)>>,
    /// zt t1/ob: the HotSpot-shaped figures of the last two walks that
    /// stated them ([`BackendGcBeans::metaspace_sample_figures`]), newest
    /// first, looked up by their `used` figure: an edge's `committed` and
    /// `Compressed Class Space` usage. Taken after `memo` when both are held.
    ///
    /// zt t3/ob (`zgc-zt2x1-wave-t1-review-residuals` item 7): each with the
    /// census key `(loaded, unloaded)` of its walk, and at most one entry per
    /// `used` figure (a newer walk with the same `used` replaces the older),
    /// so a `used` lookup can never answer another walk's `committed`.
    figures: parking_lot::Mutex<[Option<((u64, u64), MetaspaceSpaceFigures)>; 2]>,
    /// The sample taken at the start of the collection in flight;
    /// `u64::MAX` = none.
    before: AtomicU64,
    /// zt t6/ob4: the JIT's mapped code bytes at the start of the collection
    /// in flight ([`BackendGcBeans::note_code_heap_before`]); `u64::MAX` =
    /// none. Kept here with the other non-heap edge sample.
    code_before: AtomicU64,
    /// zs s6/ob5: the class-store census of the cycle records built before
    /// the pool was handed out ([`BackendGcBeans::note_metaspace_deferred`]),
    /// oldest first, at most [`METASPACE_DEFERRED_CAP`].
    deferred: parking_lot::Mutex<std::collections::VecDeque<MetaspaceDeferredCut>>,
    /// Mirror of `deferred.len()`: a drain with nothing to back-fill pays one
    /// load and no lock.
    deferred_len: std::sync::atomic::AtomicUsize,
    /// zt t6/rv3 (`zgc-zs10rv7-*` item 5): the `(class definition epoch,
    /// collection count)` at which a drain's busy-lock retries last failed;
    /// `(u64::MAX, u64::MAX)` = none. While both stand, a drain keeps the hold
    /// after one non-blocking try instead of paying the retries again
    /// ([`BackendGcBeans::metaspace_backfill_held_at`]).
    backfill_hold_epoch: AtomicU64,
    backfill_hold_collections: AtomicU64,
    /// Busy-lock retry passes the drain ran (diagnostic; the memo's control).
    backfill_retry_passes: AtomicU64,
}

/// zs s6/ob5: one cycle record whose `Metaspace` key waits for the pool to
/// be handed out: the record (`collector`, `id`) and the class-store size
/// (the next `ClassId`, so every class defined up to that edge) at its
/// start and end edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MetaspaceDeferredCut {
    collector: &'static str,
    id: u64,
    before: u32,
    after: u32,
}

/// zs s6/ob5: the most deferred `Metaspace` census entries kept. A record is
/// rendered only while it is a bean's last collection or waits in the
/// notification queue, so older entries could never be read.
const METASPACE_DEFERRED_CAP: usize = GC_NOTIFICATION_QUEUE_CAPACITY + MAX_JMX_COLLECTORS;

impl Default for MetaspacePoolState {
    fn default() -> Self {
        Self {
            published: std::sync::atomic::AtomicBool::new(false),
            max: AtomicU64::new(u64::MAX),
            memo: parking_lot::Mutex::new(None),
            figures: parking_lot::Mutex::new([None, None]),
            before: AtomicU64::new(u64::MAX),
            code_before: AtomicU64::new(u64::MAX),
            deferred: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            deferred_len: std::sync::atomic::AtomicUsize::new(0),
            backfill_hold_epoch: AtomicU64::new(u64::MAX),
            backfill_hold_collections: AtomicU64::new(u64::MAX),
            backfill_retry_passes: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
#[path = "set_time_delivery_tests.rs"]
mod set_time_delivery_tests;

impl BackendGcBeans {
    pub fn new(shape: BackendBeanShape) -> Self {
        let zgc_hotspot_order = shape == BackendBeanShape::Zgc;
        let zgc_shape = if shape == BackendBeanShape::Zgc {
            ZgcBeanShapeSwitches::from_env()
        } else {
            ZgcBeanShapeSwitches::KILLED
        };
        Self::with_switches(shape, zgc_hotspot_order, zgc_shape)
    }

    /// [`Self::new`] with the ZGC HotSpot-order switch given rather than read
    /// (tests; `false` is the `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER=0` arm). The
    /// zs s2/ob shapes are at their defaults on a ZGC shape.
    pub fn with_order(shape: BackendBeanShape, zgc_hotspot_order: bool) -> Self {
        Self::with_switches(shape, zgc_hotspot_order, ZgcBeanShapeSwitches::DEFAULT)
    }

    /// [`Self::new`] with every ZGC switch given (tests). `zgc_shape` is
    /// ignored (all off) on a G1 shape.
    pub fn with_switches(
        shape: BackendBeanShape,
        zgc_hotspot_order: bool,
        zgc_shape: ZgcBeanShapeSwitches,
    ) -> Self {
        let zgc = shape == BackendBeanShape::Zgc;
        Self {
            shape,
            counts: Default::default(),
            pause_ns: Default::default(),
            collection_used: Default::default(),
            collection_recorded: std::sync::atomic::AtomicBool::new(false),
            collection_committed: Default::default(),
            collection_stamped: std::sync::atomic::AtomicU8::new(0),
            notifications: GcNotificationQueue::with_collectors(shape.collectors()),
            zgc_hotspot_order: zgc && zgc_hotspot_order,
            zgc_shape: if zgc {
                zgc_shape
            } else {
                ZgcBeanShapeSwitches::KILLED
            },
            zgc_min_capacity: AtomicU64::new(u64::MAX),
            metaspace: MetaspacePoolState::default(),
            code_cache_layout: std::sync::OnceLock::new(),
        }
    }

    /// zt t30/nh: the VM's code cache layout (from its `-XX:` options), so a
    /// ZGC `GcInfo`'s code pool keys are the ones the pool beans list (one
    /// `CodeCache` when unsegmented) with their `init`. First call wins.
    pub fn set_code_cache_layout(&self, layout: CodeCacheLayout) {
        let _ = self.code_cache_layout.set(layout);
    }

    /// zt t30/nh: the code cache layout the code pools follow on this heap,
    /// under the same gate as [`Self::hotspot_code_heap_init_bytes`].
    pub fn hotspot_code_cache_layout(&self) -> Option<CodeCacheLayout> {
        self.hotspot_code_heap_init_bytes()?;
        Some(
            self.code_cache_layout
                .get()
                .copied()
                .unwrap_or(CodeCacheLayout::DEFAULT_SEGMENTED),
        )
    }

    /// zt t30/nh: the non-heap pools (with managers) a `GcInfo` names on this
    /// heap: [`HOTSPOT_UNSEGMENTED_NON_HEAP_POOLS`] for an unsegmented code
    /// cache, else [`HOTSPOT_NON_HEAP_POOLS`].
    fn hotspot_non_heap_pool_list(&self) -> &'static [(&'static str, &'static [&'static str])] {
        match self.hotspot_code_cache_layout() {
            Some(l) if !l.segmented => &HOTSPOT_UNSEGMENTED_NON_HEAP_POOLS,
            _ => &HOTSPOT_NON_HEAP_POOLS,
        }
    }

    /// zs s2/ob: HotSpot's `MinHeapSize` (`-Xms`, else `min(8M, initial)`:
    /// the `[gc,init] Min Capacity:` figure). HotSpot 25's `ZServiceability`
    /// builds `ZGC Young Generation` with that `init` and `ZGC Old
    /// Generation` with 0 (local 25.0.3: young `init = 8388608` by default,
    /// `67108864` at `-Xms64m`, `6291456` at `-Xmx6m`; old `init = 0` in
    /// all three). Until set (and under `CRATONVM_ZGC_HOTSPOT_POOL_INIT=0`)
    /// the pools keep the sampler's split: young 0, old the startup commit.
    pub fn set_zgc_min_capacity(&self, bytes: u64) {
        self.zgc_min_capacity.store(bytes, Ordering::Relaxed);
    }

    /// The `init` HotSpot gives ZGC pool `i` (young 0, old 1), when the
    /// switch is on and the minimum is known.
    fn zgc_pool_init(&self, i: usize) -> Option<u64> {
        if !self.zgc_shape.hotspot_pool_init {
            return None;
        }
        let min = self.zgc_min_capacity.load(Ordering::Relaxed);
        if min == u64::MAX {
            return None;
        }
        Some(if i == 0 { min } else { 0 })
    }

    /// zs s3/ob2: is the non-heap `Metaspace` pool modelled on this heap?
    /// `CRATONVM_ZGC_JMX_METASPACE_POOL` (default ON, `=0` kill) on a ZGC
    /// shape; always `false` on G1, so G1's pool and manager lists and its
    /// `GcInfo`s are unchanged.
    pub fn metaspace_pool_enabled(&self) -> bool {
        self.zgc_shape.metaspace_pool
    }

    /// `-XX:MaxMetaspaceSize` for the `Metaspace` pool's `max` (HotSpot 25:
    /// `max = 67108864` at `-XX:MaxMetaspaceSize=64m`, `-1` without it).
    pub fn set_metaspace_max(&self, bytes: Option<u64>) {
        self.metaspace
            .max
            .store(bytes.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// zt t1/ob: does this heap list HotSpot 25's five non-heap pools
    /// (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`, default ON, `=0` kill: zs
    /// s3/ob2's `Metaspace`-only set)? Always `false` on G1, which lists them
    /// through gci i15/nh's model whatever this says.
    pub fn hotspot_non_heap_pools_enabled(&self) -> bool {
        self.zgc_shape.hotspot_non_heap_pools
    }

    /// zt t3/ob: HotSpot's per-code-heap `init` when this heap lists
    /// HotSpot's non-heap pools and `CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT` is
    /// on (default; `=0`: gci i15/nh's 2 MiB on the non-profiled heap, 0 on
    /// the others). `None` on G1, which keeps gci i15/nh's figures.
    pub fn hotspot_code_heap_init_bytes(&self) -> Option<u64> {
        (self.zgc_shape.hotspot_non_heap_pools && self.zgc_shape.hotspot_code_heap_init)
            .then_some(HOTSPOT_CODE_HEAP_INIT_BYTES)
    }

    /// The `Metaspace` pool's usage for `used` measured bytes, in HotSpot
    /// 25's shape: `init = 0`, `max` = `-XX:MaxMetaspaceSize` or undefined.
    ///
    /// zt t1/ob: `committed` is the HotSpot-shaped committed figure of the
    /// walk that measured `used` ([`Self::metaspace_sample_figures`]), the
    /// figure the `Metaspace` pool bean's `getUsage()` reports (gci i33/ms);
    /// `committed = used` only for a figure no such walk stated (a
    /// prefix back-fill) and under `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0`.
    /// `max` is never below `committed`.
    ///
    /// zt t2/x1 (finding X1-6): the kill arm is gated here. The edge walks
    /// state the figures whatever the switch says (`vm_init::
    /// class_metadata_walk_figures`), so the t1 cut gave the `=0` arm --
    /// documented as zs s3/ob2's `Metaspace`-only shape, `committed = used` --
    /// the walk's committed on both its pool bean and its `GcInfo` entries:
    /// the kill did not restore the old behaviour.
    pub fn metaspace_pool_usage(&self, used: u64) -> JmxPoolUsage {
        let committed = if self.zgc_shape.hotspot_non_heap_pools {
            self.metaspace_figures_for(used)
                .map_or(used, |f| f.committed.max(used))
        } else {
            used
        };
        self.metaspace_usage_with(used, committed)
    }

    /// zt t3/ob: the `Metaspace` pool's usage for one walk's figures `f`
    /// (a back-filled prefix): its own `committed` with HotSpot's non-heap
    /// pools listed, `committed = used` under their kill (zs s3/ob2).
    pub fn metaspace_pool_usage_of(&self, f: &MetaspaceSpaceFigures) -> JmxPoolUsage {
        let committed = if self.zgc_shape.hotspot_non_heap_pools {
            f.committed.max(f.used)
        } else {
            f.used
        };
        self.metaspace_usage_with(f.used, committed)
    }

    /// The `Metaspace` `JmxPoolUsage` for `used` / `committed`
    /// (`committed >= used` is the caller's), `max` never below `committed`.
    fn metaspace_usage_with(&self, used: u64, committed: u64) -> JmxPoolUsage {
        let max = self.metaspace.max.load(Ordering::Relaxed);
        let max = (max != u64::MAX).then_some(max);
        JmxPoolUsage {
            name: JMX_POOL_METASPACE,
            init: Some(0),
            used,
            committed,
            max: max.map(|m| m.max(committed)),
            collection_used: None,
            managers: METASPACE_POOL_MANAGERS,
        }
    }

    /// zt t1/ob: the `Compressed Class Space` usage measured by the walk
    /// whose `Metaspace` figure is `used`, in the pool bean's shape
    /// (`jmx.rs::non_heap_pool_from`: `init = 0`, `max` the class space
    /// reservation, `CompressedClassSpaceSize`, never below `committed`).
    /// `None` when no remembered walk stated it (the no-class-space shape,
    /// or a figure from a used-only walk).
    pub fn class_space_pool_usage(&self, used: u64) -> Option<JmxPoolUsage> {
        class_space_usage_of(&self.metaspace_figures_for(used)?)
    }

    /// zt t1/ob: the remembered HotSpot-shaped figures whose `used` is
    /// `used`, newest first.
    fn metaspace_figures_for(&self, used: u64) -> Option<MetaspaceSpaceFigures> {
        self.metaspace
            .figures
            .lock()
            .iter()
            .flatten()
            .find(|(_, f)| f.used == used)
            .map(|&(_, f)| f)
    }

    /// zt t3/ob: the remembered figures of the walk taken for census `key`.
    fn metaspace_figures_for_key(&self, key: (u64, u64)) -> Option<MetaspaceSpaceFigures> {
        self.metaspace
            .figures
            .lock()
            .iter()
            .flatten()
            .find(|(k, _)| *k == key)
            .map(|&(_, f)| f)
    }

    /// zt t1/ob: remember `f`, taken for census `key`, as the newest walk's
    /// figures. zt t3/ob: an older entry with the same `used` or the same
    /// key is dropped, so neither lookup can be ambiguous.
    fn note_metaspace_figures(&self, key: (u64, u64), f: MetaspaceSpaceFigures) {
        let mut recent = self.metaspace.figures.lock();
        let older = recent[0].filter(|&(k, g)| k != key && g.used != f.used);
        recent[1] = older;
        recent[0] = Some((key, f));
    }

    /// `MemoryImpl.getMemoryPools0` handed the `Metaspace` pool out: from
    /// now on every collection's `GcInfo` can name it, so the edges sample.
    pub fn note_metaspace_published(&self) {
        if self.metaspace_pool_enabled() {
            self.metaspace.published.store(true, Ordering::Relaxed);
        }
    }

    /// Should a collection edge sample the class metadata? Switch on and
    /// the pool handed out (before that, no `GcInfo` can carry the key: one
    /// relaxed load per edge on every other run).
    pub fn metaspace_samples_wanted(&self) -> bool {
        self.metaspace_pool_enabled() && self.metaspace.published.load(Ordering::Relaxed)
    }

    /// The class-metadata figure for the loaded-class census `key` (loaded,
    /// unloaded): the memo when the census is the one the last walk saw,
    /// otherwise `walk()` (a class-store walk), remembered. A steady-state
    /// program (no class defined or unloaded between two collections) pays
    /// no walk at all.
    pub fn metaspace_sample(&self, key: (u64, u64), walk: impl FnOnce() -> u64) -> u64 {
        let mut memo = self.metaspace.memo.lock();
        if let Some((k, bytes)) = *memo {
            if k == key {
                return bytes;
            }
        }
        let bytes = walk();
        *memo = Some((key, bytes));
        bytes
    }

    /// zt t1/ob: [`Self::metaspace_sample`] for a walk that states HotSpot's
    /// committed and class-space figures too (`vm_init::metaspace_stats`,
    /// the same one class-store walk), so the `GcInfo` entries built from
    /// the returned `used` carry them ([`Self::metaspace_pool_usage`],
    /// [`Self::class_space_pool_usage`]). A memo for `key` taken by a
    /// used-only walk is walked again once, to get the figures.
    pub fn metaspace_sample_figures(
        &self,
        key: (u64, u64),
        walk: impl FnOnce() -> MetaspaceSpaceFigures,
    ) -> u64 {
        let mut memo = self.metaspace.memo.lock();
        if let Some((k, bytes)) = *memo {
            // zt t3/ob (x1 item 7): the figures of THIS census's walk, not
            // any remembered walk that happened to state the same `used`.
            if k == key
                && self
                    .metaspace_figures_for_key(key)
                    .is_some_and(|f| f.used == bytes)
            {
                return bytes;
            }
        }
        let f = walk();
        *memo = Some((key, f.used));
        self.note_metaspace_figures(key, f);
        f.used
    }

    /// The memo's figure if it was taken for census `key`, without walking
    /// (the before edge, inside the pause).
    ///
    /// zt t2/rv (`zgc-zs4rv3-wave-s3-review-residuals-CLOSED-20261019.md` item 11):
    /// `try_lock`. [`Self::metaspace_sample`] holds the memo for a whole
    /// class-store walk (a concurrent pool query), and the pause must not wait
    /// for it; a busy memo answers `None`, which the caller already treats as
    /// "no figure for this edge" (the after edge's stands in).
    pub fn metaspace_memo_for(&self, key: (u64, u64)) -> Option<u64> {
        match self.metaspace.memo.try_lock().and_then(|memo| *memo) {
            Some((k, bytes)) if k == key => Some(bytes),
            _ => None,
        }
    }

    /// The last walk's figure, for an edge that could not take the class
    /// lock without blocking (a stopped thread holds it for write).
    pub fn metaspace_last(&self) -> Option<u64> {
        self.metaspace.memo.lock().map(|(_, b)| b)
    }

    /// The sample at the start of the collection in flight.
    pub fn note_metaspace_before(&self, bytes: Option<u64>) {
        self.metaspace
            .before
            .store(bytes.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// Take the start sample [`Self::note_metaspace_before`] left.
    pub fn take_metaspace_before(&self) -> Option<u64> {
        let b = self.metaspace.before.swap(u64::MAX, Ordering::Relaxed);
        (b != u64::MAX).then_some(b)
    }

    /// zt t6/ob4 (`zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md` item 3):
    /// do the collection edges sample the code heaps for the cycle records
    /// ([`Self::append_code_heap_usage`])? HotSpot's non-heap pools listed
    /// and `CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES` on (default; `=0` kill).
    /// Always `false` on G1, whose `GcInfo` keeps rendering them at delivery.
    pub fn code_heap_edges_wanted(&self) -> bool {
        self.zgc_shape.hotspot_non_heap_pools && self.zgc_shape.code_heap_edges
    }

    /// zt t6/ob4: the JIT's mapped code bytes at the start of the collection
    /// in flight (one relaxed load by the caller).
    pub fn note_code_heap_before(&self, used: Option<u64>) {
        self.metaspace
            .code_before
            .store(used.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    /// zt t6/ob4: take the start sample [`Self::note_code_heap_before`] left.
    pub fn take_code_heap_before(&self) -> Option<u64> {
        let b = self.metaspace.code_before.swap(u64::MAX, Ordering::Relaxed);
        (b != u64::MAX).then_some(b)
    }

    /// zt t6/ob4: the three `CodeHeap '...'` usages for `used` mapped code
    /// bytes under code-cache cap `cap` (`None` unbounded), in the pool beans'
    /// shape (`jmx.rs::non_heap_pool_from` with `with_hotspot_code_heap_init`):
    /// the non-profiled heap carries all of it, `committed = used`, `max` the
    /// cap (never below `committed`); the other two are empty with `max`
    /// undefined. `init` is HotSpot's per heap
    /// ([`Self::hotspot_code_heap_init_bytes`]), else gci i15/nh's (2 MiB on
    /// the non-profiled heap, 0 on the others). HotSpot's pool order.
    ///
    /// zt t30/nh: the `init` of each comes from the VM's layout
    /// ([`Self::hotspot_code_cache_layout`]: `InitialCodeCacheSize` moves
    /// it), and an unsegmented code cache is the one `CodeCache` pool, which
    /// carries all of it with `max` the reserved size.
    pub fn code_heap_pool_usages(&self, used: u64, cap: Option<u64>) -> Vec<JmxPoolUsage> {
        let layout = self.hotspot_code_cache_layout();
        self.hotspot_non_heap_pool_list()
            .iter()
            .filter(|(_, managers)| *managers == CODE_HEAP_POOL_MANAGERS)
            .map(|&(name, managers)| {
                let unsegmented = name == UNSEGMENTED_CODE_CACHE_POOL;
                let carries = name == CODE_HEAP_NON_PROFILED || unsegmented;
                let gci_init = if carries {
                    GCI_NON_PROFILED_CODE_HEAP_INIT_BYTES
                } else {
                    0
                };
                let init = layout.and_then(|l| l.init_of(name)).unwrap_or(gci_init);
                let used = if carries { used } else { 0 };
                let max = match layout {
                    Some(l) if unsegmented => Some(l.reserved),
                    _ => cap,
                };
                JmxPoolUsage {
                    name,
                    init: Some(init),
                    used,
                    committed: used,
                    max: if carries {
                        max.map(|m| m.max(used))
                    } else {
                        None
                    },
                    collection_used: None,
                    managers,
                }
            })
            .collect()
    }

    /// zt t6/ob4 (item 3): add the code heaps' usage at the two edges
    /// (`before` / `after` mapped code bytes; an edge with no sample takes the
    /// other's) to every CYCLE record of one collection, so its `GcInfo`
    /// before and after entries are the edges', as HotSpot samples every pool
    /// at `gc_begin` / `gc_end` -- they were both rendered at delivery
    /// (`jmx.rs::gc_info_pool_usage`), equal, and could count code installed
    /// after the collection. Pause records are left to
    /// [`Self::unrecord_pause_usage`] (all zero). A no-op unless
    /// [`Self::code_heap_edges_wanted`], or with no sample at either edge.
    pub fn append_code_heap_usage(
        &self,
        recs: &mut [GcNotificationRecord],
        before: Option<u64>,
        after: Option<u64>,
        cap: Option<u64>,
    ) {
        if !self.code_heap_edges_wanted() {
            return;
        }
        let (Some(before), Some(after)) = (before.or(after), after.or(before)) else {
            return;
        };
        let (b, a) = (
            self.code_heap_pool_usages(before, cap),
            self.code_heap_pool_usages(after, cap),
        );
        for r in recs.iter_mut().filter(|r| r.action != GC_ACTION_PAUSE) {
            if r.before
                .iter()
                .any(|p| p.name == CODE_HEAP_NON_PROFILED || p.name == UNSEGMENTED_CODE_CACHE_POOL)
            {
                continue;
            }
            r.before.extend(b.iter().cloned());
            r.after.extend(a.iter().cloned());
        }
    }

    /// zs s6/ob5: should a collection edge read the class-store census for
    /// [`Self::note_metaspace_deferred`]? The pool is modelled, not yet
    /// handed out, and `CRATONVM_ZGC_METASPACE_FIRST_CYCLES` is on (default;
    /// `=0` kill: such a cycle record keeps no `Metaspace` key, as before).
    pub fn metaspace_census_wanted(&self) -> bool {
        self.metaspace_pool_enabled()
            && self.zgc_shape.metaspace_first_cycles
            && !self.metaspace.published.load(Ordering::Relaxed)
    }

    /// zs s6/ob5 (the first-cycle `Metaspace` key, s5/ob4's residual): keep
    /// the class-store census of `recs`' cycle records -- the next `ClassId`
    /// at the start (`before`) and end (`after`) edge; an edge with none
    /// takes the other's -- so that [`Self::backfill_metaspace`] can give
    /// them the class metadata of exactly the classes defined by then. Call
    /// it after the records are queued. A no-op unless
    /// [`Self::metaspace_census_wanted`]; pause records are skipped (they
    /// carry a zero key already, [`Self::unrecord_pause_usage`]).
    ///
    /// Why: HotSpot's `GcInfo` always has the measured `Metaspace` figure;
    /// here a collection before `MemoryImpl.getMemoryPools0` first ran
    /// recorded none (sampling it would cost every run a class-store walk),
    /// and the `GcInfoBuilder` that later renders it names every pool, so it
    /// read the JMM's undefined usage (`init = max = -1`). The census costs
    /// one read of the class-store length per edge, no walk.
    pub fn note_metaspace_deferred(
        &self,
        recs: &[GcNotificationRecord],
        before: Option<u32>,
        after: Option<u32>,
    ) {
        if !self.metaspace_census_wanted() {
            return;
        }
        let (Some(before), Some(after)) = (before.or(after), after.or(before)) else {
            return;
        };
        let mut q = self.metaspace.deferred.lock();
        for r in recs.iter().filter(|r| r.action != GC_ACTION_PAUSE) {
            // A bean with no listener queues nothing, so only its last
            // collection can still be rendered: one entry per such bean.
            if !self.notifications.is_enabled(r.collector) {
                q.retain(|d| d.collector != r.collector);
            }
            q.push_back(MetaspaceDeferredCut {
                collector: r.collector,
                id: r.id,
                before,
                after,
            });
            if q.len() > METASPACE_DEFERRED_CAP {
                q.pop_front();
            }
        }
        self.metaspace
            .deferred_len
            .store(q.len(), Ordering::Relaxed);
    }

    /// zt t6/rv3 (`zgc-zs10rv7-*` item 5): did a drain's busy-lock retries
    /// already fail at this `(class definition epoch, collection count)`?
    /// Nothing that could free the class lock for the back-fill has
    /// happened since (no class was defined, no collection ended), so the
    /// retries would spin again for nothing.
    pub fn metaspace_backfill_held_at(&self, epoch: u64, collections: u64) -> bool {
        let m = &self.metaspace;
        m.backfill_hold_epoch.load(Ordering::Relaxed) == epoch
            && m.backfill_hold_collections.load(Ordering::Relaxed) == collections
    }

    /// Record (`Some`) or clear (`None`) the stamp at which the busy-lock
    /// retries failed ([`Self::metaspace_backfill_held_at`]).
    pub fn set_metaspace_backfill_hold(&self, stamp: Option<(u64, u64)>) {
        let (epoch, collections) = stamp.unwrap_or((u64::MAX, u64::MAX));
        let m = &self.metaspace;
        m.backfill_hold_epoch.store(epoch, Ordering::Relaxed);
        m.backfill_hold_collections.store(collections, Ordering::Relaxed);
    }

    /// Count one busy-lock retry pass of the drain; the running total.
    pub fn note_metaspace_backfill_retry_pass(&self) -> u64 {
        self.metaspace.backfill_retry_passes.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Busy-lock retry passes the drain ran so far.
    pub fn metaspace_backfill_retry_passes(&self) -> u64 {
        self.metaspace.backfill_retry_passes.load(Ordering::Relaxed)
    }

    /// zs s6/ob5: the class-store sizes [`Self::backfill_metaspace`] needs a
    /// class-metadata figure for, ascending and distinct; empty (one load)
    /// when nothing waits.
    pub fn metaspace_deferred_cuts(&self) -> Vec<u32> {
        if self.metaspace.deferred_len.load(Ordering::Relaxed) == 0 {
            return Vec::new();
        }
        let mut cuts: Vec<u32> = self
            .metaspace
            .deferred
            .lock()
            .iter()
            .flat_map(|d| [d.before, d.after])
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        cuts
    }

    /// zs s6/ob5: give every waiting cycle record (still a bean's last
    /// collection or still queued for delivery) the `Metaspace` key it would
    /// have had, from `bytes_at(cut)` = the class metadata of the classes
    /// whose id is below `cut` (a walk of that prefix of the class store).
    /// The waiting entries are consumed; a cut `bytes_at` cannot answer
    /// leaves its record as it was. Called where a `GcInfo` is about to name
    /// the pool: when `MemoryImpl.getMemoryPools0` hands it out and before a
    /// notification drain. Returns the records patched.
    ///
    /// The figure is the census of the record's own edges, so classes
    /// defined after the collection are not counted; like every `Metaspace`
    /// figure here it is a lower bound (a class unloaded since then is
    /// missing; a method body decoded since then is counted).
    pub fn backfill_metaspace(&self, bytes_at: impl Fn(u32) -> Option<u64>) -> usize {
        self.backfill_metaspace_figures(|cut| {
            bytes_at(cut).map(|used| MetaspaceSpaceFigures {
                used,
                committed: used,
                class_space: None,
            })
        })
    }

    /// zt t3/ob (`zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md` item 4):
    /// [`Self::backfill_metaspace`] with each prefix's HotSpot-shaped figures
    /// (`vm_init::class_metadata_prefix_figures`), so a back-filled cycle
    /// record carries the prefix's `committed` (it read `committed = used`)
    /// and, with HotSpot's non-heap pools listed, its `Compressed Class
    /// Space` entry (it was rendered from the usage at delivery). Under
    /// `CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS=0` the `Metaspace` entry keeps
    /// `committed = used`, the zs s3/ob2 shape.
    pub fn backfill_metaspace_figures(
        &self,
        figures_at: impl Fn(u32) -> Option<MetaspaceSpaceFigures>,
    ) -> usize {
        if self.metaspace.deferred_len.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let waiting: Vec<MetaspaceDeferredCut> = {
            let mut q = self.metaspace.deferred.lock();
            self.metaspace.deferred_len.store(0, Ordering::Relaxed);
            q.drain(..).collect()
        };
        let mut patched = 0;
        self.notifications.patch_records(|r| {
            if r.action == GC_ACTION_PAUSE || r.before.iter().any(|p| p.name == JMX_POOL_METASPACE)
            {
                return;
            }
            let Some(d) = waiting
                .iter()
                .rev()
                .find(|d| d.collector == r.collector && d.id == r.id)
            else {
                return;
            };
            if let (Some(b), Some(a)) = (figures_at(d.before), figures_at(d.after)) {
                r.before.push(self.metaspace_pool_usage_of(&b));
                r.after.push(self.metaspace_pool_usage_of(&a));
                if self.zgc_shape.hotspot_non_heap_pools
                    && !r
                        .before
                        .iter()
                        .any(|p| p.name == JMX_POOL_COMPRESSED_CLASS_SPACE)
                {
                    if let (Some(cb), Some(ca)) =
                        (class_space_usage_of(&b), class_space_usage_of(&a))
                    {
                        r.before.push(cb);
                        r.after.push(ca);
                    }
                }
                patched += 1;
            }
        });
        patched
    }

    /// zs s9/ob6: is `r` a cycle record still waiting for its `Metaspace`
    /// key (a census entry names it and it carries none)? A drain whose
    /// back-fill could not run holds such a record back rather than deliver
    /// it with the JMM's undefined usage, which HotSpot never reports. One
    /// load when nothing waits.
    pub fn metaspace_record_waiting(&self, r: &GcNotificationRecord) -> bool {
        if self.metaspace.deferred_len.load(Ordering::Relaxed) == 0
            || r.action == GC_ACTION_PAUSE
            || r.before.iter().any(|p| p.name == JMX_POOL_METASPACE)
        {
            return false;
        }
        self.metaspace
            .deferred
            .lock()
            .iter()
            .any(|d| d.collector == r.collector && d.id == r.id)
    }

    /// zs s6/ob5: is a `Metaspace` usage threshold honoured
    /// (`CRATONVM_ZGC_METASPACE_THRESHOLD`, default ON, `=0` kill: the Java
    /// side keeps the threshold but no sensor ever fires, as before)?
    pub fn metaspace_threshold_enabled(&self) -> bool {
        self.metaspace_pool_enabled() && self.zgc_shape.metaspace_threshold
    }

    /// A set-time usage-threshold crossing requests the delivery thread, as
    /// HotSpot's Service Thread does without an allocation or collection.
    /// Always enabled on G1; ZGC retains its existing diagnostic kill switch
    /// `CRATONVM_ZGC_THRESHOLD_SET_DRAIN` (default on).
    pub fn threshold_set_drain_enabled(&self) -> bool {
        self.shape == BackendBeanShape::G1 || self.zgc_shape.threshold_set_drain
    }

    /// zs s6/ob5: note the `Metaspace` edge samples as that pool's peaks
    /// (`CRATONVM_ZGC_METASPACE_PEAK_AT_GC`, default ON, `=0` kill; also off
    /// with `CRATONVM_ZGC_POOL_PEAK_AT_GC=0`). HotSpot records every pool's
    /// peak at both edges of a cycle, `Metaspace` included, which differs
    /// from a query-time peak across a collection that unloaded classes.
    pub fn note_metaspace_peaks(&self, before: Option<u64>, after: Option<u64>) {
        if !(self.metaspace_pool_enabled() && self.zgc_shape.metaspace_peak_at_gc) {
            return;
        }
        let pools: Vec<JmxPoolUsage> = [before, after]
            .into_iter()
            .flatten()
            .map(|b| self.metaspace_pool_usage(b))
            .collect();
        if !pools.is_empty() {
            self.note_pool_peaks(&pools);
        }
    }

    /// zs s3/ob2: add the `Metaspace` pool's usage to every record of one
    /// collection (or ZGC mark start), as HotSpot 25's `GcInfo` carries every
    /// pool: a cycle record the measured figure at each edge (local 25.0.3,
    /// `ZGC Major Cycles`: `Metaspace init = 0 used = 841336 ... max = -1`
    /// before, `841592` after), a pause record an all-zero one
    /// ([`Self::unrecord_pause_usage`], called after this). An edge with no
    /// sample takes the other edge's; with neither, a cycle record gets no
    /// key (the `GcInfo` then reads the JMM's undefined usage, as before) and
    /// a pause record a zero one. A no-op unless
    /// [`Self::metaspace_samples_wanted`].
    pub fn append_metaspace_usage(
        &self,
        recs: &mut [GcNotificationRecord],
        before: Option<u64>,
        after: Option<u64>,
    ) {
        if !self.metaspace_samples_wanted() {
            return;
        }
        let (before, after) = (before.or(after), after.or(before));
        for r in recs.iter_mut() {
            if r.before.iter().any(|p| p.name == JMX_POOL_METASPACE) {
                continue;
            }
            let pause = r.action == GC_ACTION_PAUSE;
            let edge = |v: Option<u64>| v.or(pause.then_some(0));
            if let (Some(b), Some(a)) = (edge(before), edge(after)) {
                r.before.push(self.metaspace_pool_usage(b));
                r.after.push(self.metaspace_pool_usage(a));
                // zt t1/ob: and, with HotSpot's pool set listed, the
                // `Compressed Class Space` figures of the same walks (a
                // pause record's are zeroed by `unrecord_pause_usage`).
                if !pause && self.zgc_shape.hotspot_non_heap_pools {
                    if let (Some(cb), Some(ca)) =
                        (self.class_space_pool_usage(b), self.class_space_pool_usage(a))
                    {
                        r.before.push(cb);
                        r.after.push(ca);
                    }
                }
            }
        }
    }

    /// zs s2/ob: give every `end of GC pause` record HotSpot's unrecorded
    /// usages ([`unrecorded_pool_usage`]) under
    /// `CRATONVM_ZGC_PAUSE_GCINFO_NO_USAGE` (default ON, `=0` kill). Before,
    /// a `ZGC Major Pauses` `GcInfo` / notification carried the collection's
    /// real before / after usage, which HotSpot gives only to the cycle
    /// beans. A no-op on G1 and on cycle records.
    ///
    /// zs s5/ob4: and a pause record always carries a zero `Metaspace` key
    /// when that pool is modelled (`CRATONVM_ZGC_JMX_METASPACE_POOL`), even
    /// when [`Self::append_metaspace_usage`] added none -- a record built
    /// before `MemoryImpl.getMemoryPools0` first handed the pool out. The
    /// `GcInfoBuilder` that renders it is built later and names every pool,
    /// so a missing key became the JMM's undefined usage (`init = max = -1`)
    /// on that collection's pause `GcInfo`s; HotSpot's are all zero (host,
    /// 25.0.4, `Zs4Ob3PauseShape`: `pause_gcinfo_usage_zero=true`, ours
    /// `false`). A pause records no usage, so no sample is needed.
    ///
    /// zt t1/ob: with HotSpot's non-heap pool set listed
    /// (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`), each of its five pools
    /// gets the zero key (the three code heaps and `Compressed Class Space`
    /// too, whatever `CRATONVM_ZGC_JMX_METASPACE_POOL` says): a pool the
    /// record does not name is otherwise rendered from its usage at delivery
    /// (`jmx.rs::gc_info_pool_usage`), which is not zero, where every one of
    /// HotSpot's pause `GcInfo` entries is.
    pub fn unrecord_pause_usage(&self, recs: &mut [GcNotificationRecord]) {
        if !self.zgc_shape.pause_usage_unrecorded {
            return;
        }
        let zero_keys: Vec<JmxPoolUsage> = if self.zgc_shape.hotspot_non_heap_pools {
            // zt t30/nh: `CodeCache` in place of the three heaps when unsegmented.
            self.hotspot_non_heap_pool_list()
                .iter()
                .map(|&(name, managers)| JmxPoolUsage {
                    name,
                    init: Some(0),
                    used: 0,
                    committed: 0,
                    max: Some(0),
                    collection_used: None,
                    managers,
                })
                .collect()
        } else if self.metaspace_pool_enabled() {
            vec![unrecorded_pool_usage(&self.metaspace_pool_usage(0))]
        } else {
            Vec::new()
        };
        for r in recs.iter_mut().filter(|r| r.action == GC_ACTION_PAUSE) {
            r.before = r.before.iter().map(unrecorded_pool_usage).collect();
            r.after = r.after.iter().map(unrecorded_pool_usage).collect();
            for side in [&mut r.before, &mut r.after] {
                for m in &zero_keys {
                    if !side.iter().any(|p| p.name == m.name) {
                        side.push(m.clone());
                    }
                }
            }
        }
    }

    /// zs s2/ob: sample `pools` as each pool's peak, at an edge of a
    /// collection (HotSpot's `GCMemoryManager::gc_begin` / `gc_end` with
    /// `recordPeakUsage`, which ZGC's cycle tracer sets), under
    /// `CRATONVM_ZGC_POOL_PEAK_AT_GC` (default ON, `=0` kill: the peak is a
    /// query-time sample, as before). A no-op on G1.
    pub fn note_pool_peaks(&self, pools: &[JmxPoolUsage]) {
        if self.zgc_shape.pool_peaks_at_gc {
            self.notifications.note_pool_peaks(pools);
        }
    }

    /// The peak [`Self::note_pool_peaks`] recorded for `pool` since the last
    /// reset; `None` when the switch is off or nothing was sampled.
    pub fn pool_peak(&self, pool: &str) -> Option<(u64, u64)> {
        if self.zgc_shape.pool_peaks_at_gc {
            self.notifications.pool_peak(pool)
        } else {
            None
        }
    }

    /// `MemoryPoolMXBean.resetPeakUsage()` for the collector-sampled peak.
    pub fn reset_pool_peak(&self, pool: &'static str, used: u64, committed: u64) {
        if self.zgc_shape.pool_peaks_at_gc {
            self.notifications.reset_pool_peak(pool, used, committed);
        }
    }

    /// `getMemoryManagerNames()` of `pool` on this heap: the shape's list,
    /// except HotSpot 25's order on ZGC under `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER`.
    pub fn pool_managers(&self, pool: &str) -> &'static [&'static str] {
        if self.zgc_hotspot_order {
            ZGC_POOL_MANAGERS_HOTSPOT
        } else {
            self.shape.pool_managers(pool)
        }
    }

    /// zr z2/t2: put one collection's notifications in HotSpot's delivery
    /// order -- on ZGC under `CRATONVM_ZGC_HOTSPOT_BEAN_ORDER`, every
    /// `end of GC pause` before the `end of GC cycle` it belongs to (a stable
    /// partition, so two pauses keep their order). A no-op on G1 and with the
    /// switch off.
    pub fn order_notifications(&self, recs: &mut [GcNotificationRecord]) {
        if self.zgc_hotspot_order {
            recs.sort_by_key(|r| r.action != GC_ACTION_PAUSE);
        }
    }

    pub fn shape(&self) -> BackendBeanShape {
        self.shape
    }

    /// The notification queue and each bean's last collection.
    pub fn notifications(&self) -> &GcNotificationQueue {
        &self.notifications
    }

    fn add(&self, bean: usize, pause_ns: u64) {
        if let (Some(c), Some(t)) = (self.counts.get(bean), self.pause_ns.get(bean)) {
            c.fetch_add(1, Ordering::Relaxed);
            t.fetch_add(pause_ns, Ordering::Relaxed);
        }
    }

    /// One collection pause of `pause_ns` completed: it counts under
    /// [`BackendBeanShape::collection_beans`] for a whole-heap
    /// ([`CollectionKind::Major`]) collection.
    pub fn note_collection(&self, pause_ns: u64) {
        self.note_collection_of(pause_ns, CollectionKind::Major);
    }

    /// One collection pause of `pause_ns` and `kind` completed.
    pub fn note_collection_of(&self, pause_ns: u64, kind: CollectionKind) {
        for &i in self.shape.collection_beans(kind) {
            self.add(i, pause_ns);
        }
    }

    /// zs s4/ob3: is a ZGC collection booked as its HotSpot pauses
    /// (`CRATONVM_ZGC_PAUSE_PHASES`)? Always `false` on G1.
    pub fn pause_phases_enabled(&self) -> bool {
        self.zgc_shape.pause_phases
    }

    /// zs s4/ob3: one collection of `kind` that stopped the world from
    /// `start` to `end`. The cycle bean counts it once with the whole pause;
    /// the pause bean counts one pause per span of `split`
    /// ([`ZgcPauseSplit::spans`]), each with its own span's time, so the
    /// bean's collection time still grows by the whole pause. Without a
    /// split, with one that does not fit inside the pause, off ZGC or with
    /// the switch off, this is [`Self::note_collection_of`]. Returns the split
    /// it booked, which [`Self::split_pause_records`] must then be given.
    pub fn note_collection_split(
        &self,
        start: std::time::Instant,
        end: std::time::Instant,
        kind: CollectionKind,
        split: Option<ZgcPauseSplit>,
    ) -> Option<ZgcPauseSplit> {
        self.note_collection_split_from(None, start, end, kind, split)
    }

    /// [`Self::note_collection_split`] for a collection that closes a
    /// concurrent cycle opened at `cycle_start` (zt t31/zc11): the cycle bean
    /// is credited `[cycle_start, end]`, the cycle's whole span, as HotSpot's
    /// `ZGC Major Cycles` collection time counts whole cycles; the pause bean
    /// keeps this collection's pauses (the mark start's was booked by its own
    /// pause). `None`: the pause alone, as before.
    pub fn note_collection_split_from(
        &self,
        cycle_start: Option<std::time::Instant>,
        start: std::time::Instant,
        end: std::time::Instant,
        kind: CollectionKind,
        split: Option<ZgcPauseSplit>,
    ) -> Option<ZgcPauseSplit> {
        let pause_ns = span_nanos((start, end));
        let cycle_ns = span_nanos((cycle_start.map_or(start, |c| c.min(start)), end));
        let split = split.filter(|s| self.zgc_shape.pause_phases && s.fits(start, end));
        let Some(s) = split else {
            for &i in self.shape.collection_beans(kind) {
                if self.shape.collector_action(i) == GC_ACTION_PAUSE {
                    self.add(i, pause_ns);
                } else {
                    self.add(i, cycle_ns);
                }
            }
            return None;
        };
        let (spans, n) = s.spans(start, end);
        for &i in self.shape.collection_beans(kind) {
            if self.shape.collector_action(i) == GC_ACTION_PAUSE {
                for &span in &spans[..n] {
                    self.add(i, span_nanos(span));
                }
            } else {
                self.add(i, cycle_ns);
            }
        }
        Some(s)
    }

    /// zs s4/ob3: replace each `end of GC pause` record of one collection
    /// booked by [`Self::note_collection_split`] with one record per pause:
    /// consecutive ids ending at the record's own (the bean's count after),
    /// the record's `[start, end]` cut at the split's boundaries, everything
    /// else (cause, time stamp, usage) as the record had it. A no-op for a
    /// cycle record.
    pub fn split_pause_records(&self, recs: &mut Vec<GcNotificationRecord>, split: &ZgcPauseSplit) {
        if !recs.iter().any(|r| r.action == GC_ACTION_PAUSE) {
            return;
        }
        let mut out = Vec::with_capacity(recs.len() + split.pauses());
        for r in recs.drain(..) {
            if r.action != GC_ACTION_PAUSE {
                out.push(r);
                continue;
            }
            let (spans, n) = split.spans(r.start, r.end);
            let first_id = r.id.saturating_sub(n as u64 - 1);
            for (k, &(s, e)) in spans[..n].iter().enumerate() {
                let mut p = r.clone();
                p.id = first_id + k as u64;
                p.start = s;
                p.end = e;
                out.push(p);
            }
        }
        *recs = out;
    }

    /// One stop-the-world pause that collected nothing (a G1 initial mark or
    /// remark + cleanup, a ZGC mark start) took `pause_ns`.
    pub fn note_non_collection_pause(&self, pause_ns: u64) {
        self.add(self.shape.non_collection_pause_bean(), pause_ns);
    }

    /// gci i12/ge (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016`
    /// (b)): one G1 full collection whose pauses summed to `pause_ns`
    /// completed -- HotSpot's `G1 Old Generation` bean. Booked by the VM's
    /// one-report scope (`CRATONVM_G1_FULL_GC_ONE_REPORT`) instead of the
    /// young / concurrent beans its pauses would otherwise count under. A
    /// no-op on any other shape.
    pub fn note_full_collection(&self, pause_ns: u64) {
        if self.shape == BackendBeanShape::G1 {
            self.add(1, pause_ns);
        }
    }

    /// The collector beans, now. `time_ms` is the whole milliseconds of the
    /// bean's pause sum ([`pause_sum_as_collection_time_ms`]),
    /// HotSpot's arithmetic and [`serial_collectors`]' since gen r5w3/obs7. gce
    /// e1/o: it added one millisecond per counted pause until 2026-10-04, so
    /// `ZGC Pauses` read roughly its pause count in milliseconds.
    pub fn collectors(&self) -> Vec<JmxCollector> {
        self.shape
            .collectors()
            .iter()
            .enumerate()
            .map(|(i, &name)| {
                let count = self.counts[i].load(Ordering::Relaxed);
                let pause_ns = self.pause_ns[i].load(Ordering::Relaxed);
                let us = pause_ns / 1_000;
                JmxCollector {
                    name,
                    count,
                    time_ms: pause_sum_as_collection_time_ms(us),
                    time_ns: pause_ns,
                    pools: self.shape.collector_pools(i),
                    action: self.shape.collector_action(i),
                }
            })
            .collect()
    }

    /// Name `samples` (in [`BackendBeanShape::pools`] order) as this shape's
    /// pools, with their managers and collection usage. Extra samples are
    /// ignored; a short list describes only the pools it covers.
    pub fn pools(&self, samples: &[JmxPoolSample]) -> Vec<JmxPoolUsage> {
        let recorded = self.collection_recorded.load(Ordering::Acquire);
        self.shape
            .pools()
            .iter()
            .zip(samples)
            .enumerate()
            .map(|(i, (&name, s))| {
                let s = s.normalized();
                JmxPoolUsage {
                    name,
                    init: self.zgc_pool_init(i).or(s.init),
                    used: s.used,
                    committed: s.committed,
                    max: s.max,
                    collection_used: recorded
                        .then(|| self.collection_used[i].load(Ordering::Relaxed)),
                    managers: self.pool_managers(name),
                }
            })
            .collect()
    }

    /// Stamp each pool's usage right after a collection
    /// (`getCollectionUsage()`), from the snapshot the collection's seal took.
    pub fn note_collection_usage(&self, pools: &[JmxPoolUsage]) {
        self.note_collection_usage_of(pools, None);
    }

    /// gci i14/mp finding 3: stamp `getCollectionUsage()` -- used AND
    /// committed -- of the pools a collection AFFECTED (`affected`, `None` =
    /// all), from the snapshot `pools` its seal took (shape order). Every
    /// other pool keeps its last affecting collection's figure, or HotSpot's
    /// initial `(init, 0, 0, max)` before any (`GCMemoryManager::gc_end` sets
    /// the last collection usage only of the pools always affected by that
    /// manager, or of all when the pause affected all). Wave 9 lane G
    /// (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
    /// a G1 young-only pause passes `Some(`[`G1_YOUNG_ONLY_POOLS`]`)`.
    pub fn note_collection_usage_of(&self, pools: &[JmxPoolUsage], affected: Option<&[&str]>) {
        for (i, p) in pools.iter().enumerate().take(MAX_JMX_POOLS) {
            if affected.is_some_and(|only| !only.contains(&p.name)) {
                continue;
            }
            self.collection_used[i].store(p.used, Ordering::Relaxed);
            self.collection_committed[i].store(p.committed, Ordering::Relaxed);
            self.collection_stamped.fetch_or(1u8 << i, Ordering::Release);
        }
        self.collection_recorded.store(true, Ordering::Release);
    }

    /// gci i14/mp finding 3: each pool's committed at the end of its last
    /// affecting collection, in shape order (`None` before any).
    pub fn collection_committed(&self) -> [Option<u64>; MAX_JMX_POOLS] {
        let stamped = self.collection_stamped.load(Ordering::Acquire);
        std::array::from_fn(|i| {
            (stamped & (1u8 << i) != 0).then(|| self.collection_committed[i].load(Ordering::Relaxed))
        })
    }
}

/// The pools a G1 young-only pause affects (HotSpot 25 `G1MonitoringSupport`:
/// the young manager adds `G1 Old Gen` with `always_affected_by_gc = false`,
/// and a young-only pause is not an all-pools-affected one). Wave 9 lane G.
pub const G1_YOUNG_ONLY_POOLS: &[&str] = &[JMX_POOL_G1_EDEN, JMX_POOL_G1_SURVIVOR];

#[cfg(test)]
mod jmx_bean_tests;

// ---------------------------------------------------------------------------
// GC notifications (gen r4w3/obs2, 2026-09-23)
// ---------------------------------------------------------------------------
//
// `com.sun.management.GarbageCollectionNotificationInfo` is what Micrometer's
// `jvm.gc.pause`, Spring Boot's GC metrics and every `NotificationListener` on
// a GC bean are fed from. They read NOTIFICATIONS, not the bean counters, so a
// VM whose counters are right and which sends no notification reports no GC
// pause at all.
//
// HotSpot's split, which this reproduces:
//
// 1. Inside the pause, `TraceMemoryManagerStats` (`GCMemoryManager::gc_begin`
//    / `gc_end`) records a `GCStatInfo` for the manager that ran — its
//    collection index, start and end, and every pool's usage before and after
//    — as that manager's last GC (`GarbageCollectorMXBean.getLastGcInfo()`);
//    and, iff the manager's notification is enabled (a listener is
//    registered: `GarbageCollectorExtImpl.setNotificationEnabled`),
//    `GCNotifier::pushNotification` queues it with the action
//    (`end of minor GC` / `end of major GC`) and the cause.
// 2. Outside the pause, the Notification Thread (the Service Thread before
//    JDK 14) drains that queue and calls
//    `GarbageCollectorExtImpl.createGCNotification` on the manager's bean. No
//    Java runs inside the collection.
//
// Here (1) is [`gc_notifications_between`] + [`GcNotificationQueue::record`],
// called from the VM's per-collection accounting (`gc_events.rs`) once the
// collection has finished; (2) is a drain the collecting thread runs after its
// finalizer and Cleaner drains, where the VM already runs Java on that thread
// for the same reason. Which thread delivers is the one divergence; see
// `docs/feature-designs/generative-gc-architecture.md`.
//
// Per heap (a field of `GenerationalHeap`; for G1 and ZGC of their
// `BackendGcBeans`), so per VM: no process global.

/// `GarbageCollectionNotificationInfo.getGcAction()` of a young collection —
/// HotSpot Serial's `Copy` (`SerialHeap::do_young_collection`).
pub const GC_ACTION_MINOR: &str = "end of minor GC";
/// `GarbageCollectionNotificationInfo.getGcAction()` of a full collection —
/// HotSpot Serial's `MarkSweepCompact`.
pub const GC_ACTION_MAJOR: &str = "end of major GC";

/// Notifications a heap holds while nothing drains them. Past this the OLDEST
/// is dropped (and counted): a listener that is never given a chance to run
/// must not turn every collection into retained memory. HotSpot's queue is
/// unbounded because its dedicated thread always drains it; this one is
/// drained by collecting threads, which can defer (see
/// [`GcNotificationQueue::note_deferred`]).
pub const GC_NOTIFICATION_QUEUE_CAPACITY: usize = 1024;

/// The per-bean JMX state at one edge of a collection: every collector's
/// count and every pool's usage, and when it was taken. (Two collectors and
/// three pools — Serial's — until gc-common w7-f; a backend now describes up
/// to [`MAX_JMX_COLLECTORS`] / [`MAX_JMX_POOLS`].)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcBeanSnapshot {
    pub at: std::time::Instant,
    pub collectors: Vec<JmxCollector>,
    pub pools: Vec<JmxPoolUsage>,
}

impl GcBeanSnapshot {
    /// A snapshot stamped now. Takes the generational heap's fixed arrays as
    /// well as a backend's vectors.
    pub fn now(
        collectors: impl Into<Vec<JmxCollector>>,
        pools: impl Into<Vec<JmxPoolUsage>>,
    ) -> Self {
        Self {
            at: std::time::Instant::now(),
            collectors: collectors.into(),
            pools: pools.into(),
        }
    }
}

/// One completed collection as HotSpot's `GCNotifier` describes it: the
/// `GcInfo` (`id`, `start`/`end`, per-pool usage before and after) plus the
/// notification's `gcName`, `gcAction`, `gcCause` and time stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcNotificationRecord {
    /// The bean that ran (e.g. [`JMX_SERIAL_YOUNG_COLLECTOR`],
    /// [`JMX_G1_YOUNG_COLLECTOR`], [`JMX_ZGC_MAJOR_PAUSES_COLLECTOR`]).
    pub collector: &'static str,
    /// That bean's [`JmxCollector::action`] (e.g. [`GC_ACTION_MINOR`]).
    pub action: &'static str,
    /// HotSpot's `GCCause` spelling (`Allocation Failure`, `System.gc()`).
    pub cause: &'static str,
    /// `GcInfo.getId()`: that bean's collection count including this one,
    /// which is what HotSpot's `GCStatInfo::gc_index` is.
    pub id: u64,
    pub start: std::time::Instant,
    pub end: std::time::Instant,
    /// `Notification.getTimeStamp()`: wall-clock milliseconds at the end of the
    /// collection (`os::javaTimeMillis()` at `pushNotification`).
    pub timestamp_ms: u64,
    pub before: Vec<JmxPoolUsage>,
    pub after: Vec<JmxPoolUsage>,
}

/// One notification per bean whose count moved between `before` and `after`,
/// in bean order; empty when none did (a refused cycle).
///
/// Decided by the per-bean counts themselves, so it cannot disagree with them.
/// Serial: a cycle that reclaimed old gen moves `MarkSweepCompact` (and not
/// `Copy` — see [`serial_collectors`]), any other completed cycle moves `Copy`,
/// so exactly one record, as before gc-common w7-f. ZGC: a collection moves
/// a cycle bean and its pause bean (`ZGC Major Cycles` and `ZGC Major
/// Pauses`, or the Minor pair), and HotSpot sends one notification per
/// bean, so two. `before` and `after` are taken by one thread around ONE
/// pause (the world stopped between them), so no other pause's count can
/// leak in. A bean that moved by more than one is still one record (its `id`
/// is the count after).
pub fn gc_notifications_between(
    before: &GcBeanSnapshot,
    after: &GcBeanSnapshot,
    cause: &'static str,
    timestamp_ms: u64,
) -> Vec<GcNotificationRecord> {
    after
        .collectors
        .iter()
        .filter(|c| {
            before
                .collectors
                .iter()
                .find(|b| b.name == c.name)
                .map_or(c.count > 0, |b| c.count > b.count)
        })
        .map(|c| GcNotificationRecord {
            collector: c.name,
            action: c.action,
            cause,
            id: c.count,
            start: before.at,
            end: after.at,
            timestamp_ms,
            before: before.pools.clone(),
            after: after.pools.clone(),
        })
        .collect()
}

const NOTIF_RECORDED: usize = 0;
const NOTIF_QUEUED: usize = 1;
const NOTIF_DELIVERED: usize = 2;
const NOTIF_ORPHANED: usize = 3;
const NOTIF_DROPPED: usize = 4;
const NOTIF_FAILED: usize = 5;
const NOTIF_DEFERRED: usize = 6;

/// A heap's GC-notification state: per-bean enablement, each bean's last
/// collection (`getLastGcInfo()`), and the queue of notifications not yet
/// delivered. See the section comment above.
#[derive(Debug)]
pub struct GcNotificationQueue {
    /// The beans this queue serves, in slot order: Serial's two for
    /// [`Self::new`], a backend's for [`Self::with_collectors`].
    names: &'static [&'static str],
    /// Per bean slot: does it have a listener?
    enabled: [std::sync::atomic::AtomicBool; MAX_JMX_COLLECTORS],
    /// Mirror of `pending.len()`, so a collection with nothing to deliver costs
    /// one load and no lock.
    pending_len: std::sync::atomic::AtomicUsize,
    pending: parking_lot::Mutex<std::collections::VecDeque<GcNotificationRecord>>,
    last: parking_lot::Mutex<[Option<GcNotificationRecord>; MAX_JMX_COLLECTORS]>,
    /// One drainer at a time: delivery runs Java, which can allocate, collect
    /// and reach a drain point again on the same thread.
    draining: std::sync::atomic::AtomicBool,
    counts: [AtomicU64; 7],
    /// gen r5w2/obs6 — each pool's peak `(used, committed)` as sampled at the
    /// edges of every collection ([`Self::note_pool_peaks`]), HotSpot's
    /// `MemoryPool::record_peak_memory_usage` at `GCMemoryManager::gc_begin` /
    /// `gc_end`. That is when eden peaks (it is full at the pause), which a
    /// query-time sample almost never sees
    /// (`gengc-r4w2-obs-jmx-residuals-on-the-generational-backend` item 1).
    /// Keyed by pool name; a handful of entries.
    pool_peaks: parking_lot::Mutex<Vec<(&'static str, u64, u64)>>,
    /// gen r5w2/obs6 — the JFR identity of the concurrent cycle in flight
    /// (generational backend). Shared (`Arc`) so a pause timer that outlives
    /// its borrow of the heap can stamp it.
    concurrent_cycle_jfr: std::sync::Arc<ConcurrentCycleJfr>,
    /// gen r5w3/obs7 — the heap pools' usage / collection-usage thresholds
    /// and their sensors' state (HotSpot's `LowMemoryDetector`). Checked at
    /// the end of every collection when armed, delivered by the same drain
    /// as the GC notifications. See [`PoolThresholds`].
    pool_thresholds: PoolThresholds,
    /// gcg h1/ob: the first notification's latency edges
    /// ([`FirstDeliverySplit`]); `first_delivery_done` mirrors "all three
    /// stamped", so a later drain pays one relaxed load, no lock.
    first_delivery: parking_lot::Mutex<FirstDeliverySplit>,
    first_delivery_done: std::sync::atomic::AtomicBool,
    /// gci i47w/oe47: consecutive deliveries whose construction the heap
    /// REFUSED (heap exhaustion) and that were put back at the head of the
    /// queue ([`GcNotificationDrain::requeue_refused`]); reset by any
    /// delivery that completed. Bounds the retries of a heap that never
    /// recovers ([`GC_NOTIFICATION_REFUSED_RETRIES`]).
    refused_streak: std::sync::atomic::AtomicU32,
    /// zt t13/x10 (`CRATONVM_ZGC_NOTIFICATION_MISSED_REDRAIN`, the ZGC drain
    /// points only): a drain point found another drain in progress
    /// ([`Self::begin_drain_noting_miss`]). The drainer takes it after it
    /// lets go ([`Self::end_drain_take_missed`]) and drains again, so a
    /// record queued after its last pop is not left for a drain point that
    /// may never come.
    drain_missed: std::sync::atomic::AtomicBool,
    /// gcj j1/ob: `CRATONVM_GEN_THRESHOLD_SET_DRAIN` as read when the
    /// generational heap's queue was built ([`Self::new`]); `false` on a
    /// backend's queue. See [`Self::serial_threshold_set_drain_enabled`].
    serial_threshold_set_drain: bool,
}

/// gci i47w/oe47: how many times in a row a notification whose construction
/// the heap refused is kept queued for a later drain point before it is
/// counted `Failed` and dropped (the pre-oe47 answer). Each retry happens at a
/// drain point, i.e. after a further collection, so this is a bound in
/// collections, not a spin.
pub const GC_NOTIFICATION_REFUSED_RETRIES: u32 = 32;

/// The JFR identity of a heap's concurrent old-generation cycle while it runs:
/// its `gcId` (taken at the initial-mark pause) and the wall-clock instant its
/// concurrent marking began (the initial-mark pause's end). The remark pause
/// takes both, to report `jdk.GCPhaseConcurrent("Concurrent Mark")` and its
/// own `jdk.GCPhasePause` under the cycle's id. Per heap, so per VM.
/// gen r5w2/obs6 (2026-09-26;
/// `gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted` item 2).
///
/// Generational: only written while a JFR recording runs. G1 (gci i12/ge):
/// written on every run -- the cycle's id is also its `-Xlog:gc` `GC(n)`
/// (`vm/src/runtime/interpreter/gc_events.rs` `g1_cycle_edge`). A cycle
/// abandoned before its remark leaves its entry behind; the next initial mark
/// replaces it.
#[derive(Debug, Default)]
pub struct ConcurrentCycleJfr {
    /// `(gc_id, marking start in wall-clock ns, 0 until the initial-mark
    /// pause has ended)`.
    open: parking_lot::Mutex<Option<(u64, u64)>>,
    /// gci i12/ge (`gci-i12ge-...-review-findings-20261013` item 4): the
    /// last cycle whose marking started, `(gc_id, wall-clock ns)`, kept by
    /// [`Self::take`] / [`Self::open`] (which drop `open`'s start so a
    /// retried remark writes no second JFR marking row): the G1 cycle's
    /// `-Xlog:gc` end line (`Concurrent Mark Cycle <ms>`) still needs it.
    cycle_start: parking_lot::Mutex<Option<(u64, u64)>>,
    /// gci i13/gp (`gci-i12ge-...-review-findings-20261013` item 3, opt-in
    /// `CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE`): the G1 cycle a full
    /// collection found open and ABORTS, as HotSpot's
    /// `G1ConcurrentMark::concurrent_cycle_abort` does. This VM still runs the
    /// cycle's remark (cleanup needs it), but that remark reports the cycle as
    /// aborted (`vm/src/runtime/interpreter/gc_events.rs` `g1_cycle_edge`).
    /// Taken by [`Self::take_aborted`]; a stale entry names an id no later
    /// cycle has, so it matches nothing.
    aborted: parking_lot::Mutex<Option<u64>>,
    /// zs s10/jf3 (ported by zt t1/jf): the open ZGC cycle's mark-start
    /// pause, `(gc_id, wall start ns, duration ns)`, for a collector whose
    /// closing collection writes the cycle's whole span and every pause of it
    /// (ZGC's JFR rows). Written by [`Self::note_initial_pause`] only, from
    /// the ZGC mark start; the Generational and G1 backends never write it.
    initial_pause: parking_lot::Mutex<Option<(u64, u64, u64)>>,
    /// zt t2/jf (residual R2): the heap as the open ZGC cycle's mark-start
    /// pause found it, `(gc_id, used bytes, committed bytes)`, for the
    /// cycle's `Before GC` heap summary (HotSpot traces the heap before the
    /// cycle, not before its closing pause). Written by
    /// [`Self::note_cycle_heap_before`] only, from the ZGC mark start.
    cycle_heap_before: parking_lot::Mutex<Option<(u64, u64, u64)>>,
    /// zt t31/zc11: when the open ZGC concurrent cycle's mark start began --
    /// taken before its pause was requested -- so the collection that closes
    /// (or abandons) the cycle books the `... Cycles` bean, and starts its
    /// cycle notification record, from the cycle's start, as HotSpot's cycle
    /// `GcInfo` spans its own Pause Mark Start. Written by
    /// [`Self::note_cycle_open_at`] only, from the ZGC mark start.
    cycle_open_at: parking_lot::Mutex<Option<std::time::Instant>>,
    /// zt t32 (successor page section 10): the open ZGC concurrent cycle's
    /// HotSpot cause, noted with [`Self::note_cycle_open_at`] by the mark
    /// start that opened it, so every later pause of the cycle -- a failed
    /// Pause Mark End's included -- is booked under the cycle's cause, as
    /// HotSpot books each pause of a cycle under its `GCCause`. Cleared with
    /// the start by [`Self::take_cycle_open_at`].
    cycle_cause: parking_lot::Mutex<Option<&'static str>>,
    /// zt t32: the cause of a requested cycle (`System.gc()`, `jcmd GC.run`,
    /// JVMTI, the metadata door) whose mark start has not run yet. Set by the
    /// requesting door around its mark start ([`Self::set_requested_cause`]),
    /// so a mark start that a PEER's allocating poll runs for that request --
    /// the request is a recorded every-trigger request any poll may serve --
    /// books the requester's cause, not the poll's empty trigger latch.
    requested_cause: parking_lot::Mutex<Option<&'static str>>,
}

impl ConcurrentCycleJfr {
    /// zs s10/jf3: cycle `gc_id`'s mark-start pause ran over `[start_ns,
    /// start_ns + dur_ns)`.
    pub fn note_initial_pause(&self, gc_id: u64, start_ns: u64, dur_ns: u64) {
        *self.initial_pause.lock() = Some((gc_id, start_ns, dur_ns));
    }

    /// zs s10/jf3: take cycle `gc_id`'s mark-start pause `(start ns, duration
    /// ns)`, leaving none; `None` when none was noted for that cycle (an
    /// entry of another cycle is dropped too).
    pub fn take_initial_pause(&self, gc_id: u64) -> Option<(u64, u64)> {
        match self.initial_pause.lock().take() {
            Some((id, start, dur)) if id == gc_id => Some((start, dur)),
            _ => None,
        }
    }

    /// zt t1/jf: take whatever mark-start pause is noted, leaving none: the
    /// cycle's `(gc_id, start ns, duration ns)`. A collection that finds no
    /// open cycle drains a stale entry with it, so the row is written rather
    /// than lost (see `gc_events::zgc_collection_jfr`).
    pub fn take_any_initial_pause(&self) -> Option<(u64, u64, u64)> {
        self.initial_pause.lock().take()
    }

    /// zt t2/jf: cycle `gc_id`'s mark-start pause found `used` / `committed`
    /// heap bytes.
    pub fn note_cycle_heap_before(&self, gc_id: u64, used: u64, committed: u64) {
        *self.cycle_heap_before.lock() = Some((gc_id, used, committed));
    }

    /// zt t2/jf: take cycle `gc_id`'s `(used, committed)` before figures,
    /// leaving none; `None` for any other cycle (whose entry is dropped).
    pub fn take_cycle_heap_before(&self, gc_id: u64) -> Option<(u64, u64)> {
        match self.cycle_heap_before.lock().take() {
            Some((id, used, committed)) if id == gc_id => Some((used, committed)),
            _ => None,
        }
    }

    /// zt t31/zc11: a ZGC mark start that began at `at` opened a concurrent
    /// cycle (see the `cycle_open_at` field).
    pub fn note_cycle_open_at(&self, at: std::time::Instant) {
        *self.cycle_open_at.lock() = Some(at);
    }

    /// zt t31/zc11: take the open cycle's start, if a mark start noted one;
    /// every ZGC collection takes it (it closes or abandons that cycle).
    pub fn take_cycle_open_at(&self) -> Option<std::time::Instant> {
        *self.cycle_cause.lock() = None;
        self.cycle_open_at.lock().take()
    }

    /// zt t32: the pending requested cycle's cause (see the
    /// `requested_cause` field); `None` withdraws it.
    pub fn set_requested_cause(&self, cause: Option<&'static str>) {
        *self.requested_cause.lock() = cause;
    }

    /// zt t32: the pending requested cycle's cause, if a door set one.
    pub fn requested_cause(&self) -> Option<&'static str> {
        *self.requested_cause.lock()
    }

    /// zt t32: the open cycle's cause (see the `cycle_cause` field).
    pub fn note_cycle_cause(&self, cause: &'static str) {
        *self.cycle_cause.lock() = Some(cause);
    }

    /// zt t32: the open cycle's cause, if a mark start noted one and no
    /// collection has taken the cycle since. Read, not taken.
    pub fn open_cycle_cause(&self) -> Option<&'static str> {
        *self.cycle_cause.lock()
    }

    /// gci i13/gp: a G1 full collection aborts cycle `gc_id` (see the
    /// `aborted` field).
    pub fn mark_aborted(&self, gc_id: u64) {
        *self.aborted.lock() = Some(gc_id);
    }

    /// gci i13/gp: was cycle `gc_id` aborted? Clears the mark when it was.
    pub fn take_aborted(&self, gc_id: u64) -> bool {
        let mut aborted = self.aborted.lock();
        if *aborted == Some(gc_id) {
            *aborted = None;
            true
        } else {
            false
        }
    }

    /// A cycle's initial-mark pause took `gc_id`: it is now the open cycle.
    pub fn open(&self, gc_id: u64) {
        *self.open.lock() = Some((gc_id, 0));
    }

    /// The initial-mark pause of cycle `gc_id` ended at `wall_ns`: its
    /// concurrent marking starts there. Ignored if another cycle opened since.
    pub fn mark_started(&self, gc_id: u64, wall_ns: u64) {
        let mut stamped = false;
        if let Some((id, start)) = self.open.lock().as_mut() {
            if *id == gc_id {
                *start = wall_ns;
                stamped = true;
            }
        }
        if stamped {
            *self.cycle_start.lock() = Some((gc_id, wall_ns));
        }
    }

    /// gci i12/ge: where cycle `gc_id`'s marking started (wall-clock ns),
    /// whatever re-opens happened since; `None` for any other cycle.
    pub fn cycle_start(&self, gc_id: u64) -> Option<u64> {
        let last: Option<(u64, u64)> = *self.cycle_start.lock();
        last.filter(|(id, _)| *id == gc_id).map(|(_, start)| start)
    }

    /// The remark pause: take the open cycle's `(gc_id, marking start)`,
    /// leaving none open.
    pub fn take(&self) -> Option<(u64, u64)> {
        self.open.lock().take()
    }

    /// gci i12/ge: the open cycle's `gc_id`, leaving it open (the G1
    /// remark's deferred `gc+ref` block files under it).
    pub fn open_id(&self) -> Option<u64> {
        let open: Option<(u64, u64)> = *self.open.lock();
        open.map(|(id, _)| id)
    }
}

/// What the notification machinery did, for the `--verbose:gc` summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcNotificationCensus {
    /// Collections recorded (each is some bean's last GC).
    pub recorded: u64,
    /// Of those, queued because the bean had a listener.
    pub queued: u64,
    /// `createGCNotification` returned normally.
    pub delivered: u64,
    /// Popped, but no bean carrying the name existed to deliver to.
    pub orphaned: u64,
    /// Discarded at [`GC_NOTIFICATION_QUEUE_CAPACITY`].
    pub dropped: u64,
    /// The Java side threw (a listener's exception surfaces as the JDK's
    /// `AssertionError("Error in invoking listener")`).
    pub failed: u64,
    /// Drain points that found work and could not run Java there.
    pub deferred: u64,
    /// gcg h1/ob (`gcd-d10o-first-gc-notification-arrives-a-second-late`):
    /// the FIRST queued notification's latency, split: `(queued -> its drain
    /// began, drain began -> its delivery returned)`, microseconds. `None`
    /// until that edge happened. See [`FirstDeliverySplit`].
    pub first_delivery_us: (Option<u64>, Option<u64>),
}

impl GcNotificationCensus {
    /// `[GC] gc_notifications: …` — `notif_`-prefixed keys, unique across the
    /// whole shutdown summary. gcg h1/ob: the first notification's latency
    /// split last (`notif_first_wait_us` = queued -> drain start, the
    /// hand-off / delivery-thread start; `notif_first_deliver_us` = drain
    /// start -> `createGCNotification` returned, the cold Java path; `-` =
    /// never reached).
    pub fn summary_line(&self) -> String {
        let us = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
        format!(
            "[GC] gc_notifications: notif_recorded={} notif_queued={} notif_delivered={} \
             notif_orphaned={} notif_dropped={} notif_failed={} notif_deferred={} \
             notif_first_wait_us={} notif_first_deliver_us={}",
            self.recorded,
            self.queued,
            self.delivered,
            self.orphaned,
            self.dropped,
            self.failed,
            self.deferred,
            us(self.first_delivery_us.0),
            us(self.first_delivery_us.1),
        )
    }
}

/// gcg h1/ob (`gcd-d10o-first-gc-notification-arrives-a-second-late`): when
/// the queue's first notification was queued, when the drain that popped it
/// began, and when its delivery returned. Each edge is stamped once, by the
/// queue itself (no clock read after the first delivery: the stamps are
/// `Some` from then on, and `record` / `begin_drain` / `note_outcome` test
/// that under the mutex they already take or with one relaxed load).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FirstDeliverySplit {
    pub queued: Option<std::time::Instant>,
    pub drain_started: Option<std::time::Instant>,
    pub delivered: Option<std::time::Instant>,
}

impl FirstDeliverySplit {
    /// `(queued -> drain start, drain start -> delivered)` in microseconds.
    /// Pure.
    pub fn split_us(&self) -> (Option<u64>, Option<u64>) {
        let us = |a: Option<std::time::Instant>, b: Option<std::time::Instant>| {
            Some(u64::try_from(b?.saturating_duration_since(a?).as_micros()).unwrap_or(u64::MAX))
        };
        (
            us(self.queued, self.drain_started),
            us(self.drain_started, self.delivered),
        )
    }
}

impl Default for GcNotificationQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// HotSpot Serial's two collector beans, the generational heap's queue slots.
const SERIAL_COLLECTORS: &[&str] = &[JMX_SERIAL_YOUNG_COLLECTOR, JMX_SERIAL_OLD_COLLECTOR];

impl GcNotificationQueue {
    /// The generational heap's queue: Serial's `Copy` / `MarkSweepCompact`.
    /// gcj j1/ob: reads `CRATONVM_GEN_THRESHOLD_SET_DRAIN` once, here (one
    /// queue per heap, so per VM; [`Self::serial_threshold_set_drain_enabled`]).
    pub fn new() -> Self {
        let mut queue = Self::with_collectors(SERIAL_COLLECTORS);
        queue.serial_threshold_set_drain =
            true;
        queue    }

    /// gcj j1/ob (`g1-and-generational-set-time-threshold-crossing-waits-for-a-drain-door-20261015`
    /// residual 2, the Generational half): should a usage-threshold crossing
    /// that `setUsageThreshold` finds on this queue's pools (Serial's heap
    /// pools, or a non-heap pool armed on the generational heap's table) be
    /// handed to the VM's delivery thread at once, so it is delivered while
    /// the setting thread sleeps -- as HotSpot's Service Thread delivers it --
    /// instead of at that thread's next allocation door?
    /// `CRATONVM_GEN_THRESHOLD_SET_DRAIN`, default ON, `=0` kill (the drain
    /// hint alone, gci i16/nt). Always `false` on a backend's queue
    /// ([`Self::with_collectors`]): ZGC has its own key
    /// ([`BackendGcBeans::threshold_set_drain_enabled`]), G1 keeps the old
    /// timing.
    pub fn serial_threshold_set_drain_enabled(&self) -> bool {
        self.serial_threshold_set_drain
    }

    /// A queue for the beans `names` (at most [`MAX_JMX_COLLECTORS`]; any
    /// further name is not served). gc-common w7-f.
    pub fn with_collectors(names: &'static [&'static str]) -> Self {
        Self {
            names: &names[..names.len().min(MAX_JMX_COLLECTORS)],
            enabled: Default::default(),
            pending_len: std::sync::atomic::AtomicUsize::new(0),
            pending: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            last: parking_lot::Mutex::new(Default::default()),
            draining: std::sync::atomic::AtomicBool::new(false),
            counts: Default::default(),
            pool_peaks: parking_lot::Mutex::new(Vec::new()),
            concurrent_cycle_jfr: std::sync::Arc::new(ConcurrentCycleJfr::default()),
            pool_thresholds: PoolThresholds::default(),
            first_delivery: parking_lot::Mutex::new(FirstDeliverySplit::default()),
            first_delivery_done: std::sync::atomic::AtomicBool::new(false),
            refused_streak: std::sync::atomic::AtomicU32::new(0),
            drain_missed: std::sync::atomic::AtomicBool::new(false),
            serial_threshold_set_drain: false,
        }
    }

    /// gcg h1/ob: the first notification's latency edges so far.
    pub fn first_delivery_split(&self) -> FirstDeliverySplit {
        *self.first_delivery.lock()
    }

    /// The heap pools' thresholds and sensors (gen r5w3/obs7). See
    /// [`PoolThresholds`].
    pub fn pool_thresholds(&self) -> &PoolThresholds {
        &self.pool_thresholds
    }

    /// Raise each pool's recorded peak to `pools`' usage (HotSpot's
    /// `record_peak_memory_usage`: `used` and `committed` maximised
    /// separately). Called with the bean snapshot taken at each edge of a
    /// collection. gen r5w2/obs6.
    pub fn note_pool_peaks(&self, pools: &[JmxPoolUsage]) {
        let mut peaks = self.pool_peaks.lock();
        for p in pools {
            Self::raise_peak(&mut peaks, p.name, p.used, p.committed);
        }
    }

    /// One pool's peak sample `(used, committed)` -- the sample a
    /// `getPeakUsage()` query took (gci i14/mp finding 13: the VM records the
    /// caller's sample instead of taking a second one).
    pub fn note_pool_peak(&self, pool: &'static str, used: u64, committed: u64) {
        Self::raise_peak(&mut self.pool_peaks.lock(), pool, used, committed);
    }

    fn raise_peak(peaks: &mut Vec<(&'static str, u64, u64)>, pool: &'static str, used: u64, committed: u64) {
        match peaks.iter_mut().find(|(name, _, _)| *name == pool) {
            Some(entry) => {
                entry.1 = entry.1.max(used);
                entry.2 = entry.2.max(committed);
            }
            None => peaks.push((pool, used, committed)),
        }
    }

    /// The peak `(used, committed)` recorded for `pool` at collection edges
    /// since the last reset, or `None` before the first collection (or for a
    /// pool this heap does not describe). gen r5w2/obs6.
    pub fn pool_peak(&self, pool: &str) -> Option<(u64, u64)> {
        self.pool_peaks
            .lock()
            .iter()
            .find(|(name, _, _)| *name == pool)
            .map(|&(_, used, committed)| (used, committed))
    }

    /// `MemoryPoolMXBean.resetPeakUsage()`: `pool`'s recorded peak becomes
    /// its current usage `(used, committed)`. A pool with no entry yet gets
    /// one. gen r5w2/obs6.
    pub fn reset_pool_peak(&self, pool: &'static str, used: u64, committed: u64) {
        let mut peaks = self.pool_peaks.lock();
        match peaks.iter_mut().find(|(name, _, _)| *name == pool) {
            Some(entry) => {
                entry.1 = used;
                entry.2 = committed;
            }
            None => peaks.push((pool, used, committed)),
        }
    }

    /// The heap's concurrent-cycle JFR state (generational backend). gen
    /// r5w2/obs6; see [`ConcurrentCycleJfr`].
    pub fn concurrent_cycle_jfr(&self) -> &std::sync::Arc<ConcurrentCycleJfr> {
        &self.concurrent_cycle_jfr
    }

    /// Slot of bean `name` in the per-bean arrays, or `None` when it is not one
    /// of this queue's beans.
    fn collector_index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| *n == name)
    }

    /// `GarbageCollectorExtImpl.setNotificationEnabled(bean, on)`. `false`
    /// when `collector` is not one of this heap's beans.
    pub fn set_enabled(&self, collector: &str, on: bool) -> bool {
        match self.collector_index(collector) {
            Some(i) => {
                self.enabled[i].store(on, Ordering::Release);
                true
            }
            None => false,
        }
    }

    pub fn is_enabled(&self, collector: &str) -> bool {
        self.collector_index(collector)
            .is_some_and(|i| self.enabled[i].load(Ordering::Acquire))
    }

    /// Record one completed collection: it becomes its bean's last GC, and is
    /// queued for delivery iff that bean has a listener.
    pub fn record(&self, rec: GcNotificationRecord) {
        let Some(i) = self.collector_index(rec.collector) else {
            return;
        };
        self.counts[NOTIF_RECORDED].fetch_add(1, Ordering::Relaxed);
        if self.enabled[i].load(Ordering::Acquire) {
            let mut q = self.pending.lock();
            q.push_back(rec.clone());
            self.counts[NOTIF_QUEUED].fetch_add(1, Ordering::Relaxed);
            if q.len() > GC_NOTIFICATION_QUEUE_CAPACITY {
                q.pop_front();
                self.counts[NOTIF_DROPPED].fetch_add(1, Ordering::Relaxed);
            }
            self.pending_len.store(q.len(), Ordering::Release);
            drop(q);
            // gcg h1/ob: the first queued notification's clock starts here.
            if !self.first_delivery_done.load(Ordering::Relaxed) {
                let mut f = self.first_delivery.lock();
                if f.queued.is_none() {
                    f.queued = Some(std::time::Instant::now());
                }
            }
        }
        self.last.lock()[i] = Some(rec);
    }

    /// `collector`'s most recent collection, for `getLastGcInfo()`.
    pub fn last(&self, collector: &str) -> Option<GcNotificationRecord> {
        let i = self.collector_index(collector)?;
        self.last.lock()[i].clone()
    }

    /// zs s6/ob5: run `f` on every record that can still be rendered: each
    /// queued notification, then each bean's last collection (one lock at a
    /// time, as [`Self::record`] takes them).
    pub fn patch_records(&self, mut f: impl FnMut(&mut GcNotificationRecord)) {
        for r in self.pending.lock().iter_mut() {
            f(r);
        }
        for r in self.last.lock().iter_mut().flatten() {
            f(r);
        }
    }

    /// Is anything waiting to be delivered — a GC notification, or (gen
    /// r5w3/obs7) a pool-threshold sensor request? Two loads, no lock.
    pub fn has_pending(&self) -> bool {
        self.pending_len.load(Ordering::Acquire) != 0 || self.pool_thresholds.has_pending()
    }

    /// zt t15/x12: how many GC notifications are queued now (one load; a
    /// bound for a drain point, `CRATONVM_ZGC_FORCED_NOTIFICATION_DRAIN_BOUNDED`).
    pub fn pending_notifications(&self) -> usize {
        self.pending_len.load(Ordering::Acquire)
    }

    /// A drain point found work but could not run Java there.
    pub fn note_deferred(&self) {
        self.counts[NOTIF_DEFERRED].fetch_add(1, Ordering::Relaxed);
    }

    /// Become the one drainer, or `None` if another drain is in progress
    /// (on this thread, re-entered through a listener, or on another thread,
    /// whose loop will pick up whatever this one would have).
    pub fn begin_drain(&self) -> Option<GcNotificationDrain<'_>> {
        let drain = self
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| GcNotificationDrain { queue: self })?;
        // gcg h1/ob: the first drain that finds the first notification queued.
        if !self.first_delivery_done.load(Ordering::Relaxed) {
            let mut f = self.first_delivery.lock();
            if f.queued.is_some() && f.drain_started.is_none() {
                f.drain_started = Some(std::time::Instant::now());
            }
        }
        Some(drain)
    }

    /// zt t13/x10: [`Self::begin_drain`] for a drain point that must not lose
    /// its records to the drain in progress. `begin_drain`'s "the other
    /// drainer's loop will pick it up" holds only while that loop still pops:
    /// a record queued after its last pop, by a thread whose own drain was
    /// refused here, waited for the next drain point -- on a `--compatible`
    /// ZGC VM whose program then sleeps, never (zt t12/gn4's hand-offs made
    /// the delivery thread a frequent second drainer). On a refusal this
    /// leaves a mark the drainer reads after it lets go
    /// ([`Self::end_drain_take_missed`]), then tries once more: the drainer
    /// may have let go between the two. Both sides fence (`SeqCst`) between
    /// their store and their load, so at least one of them sees the other:
    /// either this retry wins the drain, or the drainer sees the mark.
    pub fn begin_drain_noting_miss(&self) -> Option<GcNotificationDrain<'_>> {
        if let Some(drain) = self.begin_drain() {
            return Some(drain);
        }
        self.drain_missed.store(true, Ordering::SeqCst);
        std::sync::atomic::fence(Ordering::SeqCst);
        self.begin_drain()
    }

    /// zt t13/x10: let go of `drain`, then take the mark
    /// [`Self::begin_drain_noting_miss`] leaves: `true` when a drain point
    /// was refused since the mark was last taken, so the caller drains
    /// again if anything is pending.
    pub fn end_drain_take_missed(&self, drain: GcNotificationDrain<'_>) -> bool {
        drop(drain);
        std::sync::atomic::fence(Ordering::SeqCst);
        self.drain_missed.swap(false, Ordering::SeqCst)
    }

    pub fn census(&self) -> GcNotificationCensus {
        let c = |i: usize| self.counts[i].load(Ordering::Relaxed);
        GcNotificationCensus {
            recorded: c(NOTIF_RECORDED),
            queued: c(NOTIF_QUEUED),
            delivered: c(NOTIF_DELIVERED),
            orphaned: c(NOTIF_ORPHANED),
            dropped: c(NOTIF_DROPPED),
            failed: c(NOTIF_FAILED),
            deferred: c(NOTIF_DEFERRED),
            first_delivery_us: self.first_delivery_split().split_us(),
        }
    }
}

/// How one popped notification ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcNotificationOutcome {
    /// `createGCNotification` returned normally (with or without listeners).
    Delivered,
    /// No bean of that name to deliver to.
    Orphaned,
    /// The Java side threw.
    Failed,
}

/// The drain right held by one thread; released on drop, so an unwinding
/// delivery cannot wedge the queue.
#[derive(Debug)]
pub struct GcNotificationDrain<'a> {
    queue: &'a GcNotificationQueue,
}

impl GcNotificationDrain<'_> {
    /// The oldest pending notification. The lock is held for the pop only —
    /// never across the Java delivery.
    pub fn pop(&self) -> Option<GcNotificationRecord> {
        let mut q = self.queue.pending.lock();
        let rec = q.pop_front();
        self.queue.pending_len.store(q.len(), Ordering::Release);
        rec
    }

    /// zs s9/ob6: [`Self::pop`], except that the oldest record stays queued
    /// (and nothing behind it is taken, so the delivery order is kept) when
    /// `hold` says so: a ZGC cycle record still waiting for its `Metaspace`
    /// key ([`BackendGcBeans::metaspace_record_waiting`]).
    pub fn pop_unless(
        &self,
        hold: impl Fn(&GcNotificationRecord) -> bool,
    ) -> Option<GcNotificationRecord> {
        let mut q = self.queue.pending.lock();
        if q.front().is_some_and(&hold) {
            return None;
        }
        let rec = q.pop_front();
        self.queue.pending_len.store(q.len(), Ordering::Release);
        rec
    }

    /// gci i47w/oe47: put `rec` -- just popped, whose delivery failed because
    /// the heap refused to build its `GcInfo` / `CompositeData` (heap
    /// exhaustion) -- back at the HEAD of the queue, so a later drain point
    /// (after a further collection) builds it again, in order. HotSpot builds
    /// these on its Notification Thread after the collection has freed
    /// space; a drain that meets the pre-recovery heap must not lose them.
    /// `true` = kept (no outcome is counted for this attempt; the caller
    /// should stop draining); `false` = the refusal streak reached
    /// [`GC_NOTIFICATION_REFUSED_RETRIES`]: the streak restarts and the
    /// caller counts the record `Failed`, as before.
    pub fn requeue_refused(&self, rec: GcNotificationRecord) -> bool {
        let streak = self.queue.refused_streak.load(Ordering::Relaxed);
        if !refused_notification_kept(streak) {
            self.queue.refused_streak.store(0, Ordering::Relaxed);
            return false;
        }
        self.queue
            .refused_streak
            .store(streak.saturating_add(1), Ordering::Relaxed);
        let mut q = self.queue.pending.lock();
        q.push_front(rec);
        self.queue.pending_len.store(q.len(), Ordering::Release);
        true
    }

    pub fn note_outcome(&self, outcome: GcNotificationOutcome) {
        if outcome != GcNotificationOutcome::Failed {
            // gci i47w/oe47: a completed delivery ends a refusal streak.
            self.queue.refused_streak.store(0, Ordering::Relaxed);
        }
        let i = match outcome {
            GcNotificationOutcome::Delivered => NOTIF_DELIVERED,
            GcNotificationOutcome::Orphaned => NOTIF_ORPHANED,
            GcNotificationOutcome::Failed => NOTIF_FAILED,
        };
        self.queue.counts[i].fetch_add(1, Ordering::Relaxed);
        // gcg h1/ob: the first delivery's end (whatever its outcome).
        if !self.queue.first_delivery_done.load(Ordering::Relaxed) {
            let mut f = self.queue.first_delivery.lock();
            if f.drain_started.is_some() && f.delivered.is_none() {
                f.delivered = Some(std::time::Instant::now());
                self.queue
                    .first_delivery_done
                    .store(true, Ordering::Relaxed);
            }
        }
    }
}

/// gci i47w/oe47: the pure decision of [`GcNotificationDrain::requeue_refused`]:
/// keep a refused notification while the streak is under the bound.
fn refused_notification_kept(streak: u32) -> bool {
    streak < GC_NOTIFICATION_REFUSED_RETRIES
}

#[cfg(test)]
mod gci_i47w_oe47_refused_notification_tests;

impl Drop for GcNotificationDrain<'_> {
    fn drop(&mut self) {
        self.queue.draining.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Memory-pool usage thresholds (gen r5w3/obs7, 2026-09-26)
// ---------------------------------------------------------------------------
//
// `gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds`: HotSpot Serial's
// `Tenured Gen` supports a usage threshold and every heap pool a collection
// usage threshold; the `LowMemoryDetector` compares the pools against them
// and hands `sun.management.Sensor.trigger(count, usage)` /
// `Sensor.clear(count)` calls to the Service Thread, whose Java side sends
// `MEMORY_THRESHOLD_EXCEEDED` / `MEMORY_COLLECTION_THRESHOLD_EXCEEDED`
// through the `MemoryMXBean`. The same split here:
//
// * DETECTION — [`PoolThresholds::check`], at the end of every collection
//   (all pools against their usage thresholds; the pools the collection's
//   bean manages as "affected" against their collection thresholds) and when
//   a usage threshold is set (HotSpot's `jmm_SetPoolThreshold` runs
//   `detect_low_memory(pool)` at once). No Java, no allocation: a mutex over
//   a handful of entries, taken only while a threshold is armed.
// * DELIVERY — [`PoolThresholds::take_requests`], by the VM's GC-notification
//   drain (`gc_events.rs::run_gc_notifications`), where Java may run.
//
// The sensor state machine is HotSpot's `SensorInfo` with the high and low
// thresholds equal (what `MemoryPoolImpl.setUsageThreshold0` sets): a gauge
// for the usage threshold (triggered once per crossing, cleared when usage
// falls back below), a counter for the collection threshold (triggered by
// every collection that leaves the pool at or above it).
//
// Per heap (so per VM), inside the heap's [`GcNotificationQueue`]. Not armed
// (one relaxed load per collection) until a program sets a threshold.
//
// Not modelled: HotSpot also runs the usage check on slow-path allocations
// into a collected pool (`detect_low_memory_for_collected_pools`); here the
// old generation's usage is checked at collection ends only, so a pool that
// crosses its usage threshold by direct old-generation allocation between
// collections is reported at the next collection.

/// Which of a pool's two JMM thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolThresholdKind {
    /// `MemoryPoolMXBean.setUsageThreshold` — the pool's current usage.
    Usage,
    /// `MemoryPoolMXBean.setCollectionUsageThreshold` — the pool's usage
    /// right after a collection that affected it.
    Collection,
}

impl PoolThresholdKind {
    fn slot(self) -> usize {
        match self {
            PoolThresholdKind::Usage => 0,
            PoolThresholdKind::Collection => 1,
        }
    }
}

/// HotSpot Serial's threshold support per heap pool, `(usage,
/// collection_usage)`: `ContiguousSpacePool` (eden) and
/// `SurvivorContiguousSpacePool` are built with `support_usage_threshold =
/// false`, `TenuredGenerationPool` with `true`; every heap pool supports the
/// collection-usage threshold. `(false, false)` for a name that is not one of
/// Serial's pools. gen r5w3/obs7.
pub fn serial_pool_threshold_support(pool: &str) -> (bool, bool) {
    match pool {
        JMX_POOL_TENURED => (true, true),
        JMX_POOL_EDEN | JMX_POOL_SURVIVOR => (false, true),
        _ => (false, false),
    }
}

/// HotSpot 25's threshold support per G1 / ZGC heap pool, `(usage,
/// collection_usage)`, measured on 25.0.4 (`Gcd10oBeanShapeProbe`): the old
/// pools (`G1 Old Gen`, `ZGC Old Generation`) support both, the young ones
/// (`G1 Eden Space`, `G1 Survivor Space`, `ZGC Young Generation`) the
/// collection-usage threshold only. `(false, false)` for any other name.
///
/// gcd d10/o retirement (2026-10-04): every G1 / ZGC pool answered
/// `(false, false)`, so `setUsageThreshold` threw
/// `UnsupportedOperationException` where HotSpot arms the sensor. The
/// detection ([`PoolThresholds::check`] at every collection's end) and the
/// delivery were already backend-independent; only the support and the
/// arming were Serial's alone. As on Serial between collections, a pool's
/// usage is checked at collection ends only (see the section comment's "Not
/// modelled").
pub fn backend_pool_threshold_support(pool: &str) -> (bool, bool) {
    match pool {
        JMX_POOL_G1_OLD | JMX_POOL_ZGC_OLD => (true, true),
        JMX_POOL_G1_EDEN | JMX_POOL_G1_SURVIVOR | JMX_POOL_ZGC_YOUNG => (false, true),
        _ => (false, false),
    }
}

/// One `sun.management.Sensor` call for the drain to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolSensorRequest {
    pub pool: &'static str,
    pub kind: PoolThresholdKind,
    /// The pending trigger count HotSpot passes to both calls.
    pub count: u32,
    /// `Some([init, used, committed, max])` (JMM `-1` for undefined):
    /// `Sensor.trigger(count, usage)` with the usage at the crossing. `None`:
    /// `Sensor.clear(count)`.
    pub trigger_usage: Option<[i64; 4]>,
}

/// One sensor's state: HotSpot's `SensorInfo` fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SensorLevel {
    /// `_sensor_on`, as of the last delivered request.
    on: bool,
    pending_trigger: u32,
    pending_clear: u32,
    /// The usage recorded with the most recent trigger request.
    usage: [i64; 4],
}

impl SensorLevel {
    /// `SensorInfo::set_gauge_sensor_level` with high = low = `threshold`
    /// (`threshold > 0`): request a trigger when usage reaches the threshold
    /// and the sensor is off (or about to be cleared); request a clear when
    /// usage falls below it and the sensor is on (or about to be triggered).
    fn set_gauge(&mut self, used: u64, threshold: u64, usage: [i64; 4]) {
        let over_high = used >= threshold;
        let below_low = used < threshold;
        if over_high && ((!self.on && self.pending_trigger == 0) || self.pending_clear > 0) {
            self.pending_trigger = self.pending_trigger.saturating_add(1);
            self.usage = usage;
            self.pending_clear = 0;
        } else if below_low && self.pending_clear == 0 && (self.on || self.pending_trigger > 0) {
            self.pending_clear += 1;
        }
    }

    /// `SensorInfo::set_counter_sensor_level` (`threshold > 0`): every
    /// sample at or above the threshold is one more trigger; one below it
    /// clears a sensor that is (or will be) on.
    fn set_counter(&mut self, used: u64, threshold: u64, usage: [i64; 4]) {
        if used >= threshold {
            self.pending_trigger = self.pending_trigger.saturating_add(1);
            self.usage = usage;
            self.pending_clear = 0;
        } else if self.on || self.pending_trigger > 0 {
            self.pending_clear = self.pending_clear.saturating_add(1);
        }
    }

    fn has_pending(&self) -> bool {
        self.pending_trigger > 0 || self.pending_clear > 0
    }

    /// `SensorInfo::process_pending_requests`: a pending clear wins (and
    /// carries the pending trigger count), otherwise the triggers are
    /// delivered. Returns `(count, trigger_usage)`.
    fn take(&mut self) -> Option<(u32, Option<[i64; 4]>)> {
        if !self.has_pending() {
            return None;
        }
        let count = self.pending_trigger;
        self.pending_trigger = 0;
        if self.pending_clear > 0 {
            self.pending_clear = 0;
            self.on = false;
            Some((count, None))
        } else {
            self.on = true;
            Some((count, Some(self.usage)))
        }
    }
}

#[derive(Debug)]
struct PoolThresholdEntry {
    pool: &'static str,
    /// `[usage, collection]` thresholds in bytes; `0` = not checked.
    thresholds: [u64; 2],
    sensors: [SensorLevel; 2],
}

/// A heap's pool thresholds and their sensors. See the section comment.
#[derive(Debug, Default)]
pub struct PoolThresholds {
    /// Some threshold is non-zero: [`Self::check`] has work.
    armed: std::sync::atomic::AtomicBool,
    /// Some sensor has a pending request: the drain has work.
    pending: std::sync::atomic::AtomicBool,
    entries: parking_lot::Mutex<Vec<PoolThresholdEntry>>,
    /// gci i16/nt: class-file bytes defined since the last `Metaspace` sample
    /// ([`Self::non_heap_walk_due`]).
    non_heap_grown: AtomicU64,
    /// gci i16/nt: [`Self::non_heap_grown`] at which the next `Metaspace`
    /// sample is due; `0` (never sampled) = at once.
    non_heap_walk_after: AtomicU64,
    /// gci i16/nt: the VM's unloaded-class total at the last `Metaspace`
    /// sample; a change makes the next check sample (an unload can clear the
    /// gauge).
    non_heap_unloaded_seen: AtomicU64,
}

/// gci i16/nt: the least class-file growth between two `Metaspace` samples
/// on the define path (HotSpot's metaspace commit granule is 64 KiB, and its
/// low-memory detector runs per expansion).
const NON_HEAP_WALK_MIN_BYTES: u64 = 64 * 1024;
/// gci i16/nt: and the share of the last sampled usage the growth must reach
/// besides (1/64), so a define-heavy program with a `Metaspace` threshold
/// armed pays O(log) class-store walks, not one per define.
const NON_HEAP_WALK_USED_DIVISOR: u64 = 64;

/// A pool's usage as the `MemoryUsage` `[init, used, committed, max]` a
/// sensor is triggered with (`-1` for undefined).
fn pool_usage_array(p: &JmxPoolUsage) -> [i64; 4] {
    let b = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    [
        p.init.map_or(-1, b),
        b(p.used),
        b(p.committed),
        p.max.map_or(-1, b),
    ]
}

impl PoolThresholds {
    /// Set `pool`'s `kind` threshold to `threshold` bytes (`0` disarms it);
    /// returns the previous value. The sensor's state is kept, as HotSpot's
    /// is across a threshold change.
    pub fn set(&self, pool: &'static str, kind: PoolThresholdKind, threshold: u64) -> u64 {
        let mut entries = self.entries.lock();
        let i = match entries.iter().position(|e| e.pool == pool) {
            Some(i) => i,
            None => {
                entries.push(PoolThresholdEntry {
                    pool,
                    thresholds: [0; 2],
                    sensors: [SensorLevel::default(); 2],
                });
                entries.len() - 1
            }
        };
        let prev = std::mem::replace(&mut entries[i].thresholds[kind.slot()], threshold);
        let armed = entries.iter().any(|e| e.thresholds.iter().any(|&t| t > 0));
        self.armed.store(armed, Ordering::Release);
        prev
    }

    /// `pool`'s `kind` threshold (`0` when never set or disarmed).
    pub fn threshold(&self, pool: &str, kind: PoolThresholdKind) -> u64 {
        self.entries
            .lock()
            .iter()
            .find(|e| e.pool == pool)
            .map_or(0, |e| e.thresholds[kind.slot()])
    }

    /// Is any threshold set? One load.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    /// Compare `pools` (sampled at one instant) against the usage
    /// thresholds, and — for the pools named in `collected`, the ones the
    /// collection that just ended affected — against the collection
    /// thresholds (`pools` is then the after-collection sample, i.e. each
    /// pool's last collection usage). `collected` is empty for a check that
    /// is not at a collection's end. Returns at once when nothing is armed.
    pub fn check(&self, pools: &[JmxPoolUsage], collected: &[&str]) {
        if !self.is_armed() {
            return;
        }
        let mut entries = self.entries.lock();
        let mut pending = false;
        for e in entries.iter_mut() {
            if let Some(p) = pools.iter().find(|p| p.name == e.pool) {
                let usage = pool_usage_array(p);
                let [usage_t, collection_t] = e.thresholds;
                if usage_t > 0 {
                    e.sensors[0].set_gauge(p.used, usage_t, usage);
                }
                if collection_t > 0 && collected.contains(&e.pool) {
                    e.sensors[1].set_counter(p.used, collection_t, usage);
                }
            }
            pending |= e.sensors.iter().any(SensorLevel::has_pending);
        }
        if pending {
            self.pending.store(true, Ordering::Release);
        }
    }

    /// Is a sensor request waiting for the drain? One load.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// gcf f4/r (`gengc-r5w3-obs7-proposal-usage-threshold-on-old-generation-growth`):
    /// HotSpot's `LowMemoryDetector::detect_low_memory_for_collected_pools`,
    /// which `MemAllocator` runs after each slow-path allocation, for ONE pool
    /// that grew outside a collection (the old generation, on a direct
    /// old-generation allocation). Usage threshold only (`collected` is
    /// empty). One load when nothing is armed; otherwise the `check` above
    /// (this table's own lock, a leaf: the caller may hold the old-generation
    /// lock). Returns whether a sensor request is pending for the drain -- the
    /// VM's allocation slow path, which runs right after the allocation.
    pub fn check_usage_on_growth(&self, pool: &JmxPoolUsage) -> bool {
        if !self.is_armed() {
            return false;
        }
        self.check(std::slice::from_ref(pool), &[]);
        self.has_pending()
    }

    /// gci i16/nt (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
    /// item 7's residual): is a `Metaspace` usage sample due? The VM's
    /// non-heap check samples the pool with a class-store walk, so on the
    /// define path it samples only once `grown_bytes` (this define's class-file
    /// length; `0` from a collection's end) has added up to the cadence
    /// [`Self::note_non_heap_walk`] set -- HotSpot checks per metaspace
    /// expansion, not per class -- or when `unloaded_total` (the VM's
    /// unloaded-class count) moved since the last sample. Always due before
    /// the first sample. Relaxed: a race costs one extra or one later sample.
    pub fn non_heap_walk_due(&self, grown_bytes: u64, unloaded_total: u64) -> bool {
        let grown = self
            .non_heap_grown
            .fetch_add(grown_bytes, Ordering::Relaxed)
            .saturating_add(grown_bytes);
        grown >= self.non_heap_walk_after.load(Ordering::Relaxed)
            || self.non_heap_unloaded_seen.load(Ordering::Relaxed) != unloaded_total
    }

    /// gci i16/nt: a `Metaspace` sample of `used` bytes was just checked, with
    /// the VM's unloaded-class count at `unloaded_total`: restart the growth
    /// count and set the next cadence, `max(64 KiB, used / 64)` class-file
    /// bytes.
    pub fn note_non_heap_walk(&self, used: u64, unloaded_total: u64) {
        self.non_heap_grown.store(0, Ordering::Relaxed);
        self.non_heap_walk_after.store(
            (used / NON_HEAP_WALK_USED_DIVISOR).max(NON_HEAP_WALK_MIN_BYTES),
            Ordering::Relaxed,
        );
        self.non_heap_unloaded_seen
            .store(unloaded_total, Ordering::Relaxed);
    }

    /// Take every pending sensor request, in pool order (usage before
    /// collection per pool), for delivery where Java may run.
    pub fn take_requests(&self) -> Vec<PoolSensorRequest> {
        let mut entries = self.entries.lock();
        self.pending.store(false, Ordering::Release);
        let mut out = Vec::new();
        for e in entries.iter_mut() {
            for (slot, kind) in [PoolThresholdKind::Usage, PoolThresholdKind::Collection]
                .into_iter()
                .enumerate()
            {
                if let Some((count, trigger_usage)) = e.sensors[slot].take() {
                    out.push(PoolSensorRequest {
                        pool: e.pool,
                        kind,
                        count,
                        trigger_usage,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod gc_notification_tests;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod zt6_rv3_backfill_hold_tests;

#[cfg(test)]
mod gcj_j1_ob_threshold_set_drain_tests;

#[cfg(test)]
mod tests;

/// zt t13/x10 (`CRATONVM_ZGC_NOTIFICATION_MISSED_REDRAIN`): a drain point
/// refused by the drain in progress leaves a mark the drainer takes after it
/// lets go; the plain door ([`GcNotificationQueue::begin_drain`], G1,
/// Generational and the `=0` kill) leaves none.
#[cfg(test)]
mod zt13x10_missed_drain_tests;

/// The `getCollectionTime()` figure of any source that keeps a microsecond
/// pause SUM — G1's accumulator, the Generational fallback, and the per-VM
/// pause-duration accumulator ZGC answers from (gc-common w6-f): the whole
/// milliseconds of the sum, converted once, as HotSpot's
/// `GCMemoryManager::_accumulated_timer` (and `gc_metrics::serial_collectors`
/// since gen r5w3/obs7).
///
/// gce e1/o (`gce-e1o-g1-zgc-collection-time-adds-a-millisecond-per-pause`):
/// this added one millisecond per collection so the figure moved on every
/// pause, which on ZGC's `ZGC Pauses` bean made the time roughly the pause
/// COUNT (1 000 pauses of 100 µs read 1 100 ms where HotSpot reads 100). A
/// sub-millisecond pause now leaves it unchanged, as on HotSpot; a caller
/// polling it for change (H2's `Utils.collectGarbage()`) sees it move once the
/// summed pauses cross the next millisecond.
pub fn pause_sum_as_collection_time_ms(total_pause_us: u64) -> u64 {
    total_pause_us / 1_000
}

