// SPDX-License-Identifier: Apache-2.0
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
//! Note also **what the gate cannot see**: the JIT emits its own inline
//! post-write barrier (`jit_card_table_info` → a direct release byte-store into
//! the card bitmap), which never enters `write_barrier` at all. So
//! `card_marks_executed` counts *interpreter and native* card marks only. The
//! collector-side counters below are complete regardless, because every card
//! ultimately funnels through `take_dirty_cards`.
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
//! `gengc-plumbing-duplicate-card-mark-denominator-20260920.md`: they were one
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
    /// `gengc-oldgen-dead-buffered-card-pipeline-20260920.md`) and because a
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
    /// sees interpreter and native marks only; the JIT's inline store never
    /// enters this crate.
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
    /// Objects allocated into the young generation since startup (gauge,
    /// republished from `HeapStats` at report time).
    allocated_objects: AtomicU64,
    /// Bytes allocated since startup (gauge).
    allocated_bytes: AtomicU64,
    /// Live bytes as of the last completed cycle (gauge).
    live_bytes: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
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
        }
    }
}

#[cfg(not(test))]
static COUNTERS: Counters = Counters::new();

#[cfg(not(test))]
#[inline]
fn with_counters<R>(f: impl FnOnce(&Counters) -> R) -> R {
    f(&COUNTERS)
}

#[cfg(test)]
thread_local! {
    static COUNTERS: Counters = const { Counters::new() };
}

#[cfg(test)]
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

#[cfg(not(test))]
static HOT_PATH_GATE: AtomicU8 = AtomicU8::new(GATE_UNRESOLVED);

#[cfg(test)]
thread_local! {
    static HOT_PATH_GATE: AtomicU8 = const { AtomicU8::new(GATE_UNRESOLVED) };
}

#[cfg(not(test))]
#[inline]
fn gate_load() -> u8 {
    HOT_PATH_GATE.load(Ordering::Relaxed)
}

#[cfg(not(test))]
#[inline]
fn gate_store(v: u8) {
    HOT_PATH_GATE.store(v, Ordering::Relaxed);
}

#[cfg(test)]
#[inline]
fn gate_load() -> u8 {
    HOT_PATH_GATE.with(|g| g.load(Ordering::Relaxed))
}

