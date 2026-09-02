// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Profile-Guided Optimization (PGO) data collected during interpreted execution.
//!
//! During the interpreter warmup phase each method accumulates:
//! - **Branch counts** — taken vs. not-taken at each conditional-branch bytecode.
//! - **Receiver type counts** — receiver class frequency at each invokevirtual /
//!   invokeinterface site.
//!
//! These profiles are consumed when the JIT compiles the method:
//! - Branch counts guide code-layout decisions (prefer the hot direction as fall-through).
//! - Receiver type counts pre-populate Monomorphic Inline Cache (MIC) slots so the
//!   common-case virtual dispatch is a direct call from the very first JIT execution.
//!
//! (These paragraphs were `///` on the `use` below, so they documented the
//! import rather than the module and never appeared in the module's docs.)
//!
//! # Counter integrity — read this before treating a number here as a fact
//!
//! Every counter in this module is written by many real OS threads and read by
//! a compiler thread that holds none of their locks. The consistency the
//! readers actually get is:
//!
//! * **Within one method's [`MethodProfile`]** — [`ProfileStore::get_profile`]
//!   and [`ProfileStore::snapshot_all`] clone the four maps while holding that
//!   method's own `parking_lot::Mutex`, and every recorder takes the same
//!   mutex. A snapshot is therefore a *point-in-time consistent* image: no
//!   torn read, no half-applied increment, and successive snapshots of the
//!   same method are monotone non-decreasing. This is asserted by
//!   `concurrent_receiver_recording_is_lossless_and_monotone`.
//! * **Across methods, and between a profile and its invocation counter** —
//!   nothing. `snapshot_all` walks shard by shard and
//!   [`ProfileStore::snapshot_invocation_counts`] reads `Relaxed` atomics, so
//!   two methods in one snapshot may be from different instants.
//! * **Freshness** — never guaranteed. The profile is a *lagging* image of a
//!   running program at every read, and [`ProfileStore::invalidate_class`] can
//!   drop a method's history entirely when its class unloads.
//!
//! The consequence for a consumer: a profile read is sound as a **heuristic**
//! (which branch to lay out first, which class to seed a MIC with, which site
//! to rank first) and is never sound as a **correctness input**. Any decision
//! that would be wrong if the number were stale — a devirtualised call, an
//! elided type check — must be paired with a runtime guard that re-checks the
//! property, and the profile may only choose *which* guard to emit. See
//! `docs/jit/pgo-inlining.md`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// Global profiling enable gate (perf-critical: AUDIT CRIT-3/CRIT-5/HIGH-7)
// ---------------------------------------------------------------------------
//
// The interpreter calls `ProfileStore::record_branch` / `record_backedge`
// from ~13 sites on every conditional branch / back-edge. Even with FxHashMap
// the per-call overhead used to include a parking_lot::Mutex acquire and an
// `Arc<str>` clone for the MethodKey — together visible in the
// `interpreter_counting_loop` benchmark.
//
// The fix is twofold:
//   1. Gate the profile-recording calls behind a single AtomicBool
//      (`PROFILING_ENABLED`).  Default is `false` so warmup-free workloads
//      (microbenchmarks, AOT'd JDK code) pay only one relaxed atomic load
//      per branch.  Profilers / tiered managers flip it to `true` when
//      they want to drive JIT promotion.
//   2. Replace the global `Mutex<HashMap>` with a `RwLock<HashMap<Arc<...>>>`
//      so concurrent recorders hit the read-lock path and only the rare
//      first-time-insert path takes the write-lock.

static PROFILING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable profile recording globally.
///
/// When disabled (the default), `ProfileStore::record_branch`,
/// `record_backedge`, `record_receiver`, and `record_trip_complete`
/// all return immediately after a single relaxed atomic load.  This keeps
/// the interpreter hot loop free of HashMap / lock overhead until a
/// profiler is actively driving JIT compilation.
#[inline]
pub fn enable_profiling(b: bool) {
    PROFILING_ENABLED.store(b, Ordering::Relaxed);
}

/// Returns whether profile recording is currently enabled.
#[inline(always)]
pub fn is_profiling_enabled() -> bool {
    PROFILING_ENABLED.load(Ordering::Relaxed)
}

/// Receiver-type and call-site recording, independently of the master gate.
///
/// 2026-09-02: the master gate (`CRATONVM_TIER_PGO`) had never been on by
/// default, so `classify_receiver_shape` always saw `None` and every
/// speculative inlining decision that reads a receiver profile was inert in a
/// default run. The two maps that decision reads are recorded at INVOKE sites
/// only -- a fraction of the interpreter's branch rate -- so they are cheap
/// enough to record by default; branch and back-edge recording (a per-method
/// lock on every conditional branch) stays behind the master gate.
static RECEIVER_PROFILING_ENABLED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn enable_receiver_profiling(b: bool) {
    RECEIVER_PROFILING_ENABLED.store(b, Ordering::Relaxed);
}

/// Whether receiver-type and call-site recording is on: the master gate OR the
/// receiver gate.
#[inline(always)]
pub fn is_receiver_profiling_enabled() -> bool {
    PROFILING_ENABLED.load(Ordering::Relaxed) || RECEIVER_PROFILING_ENABLED.load(Ordering::Relaxed)
}


// ---------------------------------------------------------------------------
// Key type
// ---------------------------------------------------------------------------

/// Identifies a single method for profile-store lookup.
///
/// Methods from different classes can share the same `method_name`+`descriptor`,
/// so the `class_id` is included to disambiguate them.
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct MethodKey {
    pub class_id: u32,
    pub method_name: Arc<str>,
    pub descriptor: Arc<str>,
}

// ---------------------------------------------------------------------------
// Per-branch profile
// ---------------------------------------------------------------------------

/// Taken / not-taken counts for a single conditional-branch instruction.
#[derive(Default, Clone, Debug)]
pub struct BranchCounts {
    /// Number of times the branch target was taken (condition TRUE).
    pub taken: u32,
    /// Number of times the fall-through path was executed (condition FALSE).
    pub not_taken: u32,
}

/// Observations a branch needs before either direction may be called "usual".
///
/// Twenty is the value these predicates have always used, named here rather
/// than repeated as a literal in two bodies. Its job is to stop a ratio taken
/// from a handful of samples being read as a property of the program: at
/// twenty, one further sample moves the ratio by five points, whereas at three
/// it moves it by twenty-five. The consumers are code-layout hints, so an
/// early wrong answer costs a mis-laid-out branch, not a wrong result.
pub const BRANCH_BIAS_MIN_SAMPLES: u32 = 20;

impl BranchCounts {
    /// Total observations, saturating. Once this pins at `u32::MAX` the
    /// profile has stopped counting and both predicates below describe a lower
    /// bound rather than the program — see [`Self::is_saturated`].
    pub fn total(&self) -> u32 {
        self.taken.saturating_add(self.not_taken)
    }

    /// Whether either counter, or their sum, has pinned at `u32::MAX`.
    ///
    /// The counters saturate instead of wrapping (a wrap would invert a
    /// branch's apparent direction, which is worse than losing precision), but
    /// a pinned counter is no longer proportional to execution. Callers that
    /// care about the *ratio* rather than the direction should check this.
    pub fn is_saturated(&self) -> bool {
        self.taken == u32::MAX
            || self.not_taken == u32::MAX
            || u64::from(self.taken) + u64::from(self.not_taken) > u64::from(u32::MAX)
    }

    /// Returns `true` when the branch is overwhelmingly not-taken
    /// (taken < 10 % of total observations with at least
    /// [`BRANCH_BIAS_MIN_SAMPLES`] samples).
    ///
    /// The comparison runs in `u64`: `taken * 10` overflows `u32` at 429 496 730
    /// observations of one branch, which a hot loop in a long-running server
    /// passes. Before this, that overflow panicked in debug builds and silently
    /// inverted the answer in release ones.
    pub fn is_usually_not_taken(&self) -> bool {
        let total = self.total();
        total >= BRANCH_BIAS_MIN_SAMPLES && u64::from(self.taken) * 10 < u64::from(total)
    }

    /// Returns `true` when the branch is overwhelmingly taken
    /// (taken > 90 % of total observations with at least
    /// [`BRANCH_BIAS_MIN_SAMPLES`] samples). `u64` for the same overflow reason
    /// as [`Self::is_usually_not_taken`].
    pub fn is_usually_taken(&self) -> bool {
        let total = self.total();
        total >= BRANCH_BIAS_MIN_SAMPLES && u64::from(self.taken) * 10 > u64::from(total) * 9
    }
}

// ---------------------------------------------------------------------------
// Per-call-site receiver type profile
// ---------------------------------------------------------------------------

/// Receiver class frequency at a single invokevirtual / invokeinterface site.
/// Maps raw class_id to observation count.
/// T10.9.B: FxHashMap — class_id-keyed receiver count, hot path during
/// interpreter warmup.
pub type ReceiverCounts = FxHashMap<u32, u32>;

/// Whether the counts backing a receiver profile are still exact.
///
/// **The live store does not cap the receiver type table.** Unlike
/// `pgo::ReceiverTypeProfile` (which records at most 8 classes and drops the
/// rest), [`MethodProfile::record_receiver`] inserts every distinct class it
/// sees, so a live profile never loses a *type*: [`ReceiverProfileSummary::types`]
/// is an exact count, and a one-type reading is genuinely one type rather than
/// a full table that overflowed. That distinction matters because conflating
/// "one type seen" with "one slot left after overflow" is a wrong-code bug the
/// moment anything speculates on it.
///
/// The one way a live profile does stop being exact is *magnitude*: every
/// counter is a `u32` that pins at `u32::MAX` rather than wrapping. Once a
/// counter is pinned the recorded proportions are no longer the observed
/// proportions, and a site can read as monomorphic because its majority class
/// stopped counting rather than because the program settled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProfileFidelity {
    /// No counter and no total has reached `u32::MAX`. Shares computed from
    /// this profile are the observed shares.
    Exact,
    /// At least one counter, or the total, has pinned at `u32::MAX`. Every
    /// share computed from it understates the pinned entries and overstates
    /// the rest. Usable as a hint; never as a correctness input.
    Saturated,
}

