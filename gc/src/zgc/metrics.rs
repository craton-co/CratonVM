// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC pause / phase / allocation-stall accounting — the instrument that turns
//! the "ZGC-real has no young generation" hypothesis into a measurement.
//!
//! # Why this module exists
//!
//! `zgc-real-fullsuite-regression-RETIRED-20260807.md` records the first
//! full-suite comparison of `-XX:+UseZGC` against the default Generational
//! backend: same binary, same 1975 Spring Boot classes, **50 status changes,
//! 46 of them regressions** — 35 `PASS -> HANG` concentrated on
//! `*AutoConfigurationTests` (the shape that builds and tears down many
//! `ApplicationContext`s in a tight loop) and 11 `PASS -> FAIL`.
//!
//! The working hypothesis in that document is that [`super::ZgcRealHeap`] has
//! no young-generation fast path, so every collection is a whole-heap
//! stop-the-world mark-sweep and allocation-churn workloads fall off a
//! throughput cliff. That document is explicit that the hypothesis is
//! **unverified**: no GC-count or pause-time instrumentation was pulled from
//! any of the 35 hanging logs, and there were no repeat runs to separate
//! signal from host noise.
//!
//! This module is the instrument. Downstream design decisions rest on what it
//! reports, so it is built to three rules:
//!
//! 1. **Comparable, not private.** The naming, the `[GC]` / `[GC-SUMMARY]`
//!    prefixes and the percentile arithmetic mirror [`crate::gc_metrics`] and
//!    `g1::G1PausePercentiles` so a ZGC number can be put next to a
//!    Generational or G1 number without a translation step.
//! 2. **Honest about concurrency.** ZGC's whole value proposition is that most
//!    of a cycle runs *alongside* mutators. Today's [`super::ZgcRealHeap`] runs
//!    every phase inside one stop-the-world token. Reporting the design-intent
//!    split as though it were real would produce exactly the wrong headline
//!    number for this investigation — see
//!    [`ZgcMetrics::set_phases_run_concurrently`].
//! 3. **Machine-readable.** [`ZgcMetrics::to_tsv_row`] exists so a full-suite
//!    ZGC run drops straight into the Spring/H2 harness TSV aggregation next to
//!    a Generational run.
//!
//! # Relationship to [`super::ZgcPhase`]
//!
//! The simulation half of `zgc.rs` already has a `ZgcPhase` enum. That one is a
//! **state variable** — `ZgcCollector::phase` holds exactly one value at a
//! time, and it includes a `None` "mutator running" state. [`ZgcPhase`] here is
//! a **measurement key**: it has no idle state, it adds the
//! `ConcurrentMarkContinue` and `ConcurrentSelectRelocationSet` phases that
//! OpenJDK's `-Xlog:gc+phases` prints, and it adds [`ZgcPhase::Sweep`], which is
//! not a ZGC design phase at all but is what `ZgcRealHeap` actually does. The
//! two are deliberately separate types in separate modules; do not merge them.
//!
//! ## 2026-08-07: the measurement key gained `ConcurrentRemap`
//!
//! **Premise change.** This module shipped without a remap counter. The
//! cross-module suite `gc/tests/zgc_module_integration.rs` recorded the
//! consequence: a collector driven by the simulation's enum cannot record its
//! own remap phase, so that time vanishes from the report and reads as
//! *"remapping is free"*.
//!
//! Stated precisely, because the strength of the argument is not where it first
//! looks. `zgc.rs` has the *phase* — `ZgcCollector::concurrent_remap` is its
//! "Phase 7" and sets `self.phase = ZgcPhase::ConcurrentRemap` — but its body
//! is **empty**, and honestly so: a simulation has no object graph to walk. So
//! the argument is not "there is unmeasured work today". It is:
//!
//! * the *work* is specified, in [`super::forwarding`]'s relocation protocol
//!   (step 7): walk every remaining stale reference and rewrite it through a
//!   per-page hash table. It is the precondition for
//!   [`super::forwarding::ZForwardingRegistry::clear`], i.e. for releasing the
//!   from-space pages at all — not an optional tidy-up;
//! * the *phase* is already in the enum a real collector is driven by; and
//! * the counter was the only piece missing. Adding it after the walk lands
//!   means the first real remap implementation ships unmeasured, which is
//!   exactly the position `zgc-real-fullsuite-regression-RETIRED-20260807` is stuck in
//!   for the young-generation hypothesis. The instrument goes in first.
//!
//! **Decision: add the phase, do not fold it into mark.** The tempting
//! alternative is that the omission was correct because modern OpenJDK ZGC has
//! no `Concurrent Remap` log line at all — it remaps lazily, charging the work
//! to the *next* cycle's `Concurrent Mark`. That is true of OpenJDK and false
//! of this tree, and the difference is the whole point:
//!
//! 1. **Folding is only honest when the fold actually happened.** OpenJDK loses
//!    no time by omitting the line, because the work is genuinely inside a
//!    phase that *is* counted. Here the work is in a step of its own, so
//!    omitting the counter loses it outright. An instrument may aggregate; it
//!    may not drop.
//! 2. **This enum's stated rule already answers the question.** It "covers the
//!    **whole real cycle**, not just the phases the current implementation
//!    has", and it already carries [`ZgcPhase::Sweep`] — a phase OpenJDK does
//!    not have at all — precisely because that is what the code does. Excluding
//!    remap on OpenJDK-fidelity grounds while including `Sweep` on
//!    what-the-code-does grounds would be two rules at once.
//! 3. **A zero is a finding; an absent column is not.** If a future collector
//!    really does fold remap into the next mark, `concurrent_remap` reports
//!    `count=0` and the report says so. That reading is available only if the
//!    column exists.
//!
//! Cost: [`ZGC_PHASE_COUNT`] 10 → **11**, and the TSV grows from 54 to **58**
//! columns (14 fixed + 4 per phase).
//!
//! **Column-order note.** [`ZgcMetrics::tsv_header`] otherwise documents an
//! append-only rule, and inserting `concurrent_remap` in cycle order shifts the
//! four `sweep_*` columns right by four. That is deliberate and was checked:
//! nothing in `gc/src/` calls [`ZgcMetrics::to_tsv_row`] yet (the harness
//! wiring is the step the `zgc-real-fullsuite-regression-RETIRED-20260807` round never
//! reached), so no collected aggregation can be misaligned by it. Cycle order
//! is load-bearing for [`ZgcMetrics::format_summary`]'s table, which is read by
//! humans against a HotSpot log. Once a suite run has emitted rows, the
//! append-only rule binds again.
//!
//! # Phase timing mechanism
//!
//! Timing goes through `cratonvm_jfr::phase`, the same facility `g1.rs`,
//! `gen_heap.rs` and `concurrent_mark.rs` are slated to use (see the dependency
//! comment in `gc/Cargo.toml` and `docs/observability/phase-accounting.md`
//! §10.9). [`ZgcPhaseGuard`] opens a `cratonvm_jfr::phase::PhaseSpan` in
//! `Category::GcPause` or `Category::GcConcurrent` for its lifetime, so ZGC
//! time lands in the whole-run reconciliation identity rather than in a private
//! parallel timing system. When phase accounting is off (the default) that span
//! is a `None` token: no clock read, no thread-local touch, no allocation.
//!
//! This module *also* keeps its own per-phase counters, because
//! `cratonvm_jfr::phase` aggregates into 16 fixed VM-wide categories and cannot
//! answer "how long was Concurrent Relocate specifically, at p99".
//!
//! # Cost, and why the locking is split
//!
//! Two very different call frequencies share this struct, so they get two
//! different storage strategies:
//!
//! - **Phase records** run at most a handful of times per collection. Their
//!   scalar counters (count / total / min / max / last) are plain relaxed
//!   atomics — unmeasurable against a pause. Their bounded recent-sample ring
//!   needs a multi-field consistent update, so it sits behind a small
//!   `parking_lot::Mutex` **per phase**. Uncontended that is one atomic
//!   compare-exchange, taken ~10 times per cycle, and the per-phase split means
//!   a future concurrent collector's phases never contend with each other.
//! - **Allocation stalls** are recorded from *mutator* threads, potentially
//!   many at once, at exactly the moment the collector is behind. A lock there
//!   would serialize the very contention being measured and change the answer,
//!   so stall accounting is **pure atomics with no lock and no ring**.
//!
//! The two per-run scalars that genuinely need a non-atomic type
//! ([`std::time::Instant`] for the inter-cycle interval, and the run label) are
//! touched once per cycle and once at startup respectively.
//!
//! # Allocation stalls
//!
//! ZGC's characteristic failure mode is not a long pause: it is the mutator
//! **stalling** because it cannot allocate while the collector is behind. A
//! workload that hits the 300 s suite ceiling with modest total pause time and
//! large total *stall* time is a workload starved by the collector, and that is
//! a different fix from a pause-time problem. This is the counter most likely
//! to confirm or kill the young-generation hypothesis for the 35 `PASS -> HANG`
//! classes, so it is first-class: [`ZgcMetrics::scoped_allocation_stall`],
//! [`ZgcMetrics::record_allocation_stall`], its own TSV columns, and its own
//! line in both the per-cycle and the summary report.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Phases
// ---------------------------------------------------------------------------

/// Number of [`ZgcPhase`] variants. The width of every per-phase array.
///
/// 10 → 11 on 2026-08-07 when [`ZgcPhase::ConcurrentRemap`] was added; see the
/// module header for the decision. The TSV column count is
/// `14 + 4 * ZGC_PHASE_COUNT`, so this constant is the one place that number is
/// stated — never write `58` (or `54`) anywhere.
pub const ZGC_PHASE_COUNT: usize = 11;

/// How many recent durations are retained per phase for percentiles.
///
/// **Fixed** — the ring never grows. A collector that runs for hours must not
/// accumulate one sample per phase per cycle forever; a bounded most-recent
/// window is what `g1.rs`'s `PAUSE_HISTORY_CAP` settled on for the same reason,
/// and 64 samples per phase is 5 KB for the whole struct.
///
/// Consequence, stated so nobody misreads a report: a percentile from this ring
/// describes the **most recent 64 cycles**, not the whole run. The cumulative
/// count/total/min/max fields *are* whole-run.
pub const RECENT_SAMPLE_CAP: usize = 64;

