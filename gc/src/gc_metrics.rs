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
//! not per store: cards found dirty at scan, duplicate marks, remembered-set
//! bytes retained, old→young edge count, refinement time. A handful of relaxed
//! adds per GC pause is unmeasurable against a pause, and gating them would
//! make the default report empty — which is the failure mode this whole item
//! exists to prevent. See `docs/gc/tlab-and-card-audit.md`.
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
    duplicate_card_marks: AtomicU64,
    /// Bytes of remembered-set metadata currently retained (gauge, not a
    /// total). For Generational this is the card byte-map plus the dirty-index
    /// tracking list plus the undrained pending queue.
    remembered_set_bytes: AtomicU64,
    /// Old→young reference slots discovered by the dirty-card scan, summed
    /// over every scan. Ungated.
    old_to_young_edges: AtomicU64,
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
            duplicate_card_marks: AtomicU64::new(0),
            remembered_set_bytes: AtomicU64::new(0),
            old_to_young_edges: AtomicU64::new(0),
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

// ---------------------------------------------------------------------------
// Recording — collector side (ungated, once per cycle / per drain)
// ---------------------------------------------------------------------------

/// Record `n` distinct cards handed to a dirty-card scan.
pub fn record_cards_found_dirty(n: u64) {
    with_counters(|c| {
        c.cards_found_dirty.fetch_add(n, Ordering::Relaxed);
    });
}

/// Record `n` buffered offsets that landed on a card that was already dirty.
///
/// Called from `CardTable::drain_pending`, which knows both the number of
/// offsets it consumed and the number of clean→dirty transitions it caused.
pub fn record_duplicate_card_marks(n: u64) {
    with_counters(|c| {
        c.duplicate_card_marks.fetch_add(n, Ordering::Relaxed);
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
        c.allocated_objects.store(allocated_objects, Ordering::Relaxed);
        c.allocated_bytes.store(allocated_bytes, Ordering::Relaxed);
        c.live_bytes.store(live_bytes, Ordering::Relaxed);
    });
}

/// Reset every counter. Tests only — production counters are monotonic for the
/// life of the process.
pub fn reset_metrics_for_test() {
    with_counters(|c| {
        c.card_marks_executed.store(0, Ordering::Relaxed);
        c.barrier_ref_stores.store(0, Ordering::Relaxed);
        c.cards_found_dirty.store(0, Ordering::Relaxed);
        c.duplicate_card_marks.store(0, Ordering::Relaxed);
        c.remembered_set_bytes.store(0, Ordering::Relaxed);
        c.old_to_young_edges.store(0, Ordering::Relaxed);
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
    pub duplicate_card_marks: u64,
    pub remembered_set_bytes: u64,
    pub old_to_young_edges: u64,
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
    /// Fraction of buffered card marks that hit an already-dirty card. Pure
    /// waste in the buffered path; high values argue for a per-thread
    /// last-card filter.
    pub duplicate_mark_ratio: f64,
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
            "[GC] cards: dirty_scanned={} duplicate_marks={} old_to_young_edges={} \
             rset_bytes={} refinement_ms={:.3} passes={}",
            r.cards_found_dirty,
            r.duplicate_card_marks,
            r.old_to_young_edges,
            r.remembered_set_bytes,
            r.refinement_nanos as f64 / 1.0e6,
            r.refinement_passes,
        )?;
        if self.hot_path_counters_enabled {
            writeln!(
                f,
                "[GC] cards: barrier_ref_stores={} card_marks_executed={} hit_rate={:.4} \
                 (interpreter/native only — the JIT inline barrier is not counted)",
                r.barrier_ref_stores, r.card_marks_executed, self.barrier_hit_rate,
            )?;
        } else {
            writeln!(
                f,
                "[GC] cards: barrier counters NOT ARMED (set CRATONVM_GC_CARD_METRICS=1) — \
                 card_marks_executed/barrier_hit_rate are unmeasured, not zero"
            )?;
        }
        writeln!(
            f,
            "[GC] cards/alloc: dirty_cards_per_obj={:.6} edges_per_obj={:.6} \
             card_marks_per_obj={:.6} (allocated_objects={})",
            self.dirty_cards_per_allocated_object,
            self.old_to_young_edges_per_allocated_object,
            self.card_marks_per_allocated_object,
            r.allocated_objects,
        )?;
        write!(
            f,
            "[GC] cards/live: rset_bytes_per_live_byte={:.6} refine_ns_per_live_byte={:.6} \
             edges_per_live_byte={:.9} edge_density={:.4} duplicate_ratio={:.4} \
             (live_bytes={})",
            self.remembered_set_bytes_per_live_byte,
            self.refinement_nanos_per_live_byte,
            self.old_to_young_edges_per_live_byte,
            self.old_to_young_edge_density,
            self.duplicate_mark_ratio,
            r.live_bytes,
        )
    }
}