/// A deterministic, fidelity-aware view of one call site's receiver profile.
///
/// Produced by [`summarize_receivers`]. Ranking is descending count with ties
/// broken by **ascending class id**: `FxHashMap` iteration order is not
/// deterministic, and two compilations of the same profile must plan the same
/// artifact. (`crate::classify_receiver_shape` uses the same ordering, for the
/// same reason.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReceiverProfileSummary {
    /// Saturating sum of every recorded count.
    pub observations: u32,
    /// Number of distinct receiver classes recorded. Exact — see
    /// [`ProfileFidelity`] for why the live store cannot truncate this.
    pub types: usize,
    /// Whether the counts are still proportional to execution.
    pub fidelity: ProfileFidelity,
    /// Highest-count class and its count. `None` only for an empty profile.
    pub top: Option<(u32, u32)>,
    /// Second-highest class and its count, by the same ordering.
    pub second: Option<(u32, u32)>,
}

impl ReceiverProfileSummary {
    /// Whether the counts are still exact (see [`ProfileFidelity`]).
    pub fn is_exact(&self) -> bool {
        matches!(self.fidelity, ProfileFidelity::Exact)
    }

    /// Whether `count` is at least `pct` percent of [`Self::observations`].
    ///
    /// Computed in `u64`. `count * 100` overflows `u32` at 42 949 673
    /// observations — a threshold a hot virtual site reaches in seconds — and
    /// the overflow panicked in debug builds and produced an arbitrary
    /// yes/no in release ones.
    pub fn holds_at_least_pct(&self, count: u32, pct: u32) -> bool {
        if self.observations == 0 {
            return false;
        }
        u64::from(count) * 100 >= u64::from(self.observations) * u64::from(pct)
    }

    /// Whether the top class alone holds at least `pct` percent.
    pub fn top_holds_at_least_pct(&self, pct: u32) -> bool {
        match self.top {
            Some((_, n)) => self.holds_at_least_pct(n, pct),
            None => false,
        }
    }
}

/// Summarise a call site's receiver counts deterministically.
///
/// Single pass, no allocation, no sort. An empty map yields
/// `observations == 0`, `types == 0`, `top == None` — which callers must treat
/// as *no evidence*, not as "zero receivers of some type", exactly as
/// [`CallSiteEvidence::None`] is not a count of zero.
pub fn summarize_receivers(counts: &ReceiverCounts) -> ReceiverProfileSummary {
    // Both arguments are `(class_id, count)`. Orders by descending count, ties
    // broken by ascending class id.
    fn outranks(candidate: (u32, u32), incumbent: Option<(u32, u32)>) -> bool {
        match incumbent {
            None => true,
            Some((inc_class, inc_count)) => {
                candidate.1 > inc_count || (candidate.1 == inc_count && candidate.0 < inc_class)
            }
        }
    }

    let mut top: Option<(u32, u32)> = None;
    let mut second: Option<(u32, u32)> = None;
    let mut total: u64 = 0;
    let mut any_pinned = false;
    for (&class_id, &count) in counts.iter() {
        total = total.saturating_add(u64::from(count));
        any_pinned |= count == u32::MAX;
        let candidate = (class_id, count);
        if outranks(candidate, top) {
            second = top;
            top = Some(candidate);
        } else if outranks(candidate, second) {
            second = Some(candidate);
        }
    }
    let saturated = any_pinned || total > u64::from(u32::MAX);
    ReceiverProfileSummary {
        observations: total.min(u64::from(u32::MAX)) as u32,
        types: counts.len(),
        fidelity: if saturated {
            ProfileFidelity::Saturated
        } else {
            ProfileFidelity::Exact
        },
        top,
        second,
    }
}