/// One phase of a ZGC collection cycle.
///
/// The nine ZGC-design phases are named after OpenJDK's `-Xlog:gc+phases`
/// output so a CratonVM log is legible to anyone who has read a real ZGC log,
/// plus two phases OpenJDK does not print: [`ZgcPhase::ConcurrentRemap`], which
/// this tree runs as a discrete step where OpenJDK folds it into the next
/// cycle's mark, and [`ZgcPhase::Sweep`], which is what
/// [`super::ZgcRealHeap`] does today instead of relocating.
///
/// The enum covers the **whole real cycle**, not just the phases the current
/// implementation has. A phase that never runs reports `count=0`, which is a
/// finding (see the "silence is not zero" convention in
/// [`crate::gc_metrics`]'s report) — it says the collector skipped that stage,
/// not that the stage was free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ZgcPhase {
    /// STW: colour the roots and flip the marking colour. Short by design.
    PauseMarkStart,
    /// Concurrent tri-colour marking of the object graph.
    ConcurrentMark,
    /// Concurrent marking resumed after `PauseMarkEnd` found the closure
    /// incomplete. In OpenJDK this can iterate; a non-zero count here means the
    /// mark did not converge in one pass.
    ConcurrentMarkContinue,
    /// STW: drain the remaining mark stacks and prove the closure complete.
    PauseMarkEnd,
    /// Concurrent: weak / soft / phantom / cleaner / finalizer reference
    /// processing.
    ConcurrentProcessNonStrongRefs,
    /// Concurrent: clear the previous cycle's relocation set.
    ConcurrentResetRelocationSet,
    /// Concurrent: choose which pages to evacuate this cycle.
    ConcurrentSelectRelocationSet,
    /// STW: relocate the roots so mutators resume on remapped pointers.
    PauseRelocateStart,
    /// Concurrent: evacuate the relocation set, driven by the load barrier.
    ConcurrentRelocate,
    /// Concurrent: walk the remaining stale references and rewrite them through
    /// the forwarding tables, so the tables — and the from-space pages they
    /// describe — can be released.
    ///
    /// **OpenJDK prints no `Concurrent Remap` line.** Modern ZGC remaps lazily
    /// and charges the work to the *next* cycle's `Concurrent Mark`, so there is
    /// nothing to log. This tree is not shaped that way: `zgc.rs`'s
    /// `ZgcCollector::concurrent_remap` is a discrete step (its "Phase 7") that
    /// sets `super::ZgcPhase::ConcurrentRemap`, and
    /// [`super::forwarding::ZForwardingRegistry::clear`] is documented as legal
    /// only once remapping has finished.
    ///
    /// **`count=0` is expected today**, and is not a bug: the simulation's
    /// `concurrent_remap` has an empty body (it has no object graph to walk),
    /// and [`super::ZgcRealHeap`] does not relocate at all — it sweeps. This
    /// counter exists so that the first implementation of the real walk is
    /// measured on its first run rather than instrumented afterwards. See the
    /// module header.
    ///
    /// A collector that later *does* fold remap into the next cycle's mark also
    /// leaves this at `count=0`, which is the honest report of a folded phase —
    /// a reading only available because the column exists.
    ConcurrentRemap,
    /// **Not a ZGC design phase.** The non-moving mark-sweep reclaim pass that
    /// [`super::ZgcRealHeap::collect_garbage`] actually performs instead of
    /// relocation: walk the object registry, zero the unmarked objects, return
    /// their spans to the arena free list, coalesce. It is stop-the-world, and
    /// [`ZgcPhase::is_stw`] says so.
    Sweep,
}

impl ZgcPhase {
    /// Every phase, in cycle order. A phase's position here is its index into
    /// every per-phase array and its column order in the TSV.
    pub const ALL: [ZgcPhase; ZGC_PHASE_COUNT] = [
        ZgcPhase::PauseMarkStart,
        ZgcPhase::ConcurrentMark,
        ZgcPhase::ConcurrentMarkContinue,
        ZgcPhase::PauseMarkEnd,
        ZgcPhase::ConcurrentProcessNonStrongRefs,
        ZgcPhase::ConcurrentResetRelocationSet,
        ZgcPhase::ConcurrentSelectRelocationSet,
        ZgcPhase::PauseRelocateStart,
        ZgcPhase::ConcurrentRelocate,
        ZgcPhase::ConcurrentRemap,
        ZgcPhase::Sweep,
    ];

    /// Index into [`ZgcPhase::ALL`] and into the per-phase arrays.
    pub fn index(self) -> usize {
        match self {
            ZgcPhase::PauseMarkStart => 0,
            ZgcPhase::ConcurrentMark => 1,
            ZgcPhase::ConcurrentMarkContinue => 2,
            ZgcPhase::PauseMarkEnd => 3,
            ZgcPhase::ConcurrentProcessNonStrongRefs => 4,
            ZgcPhase::ConcurrentResetRelocationSet => 5,
            ZgcPhase::ConcurrentSelectRelocationSet => 6,
            ZgcPhase::PauseRelocateStart => 7,
            ZgcPhase::ConcurrentRelocate => 8,
            ZgcPhase::ConcurrentRemap => 9,
            ZgcPhase::Sweep => 10,
        }
    }

    /// OpenJDK's log label, verbatim where one exists.
    ///
    /// Printed by [`ZgcMetrics::format_summary`]. Keep these byte-identical to
    /// `-Xlog:gc+phases` so a reader can diff a CratonVM log against a HotSpot
    /// log without a lookup table.
    pub fn label(self) -> &'static str {
        match self {
            ZgcPhase::PauseMarkStart => "Pause Mark Start",
            ZgcPhase::ConcurrentMark => "Concurrent Mark",
            ZgcPhase::ConcurrentMarkContinue => "Concurrent Mark Continue",
            ZgcPhase::PauseMarkEnd => "Pause Mark End",
            ZgcPhase::ConcurrentProcessNonStrongRefs => "Concurrent Process Non-Strong References",
            ZgcPhase::ConcurrentResetRelocationSet => "Concurrent Reset Relocation Set",
            ZgcPhase::ConcurrentSelectRelocationSet => "Concurrent Select Relocation Set",
            ZgcPhase::PauseRelocateStart => "Pause Relocate Start",
            ZgcPhase::ConcurrentRelocate => "Concurrent Relocate",
            // Not a verbatim OpenJDK label — OpenJDK prints none, because it
            // folds remap into the next cycle's mark. Named in OpenJDK's style
            // anyway so the summary table stays uniform.
            ZgcPhase::ConcurrentRemap => "Concurrent Remap",
            ZgcPhase::Sweep => "Sweep (non-moving mark-sweep reclaim)",
        }
    }

    /// Stable snake_case metrics key. Used for the TSV column names, so it is
    /// an **external contract**: renaming one silently breaks every downstream
    /// aggregation that has already been collected.
    pub fn key(self) -> &'static str {
        match self {
            ZgcPhase::PauseMarkStart => "pause_mark_start",
            ZgcPhase::ConcurrentMark => "concurrent_mark",
            ZgcPhase::ConcurrentMarkContinue => "concurrent_mark_continue",
            ZgcPhase::PauseMarkEnd => "pause_mark_end",
            ZgcPhase::ConcurrentProcessNonStrongRefs => "concurrent_process_non_strong_refs",
            ZgcPhase::ConcurrentResetRelocationSet => "concurrent_reset_relocation_set",
            ZgcPhase::ConcurrentSelectRelocationSet => "concurrent_select_relocation_set",
            ZgcPhase::PauseRelocateStart => "pause_relocate_start",
            ZgcPhase::ConcurrentRelocate => "concurrent_relocate",
            // Chosen to match the snake_case of the simulation's
            // `super::ZgcPhase::ConcurrentRemap`, so the two vocabularies join
            // on the one variant they now share.
            ZgcPhase::ConcurrentRemap => "concurrent_remap",
            ZgcPhase::Sweep => "sweep",
        }
    }

    /// Whether this phase is stop-the-world **by design**.
    ///
    /// This is the classification that makes the report worth reading: the
    /// entire claim of a low-latency collector is that the STW column stays
    /// tiny while the concurrent column carries the work. A collector whose STW
    /// share is ~100% is not delivering ZGC's guarantee, whatever its phases are
    /// named.
    ///
    /// **Design intent, not necessarily what ran.** [`super::ZgcRealHeap`]
    /// executes every phase below inside one `StopTheWorldToken`. Use
    /// [`ZgcMetrics::phase_was_stw`] for what actually happened;
    /// [`ZgcMetrics::set_phases_run_concurrently`] is the switch that separates
    /// the two.
    pub fn is_stw(self) -> bool {
        matches!(
            self,
            ZgcPhase::PauseMarkStart
                | ZgcPhase::PauseMarkEnd
                | ZgcPhase::PauseRelocateStart
                // Not a design phase; genuinely stop-the-world in this
                // implementation, and labelled honestly.
                | ZgcPhase::Sweep
        )
    }

    /// Resolve a phase from its stable [`ZgcPhase::key`].
    pub fn from_key(key: &str) -> Option<ZgcPhase> {
        ZgcPhase::ALL.iter().copied().find(|p| p.key() == key)
    }
}

// ---------------------------------------------------------------------------
// Bounded recent-sample ring
// ---------------------------------------------------------------------------

/// A fixed-capacity ring of recent phase durations (nanoseconds).
///
/// Fixed array, never a `Vec`: the point is that a long soak cannot grow this.
/// `push` overwrites the oldest sample once full.
struct RecentRing {
    samples: [u64; RECENT_SAMPLE_CAP],
    /// Number of valid entries, saturating at [`RECENT_SAMPLE_CAP`].
    len: usize,
    /// Next write index, wrapping at [`RECENT_SAMPLE_CAP`].
    next: usize,
}

impl RecentRing {
    const fn new() -> RecentRing {
        RecentRing {
            samples: [0u64; RECENT_SAMPLE_CAP],
            len: 0,
            next: 0,
        }
    }

    fn push(&mut self, nanos: u64) {
        self.samples[self.next] = nanos;
        self.next = (self.next + 1) % RECENT_SAMPLE_CAP;
        if self.len < RECENT_SAMPLE_CAP {
            self.len += 1;
        }
    }

    /// The valid samples, oldest-first order not preserved (percentiles sort
    /// anyway).
    fn snapshot(&self) -> Vec<u64> {
        self.samples[..self.len].to_vec()
    }
}