/// Snapshot the raw counters.
pub fn gc_metrics_raw() -> GcMetricsRaw {
    with_counters(|c| GcMetricsRaw {
        card_marks_executed: c.card_marks_executed.load(Ordering::Relaxed),
        barrier_ref_stores: c.barrier_ref_stores.load(Ordering::Relaxed),
        cards_found_dirty: c.cards_found_dirty.load(Ordering::Relaxed),
        duplicate_card_marks: c.duplicate_card_marks.load(Ordering::Relaxed),
        remembered_set_bytes: c.remembered_set_bytes.load(Ordering::Relaxed),
        old_to_young_edges: c.old_to_young_edges.load(Ordering::Relaxed),
        refinement_nanos: c.refinement_nanos.load(Ordering::Relaxed),
        refinement_passes: c.refinement_passes.load(Ordering::Relaxed),
        allocated_objects: c.allocated_objects.load(Ordering::Relaxed),
        allocated_bytes: c.allocated_bytes.load(Ordering::Relaxed),
        live_bytes: c.live_bytes.load(Ordering::Relaxed),
    })
}

/// The card / remembered-set cost report, normalized per allocated object and
/// per live byte.
///
/// Cheap (eleven relaxed loads and some float division); safe to call outside a
/// pause. The counters are not snapshot-consistent with each other — they are
/// observability, not a transaction.
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
    /// Non-moving: moving-young is switched off, and a live JIT frame's roots
    /// are therefore discovered conservatively and cannot be rewritten.
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

    /// One past the highest defined code.
    pub const COUNT: u8 = 10;

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
            _ => "unknown",
        }
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
            young = if self.young_moving {
                "MOVING"
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

/// A one-or-two-line human-readable statement of what the collector actually
/// did on the last cycle, and why.
///
/// This is the ground truth that settles the `docs/GC.md` ↔ `ARCHITECTURE.md`
/// disagreement about whether young collections move — see
/// `docs/gc/tlab-and-card-audit.md` §3.
pub fn collector_decision_report() -> String {
    let mut s = match last_collector_decision() {
        None => format!(
            "[GC] decision: no collection has run yet (moving_young_requested={})",
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
            s
        }
    };
    // G1 states its own last cycle. Absent on a Generational/ZGC run, so the
    // report keeps its previous shape there byte-for-byte.
    if let Some(g1) = last_g1_cycle() {
        s.push('\n');
        s.push_str(&g1.to_string());
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
/// See `docs/gc/g1-audit.md` for which invariant each one protects.
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
    /// The experimental parallel evacuator ran (`CRATONVM_G1_PARALLEL_EVAC`).
    pub const PARALLEL_EVACUATOR: u32 = 1 << 7;
    /// The collection set came out empty, so the pause did nothing.
    pub const EMPTY_COLLECTION_SET: u32 = 1 << 8;

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
        | EMPTY_COLLECTION_SET;

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
            (MARK_IMPLAUSIBLE_HEADER, "mark-implausible-header-retain-all"),
            (
                CLEANUP_CLOSURE_INCOMPLETE,
                "cleanup-closure-incomplete-retain-all",
            ),
            (JNI_PINNED_REGIONS_EXCLUDED, "jni-pinned-regions-excluded"),
            (JIT_PINNED_REGIONS_EXCLUDED, "jit-pinned-regions-excluded"),
            (PARALLEL_EVACUATOR, "experimental-parallel-evacuator"),
            (EMPTY_COLLECTION_SET, "empty-collection-set"),
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
            duplicate_card_marks: 150,
            remembered_set_bytes: 2_048,
            old_to_young_edges: 200,
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
        // 150 duplicates out of 150 + 50 total buffered marks.
        assert_eq!(r.duplicate_mark_ratio, 0.75);
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
        assert_eq!(raw.duplicate_card_marks, 3);
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
        }
        assert_eq!(
            decision_reason::label(decision_reason::COUNT),
            "unknown",
            "COUNT must be one PAST the last defined reason",
        );
    }

    #[test]
    fn decision_report_names_the_fallback_reason() {
        // Fresh test thread: nothing recorded, and the report says so rather
        // than inventing a verdict.
        assert!(last_collector_decision().is_none());
        assert!(collector_decision_report().contains("no collection has run yet"));

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
        assert_eq!(d.incomplete_reason, incomplete_reason::UNPUBLISHED_FRAME_OOP);

        let text = collector_decision_report();
        assert!(text.contains("NON-MOVING"), "{text}");
        assert!(text.contains("nonmoving-coverage-incomplete"), "{text}");
        assert!(
            text.contains("compiled-frame-oop-not-published"),
            "the report must name the ROOT SOURCE that reported incomplete \
             coverage, not merely that coverage was incomplete: {text}",
        );
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