/// Returns the dominant receiver class (most frequent) if it holds at least
/// `min_fraction_pct` percent of total observations.
///
/// **Heuristic use only.** Two callers in `jit/src/lib.rs` use this to *seed* a
/// monomorphic inline cache (`lib.rs:12803`, `lib.rs:14408`); the seeded class
/// id is then re-checked by the cache's own `CMP` at every dispatch, so a stale
/// or mis-ranked answer costs one miss and never mis-dispatches. A consumer
/// that wants to know whether the underlying counts can be *believed* must ask
/// [`summarize_receivers`] and check [`ReceiverProfileSummary::fidelity`] —
/// this function answers from a saturated profile just as readily as from an
/// exact one.
pub fn dominant_receiver(counts: &ReceiverCounts, min_fraction_pct: u32) -> Option<u32> {
    let summary = summarize_receivers(counts);
    let (class_id, count) = summary.top?;
    if summary.holds_at_least_pct(count, min_fraction_pct) {
        Some(class_id)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-loop trip count profile
// ---------------------------------------------------------------------------

/// Trip count profile for a loop, keyed by its back-edge bytecode PC.
#[derive(Default, Clone, Debug)]
pub struct LoopTripProfile {
    /// Total number of back-edges observed (sum of all trip counts).
    pub backedge_count: u64,
    /// Number of times the loop was entered (each entry eventually produces
    /// one trip-complete event with its iteration count).
    pub entry_count: u32,
    /// Sum of per-entry trip counts (for computing average).
    pub total_trips: u64,
}

impl LoopTripProfile {
    /// Record a single back-edge execution.
    #[inline]
    pub fn record_backedge(&mut self) {
        self.backedge_count = self.backedge_count.saturating_add(1);
    }

    /// Record the trip count for one loop entry (called when the loop exits).
    #[inline]
    pub fn record_trip_complete(&mut self, trip: u32) {
        self.entry_count = self.entry_count.saturating_add(1);
        self.total_trips = self.total_trips.saturating_add(trip as u64);
    }

    /// Average trip count across all observed entries.
    pub fn avg_trip_count(&self) -> f64 {
        if self.entry_count == 0 {
            0.0
        } else {
            self.total_trips as f64 / self.entry_count as f64
        }
    }

    /// Suggest an unroll factor based on the observed trip count.
    ///
    /// Returns `None` if the loop is cold (<100 back-edges) or the average
    /// trip count is too large (>128, where unrolling gives diminishing returns).
    pub fn suggests_unroll_factor(&self, max_factor: usize) -> Option<usize> {
        if self.backedge_count < 100 {
            return None; // not hot enough
        }
        let avg = self.avg_trip_count();
        if avg > 128.0 || avg < 2.0 {
            return None; // too large or trivial
        }
        // Choose factor: avg≤8 → 4x, avg≤32 → 2x, else None
        let factor = if avg <= 8.0 {
            4usize
        } else if avg <= 32.0 {
            2
        } else {
            return None;
        };
        Some(factor.min(max_factor))
    }
}

// ---------------------------------------------------------------------------
// Per-method profile
// ---------------------------------------------------------------------------

/// Collected profile data for a single method.
/// T10.9.B: FxHashMap — bytecode-PC keys, hot path per-branch during warmup.
///
/// ## Coverage — read this before basing an inlining decision on it
///
/// The four maps below are populated by *different* interpreter hooks with
/// *different* coverage, and only while [`is_profiling_enabled`] is true (the
/// process default is **false**; the tiered manager flips it on).
///
/// | Map | Recorded at | Covers |
/// |---|---|---|
/// | `branches` | every conditional-branch opcode | all branches |
/// | `loops` | every back-edge | all loops |
/// | `receivers` | the receiver-resolution path of `invokevirtual`/`invokeinterface` | **virtual + interface call sites only** |
/// | `call_sites` | [`Self::record_call_site`] | `invokestatic` + `invokespecial` (PGO-01, 2026-08) |
///
/// `call_sites` is fed from exactly four places in
/// `vm/src/runtime/interpreter/invoke.rs`: `execute_invokestatic`,
/// `execute_invokestatic_cached` (every `invokestatic`), `execute_invoke_kind`
/// and `execute_invokevirtual_cached` gated on `is_special` (every
/// `invokespecial` — its OTHER callers, `invokevirtual`/`invokeinterface`, are
/// deliberately NOT recorded here to avoid double-counting a call site both
/// ways; they rely on `receivers` alone). `receivers` is the only per-bci
/// execution evidence for virtual/interface sites, and a monomorphic static
/// or special call has no receiver to record — that's the gap this filled.
/// Use [`Self::call_site_count`], which unifies the two sources and states
/// which one answered.
#[derive(Default)]
pub struct MethodProfile {
    /// Branch counts keyed by bytecode PC.
    pub branches: FxHashMap<usize, BranchCounts>,
    /// Receiver type counts keyed by bytecode PC of the invoke instruction.
    pub receivers: FxHashMap<usize, ReceiverCounts>,
    /// Loop trip profiles keyed by back-edge bytecode PC.
    pub loops: FxHashMap<usize, LoopTripProfile>,
    /// Execution count of the invoke instruction at each bytecode PC.
    ///
    /// Kind-agnostic: unlike [`Self::receivers`] this counts `invokestatic`
    /// and `invokespecial` too, which is what an inliner needs to tell a hot
    /// call site from a cold one inside the *same* method (per-method
    /// invocation counts cannot — a call in a rarely-taken branch of a hot
    /// method looks identical to one on the hot path).
    pub call_sites: FxHashMap<usize, u32>,
}

impl MethodProfile {
    /// Record a branch observation at `pc`.
    ///
    /// `taken` is `true` when the branch was taken (condition was true).
    #[inline]
    pub fn record_branch(&mut self, pc: usize, taken: bool) {
        let entry = self.branches.entry(pc).or_default();
        if taken {
            entry.taken = entry.taken.saturating_add(1);
        } else {
            entry.not_taken = entry.not_taken.saturating_add(1);
        }
    }

    /// Record a receiver type observation at invoke `pc`.
    ///
    /// Saturating, like every other counter in this module. It was a plain
    /// `+= 1`: at 2^32 observations of one receiver class — reachable at one
    /// hot virtual site in a long-lived server — that panicked in debug builds
    /// and **wrapped to zero** in release ones, which would have made the
    /// program's majority receiver read as its rarest and inverted every
    /// decision downstream of [`dominant_receiver`]. Pinning at `u32::MAX`
    /// merely stops the profile improving, and [`ProfileFidelity`] reports
    /// that it happened.
    #[inline]
    pub fn record_receiver(&mut self, pc: usize, class_id: u32) {
        let entry = self.receivers.entry(pc).or_default();
        let count = entry.entry(class_id).or_insert(0);
        *count = count.saturating_add(1);
    }

    /// Record a back-edge execution at the given PC.
    #[inline]
    pub fn record_backedge(&mut self, pc: usize) {
        self.loops.entry(pc).or_default().record_backedge();
    }

    /// Record a completed trip count for a loop at the given back-edge PC.
    #[inline]
    pub fn record_trip_complete(&mut self, backedge_pc: usize, trip: u32) {
        self.loops
            .entry(backedge_pc)
            .or_default()
            .record_trip_complete(trip);
    }

    /// Record one execution of the invoke instruction at `pc`.
    ///
    /// Kind-agnostic — call it for `invokestatic`/`invokespecial`/`invokevirtual`/
    /// `invokeinterface`/`invokedynamic` alike. Saturating, like every other
    /// counter here.
    #[inline]
    pub fn record_call_site(&mut self, pc: usize) {
        let e = self.call_sites.entry(pc).or_insert(0);
        *e = e.saturating_add(1);
    }
}

/// Where a [`MethodProfile::call_site_count`] answer came from. An inliner
/// that treats "no evidence" as "cold" would refuse to inline every static
/// call in the VM, so the absence of data is reported distinctly from a
/// genuine zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallSiteEvidence {
    /// A direct per-call-site execution counter ([`MethodProfile::call_sites`]).
    Direct(u32),
    /// Derived by summing the receiver-type observations at this bci. Exact
    /// for `invokevirtual`/`invokeinterface`, unavailable for every other
    /// invoke kind.
    Receivers(u32),
    /// Nothing was recorded at this bci — either the site is genuinely never
    /// executed, or profiling was off, or no hook covers its invoke kind.
    /// **Not** the same as a count of zero.
    None,
}

impl CallSiteEvidence {
    /// The observed count, or `0` when there is no evidence. Only use this
    /// where "unknown" and "cold" are genuinely interchangeable.
    pub fn count_or_zero(self) -> u32 {
        match self {
            CallSiteEvidence::Direct(n) | CallSiteEvidence::Receivers(n) => n,
            CallSiteEvidence::None => 0,
        }
    }

    /// Whether any hook actually observed this call site.
    pub fn is_observed(self) -> bool {
        !matches!(self, CallSiteEvidence::None)
    }
}

impl MethodProfile {
    /// Execution evidence for the invoke instruction at `pc`.
    ///
    /// Prefers the direct counter and falls back to summing the receiver
    /// observations, which are already recorded today for virtual/interface
    /// sites. See [`CallSiteEvidence`] for why "no data" is distinguished from
    /// "zero".
    pub fn call_site_count(&self, pc: usize) -> CallSiteEvidence {
        if let Some(&n) = self.call_sites.get(&pc) {
            return CallSiteEvidence::Direct(n);
        }
        match self.receivers.get(&pc) {
            Some(counts) => CallSiteEvidence::Receivers(
                counts.values().copied().fold(0u32, u32::saturating_add),
            ),
            None => CallSiteEvidence::None,
        }
    }

    /// Fidelity-aware summary of the receiver profile at invoke `pc`.
    ///
    /// `None` means *no receiver was ever recorded here* — profiling was off,
    /// the site never ran, or its invoke kind has no receiver
    /// (`invokestatic`/`invokespecial`/`invokedynamic` record none by
    /// construction). It is emphatically **not** "zero types observed", and a
    /// consumer that collapses the two would refuse to speculate at every
    /// virtual site in a VM that happened to start with profiling off.
    pub fn receiver_summary(&self, pc: usize) -> Option<ReceiverProfileSummary> {
        self.receivers.get(&pc).map(summarize_receivers)
    }

    /// Every call site with observed evidence of at least `min_count`
    /// executions, hottest first (ties broken by ascending bci so the order is
    /// deterministic across runs — an inliner that ranks candidates must not
    /// produce a different artifact from the same profile).
    pub fn hot_call_sites(&self, min_count: u32) -> Vec<(usize, u32)> {
        let mut pcs: Vec<usize> = self
            .call_sites
            .keys()
            .copied()
            .chain(self.receivers.keys().copied())
            .collect();
        pcs.sort_unstable();
        pcs.dedup();
        let mut out: Vec<(usize, u32)> = pcs
            .into_iter()
            .filter_map(|pc| {
                let n = self.call_site_count(pc).count_or_zero();
                if n >= min_count {
                    Some((pc, n))
                } else {
                    None
                }
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

// ---------------------------------------------------------------------------
// Global profile store
// ---------------------------------------------------------------------------

/// Number of shards for the per-method maps. Must be a power of two so
/// the `hash & (PROFILE_SHARDS-1)` mapping is a single mask.
///
/// Round-11 cross-cutting HIGH-1: sharded the previously-monolithic
/// `RwLock<FxHashMap<...>>` into 16 buckets keyed by the method's
/// `MethodKey` hash (slow path) or the SipHash fingerprint of
/// `(class_id, method_name, descriptor)` (borrowed-key fast path).
/// Concurrent recorders that hash to different shards proceed without
/// contention; the rwlock contention previously visible in
/// `record_branch_borrowed`/`get_profile` under multi-thread JIT
/// warmup is divided by 16.
///
/// Each shard preserves the same `methods` + `name_index` pair as the
/// pre-shard design, so the round-7 CRIT-3 two-phase clone-then-lock
/// pattern (for `snapshot_all` / `get_profile`) still applies — it now
/// runs per-shard.
const PROFILE_SHARDS: usize = 16;

/// Loop iterations that count as one method invocation for tier-up, used by
/// [`ProfileStore::add_loop_work`].
///
/// 32 is chosen so the shape this exists for actually crosses the bar without
/// dragging in everything else: a ~300-iteration constant-pool loop credits ~9
/// invocations per call, so a few dozen classes carry the method over the
/// default 500 threshold, while a method that loops a handful of times per call
/// still has to earn compilation mostly on call count.
const LOOP_WORK_PER_INVOCATION: u32 = 32;

/// Number of shards for the per-method invocation-counter map. Power of two so
/// the shard mapping is a single mask.
///
/// Fanned out wider than [`PROFILE_SHARDS`] because this map is hotter by
/// construction: [`ProfileStore::increment_invocation`] runs on **every**
/// interpreted invocation of a not-yet-compiled method (the cached-invoke
/// `Bytecode` arm in `vm/src/runtime/interpreter.rs` calls it before the
/// warmup-threshold test), whereas the profile shards are only touched when
/// `PROFILING_ENABLED`. The steady-state path is a shared read-lock plus one
/// relaxed `fetch_add`; the only exclusive acquisition left is the first
/// invocation of a given method, and a wide fan-out keeps that from
/// serialising unrelated methods.
const INVOCATION_SHARDS: usize = 64;

/// One shard of the invocation-counter map.
///
/// The counter cell is an [`AtomicU32`] rather than a plain `u32` so the
/// common "counter already exists" path needs only a *shared* read-lock: the
/// increment itself is an atomic RMW on the cell, not a mutation of the map.
/// Before this split the whole map sat behind one process-global
/// `parking_lot::Mutex`, so every Java method call in the VM serialised on a
/// single lock (plus a hash probe) purely to bump a counter.
struct InvocationShard {
    counts: parking_lot::RwLock<FxHashMap<u64, AtomicU32>>,
}

impl InvocationShard {
    fn new() -> Self {
        Self {
            counts: parking_lot::RwLock::new(FxHashMap::default()),
        }
    }
}

/// Shard index for a packed invocation key.
///
/// The packed key is `(class_id << 32) | method_name_descriptor_hash`, so the
/// low 32 bits are the well-distributed part; the high bits would cluster
/// every method of one class into one shard.
#[inline]
fn invocation_shard_for(packed_key: u64) -> usize {
    (packed_key as u32 as usize) & (INVOCATION_SHARDS - 1)
}

/// Increment an invocation counter cell, preserving the exact
/// `u32::saturating_add(1)` semantics of the pre-sharding implementation.
///
/// The common case is a single relaxed `fetch_add`. `fetch_add` wraps rather
/// than saturates, so the (astronomically rare — 2^32 invocations of a method
/// whose compilation never succeeded) overflow case restores the saturated
/// value. Concurrent incrementers all converge, because every one of them that
/// observes the overflow stores `u32::MAX`.
#[inline]
fn saturating_inc(cell: &AtomicU32) -> u32 {
    let prev = cell.fetch_add(1, Ordering::Relaxed);
    match prev.checked_add(1) {
        Some(next) => next,
        None => {
            cell.store(u32::MAX, Ordering::Relaxed);
            u32::MAX
        }
    }
}

/// One shard of the per-method profile store. Each shard owns its own
/// `methods` rwlock + `name_index` rwlock, identical in structure to
/// the pre-sharding monolithic store.
///
/// Round-11 cross-cutting HIGH-1: extracted from `ProfileStore` so we
/// can hold an array of these and dispatch by hash.
struct ProfileShard {
    methods: parking_lot::RwLock<FxHashMap<MethodKey, Arc<parking_lot::Mutex<MethodProfile>>>>,
    name_index:
        parking_lot::RwLock<FxHashMap<u64, (MethodKey, Arc<parking_lot::Mutex<MethodProfile>>)>>,
}

impl ProfileShard {
    fn new() -> Self {
        Self {
            methods: parking_lot::RwLock::new(FxHashMap::default()),
            name_index: parking_lot::RwLock::new(FxHashMap::default()),
        }
    }
}

/// Compute the shard index for an owned `MethodKey`.
#[inline]
fn shard_for_key(key: &MethodKey) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    (h.finish() as usize) & (PROFILE_SHARDS - 1)
}

/// Compute the shard index and 64-bit fingerprint for a borrowed
/// `(class_id, method_name, descriptor)` triple in a single hash pass.
/// The fingerprint is used as the `name_index` key, and its low bits
/// pick the shard — so a fingerprint hit on shard `s` is guaranteed to
/// belong to the same shard as a `MethodKey`-hash lookup for the same
/// triple in the slow path (verified below).
#[inline]
fn shard_and_fingerprint_for_borrowed(
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    class_id.hash(&mut h);
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    let fingerprint = h.finish();
    // Shard from fingerprint low bits. The slow path computes shard
    // via `shard_for_key` (MethodKey::hash) which uses the SAME hasher
    // and the SAME field order, so both paths land on the same shard
    // for the same triple.
    let shard = (fingerprint as usize) & (PROFILE_SHARDS - 1);
    (shard, fingerprint)
}

/// Global repository of all method profiles collected during interpreted execution.
/// T10.9.B: FxHashMap — MethodKey (ClassId+name+desc, internal) and packed u64
/// keys, hot path on every interpreter invoke.
///
/// **Lock strategy (AUDIT CRIT-3/CRIT-5/HIGH-7 fix, round-11 sharded):**
/// - Sharded across `PROFILE_SHARDS` independent rwlocks keyed by the
///   `MethodKey` hash. Contention is divided by the shard count; threads
///   recording into different methods land on different shards in the
///   common case.
/// - Per shard: outer `RwLock` so concurrent recorders share a read-lock
///   for the common "method already exists" path. Only the rare first-time
///   insert escalates to a write-lock.
/// - Each `MethodProfile` is wrapped in `Arc<parking_lot::Mutex<_>>` so the
///   inner `FxHashMap`s can be mutated per-method without serialising every
///   recorder on a single global Mutex.
/// - All recording paths short-circuit on the global `PROFILING_ENABLED`
///   atomic — interpreter hot-loops pay one relaxed load per branch when
///   profiling is off (the default).
pub struct ProfileStore {
    /// Per-shard `(methods, name_index)` pair. Picked by hashing the
    /// `MethodKey` (slow path) or the borrowed triple's fingerprint
    /// (fast path).
    shards: [ProfileShard; PROFILE_SHARDS],
    /// Per-method invocation counters for JIT warmup gating.
    /// Keyed by `(class_id << 32 | method_hash)` packed into a `u64` for fast lookup.
    ///
    /// Sharded across [`INVOCATION_SHARDS`] independent rwlocks with
    /// [`AtomicU32`] cells. This was a single process-global
    /// `parking_lot::Mutex<FxHashMap<u64, u32>>`, which every Java method call
    /// in the VM had to acquire exclusively just to bump a counter — a hard
    /// scalability ceiling on multi-threaded throughput and a measurable
    /// single-thread cost. The steady-state path is now a shared read-lock and
    /// one relaxed `fetch_add`; only a method's *first* invocation takes a
    /// write-lock, and then only on its own shard.
    invocation_counts: [InvocationShard; INVOCATION_SHARDS],
    /// PERF (round-5 vm #7): auxiliary index keyed by a 64-bit fingerprint
    /// of `(class_id, method_name, descriptor)`. See `ProfileShard::name_index`
    /// for the per-shard storage — this struct field is intentionally absent
    /// post-sharding; each shard carries its own index.
    ///
    /// round-7 fix (bug 2): the cached value is `(MethodKey,
    /// Arc<Mutex<MethodProfile>>)`. The cached `MethodKey` is compared
    /// against the requested `(class_id, &name, &descriptor)` after every
    /// fingerprint hit; on the (extremely rare) SipHash-fingerprint
    /// collision we fall through to the canonical `methods` slow path
    /// instead of silently returning the wrong method's profile slot.
    ///
    /// round-7 fix (bug 2): diagnostic counter — number of fingerprint
    /// hits that turned out to be a false positive (different
    /// `MethodKey` than the requested one) and fell through to the slow
    /// path.  Expected to be 0 in normal operation; non-zero indicates
    /// a SipHash collision (or, more likely, a logic bug).
    name_index_collisions: std::sync::atomic::AtomicU64,

    /// round-9 fix (HIGH): diagnostic counter — number of times the
    /// re-probe in the collision-counter path observed an entry that
    /// matched the queried key, indicating a legal concurrent insert
    /// raced with us between our first probe and the re-probe. This is
    /// a BENIGN race (the map is insert-only; another writer simply
    /// published the same key we were about to look up), not a logic
    /// bug, so we soft-count it instead of panicking via a
    /// `debug_assert!` as the previous round-8 code did. Non-zero
    /// values here are expected under concurrent load and do not
    /// indicate corruption.
    name_index_benign_races: std::sync::atomic::AtomicU64,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| ProfileShard::new()),
            invocation_counts: std::array::from_fn(|_| InvocationShard::new()),
            name_index_collisions: std::sync::atomic::AtomicU64::new(0),
            name_index_benign_races: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Reclaim every profile and warmup counter owned by an unloaded class.
    pub fn invalidate_class(&self, class_id: u32) {
        // Sharded by the *low* 32 bits (see `invocation_shard_for`), so a
        // class's methods are spread across every shard — all of them must be
        // swept. Cold path (class unloading), so the full walk is fine.
        for shard in &self.invocation_counts {
            shard
                .counts
                .write()
                .retain(|packed, _| (*packed >> 32) as u32 != class_id);
        }
        for shard in &self.shards {
            let mut methods = shard.methods.write();
            methods.retain(|key, _| key.class_id != class_id);
            let mut index = shard.name_index.write();
            index.retain(|_, (key, _)| key.class_id != class_id);
        }
    }

    /// round-9 fix (HIGH): observed benign re-probe races (between the
    /// first probe and the collision re-probe, another writer published
    /// an entry that matches the queried key). Exposed for diagnostics
    /// only; non-zero values are normal under concurrent load.
    pub fn name_index_benign_races(&self) -> u64 {
        self.name_index_benign_races
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// round-7 fix (bug 2): observed name-index fingerprint collisions
    /// (cached slot's `MethodKey` did not match the queried triple, so
    /// we fell through to the canonical `methods` lookup).  Exposed
    /// for diagnostics; non-zero values usually indicate either a
    /// SipHash collision (probability ~2.7e-10 per pair at 100k
    /// methods) or a logic bug.
    pub fn name_index_collisions(&self) -> u64 {
        self.name_index_collisions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment the invocation counter for a method identified by the packed key
    /// `(class_id << 32) | method_hash`. Returns the new count.
    ///
    /// Called from the interpreter's cached bytecode dispatch to gate JIT compilation
    /// behind a warmup threshold instead of compiling on the second invocation.
    #[inline]
    pub fn increment_invocation(&self, packed_key: u64) -> u32 {
        let shard = &self.invocation_counts[invocation_shard_for(packed_key)];
        // Fast path (every call after a method's first): shared read-lock, one
        // relaxed atomic RMW on the cell. Unrelated threads and unrelated
        // methods never block each other here.
        {
            let read = shard.counts.read();
            if let Some(cell) = read.get(&packed_key) {
                return saturating_inc(cell);
            }
        }
        // Slow path: this method's first invocation. Escalate to a write-lock
        // and double-check — another thread may have inserted between our
        // dropping the read-lock and acquiring the write-lock.
        let mut write = shard.counts.write();
        match write.get(&packed_key) {
            Some(cell) => saturating_inc(cell),
            None => {
                write.insert(packed_key, AtomicU32::new(1));
                1
            }
        }
    }

    /// Credit `iterations` of completed loop work to a method's invocation
    /// counter, so a method whose loops do a lot of work across MANY SHORT
    /// calls tiers up like one that is simply called often.
    ///
    /// This is the same counter [`Self::increment_invocation`] drives and the
    /// same threshold gates, deliberately: HotSpot likewise sums its invocation
    /// and back-edge counters against a single trigger. Without it a method has
    /// to earn compilation purely by call count, and a loop body is invisible —
    /// which is how Tomcat's BCEL annotation scan stayed interpreted. Its
    /// `ConstantPool.<init>` runs a ~300-iteration constant-pool loop once per
    /// class: never 500 calls, and never 1000 back-edges inside one frame, so
    /// neither the invocation counter nor the per-frame OSR counter ever fires.
    ///
    /// `iterations` is scaled down by [`LOOP_WORK_PER_INVOCATION`] so a single
    /// long-running loop cannot instantly saturate the counter and drag in
    /// every method that merely happens to contain one.
    #[inline]
    pub fn add_loop_work(&self, packed_key: u64, iterations: u32) -> u32 {
        let credit = iterations / LOOP_WORK_PER_INVOCATION;
        if credit == 0 {
            return 0;
        }
        let shard = &self.invocation_counts[invocation_shard_for(packed_key)];
        {
            let read = shard.counts.read();
            if let Some(cell) = read.get(&packed_key) {
                return cell
                    .fetch_add(credit, std::sync::atomic::Ordering::Relaxed)
                    .saturating_add(credit);
            }
        }
        let mut write = shard.counts.write();
        match write.get(&packed_key) {
            Some(cell) => cell
                .fetch_add(credit, std::sync::atomic::Ordering::Relaxed)
                .saturating_add(credit),
            None => {
                write.insert(packed_key, AtomicU32::new(credit));
                credit
            }
        }
    }

    /// Fetch (or insert) the per-method profile slot.  Returns a cheap `Arc`
    /// to the inner Mutex so callers can release the outer lock immediately.
    ///
    /// Round-11 cross-cutting HIGH-1: dispatches to one of `PROFILE_SHARDS`
    /// rwlocks based on the `MethodKey` hash. Concurrent recorders for
    /// different methods rarely contend.
    #[inline]
    fn get_or_insert(&self, key: &MethodKey) -> Arc<parking_lot::Mutex<MethodProfile>> {
        let shard = &self.shards[shard_for_key(key)];
        // Fast path: read-lock + lookup.  Most calls hit this branch.
        {
            let read = shard.methods.read();
            if let Some(slot) = read.get(key) {
                return Arc::clone(slot);
            }
        }
        // Slow path: upgrade to write-lock and double-check (another writer
        // may have inserted between us dropping the read-lock and acquiring
        // the write-lock).
        let mut write = shard.methods.write();
        if let Some(slot) = write.get(key) {
            return Arc::clone(slot);
        }
        let slot = Arc::new(parking_lot::Mutex::new(MethodProfile::default()));
        write.insert(key.clone(), Arc::clone(&slot));
        slot
    }

    /// Borrowed-key variant of [`get_or_insert`]: avoids the two
    /// `Arc::clone` atomic refcount bumps that the owned-key path pays on
    /// every profiled hit (round-5 vm #7).
    ///
    /// Strategy: the hot path is a profile *hit* (the (class, method, desc)
    /// triple has already been seen).  On a hit we never need to construct
    /// a `MethodKey` at all — we walk the map looking for the unique entry
    /// whose three fields equal the borrowed triple.  Since the underlying
    /// hash-map is keyed by `MethodKey` and we cannot probe by `(u32, &str,
    /// &str)` on stable Rust (raw-entry API is nightly), we instead keep an
    /// auxiliary `name_index` index that the cold insert path populates;
    /// hot lookups consult that index by `class_id` (its key type is a
    /// cheap `u64`) and then verify the names match by `&str` comparison
    /// — no `Arc::clone` on hit.
    ///
    /// The owned `MethodKey` is constructed exactly once per (class,
    /// method, descriptor) triple, inside the cold insert branch.
    #[inline]
    fn get_or_insert_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
    ) -> Arc<parking_lot::Mutex<MethodProfile>> {
        // Compute shard + 64-bit fingerprint in a single hash pass.
        // Round-11 cross-cutting HIGH-1: fingerprint low bits also pick
        // the shard, ensuring borrowed-key fast-path lookups land on
        // the same shard the slow `MethodKey`-keyed path would (both
        // use the same `DefaultHasher` over the same `(class_id,
        // method_name, descriptor)` tuple in the same order).
        let (shard_idx, fingerprint) =
            shard_and_fingerprint_for_borrowed(class_id, method_name, descriptor);
        let shard = &self.shards[shard_idx];

        // Lock-order discipline (round-7 CRIT-2): the canonical order
        // is `methods` > `name_index` (see `docs/lock-order.md`). All
        // code paths in this function take `methods` *before*
        // `name_index`, never the other way around.
        //
        // Fast path: probe `name_index` briefly, drop it, then take
        // `methods.read()` to verify and clone the slot.  No nested
        // acquisition — the two reads are sequential.
        //
        // round-7 fix (bug 2): the cached value is now `(MethodKey,
        // Arc<...>)`.  After a fingerprint hit we verify the cached
        // key matches the query triple; on mismatch (collision) we
        // bump the diagnostic counter and fall through to the slow
        // path, which uses the canonical `MethodKey`-equality map.
        //
        // The verification happens *under* the read-lock so we only
        // pay one `Arc::clone` on the slot (matching the pre-fix
        // hot-path cost) and no `MethodKey` clone.  Verification
        // itself is u32 compare + two `&str` equality short-circuits.
        let cached: Option<Arc<parking_lot::Mutex<MethodProfile>>> = {
            let idx = shard.name_index.read();
            match idx.get(&fingerprint) {
                Some((cached_key, slot))
                    if cached_key.class_id == class_id
                        && cached_key.method_name.as_ref() == method_name.as_ref()
                        && cached_key.descriptor.as_ref() == descriptor.as_ref() =>
                {
                    Some(Arc::clone(slot))
                }
                Some(_) => None, // collision — handled below
                None => None,
            }
        };
        if let Some(slot) = cached {
            return slot;
        }
        // Distinguish "fingerprint missed entirely" from "fingerprint
        // hit but key mismatched (collision)": re-probe briefly to bump
        // the counter only on real collisions.  The probability of
        // either is so low this re-probe never shows up in a profile.
        //
        // LOAD-BEARING: this re-probe assumes the index is INSERT-ONLY
        // (no eviction).  Between the first read above and this
        // second read, the only legal transition is "absent -> present"
        // (handled by the slow path below).  If a future change adds
        // eviction or fingerprint-slot replacement, an entry that
        // mismatched on the first probe could become an unrelated entry
        // by the second probe — same fingerprint, different real key —
        // and we'd under-count collisions.  If eviction is introduced,
        // swap this counter for an atomic increment INSIDE the first
        // read's `Some(_)` collision arm above.
        {
            let idx = shard.name_index.read();
            if let Some((cached_key, _)) = idx.get(&fingerprint) {
                // Round-9 fix (HIGH): the previous `debug_assert!` here
                // panicked on a perfectly legal benign race — between
                // our first probe and this re-probe, another writer
                // can legitimately publish an entry for the SAME key
                // we were looking up (insert-only map, slow path wins
                // the race). The assertion was guarding against
                // eviction (which the map does not perform), but the
                // matching-entry-after-mismatch case it tripped on IS
                // the benign race the comment itself describes.
                //
                // Soft-count the two cases separately so the diagnostic
                // information is preserved without crashing under load.
                if cached_key.class_id == class_id
                    && cached_key.method_name.as_ref() == method_name.as_ref()
                    && cached_key.descriptor.as_ref() == descriptor.as_ref()
                {
                    // Benign race: another writer inserted our key
                    // between the two probes. Not a collision; do not
                    // bump the collision counter.
                    self.name_index_benign_races
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    // Real fingerprint collision: different MethodKey
                    // shares our SipHash fingerprint.
                    self.name_index_collisions
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        // Slow path: take `shard.methods.write()` FIRST, then
        // `shard.name_index.write()` nested inside it.  Build the
        // owned `MethodKey` (one-time per triple) for canonical
        // insertion into `methods`.
        let slot = Arc::new(parking_lot::Mutex::new(MethodProfile::default()));
        let key = MethodKey {
            class_id,
            method_name: Arc::clone(method_name),
            descriptor: Arc::clone(descriptor),
        };
        let mut write = shard.methods.write();
        // Race: another writer may have inserted between us dropping the
        // index read-lock and acquiring `methods.write()`.  Re-probe
        // under the exclusive lock and return the existing slot if so.
        if let Some(existing) = write.get(&key) {
            let existing = Arc::clone(existing);
            // methods (held) > name_index (canonical order).
            //
            // round-7 fix (bug 2): only overwrite the name_index entry
            // if its current fingerprint slot is empty or already
            // points at this same key — we must not stomp a different
            // method's slot just because our fingerprint collides
            // with theirs.  When there's already a conflicting entry
            // we leave it in place; the next borrowed-lookup for the
            // *other* method will hit, verify, and return correctly,
            // while lookups for *this* method will collide-then-fall-
            // through to here, which is correct (if slow).
            let mut idx = shard.name_index.write();
            let should_insert = match idx.get(&fingerprint) {
                None => true,
                Some((existing_key, _)) => existing_key == &key,
            };
            if should_insert {
                idx.insert(fingerprint, (key, Arc::clone(&existing)));
            }
            return existing;
        }
        write.insert(key.clone(), Arc::clone(&slot));
        // Still under methods.write() — nested name_index.write() in
        // canonical order.
        //
        // round-7 fix (bug 2): same conflict check as above before
        // inserting our key into the auxiliary index.
        let mut idx = shard.name_index.write();
        let should_insert = match idx.get(&fingerprint) {
            None => true,
            Some((existing_key, _)) => existing_key == &key,
        };
        if should_insert {
            idx.insert(fingerprint, (key, Arc::clone(&slot)));
        }
        drop(idx);
        drop(write);
        slot
    }

    /// Borrowed-key counterpart of [`record_branch`] — see
    /// [`get_or_insert_borrowed`] for the rationale.
    #[inline]
    pub fn record_branch_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        taken: bool,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_branch(pc, taken);
    }

    /// Borrowed-key counterpart of [`record_backedge`].
    #[inline]
    pub fn record_backedge_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        backedge_pc: usize,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_backedge(backedge_pc);
    }

    /// Borrowed-key counterpart of [`record_receiver`].
    #[inline]
    pub fn record_receiver_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        receiver_class_id: u32,
    ) {
        if !is_receiver_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_receiver(pc, receiver_class_id);
    }

    /// Borrowed-key counterpart of [`record_call_site`](Self::record_call_site).
    ///
    /// Intended to be called from the interpreter's invoke dispatch for EVERY
    /// invoke kind — see [`MethodProfile::call_sites`] for why per-method
    /// invocation counts cannot substitute.
    #[inline]
    pub fn record_call_site_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
    ) {
        if !is_receiver_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        slot.lock().record_call_site(pc);
    }

    /// Record one execution of the invoke instruction at `pc`.
    #[inline]
    pub fn record_call_site(&self, key: &MethodKey, pc: usize) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_call_site(pc);
    }

    /// Record a branch observation.  Called from the interpreter hot-loop.
    ///
    /// Returns immediately when profiling is disabled (the global default),
    /// keeping the interpreter dispatch loop free of HashMap/lock traffic.
    #[inline]
    pub fn record_branch(&self, key: &MethodKey, pc: usize, taken: bool) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_branch(pc, taken);
    }

    /// Record a receiver type observation.  Called from the interpreter hot-loop.
    #[inline]
    pub fn record_receiver(&self, key: &MethodKey, pc: usize, class_id: u32) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_receiver(pc, class_id);
    }

    /// Record a loop back-edge execution.  Called from the interpreter hot-loop.
    #[inline]
    pub fn record_backedge(&self, key: &MethodKey, backedge_pc: usize) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_backedge(backedge_pc);
    }

    /// Record a completed loop trip count.  Called when a loop exits.
    #[inline]
    pub fn record_trip_complete(&self, key: &MethodKey, backedge_pc: usize, trip: u32) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert(key);
        slot.lock().record_trip_complete(backedge_pc, trip);
    }

    /// Retrieve a snapshot of the profile for the given method, if any.
    ///
    /// Round-7 CRIT-3 sibling fix: same two-phase pattern as
    /// `snapshot_all` — clone the `Arc<Mutex<...>>` under the outer
    /// read-lock, drop the outer guard, then lock the per-slot Mutex
    /// independently.  Prevents the per-slot lock from being acquired
    /// while `methods.read()` is held.
    pub fn get_profile(&self, key: &MethodKey) -> Option<MethodProfile> {
        // Round-11 cross-cutting HIGH-1: dispatch to the owning shard.
        let shard = &self.shards[shard_for_key(key)];
        let slot_arc = {
            let read = shard.methods.read();
            Arc::clone(read.get(key)?)
        };
        let p = slot_arc.lock();
        // Snapshot: clone branch + receiver + loop maps
        Some(MethodProfile {
            branches: p.branches.clone(),
            receivers: p.receivers.clone(),
            loops: p.loops.clone(),
            call_sites: p.call_sites.clone(),
        })
    }

    /// Snapshot all method profiles: returns (MethodKey, MethodProfile) pairs.
    /// Used by AOT training to bulk-sync JIT profile data to the AOT recorder.
    ///
    /// Round-7 CRIT-3 fix: two-phase snapshot.  Phase 1 clones the (key, Arc)
    /// pairs under the outer read-lock and drops it.  Phase 2 locks each
    /// per-slot Mutex with the outer guard already released — so no thread
    /// holding a slot lock can deadlock against a writer trying to take
    /// `methods.write()`.
    pub fn snapshot_all(&self) -> Vec<(MethodKey, MethodProfile)> {
        // Round-11 cross-cutting HIGH-1: walk every shard. The
        // two-phase clone-then-lock pattern still applies per shard so
        // no per-slot Mutex is held while the shard's outer rwlock is
        // also held.
        let mut arcs: Vec<(MethodKey, Arc<parking_lot::Mutex<MethodProfile>>)> = Vec::new();
        for shard in self.shards.iter() {
            // Phase 1: snapshot the (key, Arc<Mutex<...>>) pairs.
            // Holding only the shard's outer read-lock — no per-slot
            // lock taken yet.
            let read = shard.methods.read();
            arcs.extend(read.iter().map(|(k, slot)| (k.clone(), Arc::clone(slot))));
            // Outer shard lock drops at end of scope; next iteration
            // moves to a different shard's lock entirely.
        }
        // Phase 2: outer locks released; iterate locking each slot
        // independently.  Slot lock is now leaf-level.
        arcs.into_iter()
            .map(|(k, slot)| {
                let p = slot.lock();
                (
                    k,
                    MethodProfile {
                        branches: p.branches.clone(),
                        receivers: p.receivers.clone(),
                        loops: p.loops.clone(),
                        call_sites: p.call_sites.clone(),
                    },
                )
            })
            .collect()
    }

    /// Snapshot all invocation counts: returns (packed_key, count) pairs.
    pub fn snapshot_invocation_counts(&self) -> Vec<(u64, u32)> {
        // Not a single atomic snapshot across shards — it never was: the
        // pre-sharding version held one lock, but callers (diagnostics /
        // tiered-manager reporting) already tolerated counters advancing
        // concurrently. Per-shard consistency is preserved.
        let mut out = Vec::new();
        for shard in &self.invocation_counts {
            let counts = shard.counts.read();
            out.extend(
                counts
                    .iter()
                    .map(|(&k, cell)| (k, cell.load(Ordering::Relaxed))),
            );
        }
        out
    }
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests record into the global ProfileStore — gate must be on or
    /// `record_*` is a no-op.  Each test calls this guard before driving
    /// the store; the inner `Mutex` serialises gate transitions so a
    /// parallel test that depends on the disabled-default doesn't observe
    /// `true` mid-run.
    fn with_profiling_enabled<R>(f: impl FnOnce() -> R) -> R {
        let _g = GATE.lock();
        let prev = is_profiling_enabled();
        enable_profiling(true);
        let r = f();
        enable_profiling(prev);
        r
    }

    /// Serialises every gate transition in this module. Hoisted out of
    /// `with_profiling_enabled` so the disabled-side helper below shares it —
    /// two helpers with private statics would not exclude each other, and a
    /// parallel test would then observe the wrong gate state.
    static GATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Inverse of [`with_profiling_enabled`], for tests that assert a recorder
    /// is a no-op while the global gate is off (its process default).
    fn with_profiling_disabled<R>(f: impl FnOnce() -> R) -> R {
        let _g = GATE.lock();
        let prev = is_profiling_enabled();
        enable_profiling(false);
        let r = f();
        enable_profiling(prev);
        r
    }

    fn make_key(class_id: u32) -> MethodKey {
        MethodKey {
            class_id,
            method_name: Arc::from("test"),
            descriptor: Arc::from("()V"),
        }
    }

    #[test]
    fn test_branch_counts_thresholds() {
        // Not enough samples — neither flag set.
        let sparse = BranchCounts {
            taken: 5,
            not_taken: 5,
        };
        assert!(!sparse.is_usually_taken());
        assert!(!sparse.is_usually_not_taken());

        // Usually taken (>90 %).
        let hot = BranchCounts {
            taken: 95,
            not_taken: 5,
        };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());

        // Usually not taken (<10 %).
        let cold = BranchCounts {
            taken: 1,
            not_taken: 99,
        };
        assert!(!cold.is_usually_taken());
        assert!(cold.is_usually_not_taken());

        // Borderline — exactly 90 % (not strictly > 90 %).
        let borderline = BranchCounts {
            taken: 18,
            not_taken: 2,
        };
        assert!(!borderline.is_usually_taken(), "90% is not >90%");
    }

    #[test]
    fn test_dominant_receiver_above_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 85);
        counts.insert(2, 15);
        // 85% ≥ 80% threshold → dominant = class 1
        assert_eq!(dominant_receiver(&counts, 80), Some(1));
    }

    #[test]
    fn test_dominant_receiver_below_threshold() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 70);
        counts.insert(2, 30);
        // 70% < 80% threshold → no dominant receiver
        assert_eq!(dominant_receiver(&counts, 80), None);
    }

    #[test]
    fn test_profile_store_branch_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(42);
            for _ in 0..15 {
                store.record_branch(&key, 10, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 10, false);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            assert_eq!(counts.taken, 15);
            assert_eq!(counts.not_taken, 5);
            assert!(!counts.is_usually_taken(), "<20 samples");

            // Add enough to make it >90% taken.
            for _ in 0..80 {
                store.record_branch(&key, 10, true);
            }
            let profile = store.get_profile(&key).unwrap();
            let counts = &profile.branches[&10];
            // 95 taken / 5 not-taken = 95% > 90% with ≥20 samples
            assert_eq!(counts.taken, 95);
            assert!(counts.is_usually_taken());
        });
    }

    #[test]
    fn test_profile_store_receiver_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(7);
            for _ in 0..90 {
                store.record_receiver(&key, 5, 100);
            }
            for _ in 0..10 {
                store.record_receiver(&key, 5, 200);
            }
            let profile = store.get_profile(&key).unwrap();
            let rcounts = &profile.receivers[&5];
            assert_eq!(dominant_receiver(rcounts, 80), Some(100));
        });
    }

    /// Disabled-by-default sanity: with PROFILING_ENABLED=false the recorder
    /// must remain a no-op (no MethodKey clone, no HashMap insert).
    ///
    /// Uses the same gate as `with_profiling_enabled` to avoid racing
    /// against tests that flip the flag on.
    #[test]
    fn profiling_disabled_is_noop() {
        with_profiling_enabled(|| {
            // Within the guard, switch the gate back off for this scope so
            // we can assert the no-op behaviour without a parallel test
            // flipping it on between calls.
            enable_profiling(false);
            let store = ProfileStore::new();
            let key = make_key(123);
            for _ in 0..100 {
                store.record_branch(&key, 1, true);
                store.record_backedge(&key, 1);
                store.record_receiver(&key, 1, 0);
                store.record_trip_complete(&key, 1, 4);
            }
            assert!(
                store.get_profile(&key).is_none(),
                "no profile entry should be created while profiling is disabled"
            );
        });
    }

    // ===== M29 snapshot_all / snapshot_invocation_counts =====

    #[test]
    fn m29_snapshot_all_returns_all_methods() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let k1 = make_key(1);
            let k2 = MethodKey {
                class_id: 2,
                method_name: Arc::from("other"),
                descriptor: Arc::from("(I)V"),
            };
            store.record_branch(&k1, 5, true);
            store.record_branch(&k2, 10, false);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 2);
        });
    }

    #[test]
    fn m29_snapshot_all_empty_store() {
        let store = ProfileStore::new();
        let all = store.snapshot_all();
        assert!(all.is_empty());
    }

    #[test]
    fn m29_snapshot_invocation_counts() {
        let store = ProfileStore::new();
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0001_0000_ABCD);
        store.increment_invocation(0x0002_0000_1234);
        let counts = store.snapshot_invocation_counts();
        assert_eq!(counts.len(), 2);
        let abcd = counts.iter().find(|(k, _)| *k == 0x0001_0000_ABCD);
        assert_eq!(abcd.unwrap().1, 2);
    }

    // --- Sharded invocation counters (global-Mutex removal) ----------------

    /// Every increment must be observed exactly once under concurrency. The
    /// old implementation held one process-global `Mutex` for this; the
    /// sharded version relies on a per-shard read-lock plus an atomic RMW, so
    /// a lost update here would mean methods warm up slower than their real
    /// call count (or never reach the JIT threshold).
    #[test]
    fn invocation_counter_concurrent_increments_are_exact() {
        const THREADS: usize = 8;
        const PER_THREAD: u32 = 2_000;
        let store = Arc::new(ProfileStore::new());
        // Two keys that differ ONLY in the high (class_id) half: they must
        // share a shard, since `invocation_shard_for` masks the low 32 bits.
        // This is the case most likely to expose a lost update.
        let key_a = 0x0000_0001_DEAD_BEEFu64;
        let key_b = 0x0000_0002_DEAD_BEEFu64;
        assert_eq!(
            invocation_shard_for(key_a),
            invocation_shard_for(key_b),
            "keys differing only in the class_id half must collide on one shard"
        );

        let mut handles = Vec::new();
        for t in 0..THREADS {
            let store = Arc::clone(&store);
            let key = if t % 2 == 0 { key_a } else { key_b };
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    store.increment_invocation(key);
                }
            }));
        }
        for h in handles {
            h.join().expect("increment worker should not panic");
        }

        let counts = store.snapshot_invocation_counts();
        let expected = PER_THREAD * (THREADS as u32 / 2);
        for key in [key_a, key_b] {
            let got = counts
                .iter()
                .find(|(k, _)| *k == key)
                .expect("key should be present")
                .1;
            assert_eq!(got, expected, "lost update on key {key:#x}");
        }
    }

    /// The counter returned by `increment_invocation` is what the interpreter
    /// compares against the warmup threshold, so it must be the *new* value
    /// and must advance by exactly one per call.
    #[test]
    fn invocation_counter_returns_monotonic_new_value() {
        let store = ProfileStore::new();
        for expected in 1..=64u32 {
            assert_eq!(store.increment_invocation(0xFEED_FACE), expected);
        }
    }

    /// Saturating semantics are preserved from the pre-sharding
    /// `u32::saturating_add(1)` implementation: the counter pins at `u32::MAX`
    /// instead of wrapping to 0 (which would restart JIT warmup).
    #[test]
    fn invocation_counter_saturates_instead_of_wrapping() {
        let store = ProfileStore::new();
        let key = 0x0BAD_C0DEu64;
        // Seed the cell directly at MAX-1 rather than calling increment 4
        // billion times.
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard
                .counts
                .write()
                .insert(key, AtomicU32::new(u32::MAX - 1));
        }
        assert_eq!(store.increment_invocation(key), u32::MAX);
        // Further increments stay pinned, and never wrap through 0.
        for _ in 0..4 {
            assert_eq!(store.increment_invocation(key), u32::MAX);
        }
    }

    /// `invalidate_class` must sweep *every* shard: because the shard index
    /// comes from the low 32 bits, one class's methods are spread across all
    /// of them. A single-shard sweep would leak counters for an unloaded
    /// class and let a recycled `class_id` inherit stale warmup state.
    #[test]
    fn invalidate_class_sweeps_all_shards() {
        let store = ProfileStore::new();
        // 256 methods of class 7 — with 64 shards this reliably populates
        // many distinct shards.
        for m in 0..256u64 {
            store.increment_invocation((7u64 << 32) | m);
        }
        // A second class that must survive the sweep.
        for m in 0..256u64 {
            store.increment_invocation((9u64 << 32) | m);
        }
        assert_eq!(store.snapshot_invocation_counts().len(), 512);

        store.invalidate_class(7);

        let remaining = store.snapshot_invocation_counts();
        assert_eq!(remaining.len(), 256, "class 7 counters should all be gone");
        assert!(
            remaining.iter().all(|(k, _)| (*k >> 32) as u32 == 9),
            "only class 9 counters should remain"
        );
    }

    #[test]
    fn m29_snapshot_all_preserves_branch_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(99);
            for _ in 0..10 {
                store.record_branch(&key, 42, true);
            }
            for _ in 0..5 {
                store.record_branch(&key, 42, false);
            }
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let counts = &profile.branches[&42];
            assert_eq!(counts.taken, 10);
            assert_eq!(counts.not_taken, 5);
        });
    }

    #[test]
    fn m29_snapshot_all_preserves_receiver_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(88);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 500);
            store.record_receiver(&key, 20, 600);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let rcounts = &profile.receivers[&20];
            assert_eq!(rcounts[&500], 2);
            assert_eq!(rcounts[&600], 1);
        });
    }

    // ===== S38 loop trip profile tests =====

    #[test]
    fn s38_loop_trip_profile_basic() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        assert_eq!(ltp.backedge_count, 50);
        assert_eq!(ltp.entry_count, 0);
        assert_eq!(ltp.avg_trip_count(), 0.0);
    }

    #[test]
    fn s38_loop_trip_suggests_unroll() {
        let mut ltp = LoopTripProfile::default();
        // Simulate a loop with avg trip count 8: 100 entries × 8 trips = 800 back-edges
        for _ in 0..800 {
            ltp.record_backedge();
        }
        for _ in 0..100 {
            ltp.record_trip_complete(8);
        }
        assert!((ltp.avg_trip_count() - 8.0).abs() < 0.01);
        assert_eq!(ltp.suggests_unroll_factor(8), Some(4)); // avg≤8 → 4x
    }

    #[test]
    fn s38_loop_trip_no_unroll_cold() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..50 {
            ltp.record_backedge();
        }
        // < 100 back-edges → not hot enough
        assert_eq!(ltp.suggests_unroll_factor(8), None);
    }

    #[test]
    fn s38_loop_trip_no_unroll_large_trip() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..200 {
            ltp.record_backedge();
        }
        for _ in 0..2 {
            ltp.record_trip_complete(100); // avg = 100
        }
        // avg > 128? No, avg=100. But the factor logic: avg≤8→4, avg≤32→2, else None
        // avg=100 > 32 → None
        assert_eq!(ltp.suggests_unroll_factor(8), None);
    }

    #[test]
    fn s38_loop_trip_medium_suggests_2x() {
        let mut ltp = LoopTripProfile::default();
        for _ in 0..500 {
            ltp.record_backedge();
        }
        for _ in 0..25 {
            ltp.record_trip_complete(20); // avg = 20
        }
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2)); // avg≤32 → 2x
    }

    #[test]
    fn s38_profile_store_backedge_accumulates() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(55);
            for _ in 0..200 {
                store.record_backedge(&key, 42);
            }
            for _ in 0..10 {
                store.record_trip_complete(&key, 42, 20);
            }
            let profile = store.get_profile(&key).unwrap();
            let ltp = &profile.loops[&42];
            assert_eq!(ltp.backedge_count, 200);
            assert_eq!(ltp.entry_count, 10);
            assert!((ltp.avg_trip_count() - 20.0).abs() < 0.01);
        });
    }

    #[test]
    fn s38_snapshot_all_preserves_loop_data() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(66);
            for _ in 0..100 {
                store.record_backedge(&key, 10);
            }
            store.record_trip_complete(&key, 10, 50);
            let all = store.snapshot_all();
            assert_eq!(all.len(), 1);
            let (_, profile) = &all[0];
            let ltp = &profile.loops[&10];
            assert_eq!(ltp.backedge_count, 100);
            assert_eq!(ltp.entry_count, 1);
        });
    }

    /// T10.9.B — smoke test: ProfileStore uses FxHashMap for methods/invocation_counts.
    /// Verifies insert/lookup semantics are preserved after the HashMap → FxHashMap swap.
    #[test]
    fn t10_9_b_fx_hashmap_swap_smoke() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            // Populate a few hundred methods and invocation counts.
            for i in 0..300u32 {
                let key = make_key(i);
                store.record_branch(&key, (i as usize) * 4, i % 2 == 0);
                store.increment_invocation((i as u64) << 32);
            }
            // Snapshot should show all 300 methods.
            let snap = store.snapshot_all();
            assert_eq!(snap.len(), 300, "every inserted method should be present");
            let ivc = store.snapshot_invocation_counts();
            assert_eq!(ivc.len(), 300, "every invocation slot should be present");
            // Spot-check a specific key survives the hash migration.
            let k = make_key(42);
            let profile = store.get_profile(&k).unwrap();
            assert!(
                profile.branches.contains_key(&(42usize * 4)),
                "FxHashMap lookup should find the inserted PC"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Per-call-site evidence
    // -----------------------------------------------------------------------

    #[test]
    fn call_site_evidence_distinguishes_absent_from_zero() {
        let p = MethodProfile::default();
        assert_eq!(p.call_site_count(7), CallSiteEvidence::None);
        assert!(!p.call_site_count(7).is_observed());
        assert_eq!(p.call_site_count(7).count_or_zero(), 0);
    }

    #[test]
    fn call_site_evidence_prefers_the_direct_counter() {
        let mut p = MethodProfile::default();
        // A virtual site with receiver observations only.
        p.record_receiver(4, 100);
        p.record_receiver(4, 100);
        p.record_receiver(4, 200);
        assert_eq!(p.call_site_count(4), CallSiteEvidence::Receivers(3));
        // Once the direct counter exists it wins (it is kind-agnostic and
        // counts executions the receiver hook may not see).
        p.record_call_site(4);
        assert_eq!(p.call_site_count(4), CallSiteEvidence::Direct(1));
    }

    #[test]
    fn call_site_counter_covers_static_calls_receivers_cannot() {
        let mut p = MethodProfile::default();
        for _ in 0..5 {
            p.record_call_site(12); // an invokestatic — no receiver to record
        }
        assert_eq!(p.call_site_count(12), CallSiteEvidence::Direct(5));
        assert!(
            p.receivers.is_empty(),
            "a static call site contributes no receiver evidence"
        );
    }

    #[test]
    fn hot_call_sites_is_ordered_and_deterministic() {
        let mut p = MethodProfile::default();
        for _ in 0..10 {
            p.record_call_site(30);
        }
        for _ in 0..10 {
            p.record_call_site(8); // ties with pc 30 — lower bci must win
        }
        for _ in 0..50 {
            p.record_call_site(20);
        }
        p.record_call_site(99); // below the threshold
        p.record_receiver(40, 7); // receiver-derived, also below threshold
        assert_eq!(p.hot_call_sites(10), vec![(20, 50), (8, 10), (30, 10)]);
        assert_eq!(p.hot_call_sites(1).len(), 5);
        // Same profile, same order — an inliner ranking candidates must not
        // produce a different artifact from run to run.
        assert_eq!(p.hot_call_sites(10), p.hot_call_sites(10));
    }

    #[test]
    fn call_site_counts_saturate_rather_than_wrap() {
        let mut p = MethodProfile::default();
        p.call_sites.insert(3, u32::MAX);
        p.record_call_site(3);
        assert_eq!(p.call_site_count(3), CallSiteEvidence::Direct(u32::MAX));
    }

    #[test]
    fn store_records_and_snapshots_call_sites() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let key = make_key(11);
            store.record_call_site(&key, 16);
            store.record_call_site(&key, 16);
            store.record_call_site(&key, 24);
            let p = store.get_profile(&key).expect("profile recorded");
            assert_eq!(p.call_site_count(16), CallSiteEvidence::Direct(2));
            assert_eq!(p.call_site_count(24), CallSiteEvidence::Direct(1));
            // The bulk snapshot must carry the new map too — AOT training and
            // the tiered manager both read it that way.
            let snap = store.snapshot_all();
            let (_, sp) = snap
                .into_iter()
                .find(|(k, _)| k.class_id == key.class_id)
                .expect("method present in snapshot_all");
            assert_eq!(sp.call_site_count(16), CallSiteEvidence::Direct(2));
        });
    }

    #[test]
    fn call_site_recording_respects_the_global_profiling_gate() {
        // Profiling defaults to OFF; the recorder must be a single atomic load.
        with_profiling_disabled(|| {
            let store = ProfileStore::new();
            let key = make_key(77);
            store.record_call_site(&key, 4);
            assert!(
                store.get_profile(&key).is_none(),
                "nothing may be recorded while profiling is disabled"
            );
        });
    }

    // -----------------------------------------------------------------------
    // Counter arithmetic: the regime where these numbers used to lie
    // -----------------------------------------------------------------------

    /// `dominant_receiver` computed `count * 100` in `u32`, which overflows at
    /// 42 949 673 observations — a threshold one hot virtual site passes in
    /// seconds. In a debug build that panicked inside the JIT's profile read;
    /// in a release build it wrapped and answered arbitrarily.
    #[test]
    fn dominant_receiver_survives_counts_past_the_u32_multiply_overflow() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 3_000_000_000);
        counts.insert(2, 100);
        assert_eq!(dominant_receiver(&counts, 80), Some(1));

        // And the negative answer is still correct at that scale: a 50/50 split
        // of four billion observations is not 80 % dominant.
        let mut split = ReceiverCounts::default();
        split.insert(1, 2_000_000_000);
        split.insert(2, 2_000_000_000);
        assert_eq!(dominant_receiver(&split, 80), None);
    }

    /// `max_by_key` over an `FxHashMap` returns whichever tied entry came last
    /// in iteration order, so two compilations of the same profile could seed
    /// different inline caches. Ties now break on ascending class id.
    #[test]
    fn dominant_receiver_tie_break_is_deterministic() {
        let mut counts = ReceiverCounts::default();
        counts.insert(31, 500);
        counts.insert(4, 500);
        counts.insert(12, 5);
        for _ in 0..32 {
            assert_eq!(dominant_receiver(&counts, 40), Some(4));
        }
    }

    #[test]
    fn dominant_receiver_of_an_all_zero_profile_is_none() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 0);
        assert_eq!(dominant_receiver(&counts, 1), None);
    }

    /// Receiver counts were the one counter here that did not saturate.
    #[test]
    fn receiver_counts_saturate_rather_than_wrap() {
        let mut p = MethodProfile::default();
        p.receivers.entry(3).or_default().insert(9, u32::MAX);
        p.record_receiver(3, 9);
        assert_eq!(p.receivers[&3][&9], u32::MAX, "must pin, not wrap to 0");

        let summary = p.receiver_summary(3).expect("site was recorded");
        assert_eq!(summary.fidelity, ProfileFidelity::Saturated);
        assert!(!summary.is_exact());
    }

    /// Saturation is also reachable through the *total* with no single counter
    /// pinned, so the summary checks both.
    #[test]
    fn summary_reports_saturation_from_the_total_alone() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 3_000_000_000);
        counts.insert(2, 3_000_000_000);
        assert!(counts.values().all(|&n| n != u32::MAX));
        let summary = summarize_receivers(&counts);
        assert_eq!(summary.fidelity, ProfileFidelity::Saturated);
        assert_eq!(summary.observations, u32::MAX);
    }

    /// `taken * 10` / `total * 9` overflow `u32` at ~430 M observations of one
    /// branch, which a hot interpreted loop reaches.
    #[test]
    fn branch_bias_predicates_do_not_overflow_u32() {
        let hot = BranchCounts {
            taken: 1_000_000_000,
            not_taken: 10,
        };
        assert!(hot.is_usually_taken());
        assert!(!hot.is_usually_not_taken());
        assert!(!hot.is_saturated());

        let cold = BranchCounts {
            taken: 10,
            not_taken: 1_000_000_000,
        };
        assert!(cold.is_usually_not_taken());
        assert!(!cold.is_usually_taken());

        let pinned = BranchCounts {
            taken: u32::MAX,
            not_taken: 1,
        };
        assert!(
            pinned.is_saturated(),
            "a pinned counter is no longer proportional to execution"
        );
        // The floor is still a floor, and it is named.
        let sparse = BranchCounts {
            taken: BRANCH_BIAS_MIN_SAMPLES - 1,
            not_taken: 0,
        };
        assert!(!sparse.is_usually_taken());
    }

    // -----------------------------------------------------------------------
    // Receiver summary
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_receivers_of_an_empty_profile_is_no_evidence() {
        let summary = summarize_receivers(&ReceiverCounts::default());
        assert_eq!(summary.observations, 0);
        assert_eq!(summary.types, 0);
        assert!(summary.top.is_none());
        assert!(summary.second.is_none());
        assert!(!summary.top_holds_at_least_pct(1));
        assert!(summary.is_exact());

        // A site with no receiver at all reports absence, not a zeroed profile.
        let p = MethodProfile::default();
        assert!(p.receiver_summary(7).is_none());
    }

    #[test]
    fn summarize_receivers_ranks_top_two_deterministically() {
        let mut counts = ReceiverCounts::default();
        counts.insert(9, 350);
        counts.insert(7, 600);
        counts.insert(3, 50);
        let summary = summarize_receivers(&counts);
        assert_eq!(summary.observations, 1000);
        assert_eq!(summary.types, 3);
        assert_eq!(summary.top, Some((7, 600)));
        assert_eq!(summary.second, Some((9, 350)));
        assert!(summary.top_holds_at_least_pct(60));
        assert!(!summary.top_holds_at_least_pct(61));

        // Ties on count fall to the lower class id, in both slots, every time.
        let mut tied = ReceiverCounts::default();
        tied.insert(31, 480);
        tied.insert(4, 480);
        tied.insert(12, 40);
        for _ in 0..32 {
            let summary = summarize_receivers(&tied);
            assert_eq!(summary.top, Some((4, 480)));
            assert_eq!(summary.second, Some((31, 480)));
        }
    }

    /// The type count is exact: the live store has no `TypeProfileWidth` cap,
    /// so a one-type reading is one type and not an overflowed table. This is
    /// the property `pgo::ReceiverTypeProfile` (cap 8) does *not* have, and the
    /// reason the two must not be treated as interchangeable evidence.
    #[test]
    fn live_receiver_profile_records_every_type_it_sees() {
        let mut p = MethodProfile::default();
        for class_id in 0..64u32 {
            p.record_receiver(5, class_id);
        }
        let summary = p.receiver_summary(5).expect("site recorded");
        assert_eq!(summary.types, 64, "no type is dropped on overflow");
        assert_eq!(summary.observations, 64);
        assert!(summary.is_exact());
    }

    // -----------------------------------------------------------------------
    // Concurrency contract
    // -----------------------------------------------------------------------

    /// The consistency claim in the module docs, asserted:
    ///
    /// * no lost updates — every recorded observation survives, because the
    ///   per-method `Mutex` serialises the read-modify-write that an atomic
    ///   counter would not have made safe (these are hash-map entries);
    /// * every snapshot is an image of a real instant — counts never move
    ///   backwards between two reads, and a summary's total always equals the
    ///   sum of its own parts, so a compiler never plans against a half-applied
    ///   increment.
    ///
    /// No timing assumption: the polling loop is allowed to observe nothing at
    /// all (its assertions hold vacuously from zero), so the test cannot flake
    /// on a slow or fast machine.
    #[test]
    fn concurrent_receiver_recording_is_lossless_and_monotone() {
        with_profiling_enabled(|| {
            const THREADS: usize = 4;
            const PER_THREAD: u32 = 2_000;
            const PC: usize = 88;

            let store = Arc::new(ProfileStore::new());
            let key = make_key(4242);
            let mut handles = Vec::new();
            for t in 0..THREADS {
                let store = Arc::clone(&store);
                let key = key.clone();
                handles.push(std::thread::spawn(move || {
                    let class_id = (t as u32 % 2) + 1;
                    for _ in 0..PER_THREAD {
                        store.record_receiver(&key, PC, class_id);
                    }
                }));
            }

            let mut prev = (0u32, 0u32);
            for _ in 0..256 {
                let Some(p) = store.get_profile(&key) else {
                    continue;
                };
                let Some(counts) = p.receivers.get(&PC) else {
                    continue;
                };
                let a = counts.get(&1).copied().unwrap_or(0);
                let b = counts.get(&2).copied().unwrap_or(0);
                assert!(
                    a >= prev.0 && b >= prev.1,
                    "a snapshot moved backwards: {prev:?} -> {:?}",
                    (a, b)
                );
                let summary = summarize_receivers(counts);
                assert_eq!(
                    summary.observations,
                    a + b,
                    "snapshot total disagrees with its own parts"
                );
                prev = (a, b);
            }

            for h in handles {
                h.join().expect("recorder thread should not panic");
            }

            let p = store.get_profile(&key).expect("profile recorded");
            let counts = &p.receivers[&PC];
            let per_class = PER_THREAD * (THREADS as u32 / 2);
            assert_eq!(counts[&1], per_class, "lost update on class 1");
            assert_eq!(counts[&2], per_class, "lost update on class 2");
            let summary = summarize_receivers(counts);
            assert_eq!(summary.observations, PER_THREAD * THREADS as u32);
            assert_eq!(summary.types, 2);
            assert!(summary.is_exact());
        });
    }
}