/// Nearest-rank percentile, identical arithmetic to
/// `g1::G1Collector::pause_summary` so the two reports are comparable.
///
/// Returns `0` for an empty sample set — a percentile of nothing reads as "not
/// observed", never as `NaN` or a panic.
fn nearest_rank(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let count = sorted.len();
    let rank = ((p * count) + 99) / 100; // ceil(p*N/100)
    let idx = rank.saturating_sub(1).min(count - 1);
    sorted[idx]
}

// ---------------------------------------------------------------------------
// Per-phase counters
// ---------------------------------------------------------------------------

/// Sentinel for "no sample yet" in the min slot. Reported as `0`.
const MIN_UNSET: u64 = u64::MAX;

/// One phase's whole-run counters plus its bounded recent window.
struct PhaseCounters {
    /// Times this phase has been recorded.
    count: AtomicU64,
    /// Σ durations, nanoseconds.
    total_ns: AtomicU64,
    /// Shortest recorded duration, or [`MIN_UNSET`].
    min_ns: AtomicU64,
    /// Longest recorded duration.
    max_ns: AtomicU64,
    /// Most recent duration.
    last_ns: AtomicU64,
    /// Bounded most-recent window, for percentiles. See the module header for
    /// why this one field takes a lock and the scalars above do not.
    recent: Mutex<RecentRing>,
}

impl PhaseCounters {
    fn new() -> PhaseCounters {
        PhaseCounters {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            min_ns: AtomicU64::new(MIN_UNSET),
            max_ns: AtomicU64::new(0),
            last_ns: AtomicU64::new(0),
            recent: Mutex::new(RecentRing::new()),
        }
    }

    fn record(&self, nanos: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(nanos, Ordering::Relaxed);
        self.min_ns.fetch_min(nanos, Ordering::Relaxed);
        self.max_ns.fetch_max(nanos, Ordering::Relaxed);
        self.last_ns.store(nanos, Ordering::Relaxed);
        self.recent.lock().push(nanos);
    }

    fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
        self.total_ns.store(0, Ordering::Relaxed);
        self.min_ns.store(MIN_UNSET, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
        self.last_ns.store(0, Ordering::Relaxed);
        *self.recent.lock() = RecentRing::new();
    }

    fn snapshot(&self, phase: ZgcPhase, stw: bool) -> ZgcPhaseStats {
        let count = self.count.load(Ordering::Relaxed);
        let raw_min = self.min_ns.load(Ordering::Relaxed);
        let mut samples = self.recent.lock().snapshot();
        samples.sort_unstable();
        ZgcPhaseStats {
            phase,
            stw,
            count,
            total_ns: self.total_ns.load(Ordering::Relaxed),
            min_ns: if raw_min == MIN_UNSET { 0 } else { raw_min },
            max_ns: self.max_ns.load(Ordering::Relaxed),
            last_ns: self.last_ns.load(Ordering::Relaxed),
            recent_samples: samples.len() as u64,
            p50_ns: nearest_rank(&samples, 50),
            p99_ns: nearest_rank(&samples, 99),
        }
    }
}

/// One phase's reduced statistics. Plain data — no locks, no globals — so the
/// report formatting is a pure function of it and is testable in isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZgcPhaseStats {
    /// Which phase.
    pub phase: ZgcPhase,
    /// Whether this phase's time was charged as **stop-the-world** in this run.
    /// See [`ZgcMetrics::phase_was_stw`]: this is what happened, which is not
    /// always [`ZgcPhase::is_stw`].
    pub stw: bool,
    /// Whole-run invocation count.
    pub count: u64,
    /// Whole-run total duration, nanoseconds.
    pub total_ns: u64,
    /// Shortest recorded duration, or `0` when `count == 0`.
    pub min_ns: u64,
    /// Longest recorded duration.
    pub max_ns: u64,
    /// Most recent duration.
    pub last_ns: u64,
    /// Samples in the bounded ring the percentiles below were taken over.
    /// Capped at [`RECENT_SAMPLE_CAP`]; when it equals the cap the percentiles
    /// describe a window, not the run.
    pub recent_samples: u64,
    /// Median over the recent window.
    pub p50_ns: u64,
    /// 99th percentile over the recent window.
    pub p99_ns: u64,
}

impl ZgcPhaseStats {
    /// Mean duration, nanoseconds. `0` when the phase never ran.
    pub fn avg_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.total_ns / self.count
        }
    }
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// ZGC pause, phase, cycle and allocation-stall accounting for one heap.
///
/// Created alongside a [`super::ZgcRealHeap`] and shared by `&`. Every method
/// takes `&self`; the type is `Send + Sync`.
///
/// The collector's side of the contract is three calls:
///
/// ```ignore
/// // once, at heap construction: does this build's "concurrent" phases
/// // actually run off the safepoint? Today's ZgcRealHeap: no (the default).
/// metrics.set_phases_run_concurrently(false);
///
/// // around each phase of collect_garbage
/// {
///     let _p = metrics.scoped(ZgcPhase::ConcurrentMark);
///     // ... marking ...
/// }
///
/// // at the end of the cycle
/// metrics.record_cycle(bytes_in_use_before, bytes_in_use_after, capacity);
/// ```
///
/// plus [`ZgcMetrics::scoped_allocation_stall`] wherever a mutator blocks
/// waiting for the collector to free space.
pub struct ZgcMetrics {
    /// Per-phase counters, indexed by [`ZgcPhase::index`].
    phases: [PhaseCounters; ZGC_PHASE_COUNT],

    /// Whether the phases [`ZgcPhase::is_stw`] calls concurrent really do run
    /// alongside mutators in this build. See
    /// [`ZgcMetrics::set_phases_run_concurrently`].
    phases_run_concurrently: AtomicBool,

    // --- cycle-level ------------------------------------------------------
    /// Completed cycles.
    cycles: AtomicU64,
    /// Σ bytes reclaimed across every cycle.
    total_bytes_reclaimed: AtomicU64,
    /// Bytes reclaimed by the most recent cycle.
    last_bytes_reclaimed: AtomicU64,
    /// Bytes in use immediately before the most recent cycle (gauge).
    last_bytes_before: AtomicU64,
    /// Bytes in use immediately after the most recent cycle — the live set
    /// (gauge).
    last_live_bytes: AtomicU64,
    /// Heap capacity as of the most recent cycle (gauge).
    last_capacity_bytes: AtomicU64,
    /// STW nanoseconds charged by the most recent cycle alone.
    last_cycle_stw_ns: AtomicU64,
    /// Concurrent nanoseconds charged by the most recent cycle alone.
    last_cycle_concurrent_ns: AtomicU64,
    /// Nanoseconds between the previous cycle's end and this one's.
    last_interval_ns: AtomicU64,
    /// Bytes the mutators allocated during `last_interval_ns`.
    last_interval_alloc_bytes: AtomicU64,
    /// Σ mutator bytes allocated between cycles, whole run.
    total_interval_alloc_bytes: AtomicU64,
    /// Σ of every inter-cycle interval, whole run. The denominator of the
    /// run-wide allocation rate.
    total_interval_ns: AtomicU64,

    /// Cumulative STW/concurrent totals sampled at the previous
    /// [`ZgcMetrics::record_cycle`], so a per-cycle split is a subtraction
    /// rather than a second set of timers that could disagree with the first.
    stw_ns_at_last_cycle: AtomicU64,
    concurrent_ns_at_last_cycle: AtomicU64,

    // --- allocation stalls (mutator threads; atomics only, see module docs) --
    /// Times a mutator blocked because it could not allocate.
    stall_count: AtomicU64,
    /// Σ stalled nanoseconds across every mutator.
    stall_total_ns: AtomicU64,
    /// Worst single stall.
    stall_max_ns: AtomicU64,

    /// Wall-clock origin for the run, and the end of the previous cycle.
    /// Touched once per cycle.
    started: Instant,
    last_cycle_end: Mutex<Option<Instant>>,

    /// Label for the TSV `run` column. Set once at startup by whoever launches
    /// the arm (`zgc-real`, `generational`, ...).
    run_label: Mutex<String>,
}

impl Default for ZgcMetrics {
    fn default() -> Self {
        ZgcMetrics::new()
    }
}

impl ZgcMetrics {
    /// A fresh recorder.
    ///
    /// [`ZgcMetrics::set_phases_run_concurrently`] defaults to **`false`**,
    /// which is the truth for today's [`super::ZgcRealHeap`]: everything runs
    /// inside one stop-the-world token.
    pub fn new() -> ZgcMetrics {
        ZgcMetrics {
            phases: std::array::from_fn(|_| PhaseCounters::new()),
            phases_run_concurrently: AtomicBool::new(false),
            cycles: AtomicU64::new(0),
            total_bytes_reclaimed: AtomicU64::new(0),
            last_bytes_reclaimed: AtomicU64::new(0),
            last_bytes_before: AtomicU64::new(0),
            last_live_bytes: AtomicU64::new(0),
            last_capacity_bytes: AtomicU64::new(0),
            last_cycle_stw_ns: AtomicU64::new(0),
            last_cycle_concurrent_ns: AtomicU64::new(0),
            last_interval_ns: AtomicU64::new(0),
            last_interval_alloc_bytes: AtomicU64::new(0),
            total_interval_alloc_bytes: AtomicU64::new(0),
            total_interval_ns: AtomicU64::new(0),
            stw_ns_at_last_cycle: AtomicU64::new(0),
            concurrent_ns_at_last_cycle: AtomicU64::new(0),
            stall_count: AtomicU64::new(0),
            stall_total_ns: AtomicU64::new(0),
            stall_max_ns: AtomicU64::new(0),
            started: Instant::now(),
            last_cycle_end: Mutex::new(None),
            run_label: Mutex::new(String::from("zgc-real")),
        }
    }

    // -- configuration ----------------------------------------------------

    /// Declare whether the phases [`ZgcPhase::is_stw`] calls concurrent are
    /// **actually** concurrent in this build.
    ///
    /// This is the switch that keeps the headline number honest. Today
    /// [`super::ZgcRealHeap::collect_garbage`] holds one `StopTheWorldToken`
    /// across mark, reference processing and sweep — nothing overlaps a
    /// mutator. Charging `ConcurrentMark` to the concurrent column anyway would
    /// print a small STW total and a large concurrent total, i.e. exactly the
    /// picture of a healthy low-latency collector, for a collector that stops
    /// the world for the whole cycle. That is the reading the
    /// `zgc-real-fullsuite-regression-RETIRED-20260807` investigation must not be
    /// handed.
    ///
    /// So the default is `false` and every phase is charged as STW, both in
    /// this module's totals and in the `cratonvm_jfr::phase` category the guard
    /// opens. Flip it to `true` in the same commit that makes the phases
    /// genuinely run off the safepoint, and the split becomes real without any
    /// other change.
    pub fn set_phases_run_concurrently(&self, concurrent: bool) {
        self.phases_run_concurrently
            .store(concurrent, Ordering::Relaxed);
    }