#[cfg(test)]
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
    let on = cratonvm_types::flags::runtime_var_os("CRATONVM_GC_CARD_METRICS").is_some();
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
/// Does **not** see the JIT's inline post-write barrier, which stores
/// `CARD_DIRTY` straight into the bitmap without entering this crate.
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
/// `docs/internal/gaps/gengc-plumbing-duplicate-card-mark-denominator-20260920.md`
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
    ///   including the JIT's inline post-write barrier, which never enters
    ///   this crate.
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
    /// Counts interpreter and native marks only. The JIT's inline post-write
    /// barrier has no clean/dirty accounting at all, so a JIT-warm run's
    /// barrier duplicate rate is *sampled* from the interpreted tail, not
    /// measured whole. That is a real limitation of the number, and it is a
    /// limitation `barrier_hit_rate` already has.
    ///
    /// **It is a mark-side number, not a scan-side one.** It says how much of
    /// the barrier's work was redundant. It does *not* say how much of the
    /// dirty-card scan was redundant: the card is keyed on the holder's object
    /// start, so one duplicate mark on a large array's header card still
    /// provokes a re-read of every slot in that array. That cost is
    /// `gengc-oldgen-imprecise-card-over-scan-20260920.md` and this ratio
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
                 (interpreter/native only — the JIT inline barrier is not counted)",
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
        writeln!(
            f,
            "[GC] cards/alloc: dirty_cards_per_obj={:.6} edges_per_obj={:.6} \
             card_marks_per_obj={:.6} (allocated_objects={} allocated_bytes={})",
            self.dirty_cards_per_allocated_object,
            self.old_to_young_edges_per_allocated_object,
            self.card_marks_per_allocated_object,
            r.allocated_objects,
            // `allocated_bytes` was STORED by `record_heap_occupancy` and read
            // by nothing: not a ratio, not a rendered field. A gauge nobody
            // prints is indistinguishable from a gauge nobody publishes, which
            // is the failure mode this module exists to prevent. It is also the
            // denominator a reader needs to turn `dirty_cards_per_obj` into a
            // per-byte figure when object sizes differ wildly between runs.
            r.allocated_bytes,
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
        )
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
    /// Non-moving because a live JIT frame's roots were discovered
    /// CONSERVATIVELY and cannot be rewritten.
    ///
    /// **This code covers TWO terms of `divert_non_moving`, and they are not
    /// the same situation.** `gen_heap::collect_garbage_inner` passes it for
    /// either of:
    ///
    /// * `has_conservative_roots && !moving_young` — the legacy rule, live only
    ///   while moving-young is switched off; and
    /// * `unrewritable_conservative_jit_roots` (added 2026-09-06) —
    ///   `moving_young && !CRATONVM_GC_NO_PEER_PIN_DIVERT && has_conservative_roots
    ///   && gc_quiescence::conservative_jit_scans() > 0`. This one fires with
    ///   moving-young **ON**, and on a JIT-warm workload it fires on nearly
    ///   every cycle, because every mutator publishes a conservative scan on its
    ///   way into the pause.
    ///
    /// Read [`CollectorDecision::moving_young_requested`] to tell them apart: a
    /// `true` there beside this reason is the second term.
    ///
    /// **As of 2026-09-20 the DISTRIBUTION is separable too**, without the
    /// second code the filed gap asked for and without any change to the
    /// recording site: [`super::record_collector_decision`] splits this reason
    /// by `moving_young_requested` into
    /// [`super::conservative_jit_root_divert_split`], which
    /// [`super::collector_decision_report`] renders as its own line. That works
    /// because `gen_heap::collect_garbage_inner` tests
    /// `divert_for_incomplete_moving_coverage` *before* this arm, so by the
    /// time this reason is chosen `moving_young == moving_young_requested` —
    /// making the flag an exact discriminator between the two terms rather than
    /// a heuristic. The code itself is deliberately NOT split: a new constant
    /// would have to be passed from `gen_heap.rs`, and a defined-but-never-
    /// recorded reason code is the inert instrumentation this module exists to
    /// prevent (see [`NON_MOVING_BACKEND_HAS_NO_YOUNG_COPY`], which is in that
    /// state today). Filed as
    /// `docs/internal/gaps/gengc-plumbing-conservative-root-divert-reason-20260920.md`.
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
    /// mark-sweep).
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

    /// One past the highest defined code.
    pub const COUNT: u8 = 17;

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
    const fn new() -> Self {
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
#[cfg(not(test))]
static DECISION_HISTOGRAM: [AtomicU64; decision_reason::COUNT as usize] =
    [const { AtomicU64::new(0) }; decision_reason::COUNT as usize];

#[cfg(test)]
thread_local! {
    static DECISION_HISTOGRAM: [AtomicU64; decision_reason::COUNT as usize] =
        const { [const { AtomicU64::new(0) }; decision_reason::COUNT as usize] };
}

#[cfg(not(test))]
fn bump_decision_histogram(reason: u8) {
    if let Some(slot) = DECISION_HISTOGRAM.get(reason as usize) {
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
fn bump_decision_histogram(reason: u8) {
    DECISION_HISTOGRAM.with(|h| {
        if let Some(slot) = h.get(reason as usize) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    });
}

#[cfg(not(test))]
fn read_decision_histogram() -> Vec<(u8, u64)> {
    DECISION_HISTOGRAM
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u8, s.load(Ordering::Relaxed)))
        .collect()
}

#[cfg(test)]
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

/// `nonmoving-conservative-jit-roots` cycles, split by whether moving-young was
/// requested — index `0` = not requested, index `1` = requested.
///
/// One reason code covers two genuinely different situations with two different
/// operator actions (see
/// [`decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS`]), and the last-cycle
/// line could already tell them apart while the histogram — the view that
/// matters for a distribution — could not. This is that split, taken from the
/// flag [`record_collector_decision`] already computes, so it costs one
/// relaxed `fetch_add` on a path that runs once per collection and needs no
/// change at the recording site in `gen_heap.rs`.
///
/// `[1]` is the 2026-09-06 `unrewritable_conservative_jit_roots` term, whose
/// only lever is `CRATONVM_GC=-peer-pin-divert` (legacy alias
/// `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`). `[0]` is the legacy
/// `has_conservative_roots && !moving_young` rule, whose lever is turning
/// moving-young on. Pointing an operator at the wrong one of those is the
/// defect this exists to stop.
#[cfg(not(test))]
static CONSERVATIVE_JIT_ROOT_DIVERTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

// Thread-local under `cfg(test)`, like every other counter in this module: the
// gc unit tests run in parallel threads of one process, so a process global
// here would make one test's recorded decisions visible to another's assertion.
#[cfg(test)]
thread_local! {
    static CONSERVATIVE_JIT_ROOT_DIVERTS: [AtomicU64; 2] =
        const { [const { AtomicU64::new(0) }; 2] };
}

#[cfg(not(test))]
fn bump_conservative_jit_root_divert(moving_young_requested: bool) {
    CONSERVATIVE_JIT_ROOT_DIVERTS[usize::from(moving_young_requested)]
        .fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
fn bump_conservative_jit_root_divert(moving_young_requested: bool) {
    CONSERVATIVE_JIT_ROOT_DIVERTS
        .with(|c| c[usize::from(moving_young_requested)].fetch_add(1, Ordering::Relaxed));
}

#[cfg(not(test))]
fn read_conservative_jit_root_diverts() -> (u64, u64) {
    (
        CONSERVATIVE_JIT_ROOT_DIVERTS[0].load(Ordering::Relaxed),
        CONSERVATIVE_JIT_ROOT_DIVERTS[1].load(Ordering::Relaxed),
    )
}

#[cfg(test)]
fn read_conservative_jit_root_diverts() -> (u64, u64) {
    CONSERVATIVE_JIT_ROOT_DIVERTS
        .with(|c| (c[0].load(Ordering::Relaxed), c[1].load(Ordering::Relaxed)))
}

/// `(moving_young_off, moving_young_on)` — the two terms behind
/// [`decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS`], counted separately.
///
/// The pair sums to that reason's histogram row. `moving_young_on` is the
/// 2026-09-06 `unrewritable_conservative_jit_roots` term and is expected to
/// dominate on any JIT-warm workload; `moving_young_off` is the legacy rule and
/// should be zero on a run that did not set `CRATONVM_NO_MOVING_YOUNG`.
pub fn conservative_jit_root_divert_split() -> (u64, u64) {
    read_conservative_jit_root_diverts()
}

#[cfg(not(test))]
static DECISION: DecisionSlot = DecisionSlot::new();

#[cfg(not(test))]
#[inline]
fn with_decision<R>(f: impl FnOnce(&DecisionSlot) -> R) -> R {
    f(&DECISION)
}

#[cfg(test)]
thread_local! {
    static DECISION: DecisionSlot = const { DecisionSlot::new() };
}

#[cfg(test)]
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
    // Split the one reason code that covers two situations, while the
    // discriminator is still in hand. See
    // `decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS` for why this flag is
    // exact rather than indicative at this point in the decision.
    if reason == decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS {
        bump_conservative_jit_root_divert(flags & FLAG_MOVING_YOUNG_REQUESTED != 0);
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

/// A one-or-two-line human-readable statement of what the collector actually
/// did on the last cycle, and why.
///
/// This is the ground truth that settles the `docs/GC.md` ↔ `ARCHITECTURE.md`
/// disagreement about whether young collections move — see
/// `tlab-and-card-audit.md` §3.
pub fn collector_decision_report() -> String {
    // Distribution first: the last decision alone has repeatedly misled.
    let (moving, non_moving, skipped, rows) = decision_histogram();
    let mut hist = String::new();
    if moving + non_moving + skipped > 0 {
        hist.push_str(&format!(
            "[GC] decision histogram: moving={moving} non_moving={non_moving} skipped={skipped}"
        ));
        for (label, n) in &rows {
            hist.push_str(&format!(" {label}={n}"));
        }
        hist.push('\n');
        // The one histogram row that cannot direct work on its own, broken out.
        // Printed only when it happened, because a zero row here would be a
        // third thing to explain rather than a fact.
        let (my_off, my_on) = conservative_jit_root_divert_split();
        if my_off + my_on > 0 {
            hist.push_str(&format!(
                "[GC] conservative-jit-root diverts: \
                 moving_young_off={my_off} (legacy rule — the lever is turning moving-young ON) \
                 moving_young_on={my_on} (unrewritable conservative roots, 2026-09-06 — \
                 the lever is CRATONVM_GC=-peer-pin-divert)\n"
            ));
        }
    }
    let mut s = match last_collector_decision() {
        // 2026-09-20: this arm used to state, flatly, that no collection had
        // run. That sentence is FALSE on any allocating run of the DEFAULT
        // backend, because ZGC never calls `record_collector_decision` — the
        // three call sites are `g1.rs` and the two generational arms. It is the
        // line an investigation starts from (it is emitted from vm-cli's
        // shutdown hook on both exit arms), and "no collection has run" is the
        // single most misleading thing it could say, because it invites the
        // reader to stop. Now it says what it actually knows, and names why the
        // default backend produces it. Filed as
        // `docs/internal/gaps/gengc-plumbing-zgc-records-no-collector-decision-20260920.md`;
        // the real repair is a `record_collector_decision` call in
        // `zgc.rs`, which this round did not own.
        None => format!(
            "[GC] decision: no collector decision has been recorded \
             (moving_young_requested={}). This does NOT mean no collection has run yet: \
             ZGC is the DEFAULT backend and records no decision, so on a default run this \
             line is expected and says nothing about GC frequency — read \
             `[GC] zgc-real: collections=N` for that.",
            crate::gc_quiescence::moving_young_enabled(),
        ),
        Some(d) => {
            let mut s = d.to_string();
            let fallbacks = crate::gc_quiescence::moving_young_coverage_fallback_count();
            let cycles = crate::gc_quiescence::moving_young_cycle_count();
            s.push('\n');
            s.push_str(&format!(
                "[GC] decision history: moving_cycles_under_live_jit={cycles} \
                 coverage_fallbacks={fallbacks}"
            ));
            // The peer ledger only runs when `peer_depth > 0`. Report the zeros
            // beside the fallbacks, because a cycle that skipped the ledger and
            // a cycle the ledger accepted are indistinguishable in every other
            // counter here -- and only one of them was actually checked.
            let pz = crate::gc_quiescence::PEER_DEPTH_ZERO_TOTAL.load(Ordering::Relaxed);
            let pz_torn = crate::gc_quiescence::PEER_DEPTH_ZERO_TORN.load(Ordering::Relaxed);
            let pz_quiet =
                crate::gc_quiescence::PEER_DEPTH_ZERO_GLOBAL_ZERO.load(Ordering::Relaxed);
            s.push('\n');
            s.push_str(&format!(
                "[GC] peer ledger skipped: peer_depth_zero={pz} \
                 (global_zero={pz_quiet} torn_global_lt_local={pz_torn})"
            ));
            s
        }
    };
    if !hist.is_empty() {
        s = format!("{}{s}", hist);
    }
    // G1 states its own last cycle. Absent on a Generational/ZGC run, so the
    // report keeps its previous shape there byte-for-byte.
    if let Some(g1) = last_g1_cycle() {
        s.push('\n');
        s.push_str(&g1.to_string());
    }
    // How often G1 found this collection's JIT root set incomplete. A rate
    // near zero says the empty-CSet fail-safe costs nothing; a high rate says
    // reclamation is being throttled by an unproven coverage obligation and
    // the histogram row names which one.
    let (g1_pauses, g1_incomplete) = g1_pause_coverage_counts();
    if g1_pauses > 0 {
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 root coverage: pauses={g1_pauses} incomplete={g1_incomplete} ({pct:.2}%){reasons}",
            pct = 100.0 * g1_incomplete as f64 / g1_pauses as f64,
            // The reason, appended rather than on its own line so a reader
            // cannot see the rate without it.
            reasons = {
                let rows = g1_coverage_reason_counts();
                if rows.is_empty() {
                    String::new()
                } else {
                    let mut s = String::from(" reasons:");
                    for (label, n) in rows {
                        s.push_str(&format!(" {label}={n}"));
                    }
                    s
                }
            },
        ));
        // The narrow sibling of the line above, and the one that can actually
        // select: how often a pause ran with a compiled frame live and NOTHING
        // published to pin. Those are the pauses the fail-safe refuses, so this
        // rate IS the cost of the fail-safe. Printed alongside so the two are
        // never read as the same number — `incomplete` near 100% is normal,
        // `empty_publication` above a trickle is not.
        let empty_pub = g1_empty_jit_publication_count();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 jit publication: pauses={g1_pauses} empty_while_in_jit={empty_pub} ({:.2}%)",
            100.0 * empty_pub as f64 / g1_pauses as f64,
        ));
    }
    // The G1 guard counters, every one of which is a process static and every
    // one of which is expected to read ZERO.
    //
    // Here rather than in `G1Collector::print_gc_summary` for the reason the
    // `promo_dest` block below already gives, and which turns out to have
    // applied to all of them: that function is reached only from `vm-cli`'s
    // normal-return teardown, and `System.exit` never unwinds Rust frames, so
    // on every JUnit workload in the suites (`JUnitCore` exits) not one of
    // these lines had ever been emitted. Their own doc comments say they print
    // "unconditionally ... before the early return" so that a zero can be
    // CITED as evidence; the zero was never printed at all.
    //
    // Unconditional here too, and for the same reason: a counter that only
    // appears when it is non-zero cannot be told apart from a counter whose
    // report never ran.
    {
        let (holder_rejected, holder_clamped) = crate::g1::evacuation_holder_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 evac_ref_rejected={} (torn={}) evac_holder_rejected={holder_rejected} evac_holder_clamped={holder_clamped} source_walk_desync={}",
            crate::g1::evacuation_refs_rejected(),
            crate::g1::evacuation_refs_rejected_torn(),
            crate::g1::evacuation_source_walk_desyncs(),
        ));
        s.push('\n');
        // LANE W4-B — SEEN and COPIED beside SKIPPED, and the
        // re-evacuation guard beside them.
        //
        // `NON_OBJECT_ROOT_COPIED`'s own doc said the pair was "still
        // printed". It was not: this line carried SKIPPED alone and
        // `non_object_root_counts()` had no caller in the workspace. The two
        // are a matched pair by construction — SKIPPED is what the root loops
        // do INSTEAD of copying, so SEEN is their denominator and COPIED is
        // structurally zero unless someone reintroduces the copy, which is the
        // regression this pair exists to catch.
        //
        // `reevacuated_after_retire` is the same shape and was dark for the
        // same reason: expected zero, non-zero means a driver retired its
        // forwards before a phase that still evacuates, which makes a second
        // live copy of an object and overwrites the correct `pointer_map`
        // entry — the object-IDENTITY defect the Spring Boot residual
        // measured as reflection metadata reading back null. Its static
        // survived only inside a `tracing::warn!`, so on a clean run the
        // number could not be read at all and its silence was
        // indistinguishable from the report never running.
        //
        // Capitals on `COPIED` and `REEVACUATED` for the reason
        // `root_remap_audit: UNREMAPPED` shouts: these are correctness
        // signals, not engagement numbers, and a reader scanning a wall of
        // census lines has to be able to see which zeros are load-bearing.
        //
        // The `[GC] g1 non_object_roots_skipped=` PREFIX is unchanged so the
        // reachability test that pins it keeps pinning it.
        let (nor_seen, nor_copied) = crate::g1::non_object_root_counts();
        s.push_str(&format!(
            "[GC] g1 non_object_roots_skipped={} seen={nor_seen} COPIED={nor_copied} \
             REEVACUATED_AFTER_RETIRE={}",
            crate::g1::non_object_roots_skipped(),
            crate::g1::reevacuated_after_retire(),
        ));
        // Which evacuator actually ran. Without this pair an A/B over the
        // worker count measures an unknown, and the nearest-looking counter
        // belongs to a different collector.
        let (par, ser, workers) = crate::g1::g1_young_evac_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 young evacuation: parallel={par} serial={ser} workers_last={workers}"
        ));
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 implausible_legacy_headers={} copy_shape_drift={}",
            crate::g1::evacuation_implausible_class0_copies(),
            crate::g1::evacuation_copy_shape_drifts(),
        ));
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 flat_walk_refused_array={} kept_seed_rejected={}",
            crate::g1::flat_walks_refused_for_array(),
            crate::g1::kept_seeds_rejected(),
        ));
    }
    // Promotion destination supply. Here rather than in
    // `G1Collector::print_gc_summary` because that function runs on the
    // normal-return arm only, and every workload this number is wanted for
    // ends in `System.exit`.
    //
    // `resumed_dest_regions` near zero beside a large pause count is the
    // pre-2026-09-06 behaviour, in which each worker's promotion TLAB took a
    // whole fresh Free region every pause and the old generation grew by the
    // WORKER COUNT per young pause however little was promoted. Read it
    // against `[GC] g1 cycle` above. See `g1::SharedEvac::resume`.
    {
        use std::sync::atomic::Ordering as O;
        let resumed = crate::g1::PARALLEL_EVAC_RESUMED_DEST_REGIONS.load(O::Relaxed);
        let shared = crate::g1::PARALLEL_SHARED_DEST_ALLOCS.load(O::Relaxed);
        let exhausted = crate::g1::PARALLEL_TLAB_POOL_EXHAUSTED.load(O::Relaxed);
        // LANE D wave 2 — `proved_empty` beside `tlab_pool_exhausted` is what
        // says whether the shared-destination fallback is DOING anything or
        // merely being asked. Before the latch the two were equal by
        // construction and `shared_dest_allocs` was zero, which is a fallback
        // that costs a full region-table pass per object and cannot succeed.
        let proved = crate::g1::PARALLEL_SHARED_DEST_PROVED_EMPTY.load(O::Relaxed);
        if resumed | shared | exhausted != 0 || proved != 0 {
            s.push('\n');
            s.push_str(&format!(
                "[GC] g1 promo_dest: resumed_dest_regions={resumed} shared_dest_allocs={shared} tlab_pool_exhausted={exhausted} shared_dest_proved_empty={proved}"
            ));
        }
    }
    // LANE `gate` (2026-09-22) — THE BLOCK-OFFSET TABLE'S THREE CENSUSES, on
    // one line, because none of the three means anything read alone.
    //
    // `g1::block_offset_no_jump_census` was filed as an orphan by
    // `scripts/check-orphan-instruments.sh` on its first real run
    // (`docs/internal/fixed-bugs/r10-gate-two-g1-censuses-have-no-reader-FIXED-20260922.md`)
    // and the sweep that followed it found its two siblings in the same state:
    // every reader of all three was under `gc/tests/`, so on a shipped binary
    // the block-offset lever could be turned on and produce no reading at all.
    //
    // The three answer one question between them, and `g1.rs` says so at each
    // definition:
    //
    // * `jumps` / `bytes_jumped` / `entries_refused` — did the table move the
    //   walk, how far, and how often did it name something that is not an
    //   object header (`refused` is the signal that says turn the lever back
    //   off);
    // * `tail_breaks` / `no_entry` — WHY `jumps == 0`, which has three causes
    //   wanting three different responses. A high `tail_breaks` with a zero
    //   `no_entry` says the walk never had a gap to jump over — a fact about
    //   the workload. A non-zero `no_entry` says it did and the table had no
    //   answer — a fact about the table;
    // * `audit_spans` / `audit_objects` / `audit_violations` —
    //   `CRATONVM_G1_BLOCK_OFFSET_AUDIT`'s verdict. The third is a documented
    //   MUST-BE-ZERO: a non-zero says an object holding a cross-region
    //   reference was inside bytes a jump skipped, which is the producer rule
    //   breaking and the premise the whole optimisation rests on.
    //
    // Unconditional and printed with its zeros, per the rule the blocks above
    // and below keep re-deriving: a census that appears only when it is
    // non-zero cannot be told apart from a census whose report never ran.
    // `audit_spans=0` is itself the reading that says the audit lever was off.
    {
        let (jumps, bytes_jumped, refused) = crate::g1::block_offset_process_census();
        let (tail_breaks, no_entry) = crate::g1::block_offset_no_jump_census();
        let (audit_spans, audit_objects, audit_violations) = crate::g1::block_offset_audit_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 block-offset: jumps={jumps} bytes_jumped={bytes_jumped} \
             entries_refused={refused} tail_breaks={tail_breaks} no_entry={no_entry} \
             audit_spans={audit_spans} audit_objects={audit_objects} \
             audit_violations={audit_violations}"
        ));
    }
    // LANE D (2026-09-20) — the parallel evacuator's PER-WORKER engagement
    // split. Here for exactly the reason the `promo_dest` block above is:
    // this is the census emitted on both exit arms, and the workloads whose
    // parallelism is in question all end in `System.exit`.
    //
    // Unconditional, including the "no parallel evacuation ran" form, because
    // the question this answers — "did the 23 workers this pause woke do any
    // of the work?" — has a wrong answer that looks exactly like a missing
    // report. See `g1::EvacWorkerCensus`.
    {
        s.push('\n');
        s.push_str(&crate::g1::g1_evac_worker_census_report());
    }
    // LANE W6-P (2026-09-21) — THE MIXED-PAUSE DENOMINATOR, one line.
    //
    // Unconditional, and printed as `0 0` when no mixed pause ran, because
    // THAT is the reading. Every A/B this lever has been given compared two
    // whole runs without establishing that either took a mixed pause, and on
    // the current battery neither does: zero mixed pauses in up to 8,945 young
    // ones across eight `G1OldBurstProbe` configurations. A run that prints
    // `mixed-route: serial=0 parallel=0` has measured nothing about
    // `CRATONVM_G1_PARALLEL_MIXED`, whatever its wall clock says.
    {
        // Leading newline: the per-worker census report above does not end in
        // one, so without this the two lines are emitted concatenated
        // (`idle_ms=330[GC] g1 mixed-route: ...`). Observed on a real run.
        let (serial, parallel) = crate::g1::g1_mixed_route_counts();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mixed-route: serial={serial} parallel={parallel} total={}\n",
            serial + parallel
        ));
    }
    // LANE W7-W (2026-09-21) — HOW CLOSE THE EVACUATOR'S LOAD-BALANCING VALVE
    // CAME TO OPENING, which `spills` alone cannot say.
    //
    // `EVAC_LOCAL_PUBLISH_HIGH = 256` is compared against a worker's local
    // stack depth. `spills=0` is reported by a workload whose stack never
    // exceeded 4 AND by one that reached 255 on every push — opposite
    // findings about whether work sharing is reachable on real graphs, with
    // identical output. This block prints the distribution behind the count.
    //
    // Unconditional including the disarmed form, for the reason the block
    // above gives for printing `0 0`: a line that appears only when it has
    // something to say cannot be told apart from a report that never ran.
    // See `g1::EvacShareCensus` and
    // `docs/internal/g1-2026-09-20/w7w-the-256-line-is-a-stack-depth-not-a-child-count.md`.
    {
        s.push_str(&crate::g1::g1_evac_share_census_report());
        s.push('\n');
    }
    // LANE W3-C (2026-09-21) — THE MARKING CYCLE'S OWN NUMBERS.
    //
    // Everything in this block was already being counted and none of it had a
    // reader on any path a shipped binary takes. That is the other half of
    // P7's failure — P7 is a correct report on an arm the process does not
    // reach; these were correct counters with no report at all — and it ends
    // the same way: a question that cannot be answered on a release build.
    //
    // HERE rather than in `G1Collector::print_gc_summary` for a reason that is
    // now about the DATA rather than about reachability. `print_gc_summary` is
    // reachable on both arms (`vm-cli`'s `VM_FOR_SHUTDOWN` weak handle), so
    // that is no longer the discriminator. The discriminator is `&self`: every
    // counter below is a process static, so it is readable without a live
    // collector and survives a VM that has already been torn down, whereas
    // `print_gc_summary` can only run while one is alive. A process static
    // belongs on the report that needs no VM. See
    // `w3c-print-gc-summary-and-the-system-exit-arm.md`.
    {
        // The mark cycle's two stop-the-world pauses, NEITHER of which was
        // timed before this line existed: `remark` and `cleanup` do not go
        // through `record_collection_with_phases`, so they are absent from
        // `[GC-STAT]` and from the pause percentiles.
        //
        // `cleanup_pauses=0` is the engagement reading and it is the first
        // thing to check in any marking A/B: on the default build the JIT
        // never drives the cycle (`CRATONVM_G1_JIT_MARK_DRIVER` is opt-in), so
        // a JIT'd workload runs NO mark cycles and every marking-lane flag in
        // this round is inert on it whatever its own counter says.
        let (remark_n, remark_us, cleanup_n, cleanup_us) = crate::g1::mark_pause_totals();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-pauses: remark_pauses={remark_n} remark_us={remark_us} \
             cleanup_pauses={cleanup_n} cleanup_us={cleanup_us} jit_mark_driver={}",
            cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_JIT_MARK_DRIVER"),
        ));
        // LANE W7-D (2026-09-21) — WHICH DOOR, and what it saw.
        //
        // The line above is the outcome; this block is the cause. A default
        // build reads `remark_pauses=1, cleanup_pauses=0` — one cycle opened,
        // none closed — and that single reading is produced by two opposite
        // defects: no driver ran on the path the pauses took, or a driver ran
        // and found the marker unfinished every time. `visits` against
        // `waiting` separates them, per driver, and the rows are printed
        // including the zeroes because a door that was never reached IS the
        // finding (README §5 rule 8: a check that cannot see anything and a
        // check that saw nothing must not read alike).
        //
        // Also the engagement denominator for `CRATONVM_G1_ALLOC_MARK_DRIVE`:
        // an arm on which `door=alloc_fail visits=0` is an arm on which that
        // flag could not have acted, whatever the rest of the run says.
        {
            s.push('\n');
            s.push_str(&crate::g1::mark_door_report());
        }
        // LANE W7-D — WHICH DOOR TOOK THE PAUSE, beside which door drove the
        // cycle. The two questions are different and the answer to the second
        // is only interpretable against the first.
        //
        // `gc_entry_census` has carried this since the ZGC wedge work and has
        // been printed on the ZGC arm only, so on G1 — the collector whose
        // mark-cycle drivers hang off specific entry points — the one census
        // that says which entry point runs had no reader at all. It is what
        // turns "the allocation-failure pause had no lifecycle call" from a
        // reading of the source into a measurement: `forced by
        // alloc-object-shared: N` against `maybe_gc_needs=M` is the ratio the
        // whole default decision rests on.
        {
            let (needs, requested, forced, native) = cratonvm_types::gc_entry_census::totals();
            s.push('\n');
            s.push_str(&format!(
                "[GC] g1 gc-entry: maybe_gc_needs={needs} maybe_gc_requested={requested} \
                 forced={forced} from_native={native}"
            ));
            for (site, n) in cratonvm_types::gc_entry_census::forced_sites() {
                s.push('\n');
                s.push_str(&format!("[GC] g1 gc-entry:   forced by {site}: {n}"));
            }
        }
        // The SATB per-thread registry walk, inside both of those pauses.
        // `threads_visited` is the lock acquisitions the walk pays;
        // `threads_held` is how many of them found anything. The gap between
        // the two is the entire value of a dirty-buffer list, which is the
        // first of the three fixes
        // `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md` refused
        // to choose between without this number.
        let (wc, tv, th, ov, oh, we, wn) = crate::satb::registry_walk_census();
        // `barrier_logs` is on the SAME line as `entries` on purpose. A zero
        // `entries` means "no mutator held an unflushed bucket at a cycle
        // boundary" only if the barrier fired at all; without the denominator
        // beside it the same zero also reads as "SATB never ran", which would
        // be a correctness event rather than a cost finding. That is §7.2 of
        // `orchestrator-wave-1-measurements.md` — a probe that did not perform
        // the operation it studied — and it is one number away.
        let (blogs, bspills) = crate::satb::barrier_log_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 satb-walk: calls={wc} threads_visited={tv} threads_held={th} \
             orphans_visited={ov} orphans_held={oh} entries={we} total_us={} \
             us_per_call={:.1} barrier_logs={blogs} barrier_spills={bspills} discard={}",
            wn / 1_000,
            if wc == 0 {
                0.0
            } else {
                wn as f64 / 1_000.0 / wc as f64
            },
            crate::gc_flags().g1_cleanup_satb_discard,
        ));
        // The evacuation-failure drain's repeated whole-heap fix-up.
        //
        // Wave 2 added these four because each drain pass OVERWROTE the
        // previous pass's `phases.fixup_*`, so a pause that spent its time in
        // eight whole-heap walks reported the cost of the last one — and then
        // nothing read them, so the eight-walk claim STILL could not be checked
        // on a shipped binary. `walks` above `pauses_that_drained` is the
        // repetition factor the page is about.
        let (df_walks, df_regions, df_bytes, df_us) = crate::g1::drain_fixup_totals();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 drain-fixup: walks={df_walks} regions={df_regions} bytes={df_bytes} \
             us={df_us} narrow={}",
            cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_NARROW_DRAIN_FIXUP"),
        ));
        // Two more guard counters that had no reader anywhere.
        //
        // `region_type_decode_failsafe` is documented "expected to be ZERO, and
        // structurally unreachable" and is a counter rather than a
        // `debug_assert!` precisely so it can be read instead of aborting a GC
        // worker — but nothing read it, so the argument for preferring a
        // counter to an assertion was not being honoured. `resurrect_*` is the
        // denominator its own doc calls "the question the collector could not
        // answer about itself": whether Phase 3.5 runs on this workload at all.
        let (res_runs, res_copied) = crate::g1::resurrect_phase_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-residue: resurrect_runs={res_runs} resurrect_copied={res_copied} \
             region_type_decode_failsafes={}",
            crate::region::region_type_decode_failsafes(),
        ));
        // The class-loader registry population, which is what decides whether
        // `CRATONVM_G1_MARK_SIDE_TABLES` has anything to remove on this
        // workload. `cycles_with_loader_pins=0` falsifies any A/B over that
        // flag before its wall clock is even read. See
        // `crate::g1::MARK_CYCLES_STARTED`.
        let (mc_started, mc_pins, mc_hits) = crate::g1::mark_loader_pin_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 mark-loader-pins: cycles_started={mc_started} \
             cycles_with_loader_pins={mc_pins} loader_tail_hits={mc_hits} side_tables={}",
            crate::gc_flags().g1_mark_side_tables,
        ));
        // The IHOP policy's ENGAGEMENT, as opposed to its state (which
        // `print_gc_summary`'s `[GC] g1 ihop:` line carries).
        //
        // `polls` is how many times the mark-cycle trigger consulted the
        // policy at all. Until this wave the answer was ZERO on every
        // production run: `VmHeap::g1_should_start_marking` re-implemented the
        // level test inline and never called `check_ihop`, so the back-off
        // behind `CRATONVM_G1_IHOP_BACKOFF` was unreachable. The lever is now
        // connected, and this number is what says so — a reader must be able
        // to tell "armed and never fired" from "never asked".
        let (polls, untimed) = crate::g1::ihop_poll_census();
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 ihop-engagement: polls={polls} untimed_cycles={untimed} \
             adaptive={} gross_growth={} backoff={} backoff_deadline={}",
            crate::gc_flags().g1_adaptive_ihop,
            crate::gc_flags().g1_ihop_gross_growth,
            crate::gc_flags().g1_ihop_backoff,
            // LANE W5-C — the back-off's counted fail-safe. On this line for
            // the reason its three neighbours are: this is the line a reader
            // consults to find out whether a marking lever could have run at
            // all, and a lever missing from it is one whose zero cannot be
            // told from an absence.
            crate::gc_flags().g1_mark_backoff_deadline,
        ));
    }
    // THE 2026-09-08 EVACUATION SCREENS, printed UNCONDITIONALLY so a zero is
    // citable evidence rather than a silence.
    //
    // Every one of these is expected to be ZERO on a healthy run, and each
    // names a distinct door into
    // `g1-eight-byte-write-at-a-live-objects-base-20260906`'s two families:
    //
    //  * `fwd_target_implausible` -- a `MARK_FORWARDED` word decoded to a
    //    target `make_forwarded` could not have installed (`forwarding_target`
    //    strips the low TWO bits, the install asserts the low THREE), so a
    //    non-address was about to be stored into a live reference slot and
    //    pushed onto the gray worklist;
    //  * `supply_non_object` -- an evacuation supply address that is not an
    //    object start;
    //  * `candidate_arena_ptr` -- a candidate whose `class_id`/`shape` pair
    //    recombines to a pointer into this arena, i.e. a body word read at an
    //    address that is not an object start;
    //  * `empty_header_refused` / `empty_header_proved` -- the shape no header
    //    screen can reject (a null `Value` cell's payload word reads as a
    //    zero-field class-0 object), and how often the grid had to be walked to
    //    say so. A large `proved` beside a zero `refused` means the shape is
    //    common and genuine and the proof is paying for nothing;
    //  * `tagged_ref_writes` -- a reference-slot write whose value is not
    //    8-aligned, i.e. a mark word stored where an object address belongs.
    {
        use std::sync::atomic::Ordering as O;
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 evac-screens: fwd_target_implausible={} supply_non_object={} \
             candidate_arena_ptr={} empty_header_refused={} empty_header_proved={} \
             empty_header_waived={} tagged_ref_writes={}",
            crate::g1::FORWARD_TARGET_IMPLAUSIBLE.load(O::Relaxed),
            crate::g1::EVAC_SUPPLY_NON_OBJECT.load(O::Relaxed),
            crate::g1::EVAC_REF_REJECTED_ARENA.load(O::Relaxed),
            crate::g1::EVAC_EMPTY_HEADER_REFUSED.load(O::Relaxed),
            crate::g1::EVAC_EMPTY_HEADER_PROVED.load(O::Relaxed),
            crate::g1::EVAC_EMPTY_HEADER_WAIVED.load(O::Relaxed),
            crate::g1::REF_WRITE_TAGGED_VALUE.load(O::Relaxed),
        ));
    }
    // LANE W6-A — the receiver-side dead-reference screen's POPULATION.
    //
    // `note_deadref_recv` counts a field access whose RECEIVER names no live
    // object: a native that captured a receiver before an allocation and used
    // it afterwards writes into a dead address, and the field it meant to set
    // stays at its JVM default. It is a correctness event and it is expected
    // to be ZERO.
    //
    // It already prints — the first TWENTY-FOUR occurrences, by `eprintln!`,
    // and then nothing. Its own write site says why the counter exists beside
    // that printout: *"Counted before the rate limit, so the number is the
    // population and not the printout … its headline claim — that a
    // receiver-side store is now screened — is exactly the kind of claim a
    // silent instrument can appear to support while being unwired."* The
    // counter was then read by one test and nothing else, so the population
    // was unpublished and a run with 24 reports and a run with 24 000 were
    // indistinguishable. That is rule 3 of this round's method, on a
    // correctness counter.
    //
    // `screen=` beside it is not decoration. The screen is entirely behind
    // `CRATONVM_DBG_DEADREF_STORE` (`gc_flags().dbg_deadref_store` guards all
    // five call sites), so on a default build this number is zero BY
    // CONSTRUCTION. Without the resolved value of the gate, "armed and clean"
    // and "never asked" are the same zero — §7.1 of
    // `orchestrator-wave-1-measurements.md`, which is the failure that
    // retracted a whole measurement this round.
    //
    // Unconditional, including the zero, for the reason every neighbour above
    // is: a counter that appears only when it is non-zero cannot be told apart
    // from a report that never ran.
    {
        s.push('\n');
        s.push_str(&format!(
            "[GC] deadref-recv: DEADREF_RECEIVER_HITS={} screen={}",
            crate::gen_heap::deadref_recv_hits(),
            crate::gc_flags().dbg_deadref_store,
        ));
    }
    // I-6 coverage. Printed whenever the verifier ran at all, including the
    // budgeted release pass, because the interesting reading is `objects`: a
    // zero `dangling` means nothing without the number of objects it is a
    // statement about. `budget_truncated` says how often the budget, rather
    // than the end of the heap, is what ended the pass.
    let verify = gc_metrics_raw();
    if verify.cset_verify_pauses > 0 {
        s.push('\n');
        s.push_str(&format!(
            // LANE W4-B — `bytes`, `unbounded_pauses` and the two switches.
            //
            // W3-C's rule ("every census line names the switch that governs
            // it") applies with unusual force here, because this line has TWO
            // governing switches and one of them is the flag people use to
            // make the line appear. `unbounded_pauses` is the count of pauses
            // that had no budget at all; when it equals `pauses`, every other
            // number on the line describes a whole-heap sweep and
            // `budget_truncated=0` is a tautology rather than a result.
            //
            // `bytes` is the coverage denominator's numerator: paired with the
            // occupied heap it is the sweep RATE, which is the quantity the
            // sampler's whole claim rests on and which nothing published
            // before. See `g1::verify_sweep_pauses` for the rates measured at
            // three heap sizes and for why a flat object budget makes that
            // rate fall linearly with the live set.
            "[GC] g1 cset-verify: pauses={} objects={} bytes={} dangling={} budget_truncated={} unbounded_pauses={} (objects/pause={:.1} bytes/pause={:.0}) budget={} sweep_pauses={}",
            verify.cset_verify_pauses,
            verify.cset_verify_objects,
            verify.cset_verify_bytes,
            verify.cset_verify_dangling,
            verify.cset_verify_truncated,
            verify.cset_verify_unbounded,
            verify.cset_verify_objects as f64 / verify.cset_verify_pauses as f64,
            verify.cset_verify_bytes as f64 / verify.cset_verify_pauses as f64,
            crate::g1::verify_budget_for_report(),
            crate::g1::verify_sweep_pauses_for_report(),
        ));
    }
    if verify.humongous_eager_spans > 0 || verify.humongous_eager_declined > 0 {
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 humongous-eager: spans={} bytes={} declined_pauses={} \
             marked_live={} root_seeded={} walked_sources={}{}",
            verify.humongous_eager_spans,
            verify.humongous_eager_bytes,
            verify.humongous_eager_declined,
            verify.humongous_eager_marked,
            verify.humongous_eager_root_seeded,
            verify.humongous_eager_walked_sources,
            {
                let reasons = g1_eager_decline_counts();
                if reasons.is_empty() {
                    String::new()
                } else {
                    let body: Vec<String> = reasons
                        .iter()
                        .map(|(label, n)| format!("{label}={n}"))
                        .collect();
                    format!(" declined_by: {}", body.join(" "))
                }
            },
        ));
    }
    s
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
    const fn new() -> Self {
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

#[cfg(not(test))]
static G1_CYCLE: G1CycleSlot = G1CycleSlot::new();

#[cfg(not(test))]
#[inline]
fn with_g1_cycle<R>(f: impl FnOnce(&G1CycleSlot) -> R) -> R {
    f(&G1_CYCLE)
}

#[cfg(test)]
thread_local! {
    static G1_CYCLE: G1CycleSlot = const { G1CycleSlot::new() };
}

#[cfg(test)]
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
pub(crate) fn g1_eager_decline_counts() -> Vec<(&'static str, u64)> {
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
// docs/internal/retired/r10-diagread-orphan-instrument-debt-register-20260921-RETIRED-20260922.md.)

/// The non-zero rows of the per-reason census, as `(label, count)`.
pub(crate) fn g1_coverage_reason_counts() -> Vec<(&'static str, u64)> {
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
pub(crate) fn g1_empty_jit_publication_count() -> u64 {
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc_quiescence::incomplete_reason;

    #[test]
    fn normalization_is_a_pure_function_of_the_raw_counters() {
        let raw = GcMetricsRaw {
            card_marks_executed: 400,
            barrier_ref_stores: 1_000,
            cards_found_dirty: 50,
            // The legacy total is the sum of the two halves below; `from_raw`
            // is pure, so this literal states the relationship that
            // `gc_metrics_raw` maintains rather than deriving it.
            duplicate_card_marks: 150,
            duplicate_card_marks_buffered: 50,
            duplicate_card_marks_barrier: 100,
            remembered_set_bytes: 2_048,
            old_to_young_edges: 200,
            // The CSet-verify counters are pure observability: they take no
            // part in any normalization below, so this literal pins them at
            // zero to say so rather than to exercise them.
            cset_verify_objects: 0,
            cset_verify_pauses: 0,
            cset_verify_dangling: 0,
            cset_verify_truncated: 0,
            cset_verify_bytes: 0,
            cset_verify_unbounded: 0,
            rset_coarsened: 0,
            humongous_eager_marked: 0,
            humongous_eager_root_seeded: 0,
            humongous_eager_walked_sources: 0,
            humongous_eager_spans: 0,
            humongous_eager_bytes: 0,
            humongous_eager_declined: 0,
            refinement_nanos: 4_000_000,
            refinement_passes: 4,
            allocated_objects: 1_000,
            allocated_bytes: 64_000,
            live_bytes: 8_192,
        };
        let r = GcMetricsReport::from_raw(raw, true);

        assert_eq!(r.card_marks_per_allocated_object, 0.4);
        assert_eq!(r.dirty_cards_per_allocated_object, 0.05);
        assert_eq!(r.old_to_young_edges_per_allocated_object, 0.2);
        assert_eq!(r.remembered_set_bytes_per_live_byte, 0.25);
        assert_eq!(r.refinement_nanos_per_live_byte, 4_000_000.0 / 8_192.0);
        assert_eq!(r.old_to_young_edges_per_live_byte, 200.0 / 8_192.0);
        // 200 edges across 50 dirty cards.
        assert_eq!(r.old_to_young_edge_density, 4.0);
        // The frozen legacy figure: 150 / (150 + 50). Kept bit-for-bit across
        // the 2026-09-21 numerator split precisely so no embedder's number
        // changes meaning under them; it is superseded, not redefined.
        assert_eq!(r.duplicate_mark_ratio, 0.75);
        // The honest one: 100 barrier duplicates out of 400 barrier marks.
        // Same barrier, same gate, so this one is a fraction.
        assert_eq!(r.barrier_duplicate_mark_ratio, 0.25);
        assert_eq!(r.barrier_hit_rate, 0.4);
        assert_eq!(r.refinement_nanos_per_pass, 1_000_000.0);
    }

    /// A report taken before anything has been collected must read as "no cost
    /// observed", never `NaN` — a `NaN` in a summary line is indistinguishable
    /// from a parse bug for whoever reads the log.
    #[test]
    fn every_ratio_is_zero_when_its_denominator_is_zero() {
        let r = GcMetricsReport::from_raw(GcMetricsRaw::default(), false);
        for v in [
            r.card_marks_per_allocated_object,
            r.dirty_cards_per_allocated_object,
            r.old_to_young_edges_per_allocated_object,
            r.remembered_set_bytes_per_live_byte,
            r.refinement_nanos_per_live_byte,
            r.old_to_young_edges_per_live_byte,
            r.old_to_young_edge_density,
            r.duplicate_mark_ratio,
            r.barrier_duplicate_mark_ratio,
            r.barrier_hit_rate,
            r.refinement_nanos_per_pass,
        ] {
            assert_eq!(v, 0.0, "a zero denominator must normalize to 0.0, not NaN");
            assert!(v.is_finite());
        }
    }

    #[test]
    fn collector_side_counters_accumulate_without_the_hot_path_gate() {
        reset_metrics_for_test();
        set_hot_path_counters_enabled(false);

        record_cards_found_dirty(7);
        record_duplicate_card_marks(3);
        record_old_to_young_edges(21);
        record_refinement(1_500);
        record_refinement(2_500);
        record_remembered_set_bytes(4_096);
        record_remembered_set_bytes(8_192); // gauge: last value wins
        record_heap_occupancy(100, 3_200, 1_024);

        let raw = gc_metrics_raw();
        assert_eq!(raw.cards_found_dirty, 7);
        assert_eq!(raw.duplicate_card_marks_buffered, 3);
        assert_eq!(
            raw.duplicate_card_marks, 3,
            "the legacy total reads the same as before the split"
        );
        assert_eq!(raw.old_to_young_edges, 21);
        assert_eq!(raw.refinement_nanos, 4_000);
        assert_eq!(raw.refinement_passes, 2);
        assert_eq!(raw.remembered_set_bytes, 8_192, "rset bytes is a gauge");
        assert_eq!(raw.allocated_objects, 100);
        assert_eq!(raw.live_bytes, 1_024);

        // The gated counters stayed untouched, and the report says so rather
        // than reporting a measured zero.
        assert_eq!(raw.card_marks_executed, 0);
        let report = gc_metrics_report();
        assert!(!report.hot_path_counters_enabled);
        assert_eq!(report.old_to_young_edge_density, 3.0);
        assert!(report.to_string().contains("NOT ARMED"));
    }

    /// `reset_metrics_for_test` must clear EVERY counter, not the eleven of
    /// twenty-two it used to.
    ///
    /// A partial reset is the worst shape a reset can have: the caller reads it
    /// as a clean slate, so the fields it silently skipped turn every later
    /// assertion on them into an assertion about which other test in this
    /// binary ran first. Written as "set everything, reset, expect
    /// `GcMetricsRaw::default()`" so a counter added to `Counters` without a
    /// matching line in the reset fails here rather than somewhere unrelated.
    #[test]
    fn reset_clears_every_counter_not_just_the_card_ones() {
        reset_metrics_for_test();
        set_hot_path_counters_enabled(true);

        record_barrier_ref_store();
        record_card_mark();
        record_cards_found_dirty(3);
        record_duplicate_card_marks(4);
        // gengc-round3: the barrier half is a SEPARATE counter and needs its
        // own line here. This is the field the gap page warned keeps drifting.
        record_duplicate_card_mark_barrier();
        record_old_to_young_edges(5);
        record_remembered_set_bytes(6);
        record_refinement(7);
        record_heap_occupancy(8, 9, 10);
        // The 2026-09-20 G1 round widened this to carry BYTES and the
        // unbounded-pass flag, so the fixture has to move those two as
        // well: a counter the fixture never touches cannot show that
        // `reset` clears it.
        record_g1_cset_verify(11, 1112, 12, true, true);
        record_g1_rset_coarsened();
        record_g1_humongous_liveness(13, 14, 15);
        record_g1_eager_humongous(16, 17, true);

        let dirty = gc_metrics_raw();
        assert_ne!(
            dirty,
            GcMetricsRaw::default(),
            "the fixture must actually move the counters"
        );
        // Each of the eleven the old reset skipped, plus the two halves of the
        // 2026-09-21 duplicate-mark split, individually — so a regression names
        // the field rather than just the aggregate. (These two are here because
        // the fixture must be shown to MOVE them: a counter the fixture never
        // touches would pass the `== default()` check below while having no
        // reset line at all.)
        for (name, v) in [
            (
                "duplicate_card_marks_buffered",
                dirty.duplicate_card_marks_buffered,
            ),
            (
                "duplicate_card_marks_barrier",
                dirty.duplicate_card_marks_barrier,
            ),
            ("cset_verify_objects", dirty.cset_verify_objects),
            ("cset_verify_pauses", dirty.cset_verify_pauses),
            ("cset_verify_dangling", dirty.cset_verify_dangling),
            ("cset_verify_truncated", dirty.cset_verify_truncated),
            ("rset_coarsened", dirty.rset_coarsened),
            ("humongous_eager_marked", dirty.humongous_eager_marked),
            (
                "humongous_eager_root_seeded",
                dirty.humongous_eager_root_seeded,
            ),
            (
                "humongous_eager_walked_sources",
                dirty.humongous_eager_walked_sources,
            ),
            ("humongous_eager_spans", dirty.humongous_eager_spans),
            ("humongous_eager_bytes", dirty.humongous_eager_bytes),
            ("humongous_eager_declined", dirty.humongous_eager_declined),
        ] {
            assert_ne!(v, 0, "fixture did not move {name}");
        }

        reset_metrics_for_test();
        assert_eq!(
            gc_metrics_raw(),
            GcMetricsRaw::default(),
            "reset_metrics_for_test must clear EVERY field of Counters; a field \
             it skips makes a later assertion on that field an assertion about \
             test ordering",
        );

        set_hot_path_counters_enabled(false);
    }

    /// Every counter something increments must appear in the rendered report.
    ///
    /// `rset_coarsened` and `allocated_bytes` were both written by production
    /// code (`region.rs`'s coarsening path and `record_heap_occupancy`) and
    /// read by nothing: no ratio, no rendered field, and no in-tree caller of
    /// `gc_metrics_raw()` outside this module. A counter in that state is
    /// indistinguishable from a counter that is never published, which is the
    /// exact failure this module's header says it exists to prevent.
    #[test]
    fn the_report_renders_the_counters_nothing_else_reads() {
        reset_metrics_for_test();
        // Pinned explicitly: the assertions below distinguish the armed
        // barrier line from the NOT-ARMED one, and under `--test-threads=1`
        // libtest runs cases on one thread, where this thread-local gate
        // would otherwise carry whatever the previous test left.
        set_hot_path_counters_enabled(false);
        record_heap_occupancy(7, 4_242, 99);
        record_g1_rset_coarsened();
        record_g1_rset_coarsened();

        let text = gc_metrics_report().to_string();
        assert!(
            text.contains("allocated_bytes=4242"),
            "allocated_bytes is a published gauge and must be rendered: {text}",
        );
        assert!(
            text.contains("rset_coarsened=2"),
            "rset_coarsened is bumped by region.rs and must be rendered: {text}",
        );
        // And the renamed duplicate key: the old `duplicate_ratio=` named a
        // quantity this number is not (see `duplicate_mark_ratio`).
        assert!(
            text.contains("dup_over_dup_plus_scanned="),
            "the duplicate figure must be keyed by its arithmetic, not by the \
             buffered-path fraction it is not: {text}",
        );
        assert!(
            !text.contains("duplicate_ratio="),
            "the misleading key must be gone: {text}",
        );
        // gengc-round3: the buffered half is rendered unconditionally (it is
        // ungated), and it is what tells a reader whether the dead buffered
        // pipeline has a producer again.
        assert!(
            text.contains("dup_buffered=0"),
            "the buffered duplicate half must be rendered: {text}",
        );
        // The barrier half and its ratio are rendered ONLY on the armed line,
        // because both terms are gated. Unarmed, the report must say so rather
        // than print a measured-looking zero.
        assert!(
            !text.contains("dup_barrier="),
            "the gated barrier half must not be rendered while the gate is \
             off — that is the 'unmeasured is not zero' rule: {text}",
        );
        assert!(
            text.contains("dup_barrier are unmeasured, not zero"),
            "and the unarmed line must name it: {text}",
        );

        reset_metrics_for_test();
    }

    /// gengc-round3 — the numerator and its denominator must be armed
    /// together, and the legacy total must keep its old value.
    ///
    /// `duplicate_card_marks` used to be one counter fed by two sites on two
    /// clocks: `drain_pending` (ungated, per drain) and the write barrier's
    /// already-dirty arm (gated, per store). No denominator divides that sum,
    /// which is why `duplicate_mark_ratio` could not be repaired by changing
    /// its divisor — see
    /// `docs/internal/gaps/gengc-plumbing-duplicate-card-mark-denominator-20260920.md`.
    ///
    /// Three things are asserted here, in the order they would break:
    ///
    /// 1. the barrier counter disarms with `card_marks_executed`, so the ratio
    ///    never has one live term and one dead one;
    /// 2. the buffered counter does **not** disarm, because it is per-drain;
    /// 3. `duplicate_card_marks` still reads as the sum, so an embedder that
    ///    was reading it sees no change.
    #[test]
    fn the_duplicate_numerator_and_its_denominator_arm_together() {
        reset_metrics_for_test();

        // Disarmed: the barrier half and its denominator both stay put; the
        // buffered half still accumulates.
        set_hot_path_counters_enabled(false);
        record_card_mark();
        record_duplicate_card_mark_barrier();
        record_duplicate_card_marks(2);
        let raw = gc_metrics_raw();
        assert_eq!(raw.card_marks_executed, 0, "denominator is gated");
        assert_eq!(raw.duplicate_card_marks_barrier, 0, "numerator is gated");
        assert_eq!(
            raw.duplicate_card_marks_buffered, 2,
            "the buffered half is per-drain and must NOT be gated",
        );

        // Armed: four barrier marks, one of which was a duplicate.
        set_hot_path_counters_enabled(true);
        for _ in 0..4 {
            record_card_mark();
        }
        record_duplicate_card_mark_barrier();

        let raw = gc_metrics_raw();
        assert_eq!(raw.card_marks_executed, 4);
        assert_eq!(raw.duplicate_card_marks_barrier, 1);
        assert!(
            raw.duplicate_card_marks_barrier <= raw.card_marks_executed,
            "holds by construction: `write_barrier` records a card mark on \
             both arms of the branch whose else-arm records the duplicate",
        );
        assert_eq!(
            raw.duplicate_card_marks, 3,
            "the legacy field is the sum of the two halves and nothing else",
        );

        let report = GcMetricsReport::from_raw(raw, true);
        assert_eq!(report.barrier_duplicate_mark_ratio, 0.25);
        let text = report.to_string();
        assert!(
            text.contains("dup_barrier=1") && text.contains("dup_barrier_over_card_marks=0.2500"),
            "the armed line renders the pair and spells its arithmetic: {text}",
        );
        assert!(
            text.contains("duplicate_marks=3") && text.contains("dup_buffered=2"),
            "the first line renders the total and the buffered half: {text}",
        );

        set_hot_path_counters_enabled(false);
        reset_metrics_for_test();
    }

    /// A COMPLETE G1 pause must not attribute itself to
    /// `incomplete_reason::NONE`.
    ///
    /// `NONE`'s label is `"none"`, so an `incomplete.then_some(NONE)` would
    /// render `incomplete=N ... reasons: none=N` — "the root set was
    /// incomplete, for no reason".
    ///
    /// This used to drive the boolean-only recorder
    /// (`record_g1_pause_coverage`), which round 10 deleted for having no caller
    /// anywhere in the tree. The property is the reason form's now, and the
    /// reason form is the one that ships.
    ///
    /// # Why the totals are `>=` and the reason row is `==`
    ///
    /// The asymmetry is deliberate and is what keeps this test off the flaky
    /// list. `G1_PAUSES` / `G1_PAUSES_COVERAGE_INCOMPLETE` are process-global
    /// statics that `g1.rs` ALSO feeds from every real collection, and this
    /// binary runs its tests in parallel — so a concurrent pause can raise
    /// either total between the two reads. They are monotone
    /// (`reset_metrics_for_test` does not touch them), so `>=` is the strongest
    /// claim that is true, and it still fails if this call stops counting.
    ///
    /// Row 0 is different: the ONLY way a concurrent collection can raise it is
    /// by passing `Some(incomplete_reason::NONE)`, which is precisely the defect
    /// this test exists to catch. So `==` there is not a race — it is a second
    /// chance to catch the same bug, from another thread.
    #[test]
    fn a_complete_pause_attributes_no_reason() {
        let none_before = g1_coverage_reason_counts()
            .into_iter()
            .find(|(label, _)| *label == crate::gc_quiescence::incomplete_reason::label(0))
            .map(|(_, n)| n)
            .unwrap_or(0);
        let (pauses_before, incomplete_before) = g1_pause_coverage_counts();

        record_g1_pause_coverage_reason(None);
        record_g1_pause_coverage_reason(Some(1));

        let (pauses_after, incomplete_after) = g1_pause_coverage_counts();
        assert!(
            pauses_after >= pauses_before + 2,
            "both pauses are counted: before={pauses_before} after={pauses_after}"
        );
        assert!(
            incomplete_after >= incomplete_before + 1,
            "and at least one of them as incomplete: \
             before={incomplete_before} after={incomplete_after}"
        );

        let none_after = g1_coverage_reason_counts()
            .into_iter()
            .find(|(label, _)| *label == crate::gc_quiescence::incomplete_reason::label(0))
            .map(|(_, n)| n)
            .unwrap_or(0);
        assert_eq!(
            none_after, none_before,
            "a complete pause must leave the per-reason census untouched — a \
             `none=N` row reads as a reason, and `none` is not one",
        );
    }

    #[test]
    fn barrier_counters_record_only_while_armed() {
        reset_metrics_for_test();

        set_hot_path_counters_enabled(false);
        for _ in 0..10 {
            record_barrier_ref_store();
            record_card_mark();
        }
        assert_eq!(gc_metrics_raw().card_marks_executed, 0);
        assert_eq!(gc_metrics_raw().barrier_ref_stores, 0);

        set_hot_path_counters_enabled(true);
        for _ in 0..10 {
            record_barrier_ref_store();
        }
        for _ in 0..4 {
            record_card_mark();
        }
        let raw = gc_metrics_raw();
        assert_eq!(raw.barrier_ref_stores, 10);
        assert_eq!(raw.card_marks_executed, 4);
        assert_eq!(
            GcMetricsReport::from_raw(raw, true).barrier_hit_rate,
            0.4,
            "4 of 10 reference stores crossed a generation"
        );

        set_hot_path_counters_enabled(false);
    }

    #[test]
    fn every_decision_reason_has_a_label_and_a_moving_verdict() {
        for code in decision_reason::UNRECORDED..decision_reason::COUNT {
            assert_ne!(
                decision_reason::label(code),
                "unknown",
                "decision reason {code} needs a label — the report prints it verbatim",
            );
            // `is_moving` must be total: every defined code answers.
            let moving = decision_reason::is_moving(code);
            assert_eq!(
                moving,
                decision_reason::label(code).starts_with("moving-"),
                "the label prefix and `is_moving` must agree for code {code}",
            );
            // 2026-09-21: and the same for the third arm. A `skipped-*` code
            // that answered `is_moving` would be counted as a relocating
            // cycle; one that answered neither predicate would be silently
            // folded into `non_moving`, which is the claim that the sweep ran.
            let skipped = decision_reason::is_skipped(code);
            assert_eq!(
                skipped,
                decision_reason::label(code).starts_with("skipped-"),
                "the label prefix and `is_skipped` must agree for code {code}",
            );
            assert!(
                !(skipped && moving),
                "a refused cycle relocated nothing; code {code} cannot be both",
            );
        }
        assert_eq!(
            decision_reason::label(decision_reason::COUNT),
            "unknown",
            "COUNT must be one PAST the last defined reason",
        );
    }

    /// The G1 guard counters must be in the report, and must be there when
    /// NOTHING has collected.
    ///
    /// They used to live in `G1Collector::print_gc_summary`, whose comments
    /// called them "unconditional ... before the early return" so that a zero
    /// could be cited as evidence. That function is reached only from
    /// `vm-cli`'s normal-return teardown, and `JUnitCore` -- every Tomcat, H2
    /// and Spring Boot workload in the suites -- ends in `System.exit`, which
    /// never unwinds Rust frames. So the zero was never printed once. This
    /// report is the census emitted on BOTH exit arms.
    ///
    /// Asserted on the LINE PREFIXES, not on the values: the counters behind
    /// them are process statics that every other test in this binary shares,
    /// and a test that pinned their values would be asserting test ordering.
    /// Presence is exactly the property that was broken.
    #[test]
    fn the_decision_report_carries_the_g1_guard_counters_with_no_collection() {
        // Fresh test thread — the decision record is thread-local, so this is
        // the "nothing has run" shape a real process has before its first
        // collection, and the one the old location could not report at all.
        assert!(last_collector_decision().is_none());
        let text = collector_decision_report();
        for needle in [
            "[GC] g1 evac_ref_rejected=",
            "evac_holder_rejected=",
            "evac_holder_clamped=",
            "source_walk_desync=",
            "[GC] g1 non_object_roots_skipped=",
            // LANE W4-B — the two halves that used to have no reader.
            "COPIED=",
            "REEVACUATED_AFTER_RETIRE=",
            "[GC] g1 young evacuation: parallel=",
            "workers_last=",
            "[GC] g1 implausible_legacy_headers=",
            "copy_shape_drift=",
            "[GC] g1 flat_walk_refused_array=",
            "kept_seed_rejected=",
        ] {
            assert!(
                text.contains(needle),
                "the decision report must carry `{needle}`: it is the only shutdown census emitted on BOTH exit arms. Report was: {text}"
            );
        }
    }

    #[test]
    fn decision_report_names_the_fallback_reason() {
        // Fresh test thread: nothing recorded, and the report says so rather
        // than inventing a verdict.
        assert!(last_collector_decision().is_none());
        let empty = collector_decision_report();
        // 2026-09-20: the needle used to be "no collection has run yet", which
        // is what the line USED to claim. It is a false claim on a default
        // (ZGC) run, which records no decision however much it collects, so the
        // line now reports what it knows and denies the stronger reading. The
        // old phrase still appears inside that denial, so assert on the part
        // that carries the meaning rather than on a substring that survives
        // either wording.
        assert!(
            empty.contains("no collector decision has been recorded"),
            "the empty report must say what it actually knows: {empty}"
        );
        assert!(
            empty.contains("This does NOT mean no collection has run yet"),
            "the empty report must deny the reading that ends an investigation \
             early on the default backend: {empty}"
        );

        crate::gc_quiescence::begin_moving_young_coverage_cycle();
        crate::gc_quiescence::mark_moving_young_coverage_incomplete_because(
            incomplete_reason::UNPUBLISHED_FRAME_OOP,
        );
        record_collector_decision(
            "generational",
            decision_reason::NON_MOVING_COVERAGE_INCOMPLETE,
            crate::gc_quiescence::moving_young_incomplete_reason(),
        );

        let d = last_collector_decision().expect("a decision was just recorded");
        assert_eq!(d.sequence, 1);
        assert_eq!(d.backend, "generational");
        assert!(!d.young_moving, "a coverage fallback is a NON-moving cycle");
        assert_eq!(
            d.incomplete_reason,
            incomplete_reason::UNPUBLISHED_FRAME_OOP
        );

        let text = collector_decision_report();
        assert!(text.contains("NON-MOVING"), "{text}");
        assert!(text.contains("nonmoving-coverage-incomplete"), "{text}");
        assert!(
            text.contains("compiled-frame-oop-not-published"),
            "the report must name the ROOT SOURCE that reported incomplete \
             coverage, not merely that coverage was incomplete: {text}",
        );
    }

    /// The histogram totals every recorded cycle on BOTH arms.
    ///
    /// This is now load-bearing outside this module: `VmHeap::print_gc_summary`
    /// takes `moving_cycles_total` / `non_moving_cycles_total` from here for the
    /// `[GC] moving_young:` line, because the counter that line used to print
    /// (`gc_quiescence::moving_young_cycle_count`) is bumped only inside
    /// `gen_heap`'s `if moving_young && has_conservative_roots` — i.e. only for
    /// moving cycles taken while a JIT frame was live. A run with no compiled
    /// frames that relocated on every collection reported zero.
    ///
    /// Fresh test thread: the histogram is thread-local under `cfg(test)`.
    #[test]
    fn the_decision_histogram_totals_every_recorded_cycle() {
        let (m0, n0, s0, _) = decision_histogram();
        assert_eq!((m0, n0, s0), (0, 0, 0), "fresh test thread");

        // Three moving cycles, none of them under a live JIT frame — the shape
        // `moving_young_cycle_count()` reports as zero.
        for _ in 0..3 {
            record_collector_decision(
                "generational",
                decision_reason::MOVING_NO_JIT_FRAMES,
                incomplete_reason::NONE,
            );
        }
        record_collector_decision(
            "generational",
            decision_reason::NON_MOVING_PROMOTION_OOM_RISK,
            incomplete_reason::NONE,
        );

        // And one REFUSED cycle, which is neither.
        record_collector_decision(
            "generational",
            decision_reason::SKIPPED_YOUNG_RESERVED_TLAB_TAILS,
            incomplete_reason::NONE,
        );

        let (moving, non_moving, skipped, rows) = decision_histogram();
        assert_eq!(
            (moving, non_moving, skipped),
            (3, 1, 1),
            "every recorded decision lands on exactly one of the THREE arms — a refusal counted as non-moving would claim the non-moving sweep ran, and nothing ran",
        );
        assert_eq!(
            crate::gc_quiescence::moving_young_cycle_count(),
            0,
            "…and this is the counter that cannot see them: it only counts \
             moving cycles taken under a live JIT frame",
        );
        assert!(rows
            .iter()
            .any(|(l, n)| *l == "moving-no-jit-frames-live" && *n == 3));
        assert!(rows
            .iter()
            .any(|(l, n)| *l == "nonmoving-promotion-oom-risk" && *n == 1));
        assert!(rows
            .iter()
            .any(|(l, n)| *l == "skipped-young-reserved-tlab-tails" && *n == 1));
    }

    /// The one reason code that covers two situations is separable **in the
    /// distribution**, not merely on the last-cycle line.
    ///
    /// `nonmoving-conservative-jit-roots` is recorded for two different terms
    /// of `divert_non_moving` with two different operator actions: the legacy
    /// `has_conservative_roots && !moving_young` rule (turn moving-young on)
    /// and the 2026-09-06 `unrewritable_conservative_jit_roots` term (only
    /// `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` touches it). Before 2026-09-20 the
    /// histogram showed one row and an operator reading it was pointed at a
    /// flag that was already set correctly.
    ///
    /// The discriminator is exact rather than indicative because
    /// `gen_heap::collect_garbage_inner` tests
    /// `divert_for_incomplete_moving_coverage` *first*, so by the time this
    /// reason is chosen `moving_young == moving_young_requested`.
    #[test]
    fn conservative_jit_root_diverts_are_split_by_the_moving_young_gate() {
        assert_eq!(
            conservative_jit_root_divert_split(),
            (0, 0),
            "fresh test thread"
        );

        // The legacy rule: moving-young is off, a compiled frame is live, its
        // roots were found conservatively.
        crate::gc_quiescence::publish_moving_young_enabled(false);
        record_collector_decision(
            "generational",
            decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS,
            incomplete_reason::NONE,
        );

        // The 2026-09-06 term: moving-young is ON and the cycle is diverted
        // anyway, because a conservative scan ran and a Cheney copy cannot
        // honour a pin.
        crate::gc_quiescence::publish_moving_young_enabled(true);
        for _ in 0..4 {
            record_collector_decision(
                "generational",
                decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS,
                incomplete_reason::NONE,
            );
        }

        let (off, on) = conservative_jit_root_divert_split();
        assert_eq!(
            (off, on),
            (1, 4),
            "the split must attribute each cycle to the term that actually forced it",
        );

        let (_, _, _, rows) = decision_histogram();
        let row = rows
            .iter()
            .find(|(l, _)| *l == "nonmoving-conservative-jit-roots")
            .expect("the reason is still one histogram row");
        assert_eq!(
            row.1,
            off + on,
            "the split must PARTITION the histogram row, not sample it — a row \
             and a breakdown that disagree is worse than no breakdown",
        );

        // A decision recorded under a different reason must not leak into the
        // split: this counter is keyed on the reason, not on the gate.
        record_collector_decision(
            "generational",
            decision_reason::NON_MOVING_PROMOTION_OOM_RISK,
            incomplete_reason::NONE,
        );
        assert_eq!(conservative_jit_root_divert_split(), (1, 4));

        let text = collector_decision_report();
        assert!(
            text.contains("[GC] conservative-jit-root diverts: moving_young_off=1"),
            "the breakdown must reach the rendered report: {text}"
        );
        assert!(text.contains("moving_young_on=4"), "{text}");
    }

    #[test]
    fn decision_report_distinguishes_a_proven_moving_cycle() {
        record_collector_decision(
            "generational",
            decision_reason::MOVING_WITH_PROVEN_JIT_COVERAGE,
            incomplete_reason::NONE,
        );
        let d = last_collector_decision().unwrap();
        assert!(d.young_moving);
        let text = d.to_string();
        assert!(text.contains("MOVING"), "{text}");
        assert!(
            !text.contains("unproven_obligation"),
            "a proven cycle must not print a fallback obligation: {text}",
        );
    }

    // -----------------------------------------------------------------------
    // G1 per-cycle facts
    // -----------------------------------------------------------------------

    #[test]
    fn every_g1_degraded_flag_has_a_label() {
        // Walk every bit in ALL and require a distinct, non-empty label. This
        // is what stops a new fail-safe from being added to the collector and
        // then printing as an opaque bitmask in the summary.
        let mut seen: Vec<&str> = Vec::new();
        for bit in 0..32u32 {
            let mask = 1u32 << bit;
            if g1_degraded::ALL & mask == 0 {
                continue;
            }
            let labels = g1_degraded::labels(mask);
            assert_eq!(labels.len(), 1, "bit {bit} must map to exactly one label");
            assert!(!labels[0].is_empty(), "bit {bit} has an empty label");
            assert!(
                !seen.contains(&labels[0]),
                "duplicate label {:?} for bit {bit}",
                labels[0],
            );
            seen.push(labels[0]);
        }
        assert_eq!(
            g1_degraded::labels(g1_degraded::NONE).len(),
            0,
            "an undegraded cycle names no fail-safe",
        );
        // A bit outside ALL contributes nothing rather than an "unknown" entry.
        assert!(g1_degraded::labels(1 << 31).is_empty());
    }

    #[test]
    fn g1_cycle_is_absent_until_a_g1_pause_records_one() {
        // A Generational-only run must not grow a G1 line: this is the check
        // that keeps the report honest about which backend produced it.
        assert!(last_g1_cycle().is_none());
        record_collector_decision(
            "generational",
            decision_reason::MOVING_NO_JIT_FRAMES,
            incomplete_reason::NONE,
        );
        let text = collector_decision_report();
        assert!(!text.contains("g1 cycle"), "{text}");
    }

    #[test]
    fn g1_cycle_report_names_every_degraded_mode_that_fired() {
        record_g1_cycle(
            g1_cycle_kind::MIXED,
            6,
            2,
            3,
            11,
            g1_degraded::EVACUATION_FAILURE
                | g1_degraded::JIT_PINNED_REGIONS_EXCLUDED
                | g1_degraded::MARK_WORKLIST_OVERFLOW,
        );
        let f = last_g1_cycle().expect("just recorded");
        assert_eq!(f.sequence, 1);
        assert_eq!(f.kind, g1_cycle_kind::MIXED);
        assert_eq!((f.cset_young, f.cset_old), (6, 2));
        assert_eq!(f.regions_pinned_out, 3);
        assert_eq!(f.rset_sources_scanned, 11);

        let text = f.to_string();
        assert!(text.contains("kind=mixed"), "{text}");
        assert!(text.contains("cset_young=6 cset_old=2"), "{text}");
        assert!(text.contains("pinned_out=3"), "{text}");
        assert!(text.contains("evacuation-failure-self-forwarded"), "{text}");
        assert!(text.contains("jit-pinned-regions-excluded"), "{text}");
        assert!(text.contains("mark-worklist-overflow-rescan"), "{text}");
    }

    #[test]
    fn an_undegraded_g1_cycle_says_so_rather_than_printing_nothing() {
        // "degraded=" with an empty tail would be indistinguishable from a
        // truncated log line; a clean cycle must say `none` explicitly.
        record_g1_cycle(g1_cycle_kind::YOUNG, 4, 0, 0, 2, g1_degraded::NONE);
        let text = last_g1_cycle().unwrap().to_string();
        assert!(text.contains("degraded=none"), "{text}");
    }

    #[test]
    fn g1_cycle_line_is_appended_to_the_collector_decision_report() {
        record_collector_decision(
            "g1",
            decision_reason::MOVING_BACKEND_ALWAYS_EVACUATES,
            incomplete_reason::NONE,
        );
        record_g1_cycle(
            g1_cycle_kind::YOUNG,
            3,
            0,
            1,
            5,
            g1_degraded::JNI_PINNED_REGIONS_EXCLUDED,
        );
        let text = collector_decision_report();
        assert!(text.contains("backend=g1"), "{text}");
        assert!(text.contains("young=MOVING"), "{text}");
        assert!(text.contains("[GC] g1 cycle #1"), "{text}");
        assert!(text.contains("jni-pinned-regions-excluded"), "{text}");
    }
}