    /// Whether concurrent-by-design phases really run concurrently here.
    pub fn phases_run_concurrently(&self) -> bool {
        self.phases_run_concurrently.load(Ordering::Relaxed)
    }

    /// Whether `phase`'s time is charged as stop-the-world **in this run**.
    ///
    /// `phase.is_stw() || !self.phases_run_concurrently()`. Use this, not
    /// [`ZgcPhase::is_stw`], for anything that adds up time.
    pub fn phase_was_stw(&self, phase: ZgcPhase) -> bool {
        phase.is_stw() || !self.phases_run_concurrently()
    }

    /// Set the `run` column of [`ZgcMetrics::to_tsv_row`].
    ///
    /// The whole point of the TSV is putting a ZGC row next to a Generational
    /// row; without a label the two rows are indistinguishable after
    /// concatenation.
    pub fn set_run_label(&self, label: &str) {
        let mut slot = self.run_label.lock();
        slot.clear();
        // Tabs and newlines would split or terminate the TSV row.
        for c in label.chars() {
            if c == '\t' || c == '\n' || c == '\r' {
                slot.push('_');
            } else {
                slot.push(c);
            }
        }
    }

    /// The current TSV run label.
    pub fn run_label(&self) -> String {
        self.run_label.lock().clone()
    }

    // -- phase recording --------------------------------------------------

    /// Time `phase` for the lifetime of the returned guard.
    ///
    /// This is the API the collector calls. The guard records on drop and also
    /// opens a `cratonvm_jfr::phase` span in `Category::GcPause` or
    /// `Category::GcConcurrent` (chosen by [`ZgcMetrics::phase_was_stw`]), so
    /// ZGC time participates in the whole-run phase-accounting reconciliation
    /// rather than being timed twice by two systems that can disagree.
    ///
    /// ```ignore
    /// let _p = metrics.scoped(ZgcPhase::Sweep);
    /// ```
    pub fn scoped(&self, phase: ZgcPhase) -> ZgcPhaseGuard<'_> {
        let category = if self.phase_was_stw(phase) {
            cratonvm_jfr::phase::Category::GcPause
        } else {
            cratonvm_jfr::phase::Category::GcConcurrent
        };
        ZgcPhaseGuard {
            metrics: self,
            phase,
            start: Instant::now(),
            span: cratonvm_jfr::phase::enter(category),
        }
    }

    /// Record one completed `phase` of `nanos` nanoseconds directly.
    ///
    /// [`ZgcMetrics::scoped`] is the ergonomic path; this exists for call sites
    /// that already have a duration (a replayed log, a test) and for the guard
    /// itself.
    pub fn record_phase(&self, phase: ZgcPhase, nanos: u64) {
        self.phases[phase.index()].record(nanos);
    }

    /// Reduced statistics for one phase.
    pub fn phase_stats(&self, phase: ZgcPhase) -> ZgcPhaseStats {
        self.phases[phase.index()].snapshot(phase, self.phase_was_stw(phase))
    }

    /// Reduced statistics for every phase, in [`ZgcPhase::ALL`] order.
    pub fn all_phase_stats(&self) -> Vec<ZgcPhaseStats> {
        ZgcPhase::ALL.iter().map(|p| self.phase_stats(*p)).collect()
    }

    /// Σ nanoseconds charged to stop-the-world phases, whole run.
    ///
    /// **The headline number**, together with
    /// [`ZgcMetrics::total_concurrent_ns`]. A collector whose STW total is
    /// ~100% of its collection time is not a low-latency collector.
    pub fn total_stw_ns(&self) -> u64 {
        let mut total: u64 = 0;
        for phase in ZgcPhase::ALL {
            if self.phase_was_stw(phase) {
                total = total
                    .saturating_add(self.phases[phase.index()].total_ns.load(Ordering::Relaxed));
            }
        }
        total
    }

    /// Σ nanoseconds charged to phases that ran alongside mutators, whole run.
    pub fn total_concurrent_ns(&self) -> u64 {
        let mut total: u64 = 0;
        for phase in ZgcPhase::ALL {
            if !self.phase_was_stw(phase) {
                total = total
                    .saturating_add(self.phases[phase.index()].total_ns.load(Ordering::Relaxed));
            }
        }
        total
    }

    /// `total_stw_ns / (total_stw_ns + total_concurrent_ns)`, or `0.0` when
    /// nothing has been collected. Never `NaN` — a `NaN` in a summary line is
    /// indistinguishable from a parse bug for whoever reads the log.
    pub fn stw_share(&self) -> f64 {
        let stw = self.total_stw_ns();
        let total = stw.saturating_add(self.total_concurrent_ns());
        if total == 0 {
            0.0
        } else {
            stw as f64 / total as f64
        }
    }

    // -- cycle recording --------------------------------------------------

    /// Record one completed collection cycle.
    ///
    /// Call at the very end of `collect_garbage`, after every phase guard has
    /// been dropped — the per-cycle STW/concurrent split is computed by
    /// subtracting the cumulative totals sampled at the previous cycle, so a
    /// phase closed after this call is charged to the *next* cycle.
    ///
    /// * `bytes_in_use_before` — bytes of live+dead payload outstanding when
    ///   the cycle started (`ZgcRealHeap::allocated` on entry).
    /// * `bytes_in_use_after` — bytes surviving the sweep (`bytes_copied`).
    /// * `capacity_bytes` — total heap capacity, for the `(%)` figures in the
    ///   `-Xlog:gc`-shaped line.
    ///
    /// The mutator allocation rate between cycles is derived, not passed in:
    /// `bytes_in_use_before` of this cycle minus `bytes_in_use_after` of the
    /// previous one, over the elapsed interval. Deriving it here means the
    /// collector cannot report an allocation rate that disagrees with the
    /// occupancy figures printed on the same line.
    pub fn record_cycle(
        &self,
        bytes_in_use_before: u64,
        bytes_in_use_after: u64,
        capacity_bytes: u64,
    ) {
        let now = Instant::now();

        let reclaimed = bytes_in_use_before.saturating_sub(bytes_in_use_after);
        let previous_live = self.last_live_bytes.load(Ordering::Relaxed);
        let had_previous = self.cycles.load(Ordering::Relaxed) > 0;

        // Interval since the previous cycle (or since construction for the
        // first one — that interval is real mutator time and dropping it would
        // overstate the allocation rate of every later cycle).
        let interval_ns = {
            let mut slot = self.last_cycle_end.lock();
            // Explicit deref: `slot` is a MutexGuard, and `Option<Instant>` is
            // Copy, so this reads the inner value rather than moving the guard.
            let base: Instant = (*slot).unwrap_or(self.started);
            let elapsed = now.saturating_duration_since(base);
            *slot = Some(now);
            elapsed.as_nanos().min(u64::MAX as u128) as u64
        };

        let allocated_since = if had_previous {
            bytes_in_use_before.saturating_sub(previous_live)
        } else {
            bytes_in_use_before
        };

        // Per-cycle STW/concurrent split by subtraction from the cumulative
        // totals — one owner per number.
        let stw_now = self.total_stw_ns();
        let conc_now = self.total_concurrent_ns();
        let cycle_stw = stw_now.saturating_sub(self.stw_ns_at_last_cycle.load(Ordering::Relaxed));
        let cycle_conc =
            conc_now.saturating_sub(self.concurrent_ns_at_last_cycle.load(Ordering::Relaxed));
        self.stw_ns_at_last_cycle.store(stw_now, Ordering::Relaxed);
        self.concurrent_ns_at_last_cycle
            .store(conc_now, Ordering::Relaxed);

        self.last_bytes_before
            .store(bytes_in_use_before, Ordering::Relaxed);
        self.last_live_bytes
            .store(bytes_in_use_after, Ordering::Relaxed);
        self.last_capacity_bytes
            .store(capacity_bytes, Ordering::Relaxed);
        self.last_bytes_reclaimed
            .store(reclaimed, Ordering::Relaxed);
        self.total_bytes_reclaimed
            .fetch_add(reclaimed, Ordering::Relaxed);
        self.last_cycle_stw_ns.store(cycle_stw, Ordering::Relaxed);
        self.last_cycle_concurrent_ns
            .store(cycle_conc, Ordering::Relaxed);
        self.last_interval_ns.store(interval_ns, Ordering::Relaxed);
        self.last_interval_alloc_bytes
            .store(allocated_since, Ordering::Relaxed);
        self.total_interval_alloc_bytes
            .fetch_add(allocated_since, Ordering::Relaxed);
        self.total_interval_ns
            .fetch_add(interval_ns, Ordering::Relaxed);
        // Written LAST, same publication rule as `gc_metrics`' decision slot: a
        // reader that sees cycles == N has seen at least cycle N's fields.
        self.cycles.fetch_add(1, Ordering::Release);
    }

    /// Completed cycles.
    pub fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Acquire)
    }

    /// Bytes reclaimed by the most recent cycle.
    pub fn last_bytes_reclaimed(&self) -> u64 {
        self.last_bytes_reclaimed.load(Ordering::Relaxed)
    }

    /// Bytes reclaimed across the whole run.
    pub fn total_bytes_reclaimed(&self) -> u64 {
        self.total_bytes_reclaimed.load(Ordering::Relaxed)
    }

    /// Live bytes after the most recent cycle (gauge).
    pub fn live_bytes(&self) -> u64 {
        self.last_live_bytes.load(Ordering::Relaxed)
    }

    /// Mutator allocation rate between the last two cycles, bytes per second.
    /// `0.0` when no interval has elapsed.
    pub fn last_allocation_rate_bytes_per_sec(&self) -> f64 {
        rate_per_sec(
            self.last_interval_alloc_bytes.load(Ordering::Relaxed),
            self.last_interval_ns.load(Ordering::Relaxed),
        )
    }

    /// Mutator allocation rate over every inter-cycle interval of the run,
    /// bytes per second. `0.0` before the first cycle.
    pub fn allocation_rate_bytes_per_sec(&self) -> f64 {
        rate_per_sec(
            self.total_interval_alloc_bytes.load(Ordering::Relaxed),
            self.total_interval_ns.load(Ordering::Relaxed),
        )
    }

    // -- allocation stalls -------------------------------------------------

    /// Time a mutator allocation stall for the lifetime of the returned guard.
    ///
    /// Wrap the block where a mutator waits for the collector to make space.
    /// The guard charges its time to `Category::GcPause` in
    /// `cratonvm_jfr::phase` — which is what that category documents ("*including
    /// the time mutators spend blocked in them*") — and to the stall counters
    /// here.
    ///
    /// This is the number that distinguishes "the collector is slow" from "the
    /// collector is starving the application", and the two have different
    /// fixes. See the module header.
    pub fn scoped_allocation_stall(&self) -> ZgcStallGuard<'_> {
        ZgcStallGuard {
            metrics: self,
            start: Instant::now(),
            span: cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcPause),
        }
    }

    /// Record one allocation stall of `nanos` nanoseconds.
    ///
    /// Three relaxed atomic RMWs, no lock: this runs on mutator threads, and
    /// under the exact contention it is measuring. A lock here would serialize
    /// the stalling threads and inflate the number it reports.
    pub fn record_allocation_stall(&self, nanos: u64) {
        self.stall_count.fetch_add(1, Ordering::Relaxed);
        self.stall_total_ns.fetch_add(nanos, Ordering::Relaxed);
        self.stall_max_ns.fetch_max(nanos, Ordering::Relaxed);
    }

    /// `(count, total_ns, max_ns)` of mutator allocation stalls.
    pub fn allocation_stalls(&self) -> (u64, u64, u64) {
        (
            self.stall_count.load(Ordering::Relaxed),
            self.stall_total_ns.load(Ordering::Relaxed),
            self.stall_max_ns.load(Ordering::Relaxed),
        )
    }

    // -- test support ------------------------------------------------------

    /// Zero every counter. Tests only — production counters are monotonic for
    /// the life of the heap.
    pub fn reset_for_test(&self) {
        for counters in self.phases.iter() {
            counters.reset();
        }
        for slot in [
            &self.cycles,
            &self.total_bytes_reclaimed,
            &self.last_bytes_reclaimed,
            &self.last_bytes_before,
            &self.last_live_bytes,
            &self.last_capacity_bytes,
            &self.last_cycle_stw_ns,
            &self.last_cycle_concurrent_ns,
            &self.last_interval_ns,
            &self.last_interval_alloc_bytes,
            &self.total_interval_alloc_bytes,
            &self.total_interval_ns,
            &self.stw_ns_at_last_cycle,
            &self.concurrent_ns_at_last_cycle,
            &self.stall_count,
            &self.stall_total_ns,
            &self.stall_max_ns,
        ] {
            slot.store(0, Ordering::Relaxed);
        }
        *self.last_cycle_end.lock() = None;
    }

    // -- text report -------------------------------------------------------

    /// One `-Xlog:gc`-shaped line describing the **most recent** cycle.
    ///
    /// Emit it once per cycle (from the verbose-GC path — see the
    /// `"[GC] Verbose GC logging enabled (ZGC-real collector)"` line in
    /// `gc/src/vm_heap.rs`) and a run's log has one line per cycle, the same
    /// shape a `-Xlog:gc` HotSpot log has.
    ///
    /// Modelled on OpenJDK ZGC's
    /// `GC(3) Garbage Collection (Allocation Rate) 1026M(50%)->140M(7%)`, with
    /// the CratonVM `[GC]` prefix (see [`crate::gc_metrics::GcMetricsReport`]'s
    /// `Display`) and the three CratonVM-specific tails: the STW/concurrent
    /// split for this cycle, the allocation rate the cycle was triggered at,
    /// and the stall counter.
    ///
    /// Reads `"no cycle recorded"` before the first collection rather than
    /// printing a row of zeroes that looks like a completed cycle.
    pub fn format_cycle_line(&self) -> String {
        let cycles = self.cycles();
        if cycles == 0 {
            return String::from("[GC] zgc: no cycle recorded yet");
        }
        let before = self.last_bytes_before.load(Ordering::Relaxed);
        let after = self.last_live_bytes.load(Ordering::Relaxed);
        let capacity = self.last_capacity_bytes.load(Ordering::Relaxed);
        let stw = self.last_cycle_stw_ns.load(Ordering::Relaxed);
        let conc = self.last_cycle_concurrent_ns.load(Ordering::Relaxed);
        let (stall_count, stall_total_ns, stall_max_ns) = self.allocation_stalls();
        format!(
            "[GC] GC({cycle}) Garbage Collection (Allocation Rate) \
             {before_h}({before_pct}%)->{after_h}({after_pct}%) \
             reclaimed={reclaimed_h} stw={stw_ms:.3}ms concurrent={conc_ms:.3}ms \
             alloc_rate={rate_h}/s stalls={stall_count} stall_total={stall_ms:.3}ms \
             stall_max={stall_max_ms:.3}ms",
            // GC(N) is 0-based in OpenJDK; `cycles` is a 1-based count.
            cycle = cycles - 1,
            before_h = human_bytes(before),
            before_pct = percent_of(before, capacity),
            after_h = human_bytes(after),
            after_pct = percent_of(after, capacity),
            reclaimed_h = human_bytes(self.last_bytes_reclaimed()),
            stw_ms = stw as f64 / 1.0e6,
            conc_ms = conc as f64 / 1.0e6,
            rate_h = human_bytes(self.last_allocation_rate_bytes_per_sec() as u64),
            stall_count = stall_count,
            stall_ms = stall_total_ns as f64 / 1.0e6,
            stall_max_ms = stall_max_ns as f64 / 1.0e6,
        )
    }

    /// The end-of-run table: headline split, per-phase rows, stalls.
    ///
    /// `[GC-SUMMARY]`-prefixed to match `g1::G1Collector::print_gc_summary`, so
    /// a harness that already scrapes G1 summary lines picks these up too.
    pub fn format_summary(&self) -> String {
        let stw = self.total_stw_ns();
        let conc = self.total_concurrent_ns();
        let cycles = self.cycles();
        let (stall_count, stall_total_ns, stall_max_ns) = self.allocation_stalls();
        let mut s = String::with_capacity(2048);

        s.push_str(&format!(
            "[GC-SUMMARY] zgc cycles={cycles} stw_ms={stw_ms:.3} concurrent_ms={conc_ms:.3} \
             stw_share={share:.4} reclaimed={reclaimed} live_bytes={live} \
             alloc_rate_bytes_per_sec={rate:.0}\n",
            cycles = cycles,
            stw_ms = stw as f64 / 1.0e6,
            conc_ms = conc as f64 / 1.0e6,
            share = self.stw_share(),
            reclaimed = self.total_bytes_reclaimed(),
            live = self.live_bytes(),
            rate = self.allocation_rate_bytes_per_sec(),
        ));

        // State the concurrency mode explicitly. Without this line the
        // stw_share above is unreadable: 1.0000 could mean "the collector
        // stops the world for everything" or "the phases are mislabelled".
        if self.phases_run_concurrently() {
            s.push_str(
                "[GC-SUMMARY] zgc concurrency=real (phases marked concurrent ran off the \
                 safepoint)\n",
            );
        } else {
            s.push_str(
                "[GC-SUMMARY] zgc concurrency=none (every phase ran inside one \
                 StopTheWorldToken — the concurrent column is 0 BY CONSTRUCTION, not by \
                 measurement)\n",
            );
        }

        s.push_str(&format!(
            "[GC-SUMMARY] zgc phase {:<42} {:>4} {:>8} {:>11} {:>10} {:>10} {:>10} {:>10} {:>10}\n",
            "name", "stw", "count", "total_ms", "min_ms", "avg_ms", "max_ms", "p99_ms", "last_ms",
        ));
        for stats in self.all_phase_stats() {
            s.push_str(&format!(
                "[GC-SUMMARY] zgc phase {:<42} {:>4} {:>8} {:>11.3} {:>10.3} {:>10.3} {:>10.3} \
                 {:>10.3} {:>10.3}\n",
                stats.phase.label(),
                if stats.stw { "STW" } else { "conc" },
                stats.count,
                stats.total_ns as f64 / 1.0e6,
                stats.min_ns as f64 / 1.0e6,
                stats.avg_ns() as f64 / 1.0e6,
                stats.max_ns as f64 / 1.0e6,
                stats.p99_ns as f64 / 1.0e6,
                stats.last_ns as f64 / 1.0e6,
            ));
        }

        // Stalls get their own line rather than a column: this is the metric
        // most likely to explain the 35 PASS->HANG classes, and it must not be
        // lost in a wide table.
        s.push_str(&format!(
            "[GC-SUMMARY] zgc allocation stalls: count={count} total_ms={total_ms:.3} \
             max_ms={max_ms:.3} avg_ms={avg_ms:.3}\n",
            count = stall_count,
            total_ms = stall_total_ns as f64 / 1.0e6,
            max_ms = stall_max_ns as f64 / 1.0e6,
            avg_ms = if stall_count == 0 {
                0.0
            } else {
                (stall_total_ns / stall_count) as f64 / 1.0e6
            },
        ));
        s.push_str(&format!(
            "[GC-SUMMARY] zgc note: percentiles cover the most recent {cap} \
             samples per phase; count/total/min/max are whole-run\n",
            cap = RECENT_SAMPLE_CAP,
        ));
        s
    }

    // -- machine-readable --------------------------------------------------

    /// The TSV header for [`ZgcMetrics::to_tsv_row`].
    ///
    /// **Intent: direct comparability across collector arms.** The Spring Boot
    /// and H2 suite harnesses already aggregate per-class `results.tsv` files
    /// (see the paths in `zgc-real-fullsuite-regression-RETIRED-20260807.md`).
    /// Emitting one row per VM here means a full-suite `-XX:+UseZGC` run can be
    /// joined against a Generational run with `sort`/`join` instead of by
    /// hand-parsing `[GC-SUMMARY]` lines out of 1975 logs — which is exactly the
    /// step that did not happen in the round that produced that document, and
    /// is why the young-generation hypothesis is still unverified.
    ///
    /// Column order is fixed and is the same loop
    /// [`ZgcMetrics::to_tsv_row`] uses. Append new columns at the end only;
    /// never reorder or remove one, and never let the two functions drift —
    /// `tsv_header_and_row_have_the_same_column_count` is the guard, because a
    /// silent off-by-one shifts every value into the wrong column and corrupts
    /// the whole downstream analysis without erroring anywhere.
    ///
    /// **The append-only rule has been broken exactly once**, on 2026-08-07,
    /// when [`ZgcPhase::ConcurrentRemap`] was inserted in cycle order and the
    /// four `sweep_*` columns moved right by four (54 → 58 columns). It was
    /// safe only because nothing in `gc/src/` calls
    /// [`ZgcMetrics::to_tsv_row`] yet, so no aggregation existed to misalign.
    /// The module header has the reasoning. The rule binds from the first
    /// emitted suite row onwards; after that, a new phase goes at the end of
    /// [`ZgcPhase::ALL`] even at the cost of cycle order.
    pub fn tsv_header() -> &'static str {
        static HEADER: OnceLock<String> = OnceLock::new();
        HEADER.get_or_init(ZgcMetrics::build_tsv_header).as_str()
    }

    /// Build the header once. Split out of [`ZgcMetrics::tsv_header`] so the
    /// latter can hand back a `&'static str` (which is what the harness call
    /// sites want) without rebuilding the string on every call.
    fn build_tsv_header() -> String {
        let mut cols: Vec<String> = vec![
            "run".to_string(),
            "cycles".to_string(),
            "phases_run_concurrently".to_string(),
            "stw_ns".to_string(),
            "concurrent_ns".to_string(),
            "stw_share".to_string(),
            "total_bytes_reclaimed".to_string(),
            "live_bytes".to_string(),
            "heap_capacity_bytes".to_string(),
            "alloc_rate_bytes_per_sec".to_string(),
            "last_alloc_rate_bytes_per_sec".to_string(),
            "stall_count".to_string(),
            "stall_total_ns".to_string(),
            "stall_max_ns".to_string(),
        ];
        for phase in ZgcPhase::ALL {
            let key = phase.key();
            cols.push(format!("{key}_count"));
            cols.push(format!("{key}_total_ns"));
            cols.push(format!("{key}_max_ns"));
            cols.push(format!("{key}_p99_ns"));
        }
        cols.join("\t")
    }

    /// One TSV row: this VM's whole-run ZGC accounting.
    ///
    /// Same column order as [`ZgcMetrics::tsv_header`]. All durations are
    /// **nanoseconds** (integers, so no locale-dependent decimal separator can
    /// break a downstream parse); the two ratios are the only floats.
    pub fn to_tsv_row(&self) -> String {
        let (stall_count, stall_total_ns, stall_max_ns) = self.allocation_stalls();
        let mut cols: Vec<String> = vec![
            self.run_label(),
            self.cycles().to_string(),
            u8::from(self.phases_run_concurrently()).to_string(),
            self.total_stw_ns().to_string(),
            self.total_concurrent_ns().to_string(),
            format!("{:.6}", self.stw_share()),
            self.total_bytes_reclaimed().to_string(),
            self.live_bytes().to_string(),
            self.last_capacity_bytes.load(Ordering::Relaxed).to_string(),
            format!("{:.3}", self.allocation_rate_bytes_per_sec()),
            format!("{:.3}", self.last_allocation_rate_bytes_per_sec()),
            stall_count.to_string(),
            stall_total_ns.to_string(),
            stall_max_ns.to_string(),
        ];
        for phase in ZgcPhase::ALL {
            let stats = self.phase_stats(phase);
            cols.push(stats.count.to_string());
            cols.push(stats.total_ns.to_string());
            cols.push(stats.max_ns.to_string());
            cols.push(stats.p99_ns.to_string());
        }
        cols.join("\t")
    }

    /// Log the summary at `info` on the `zgc` target.
    ///
    /// `tracing` rather than `eprintln!` so the summary honours the run's log
    /// filter, matching [`crate::gc_metrics`]. `g1.rs`'s `print_gc_summary`
    /// uses `eprintln!` because it is called at shutdown after the subscriber
    /// may be gone; call this one while the VM is still up.
    pub fn log_summary(&self) {
        for line in self.format_summary().lines() {
            tracing::info!(target: "zgc", "{}", line);
        }
    }
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

/// RAII timer for one [`ZgcPhase`]. Records on drop.
///
/// Holds a `cratonvm_jfr::phase::PhaseSpan` for its whole lifetime, so the same
/// interval is charged to the VM-wide `gc_pause` / `gc_concurrent` category and
/// to this module's per-phase counters. Two sinks, **one** clock read pair —
/// there is no second timing system that could disagree with the first.
#[must_use = "a ZgcPhaseGuard records its phase on drop; binding it to `_` ends it immediately"]
pub struct ZgcPhaseGuard<'a> {
    metrics: &'a ZgcMetrics,
    phase: ZgcPhase,
    start: Instant,
    /// Dropped after the fields above (declaration order), which is harmless:
    /// the two sinks measure the same interval to within a few nanoseconds of
    /// destructor overhead.
    span: cratonvm_jfr::phase::PhaseSpan,
}

impl ZgcPhaseGuard<'_> {
    /// Which phase this guard is timing.
    pub fn phase(&self) -> ZgcPhase {
        self.phase
    }

    /// Whether the JFR span is actually recording (phase accounting is off by
    /// default — see `cratonvm_jfr::phase`'s `FLAG_ENABLE`). This module's own
    /// counters record either way.
    pub fn jfr_span_is_measuring(&self) -> bool {
        self.span.is_measuring()
    }

    /// End the phase now instead of at end of scope.
    pub fn end(self) {
        drop(self);
    }
}

impl Drop for ZgcPhaseGuard<'_> {
    fn drop(&mut self) {
        let nanos = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.metrics.record_phase(self.phase, nanos);
    }
}

/// RAII timer for one mutator allocation stall. Records on drop.
///
/// See [`ZgcMetrics::scoped_allocation_stall`].
#[must_use = "a ZgcStallGuard records its stall on drop; binding it to `_` ends it immediately"]
pub struct ZgcStallGuard<'a> {
    metrics: &'a ZgcMetrics,
    start: Instant,
    span: cratonvm_jfr::phase::PhaseSpan,
}

impl ZgcStallGuard<'_> {
    /// Whether the JFR span is actually recording.
    pub fn jfr_span_is_measuring(&self) -> bool {
        self.span.is_measuring()
    }

    /// End the stall now instead of at end of scope.
    pub fn end(self) {
        drop(self);
    }
}

impl Drop for ZgcStallGuard<'_> {
    fn drop(&mut self) {
        let nanos = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.metrics.record_allocation_stall(nanos);
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// `bytes / nanos` as a per-second rate. `0.0` for a zero interval — never
/// `inf`.
fn rate_per_sec(bytes: u64, nanos: u64) -> f64 {
    if nanos == 0 {
        0.0
    } else {
        bytes as f64 * 1.0e9 / nanos as f64
    }
}

/// `value` as an integer percentage of `total`. `0` when `total` is zero.
fn percent_of(value: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        ((value as u128) * 100 / (total as u128)).min(u64::MAX as u128) as u64
    }
}

/// OpenJDK's `-Xlog:gc` byte shorthand: `1026M`, `140M`, `4G`, `512K`, `96B`.
///
/// Truncating (not rounding) division, matching HotSpot's `proper_unit_for_byte_size`
/// so the numbers on a CratonVM line and a HotSpot line mean the same thing.
fn human_bytes(bytes: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * 1024 * 1024;
    if bytes >= G {
        format!("{}G", bytes / G)
    } else if bytes >= M {
        format!("{}M", bytes / M)
    } else if bytes >= K {
        format!("{}K", bytes / K)
    } else {
        format!("{bytes}B")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// No test in this module asserts an elapsed duration. Fixed wall-clock
    /// bounds in a test are a latent CI flake on a loaded shared host (the
    /// repo has a standing note about exactly this), and every value this
    /// module produces from a real clock is unbounded from below *and* above
    /// on a contended box. Assertions are on counts, ordering, classification
    /// and column arithmetic only; durations are injected via
    /// [`ZgcMetrics::record_phase`] / [`ZgcMetrics::record_allocation_stall`],
    /// which take an explicit `nanos`.
    fn metrics() -> ZgcMetrics {
        ZgcMetrics::new()
    }

    // -- phase table ------------------------------------------------------

    #[test]
    fn every_phase_has_a_distinct_index_label_and_key() {
        assert_eq!(ZgcPhase::ALL.len(), ZGC_PHASE_COUNT);
        let mut seen_keys: Vec<&str> = Vec::new();
        let mut seen_labels: Vec<&str> = Vec::new();
        for (i, phase) in ZgcPhase::ALL.iter().enumerate() {
            assert_eq!(phase.index(), i, "{phase:?} index must match ALL position");
            let key = phase.key();
            let label = phase.label();
            assert!(!key.is_empty());
            assert!(!label.is_empty());
            assert!(!seen_keys.contains(&key), "duplicate key {key:?}");
            assert!(!seen_labels.contains(&label), "duplicate label {label:?}");
            // The TSV column names are built from `key`; a tab or a space in
            // one would corrupt the header.
            assert!(
                !key.contains('\t') && !key.contains(' '),
                "TSV key {key:?} must be a bare snake_case token",
            );
            seen_keys.push(key);
            seen_labels.push(label);
            assert_eq!(ZgcPhase::from_key(key), Some(*phase));
        }
        assert_eq!(ZgcPhase::from_key("not-a-phase"), None);
    }

    #[test]
    fn stw_classification_matches_the_pause_prefix() {
        // Every OpenJDK "Pause ..." phase is STW; every "Concurrent ..." phase
        // is not. `Sweep` is ours and is STW. This pins the label and the
        // verdict together so they cannot drift, the same way
        // `gc_metrics::decision_reason` pins `label` to `is_moving`.
        for phase in ZgcPhase::ALL {
            let by_label = phase.label().starts_with("Pause ");
            let expected = by_label || phase == ZgcPhase::Sweep;
            assert_eq!(
                phase.is_stw(),
                expected,
                "{phase:?}: label {:?} disagrees with is_stw()",
                phase.label(),
            );
        }
        assert!(ZgcPhase::Sweep.is_stw(), "the mark-sweep reclaim is STW");
        assert!(!ZgcPhase::ConcurrentRelocate.is_stw());
    }

    // FINDING 8's first half is gone with the simulation it cross-checked
    // (2026-09-02).
    //
    // `every_simulation_phase_except_idle_has_a_metrics_counter` matched
    // exhaustively over `zgc::ZgcPhase` -- the SIMULATION's phase state
    // variable -- so that adding a variant there without deciding where its
    // nanoseconds landed was a compile error here. That was a real gate and it
    // caught a real gap (`ConcurrentRemap` had a phase and no counter).
    //
    // Its subject no longer exists: the simulation was deleted, and the
    // collector that runs (`ZgcRealHeap`) drives this module directly. The
    // obligation the test encoded now belongs to whichever phase machine a
    // future concurrent collector uses; `ZgcPhase::ALL` and the
    // label/`is_stw` pinning above are what remain, and they are about THIS
    // module's own consistency rather than about a second enum's.

    /// FINDING 8, second half: the remap phase is a *concurrent* counter with
    /// its own four TSV columns, not an alias of anything.
    ///
    /// If a future collector genuinely folds remap into the next cycle's mark
    /// (OpenJDK's behaviour), this phase reports `count=0` and the report says
    /// so — which is the reading an absent column cannot give.
    #[test]
    fn the_remap_phase_is_concurrent_by_design_and_has_its_own_tsv_columns() {
        assert_eq!(
            ZgcPhase::from_key("concurrent_remap"),
            Some(ZgcPhase::ConcurrentRemap)
        );
        assert!(
            !ZgcPhase::ConcurrentRemap.is_stw(),
            "remap is driven by the load barrier alongside mutators; charging it \
             STW by design would overstate the pause",
        );

        let header = ZgcMetrics::tsv_header();
        for suffix in ["count", "total_ns", "max_ns", "p99_ns"] {
            let col = format!("concurrent_remap_{suffix}");
            assert!(
                header.split('\t').any(|c| c == col),
                "TSV header is missing {col}; remap time would vanish from every \
                 aggregation: {header}",
            );
        }

        // With real concurrency the time is concurrent; without it, STW —
        // exactly like every other concurrent-by-design phase.
        let m = metrics();
        m.set_phases_run_concurrently(true);
        m.record_phase(ZgcPhase::ConcurrentRemap, 7_000);
        assert_eq!(m.total_concurrent_ns(), 7_000);
        assert_eq!(m.total_stw_ns(), 0);
        m.set_phases_run_concurrently(false);
        assert_eq!(m.total_stw_ns(), 7_000);
        assert_eq!(m.total_concurrent_ns(), 0);

        // A phase that never ran reports zero rather than being absent.
        let quiet = metrics();
        assert_eq!(quiet.phase_stats(ZgcPhase::ConcurrentRemap).count, 0);
        assert!(
            quiet.format_summary().contains("Concurrent Remap"),
            "a folded (never-run) remap must still get a summary row saying so",
        );
    }

    // -- the guard --------------------------------------------------------

    #[test]
    fn guard_records_exactly_one_sample_on_drop() {
        let m = metrics();
        assert_eq!(m.phase_stats(ZgcPhase::ConcurrentMark).count, 0);
        {
            let guard = m.scoped(ZgcPhase::ConcurrentMark);
            assert_eq!(guard.phase(), ZgcPhase::ConcurrentMark);
            // Still open: nothing recorded yet. This is the ordering the whole
            // RAII contract rests on.
            assert_eq!(
                m.phase_stats(ZgcPhase::ConcurrentMark).count,
                0,
                "an OPEN guard must not have recorded",
            );
        }
        assert_eq!(
            m.phase_stats(ZgcPhase::ConcurrentMark).count,
            1,
            "dropping the guard must record exactly one sample",
        );
        // And only its own phase.
        assert_eq!(m.phase_stats(ZgcPhase::Sweep).count, 0);
    }

    #[test]
    fn explicit_end_records_once_not_twice() {
        let m = metrics();
        m.scoped(ZgcPhase::PauseMarkStart).end();
        assert_eq!(m.phase_stats(ZgcPhase::PauseMarkStart).count, 1);
    }

    #[test]
    fn stall_guard_records_on_drop() {
        let m = metrics();
        assert_eq!(m.allocation_stalls().0, 0);
        {
            let _g = m.scoped_allocation_stall();
            assert_eq!(m.allocation_stalls().0, 0, "an OPEN stall must not count");
        }
        assert_eq!(m.allocation_stalls().0, 1);
    }

    // -- STW vs concurrent ------------------------------------------------

    #[test]
    fn stw_and_concurrent_totals_split_by_classification() {
        let m = metrics();
        m.set_phases_run_concurrently(true); // pretend the real collector

        m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
        m.record_phase(ZgcPhase::PauseMarkEnd, 2_000);
        m.record_phase(ZgcPhase::PauseRelocateStart, 3_000);
        m.record_phase(ZgcPhase::Sweep, 4_000);
        m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
        m.record_phase(ZgcPhase::ConcurrentRelocate, 200_000);
        m.record_phase(ZgcPhase::ConcurrentSelectRelocationSet, 50_000);

        assert_eq!(m.total_stw_ns(), 1_000 + 2_000 + 3_000 + 4_000);
        assert_eq!(m.total_concurrent_ns(), 100_000 + 200_000 + 50_000);
        // 10_000 STW out of 360_000 total.
        assert_eq!(m.stw_share(), 10_000.0 / 360_000.0);
    }

    #[test]
    fn without_real_concurrency_every_phase_is_charged_as_stw() {
        // The default. This is the reading the ZGC-real regression
        // investigation must get: `ZgcRealHeap` holds one StopTheWorldToken
        // across the whole cycle, so a concurrent total above zero would be a
        // fabrication.
        let m = metrics();
        assert!(!m.phases_run_concurrently());
        m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
        m.record_phase(ZgcPhase::PauseMarkStart, 1_000);

        assert!(m.phase_was_stw(ZgcPhase::ConcurrentMark));
        assert_eq!(m.total_concurrent_ns(), 0);
        assert_eq!(m.total_stw_ns(), 101_000);
        assert_eq!(m.stw_share(), 1.0);

        // The design classification is untouched — only the accounting moved.
        assert!(!ZgcPhase::ConcurrentMark.is_stw());

        let summary = m.format_summary();
        assert!(summary.contains("concurrency=none"), "{summary}");
        assert!(
            summary.contains("BY CONSTRUCTION"),
            "a zero concurrent column must say why: {summary}",
        );
    }

    #[test]
    fn stw_share_is_zero_not_nan_before_anything_is_collected() {
        let m = metrics();
        assert_eq!(m.stw_share(), 0.0);
        assert!(m.stw_share().is_finite());
        assert_eq!(m.allocation_rate_bytes_per_sec(), 0.0);
        assert!(m.allocation_rate_bytes_per_sec().is_finite());
        assert!(m.last_allocation_rate_bytes_per_sec().is_finite());
    }

    // -- min / max / last / avg -------------------------------------------

    #[test]
    fn min_max_last_and_avg_track_the_recorded_samples() {
        let m = metrics();
        let p = ZgcPhase::ConcurrentRelocate;

        // Before anything: min reports 0, not the u64::MAX sentinel.
        let empty = m.phase_stats(p);
        assert_eq!(empty.count, 0);
        assert_eq!(empty.min_ns, 0, "an unset min must report 0, not u64::MAX");
        assert_eq!(empty.max_ns, 0);
        assert_eq!(empty.last_ns, 0);
        assert_eq!(empty.avg_ns(), 0);

        m.record_phase(p, 50);
        m.record_phase(p, 5);
        m.record_phase(p, 100);
        m.record_phase(p, 45); // last

        let s = m.phase_stats(p);
        assert_eq!(s.count, 4);
        assert_eq!(s.total_ns, 200);
        assert_eq!(s.min_ns, 5);
        assert_eq!(s.max_ns, 100);
        assert_eq!(s.last_ns, 45, "last is the most recent, not the largest");
        assert_eq!(s.avg_ns(), 50);
    }

    #[test]
    fn percentiles_use_nearest_rank_like_g1() {
        let m = metrics();
        let p = ZgcPhase::ConcurrentMark;
        for i in 1..=10u64 {
            m.record_phase(p, i * 10);
        }
        let s = m.phase_stats(p);
        // nearest-rank over [10..100]: p50 -> ceil(0.5*10)=5th -> 50;
        // p99 -> ceil(0.99*10)=10th -> 100. Same arithmetic as g1.rs.
        assert_eq!(s.p50_ns, 50);
        assert_eq!(s.p99_ns, 100);
        assert_eq!(s.recent_samples, 10);
    }

    // -- the ring ---------------------------------------------------------

    #[test]
    fn recent_ring_wraps_without_growing() {
        let m = metrics();
        let p = ZgcPhase::Sweep;
        let n = RECENT_SAMPLE_CAP * 3 + 7;
        for i in 0..n {
            m.record_phase(p, (i as u64) + 1);
        }
        let s = m.phase_stats(p);
        assert_eq!(
            s.count, n as u64,
            "the whole-run count is NOT bounded by the ring",
        );
        assert_eq!(
            s.recent_samples, RECENT_SAMPLE_CAP as u64,
            "the ring must saturate at its capacity rather than grow",
        );
        // Whole-run min/max survive eviction from the ring; only the
        // percentiles are windowed. Losing that distinction would make a long
        // soak's summary silently under-report its worst pause.
        assert_eq!(s.min_ns, 1);
        assert_eq!(s.max_ns, n as u64);
        assert_eq!(s.last_ns, n as u64);
        // The window holds only the most recent CAP samples, so its p50 is far
        // above the run's median.
        assert!(
            s.p50_ns > (n as u64 - RECENT_SAMPLE_CAP as u64),
            "the percentile window must be the RECENT samples: p50={} n={}",
            s.p50_ns,
            n,
        );
    }

    #[test]
    fn ring_len_never_exceeds_capacity_directly() {
        let mut ring = RecentRing::new();
        for i in 0..(RECENT_SAMPLE_CAP * 5) {
            ring.push(i as u64);
            assert!(ring.len <= RECENT_SAMPLE_CAP);
            assert!(ring.next < RECENT_SAMPLE_CAP);
            assert_eq!(ring.samples.len(), RECENT_SAMPLE_CAP, "fixed array");
        }
        assert_eq!(ring.len, RECENT_SAMPLE_CAP);
        assert_eq!(ring.snapshot().len(), RECENT_SAMPLE_CAP);
    }

    // -- cycles -----------------------------------------------------------

    #[test]
    fn record_cycle_derives_reclaimed_and_live_bytes() {
        let m = metrics();
        assert_eq!(m.cycles(), 0);

        m.record_cycle(1_000_000, 250_000, 4_000_000);
        assert_eq!(m.cycles(), 1);
        assert_eq!(m.last_bytes_reclaimed(), 750_000);
        assert_eq!(m.live_bytes(), 250_000);
        assert_eq!(m.total_bytes_reclaimed(), 750_000);

        m.record_cycle(900_000, 300_000, 4_000_000);
        assert_eq!(m.cycles(), 2);
        assert_eq!(m.last_bytes_reclaimed(), 600_000);
        assert_eq!(m.live_bytes(), 300_000);
        assert_eq!(m.total_bytes_reclaimed(), 1_350_000);
    }

    #[test]
    fn per_cycle_stw_split_is_a_subtraction_of_the_cumulative_totals() {
        let m = metrics();
        m.set_phases_run_concurrently(true);

        m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
        m.record_phase(ZgcPhase::ConcurrentMark, 10_000);
        m.record_cycle(1_000, 500, 8_000);
        let line1 = m.format_cycle_line();
        assert!(line1.contains("GC(0)"), "{line1}");

        m.record_phase(ZgcPhase::PauseMarkStart, 3_000);
        m.record_phase(ZgcPhase::ConcurrentMark, 40_000);
        m.record_cycle(2_000, 700, 8_000);

        // Cumulative totals are the sum; the SECOND cycle's own split is the
        // difference, not the total.
        assert_eq!(m.total_stw_ns(), 4_000);
        assert_eq!(m.total_concurrent_ns(), 50_000);
        assert_eq!(m.last_cycle_stw_ns.load(Ordering::Relaxed), 3_000);
        assert_eq!(m.last_cycle_concurrent_ns.load(Ordering::Relaxed), 40_000);

        let line2 = m.format_cycle_line();
        assert!(line2.contains("GC(1)"), "{line2}");
    }

    #[test]
    fn cycle_line_says_so_before_the_first_collection() {
        let m = metrics();
        let line = m.format_cycle_line();
        assert!(
            line.contains("no cycle recorded"),
            "a report before the first cycle must not look like a completed \
             cycle of zeroes: {line}",
        );
    }

    #[test]
    fn cycle_line_has_the_xlog_gc_shape() {
        let m = metrics();
        m.record_cycle(64 * 1024 * 1024, 8 * 1024 * 1024, 128 * 1024 * 1024);
        let line = m.format_cycle_line();
        assert!(line.starts_with("[GC] "), "{line}");
        assert!(
            line.contains("Garbage Collection (Allocation Rate)"),
            "{line}"
        );
        // 64M of 128M = 50%, 8M of 128M = 6%.
        assert!(line.contains("64M(50%)->8M(6%)"), "{line}");
        assert!(line.contains("stalls="), "{line}");
    }

    #[test]
    fn human_bytes_matches_the_hotspot_shorthand() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(96), "96B");
        assert_eq!(human_bytes(1024), "1K");
        assert_eq!(human_bytes(1024 * 1024), "1M");
        assert_eq!(human_bytes(1026 * 1024 * 1024), "1G");
        assert_eq!(human_bytes(140 * 1024 * 1024), "140M");
    }

    #[test]
    fn percent_of_is_zero_rather_than_a_division_by_zero() {
        assert_eq!(percent_of(10, 0), 0);
        assert_eq!(percent_of(0, 100), 0);
        assert_eq!(percent_of(50, 200), 25);
        // No overflow on a huge heap.
        assert_eq!(percent_of(u64::MAX, u64::MAX), 100);
    }

    // -- allocation stalls -------------------------------------------------

    #[test]
    fn stall_accounting_tracks_count_total_and_max() {
        let m = metrics();
        assert_eq!(m.allocation_stalls(), (0, 0, 0));

        m.record_allocation_stall(1_000);
        m.record_allocation_stall(9_000);
        m.record_allocation_stall(500);

        let (count, total, max) = m.allocation_stalls();
        assert_eq!(count, 3);
        assert_eq!(total, 10_500);
        assert_eq!(max, 9_000, "max is not the last sample");

        let summary = m.format_summary();
        assert!(summary.contains("allocation stalls: count=3"), "{summary}");
        // The averaged line must be present so a run with many small stalls
        // reads differently from a run with one enormous one.
        assert!(summary.contains("max_ms="), "{summary}");
    }

    #[test]
    fn stalls_are_independent_of_phases() {
        // A stall is mutator time, not collector time: it must not leak into
        // the phase totals, or the STW/concurrent split stops meaning what the
        // summary says it means.
        let m = metrics();
        m.record_allocation_stall(1_000_000);
        assert_eq!(m.total_stw_ns(), 0);
        assert_eq!(m.total_concurrent_ns(), 0);
        for phase in ZgcPhase::ALL {
            assert_eq!(m.phase_stats(phase).count, 0);
        }
    }

    // -- TSV ---------------------------------------------------------------

    #[test]
    fn tsv_header_and_row_have_the_same_column_count() {
        // A header/row mismatch shifts every value one column left or right
        // and silently corrupts every downstream aggregation without erroring
        // anywhere. This is the assertion that stops it.
        let m = metrics();
        let header = ZgcMetrics::tsv_header();
        let row = m.to_tsv_row();
        let header_cols: Vec<&str> = header.split('\t').collect();
        let row_cols: Vec<&str> = row.split('\t').collect();
        assert_eq!(
            header_cols.len(),
            row_cols.len(),
            "TSV header has {} columns but the row has {}",
            header_cols.len(),
            row_cols.len(),
        );
        // 14 fixed columns + 4 per phase.
        assert_eq!(header_cols.len(), 14 + 4 * ZGC_PHASE_COUNT);
        assert_eq!(header_cols[0], "run");

        // The same still holds once every counter is populated, which is the
        // case a length-only check on an empty recorder would miss.
        for (i, phase) in ZgcPhase::ALL.iter().enumerate() {
            m.record_phase(*phase, (i as u64 + 1) * 1_000);
        }
        m.record_allocation_stall(42);
        m.record_cycle(4_096, 1_024, 65_536);
        let row = m.to_tsv_row();
        assert_eq!(row.split('\t').count(), header_cols.len());
        assert!(!row.contains('\n'), "a TSV row must be one line");
    }

    #[test]
    fn tsv_header_names_every_phase_column() {
        let header = ZgcMetrics::tsv_header();
        for phase in ZgcPhase::ALL {
            for suffix in ["count", "total_ns", "max_ns", "p99_ns"] {
                let col = format!("{}_{}", phase.key(), suffix);
                assert!(
                    header.split('\t').any(|c| c == col),
                    "TSV header is missing column {col}: {header}",
                );
            }
        }
        for col in [
            "stall_count",
            "stall_total_ns",
            "stall_max_ns",
            "stw_ns",
            "concurrent_ns",
            "phases_run_concurrently",
        ] {
            assert!(
                header.split('\t').any(|c| c == col),
                "TSV header is missing column {col}",
            );
        }
    }

    #[test]
    fn run_label_is_settable_and_cannot_break_the_row() {
        let m = metrics();
        assert_eq!(m.run_label(), "zgc-real");
        assert!(m.to_tsv_row().starts_with("zgc-real\t"));

        m.set_run_label("generational");
        assert!(m.to_tsv_row().starts_with("generational\t"));

        // A label carrying a tab would inject a column.
        m.set_run_label("bad\tlabel\nhere");
        let row = m.to_tsv_row();
        assert!(!row.contains("bad\tlabel"), "{row}");
        assert!(!row.contains('\n'), "{row}");
        assert_eq!(
            row.split('\t').count(),
            ZgcMetrics::tsv_header().split('\t').count(),
        );
    }

    // -- summary -----------------------------------------------------------

    #[test]
    fn summary_lists_every_phase_with_its_stw_verdict() {
        let m = metrics();
        m.set_phases_run_concurrently(true);
        m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
        m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
        m.record_cycle(2_048, 512, 8_192);

        let summary = m.format_summary();
        for phase in ZgcPhase::ALL {
            assert!(
                summary.contains(phase.label()),
                "summary omits {:?}: {summary}",
                phase,
            );
        }
        assert!(summary.contains("[GC-SUMMARY] zgc cycles=1"), "{summary}");
        assert!(summary.contains("concurrency=real"), "{summary}");
        assert!(summary.contains("stw_share="), "{summary}");
        // Every line carries the scrapeable prefix.
        for line in summary.lines() {
            assert!(line.starts_with("[GC-SUMMARY] zgc"), "stray line: {line}");
        }
    }

    #[test]
    fn summary_states_that_percentiles_are_windowed() {
        // A p99 over the last 64 cycles of a 10 000-cycle run is a different
        // number from a p99 over the run, and a reader who does not know which
        // one they have will draw the wrong conclusion.
        let summary = metrics().format_summary();
        assert!(summary.contains("most recent"), "{summary}");
        assert!(
            summary.contains("whole-run"),
            "the summary must say which columns are whole-run: {summary}",
        );
    }

    // -- reset -------------------------------------------------------------

    #[test]
    fn reset_clears_every_counter() {
        let m = metrics();
        m.record_phase(ZgcPhase::Sweep, 1_234);
        m.record_allocation_stall(99);
        m.record_cycle(100, 10, 1_000);

        m.reset_for_test();

        assert_eq!(m.cycles(), 0);
        assert_eq!(m.total_stw_ns(), 0);
        assert_eq!(m.total_concurrent_ns(), 0);
        assert_eq!(m.allocation_stalls(), (0, 0, 0));
        assert_eq!(m.total_bytes_reclaimed(), 0);
        for phase in ZgcPhase::ALL {
            let s = m.phase_stats(phase);
            assert_eq!(s.count, 0);
            assert_eq!(s.total_ns, 0);
            assert_eq!(s.min_ns, 0);
            assert_eq!(s.max_ns, 0);
            assert_eq!(s.recent_samples, 0);
        }
    }

    #[test]
    fn metrics_is_send_and_sync() {
        // The collector shares this by `&` across mutator and GC threads; if
        // it stops being Sync the heap stops compiling, and this test names
        // the reason rather than leaving a confusing error at the use site.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ZgcMetrics>();
    }
}
