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

/// Outstanding C1->C2 nominations that want a branch profile.
///
/// Branch and back-edge recording is a per-method lock on every conditional
/// branch in the interpreter, which is why `CRATONVM_TIER_PGO` has never
/// shipped on -- it is a GLOBAL cost paid for a LOCAL benefit, and the local
/// benefit is one tier's block layout.
///
/// The window makes the cost proportional to the benefit. It opens when a
/// method is nominated for the optimizing tier and closes when the last such
/// nomination has been compiled, so a program that never tiers up pays exactly
/// what it paid before (one relaxed load per `execute_frame`), and a program
/// that does pays branch recording only while there is a compile waiting to
/// read the result.
static C2_PROFILE_WINDOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The base state to restore when the window closes: whatever
/// `CRATONVM_TIER_PGO` asked for. Captured on the first arm rather than assumed
/// `false`, so arming does not silently DISABLE profiling for a run that asked
/// for it globally.
static C2_PROFILE_BASE: AtomicBool = AtomicBool::new(false);
static C2_PROFILE_BASE_CAPTURED: AtomicBool = AtomicBool::new(false);

/// Open the window for one nomination.
pub fn arm_branch_profiling_for_c2() {
    if !c2_branch_window_enabled() {
        return;
    }
    if !C2_PROFILE_BASE_CAPTURED.swap(true, Ordering::AcqRel) {
        C2_PROFILE_BASE.store(is_profiling_enabled(), Ordering::Relaxed);
    }
    // `SeqCst` on the count and on the gate store, pairing with the re-check
    // in `disarm_branch_profiling_for_c2`: see there.
    C2_PROFILE_WINDOW.fetch_add(1, Ordering::SeqCst);
    PROFILING_ENABLED.store(true, Ordering::SeqCst);
    C2_WINDOW_OPENED.fetch_add(1, Ordering::Relaxed);
}

/// Close the window for one nomination. The last one out restores the base.
///
/// A `saturating_sub` shape rather than a bare decrement: a nomination that is
/// dropped without ever reaching a compile (a full queue, a shutdown) must not
/// wrap the counter and pin profiling on for the life of the process.
///
/// # The re-check after restoring the base
///
/// "Decrement to zero" and "store the base" are two steps, and an arm can land
/// between them: this thread takes the count 1 -> 0, another thread's
/// nomination takes it 0 -> 1 and stores `true`, and only then does this
/// thread store the base (`false` in a default run). The window then reads
/// OPEN (count 1) with recording OFF -- the nominated compile gets exactly the
/// empty `branch_counts` the window exists to prevent, and it stays that way
/// until some later arm happens to re-store `true`. The compile worker and the
/// mutator arm and disarm concurrently, so this is a real interleaving, not a
/// theoretical one.
///
/// Re-reading the count after the store closes it. Every order of the four
/// `SeqCst` operations either has the arm's `true` store land after this
/// store (correct as is), or has the arm's increment precede this store and
/// therefore be seen by the re-read, which puts `true` back.
pub fn disarm_branch_profiling_for_c2() {
    if !c2_branch_window_enabled() {
        return;
    }
    let prev = C2_PROFILE_WINDOW
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        })
        .unwrap_or(0);
    if prev <= 1 {
        PROFILING_ENABLED.store(C2_PROFILE_BASE.load(Ordering::Relaxed), Ordering::SeqCst);
        C2_WINDOW_CLOSED.fetch_add(1, Ordering::Relaxed);
        if C2_PROFILE_WINDOW.load(Ordering::SeqCst) > 0 {
            PROFILING_ENABLED.store(true, Ordering::SeqCst);
        }
    }
}

static C2_WINDOW_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static C2_WINDOW_CLOSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(opened, closed, still_open)`. `still_open` large at exit means nominations
/// are being armed and never disarmed, which pins branch recording on -- the
/// failure this counter exists to make visible.
pub fn c2_branch_window_census() -> (u64, u64, u64) {
    (
        C2_WINDOW_OPENED.load(Ordering::Relaxed),
        C2_WINDOW_CLOSED.load(Ordering::Relaxed),
        C2_PROFILE_WINDOW.load(Ordering::Relaxed),
    )
}

/// **Default ON** since 2026-09-06.
/// `CRATONVM_TIER_PGO_C2_WINDOW=0` restores the pre-window behaviour, in which
/// the optimizing tier's scheduler received an empty `branch_counts` in every
/// default run.
pub fn c2_branch_window_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_TIER_PGO_C2_WINDOW").as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
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
        // The exact `u64` sum, not the saturating `u32` [`Self::total`] (r11-tier):
        // with both directions near 3.9e9 that pinned at `u32::MAX` and read a
        // 50/50 branch as 90 % taken.
        let total = self.total_exact();
        total >= u64::from(BRANCH_BIAS_MIN_SAMPLES) && u64::from(self.taken) * 10 < total
    }

    /// `taken + not_taken` without saturation, for the ratio tests.
    fn total_exact(&self) -> u64 {
        u64::from(self.taken) + u64::from(self.not_taken)
    }

    /// Returns `true` when the branch is overwhelmingly taken
    /// (taken > 90 % of total observations with at least
    /// [`BRANCH_BIAS_MIN_SAMPLES`] samples). `u64` for the same overflow reason
    /// as [`Self::is_usually_not_taken`].
    pub fn is_usually_taken(&self) -> bool {
        let total = self.total_exact();
        total >= u64::from(BRANCH_BIAS_MIN_SAMPLES) && u64::from(self.taken) * 10 > total * 9
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
/// **The live store does not cap the receiver type table.** Unlike the
/// `pgo::ReceiverTypeProfile` of the deleted `pgo.rs` (which recorded at most
/// 8 classes and dropped the rest), [`MethodProfile::record_receiver`] inserts
/// every distinct class it sees, so a live profile never loses a *type*: [`ReceiverProfileSummary::types`]
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

/// The fewest observations at which [`dominant_receiver`] names a class.
///
/// Far below `classify_receiver_shape`'s inlining floor on purpose: this
/// answer seeds guarded caches and splice choices whose miss edge is cheap,
/// so it only has to rule out the one-or-two-call site, not prove a shape.
pub const DOMINANT_RECEIVER_MIN_OBSERVATIONS: u32 = 16;

/// Returns the dominant receiver class (most frequent) if it holds at least
/// `min_fraction_pct` percent of total observations.
///
/// **Heuristic use only.** Its callers are all in `jit/src/lib.rs` (three at
/// round 11: the MIC seed, the nested-splice receiver choice and the NIO
/// bind — `rg -n "dominant_receiver\(" jit/src/lib.rs`), and each re-checks the
/// chosen class at run time with its own guard, so a stale or mis-ranked answer
/// costs a miss and never mis-dispatches. It is the seed-a-guarded-cache policy
/// of [`ReceiverEvidence`] (round 12 wave 3), beside
/// [`crate::classify_receiver_shape`]'s speculate policy: a low floor and no
/// fidelity check. A consumer that wants to know whether the counts can be
/// *believed* must check [`ReceiverProfileSummary::fidelity`] — this function
/// answers from a saturated profile just as readily as from an exact one.
///
/// Round 12 wave 1 (lane tier): a site with fewer than
/// [`DOMINANT_RECEIVER_MIN_OBSERVATIONS`] observations has no dominant receiver.
/// One observation used to be 100 %, so a nested splice or the NIO bind could
/// be chosen on a single interpreted call. `CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR=0`
/// restores the floor-less answer.
pub fn dominant_receiver(counts: &ReceiverCounts, min_fraction_pct: u32) -> Option<u32> {
    ReceiverEvidence::of(counts).seed_a_guarded_cache(min_fraction_pct)
}

/// What a receiver profile says about a guard on one class
/// ([`ReceiverEvidence::guard_evidence`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuardEvidence {
    /// Too few samples (or none): the profile says nothing.
    Silent,
    /// The class holds at least the asked share.
    Supports,
    /// Enough samples, and the class holds less than the asked share.
    Rejects,
}

/// One reading of one call site's receiver profile, and the named policies
/// its consumers ask of it (tier proposal R12-5, round 12 wave 3).
///
/// Three policies used to read receiver counts, each with its own sum, sort
/// and share arithmetic:
///
/// * **speculate** (`crate::classify_receiver_shape`, which gates inlining):
///   a saturation refusal, a 250-observation floor, 90 % for one class, 95 %
///   for two. It reads [`Self::summary`]: `top` and `second` are ranked the way
///   its own sort ranks them (descending count, ascending class id), and
///   `observations` / `fidelity` are its total and its saturation test;
/// * **seed a guarded cache** ([`Self::seed_a_guarded_cache`], formerly the
///   body of [`dominant_receiver`]): the MIC seeds of both tiers, the nested
///   splice choice and the NIO bind. A cheap miss edge, so a low floor
///   ([`DOMINANT_RECEIVER_MIN_OBSERVATIONS`]) and the caller's share;
/// * **guard a trap-on-miss site** ([`Self::guard_evidence`], what
///   `crate::receiver_profile_supports_guard` / `receiver_profile_rejects_guard`
///   compute with `MIN_GUARDED_RECEIVER_EVIDENCE` and
///   `MIN_GUARDED_RECEIVER_PCT`): a miss deopts, so a higher floor and a 90 %
///   share, with "the profile says nothing" kept apart from "the profile says
///   no".
///
/// Each keeps its own thresholds, so moving a caller here changes no answer.
/// The sums agree: `summary.observations` is the `u64` total clamped to
/// `u32::MAX`, which is what a `u32::saturating_add` fold computes.
#[derive(Clone, Copy, Debug)]
pub struct ReceiverEvidence<'a> {
    counts: &'a ReceiverCounts,
    summary: ReceiverProfileSummary,
}

impl<'a> ReceiverEvidence<'a> {
    /// Summarise `counts` once.
    pub fn of(counts: &'a ReceiverCounts) -> Self {
        Self {
            counts,
            summary: summarize_receivers(counts),
        }
    }

    /// The deterministic summary every policy reads.
    pub fn summary(&self) -> &ReceiverProfileSummary {
        &self.summary
    }

    /// The seed-a-guarded-cache policy: the top class, when the site has at
    /// least [`DOMINANT_RECEIVER_MIN_OBSERVATIONS`] observations and the class
    /// holds at least `min_fraction_pct` percent of them. No fidelity check:
    /// a saturated site still ranks its top class well enough for a guarded
    /// hint. `CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR=0` drops the floor.
    pub fn seed_a_guarded_cache(&self, min_fraction_pct: u32) -> Option<u32> {
        let (class_id, count) = self.summary.top?;
        if self.summary.observations < DOMINANT_RECEIVER_MIN_OBSERVATIONS
            && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR")
        {
            return None;
        }
        if self.summary.holds_at_least_pct(count, min_fraction_pct) {
            Some(class_id)
        } else {
            None
        }
    }

    /// The trap-on-miss guard policy: [`GuardEvidence::Silent`] under
    /// `min_samples` observations, otherwise whether `class_id` holds at
    /// least `min_pct` percent of them.
    ///
    /// A SATURATED summary never answers `Supports` (tier proposal W4-4,
    /// round 12 wave 5): its `observations` is clamped at `u32::MAX` below
    /// the true total and a pinned count stops growing, so a class can read
    /// at 90 % after it has fallen well under it, and `Supports` is what
    /// leaves a deopting guard standing. `Rejects` stays sound there: the
    /// true total is at least the clamped one, so a share under `min_pct`
    /// of the clamped total is under it of the true one too, and a pinned
    /// `class_id` reads at 100 %, never as a rejection. The saturated
    /// `Supports` becomes `Silent`, each predicate's uncertain-case answer.
    /// `CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY=0` (default on) answers
    /// `Supports` from saturated counts as before.
    pub fn guard_evidence(&self, class_id: u32, min_samples: u32, min_pct: u32) -> GuardEvidence {
        if self.summary.observations < min_samples {
            return GuardEvidence::Silent;
        }
        let hits = self.counts.get(&class_id).copied().unwrap_or(0);
        if self.summary.holds_at_least_pct(hits, min_pct) {
            // The flag is read only for a saturated site, which takes over
            // four billion interpreted executions of one call site.
            if !self.summary.is_exact()
                && cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY",
                )
            {
                return GuardEvidence::Silent;
            }
            GuardEvidence::Supports
        } else {
            GuardEvidence::Rejects
        }
    }

    /// Round 14 wave 2 (lane guard, GS-1): the two-guards-then-a-call policy.
    /// `Some((top, second))` when the site has at least `min_samples`
    /// observations, its top class holds LESS than `max_top_pct` percent (a
    /// site at or over it is the one-class policy's), the runner-up holds at
    /// least `min_second_pct`, and the two together at least `min_pair_pct`.
    /// Class id 0 (unknown) is never named. No fidelity check: like
    /// [`Self::seed_a_guarded_cache`] it answers a hint whose miss edge is a
    /// call, and a saturated site still ranks its top two well enough.
    pub fn bimorphic_pair(
        &self,
        min_samples: u32,
        max_top_pct: u32,
        min_second_pct: u32,
        min_pair_pct: u32,
    ) -> Option<(u32, u32)> {
        let s = &self.summary;
        if s.observations < min_samples || s.observations == 0 {
            return None;
        }
        let (top, top_n) = s.top?;
        let (second, second_n) = s.second?;
        if top == 0 || second == 0 || s.top_holds_at_least_pct(max_top_pct) {
            return None;
        }
        // In `u64`, as `holds_at_least_pct` computes: two counts near
        // `u32::MAX` must not wrap.
        let pair = u64::from(top_n) + u64::from(second_n);
        let total = u64::from(s.observations);
        if !s.holds_at_least_pct(second_n, min_second_pct)
            || pair * 100 < total * u64::from(min_pair_pct)
        {
            return None;
        }
        Some((top, second))
    }

    /// Round 14 wave 2 (lane guard, CH5-5): the trap-on-miss policy for a
    /// guarded splice. The site's only class, when it has exactly one, at
    /// least `min_samples` observations, and exact (unsaturated) counts; the
    /// unknown class id 0 is never named. Stricter than
    /// [`Self::guard_evidence`] on purpose: a miss here deopts the whole
    /// compiled frame, and the gain (the receiver is exact past the test) is
    /// worth it only where no other class was ever seen.
    pub fn only_class(&self, min_samples: u32) -> Option<u32> {
        let s = &self.summary;
        if s.types != 1 || s.observations < min_samples || !s.is_exact() {
            return None;
        }
        s.top.map(|(class_id, _)| class_id).filter(|&c| c != 0)
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
    /// [`Self::backedge_count`] as of the previous [`Self::record_exit`], so
    /// an exit can charge the back edges taken since then as its trips.
    /// Replayed back edges are added here too
    /// ([`MethodProfile::seed_replayed_loop`]), so a seed is never charged to
    /// the first live exit.
    backedges_at_last_exit: u64,
}

impl LoopTripProfile {
    /// Record a single back-edge execution.
    #[inline]
    pub fn record_backedge(&mut self) {
        self.backedge_count = self.backedge_count.saturating_add(1);
    }

    /// Record one exit of the loop through its own top test (round 11 wave
    /// 19, the live producer of r11-tier-profile-replay-residuals item 6): one
    /// more entry, whose trips are the back edges taken since the previous
    /// exit.
    ///
    /// Per loop, not per frame: two threads in one loop charge each other's
    /// back edges to whichever exits first, which moves trips between entries
    /// but not the total, so the average stays right. An exit the interpreter
    /// does not see (a `return`, a throw, an OSR transfer, a branch on the
    /// decoded slow path; a `break` is seen since round 12 wave 3, see
    /// [`LoopExtents`]) charges its back edges to the next recorded exit,
    /// which over-states the average; `suggests_unroll_factor` may then
    /// answer a larger factor than the loop earns. That costs code size, never an answer: the unroll
    /// planner decides legality on its own.
    #[inline]
    pub fn record_exit(&mut self) {
        let trips = self.backedge_count.saturating_sub(self.backedges_at_last_exit);
        self.backedges_at_last_exit = self.backedge_count;
        self.entry_count = self.entry_count.saturating_add(1);
        self.total_trips = self.total_trips.saturating_add(trips);
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
    /// `None` means "no hint" (the loop is cold, under 100 back edges, or no
    /// exit through its top test was ever seen, so there is no average), and
    /// the single-pass planner then uses its static size band. `Some(1)` means
    /// "do not unroll": the planner refuses a plan with no extra copies.
    ///
    /// Round 12 wave 2 (tier proposal R12-2) re-derived the mapping. This
    /// unroller keeps every copy's own exit test, so a copy buys one back-edge
    /// `JMP`, poll and counter bump per unrolled iteration and nothing else. A
    /// loop averaging 3 trips unrolled 4x still takes about one back edge per
    /// entry, and pays three body copies of i-cache for it; one averaging 64
    /// trips saves 48 of its 64 back edges. So: under 8 trips no copies, 8 to
    /// 32 trips 2x, above 32 trips 4x (capped at `max_factor`). The old
    /// mapping (4x for 2-8 trips, 2x for 9-32, no hint above 32 or below 2)
    /// was the reverse, and is kept behind
    /// `CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR=1` for the A/B measurement.
    /// Read per call: callers ask once per profiled loop per compile, and only
    /// under `CRATONVM_TIER_PGO` does a profile carry loops.
    pub fn suggests_unroll_factor(&self, max_factor: usize) -> Option<usize> {
        unroll_factor_for(
            self.backedge_count,
            self.entry_count,
            self.avg_trip_count(),
            max_factor,
            cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR"),
        )
    }
}

/// [`LoopTripProfile::suggests_unroll_factor`] over plain numbers, so both
/// mappings can be tested without an environment variable.
fn unroll_factor_for(
    backedge_count: u64,
    entry_count: u32,
    avg: f64,
    max_factor: usize,
    legacy: bool,
) -> Option<usize> {
    if backedge_count < 100 {
        return None; // not hot enough
    }
    if legacy {
        if avg > 128.0 || avg < 2.0 {
            return None;
        }
        let factor = if avg <= 8.0 {
            4usize
        } else if avg <= 32.0 {
            2
        } else {
            return None;
        };
        return Some(factor.min(max_factor));
    }
    if entry_count == 0 {
        return None; // back edges but no recorded exit: no average to read
    }
    let factor = if avg < 8.0 {
        1usize
    } else if avg <= 32.0 {
        2
    } else {
        4
    };
    Some(factor.min(max_factor))
}

/// Where the loops of one method body are, so the interpreter records a
/// loop's back edges and exits under ONE key: the header's last back edge
/// (tier proposal W2-3, round 12 wave 3).
///
/// That is the only key the single-pass unroll planner reads a trip hint under
/// (`x64::driver::last_back_edge_per_header` over `detect_loops`' back edges:
/// the 16-bit `if*` / `goto` / `ifnull` / `ifnonnull` forms). The interpreter
/// used to key by the branch it saw: a back edge under its own `goto`, and an
/// exit under the backward `goto` right before the exit's target. In javac's
/// `while (c) { if (x) { A; continue; } B; }` the `continue` is a second
/// backward `goto` to the header, so the loop's back edges were split over two
/// keys, and the `if`'s own forward branch to `B` (which follows the
/// `continue`'s `goto`) was charged as an exit of a "loop" keyed by the
/// `continue`. Round 12 wave 2 made the planner ignore that key; this table
/// stops the interpreter from writing it:
///
/// * [`Self::back_edge_key`]: every back edge of a header is recorded under
///   the header's last back edge, so the key's back-edge count is the loop's
///   iteration count, `continue`s included;
/// * [`Self::exits_at`]: a TAKEN forward branch (`if*`, `goto`, `goto_w`) at
///   `pc` inside a loop `[header, last]` closed by a `goto`, whose target lies
///   past `last`, is one exit of that loop (of every such loop, for a labelled
///   `break` out of several). That counts `break` exits too, which used to
///   charge their back edges to the next exit through the top test.
///
/// A pure function of the bytes (no exception table, no constant pool), so a
/// copy may be reused for any body with equal bytes. The answers are hints
/// only: the planner decides legality on its own.
///
/// `CRATONVM_JIT_PGO_LOOP_KEYS=0` builds the pre-W2-3 table instead: back
/// edges under their own pc, and an exit only for a taken forward CONDITIONAL
/// branch whose target follows a backward `goto` to a header at or before the
/// branch (the old byte test, byte for byte). Read once per build.
#[derive(Debug, Default)]
pub struct LoopExtents {
    /// Back-edge pc -> the pc its records go under. Absent means "itself".
    back_edge_keys: FxHashMap<u32, u32>,
    /// Forward-branch pc -> the keys of the loops a taken branch there leaves.
    exits: FxHashMap<u32, Box<[u32]>>,
}

/// Loops per method beyond which [`LoopExtents::build`] records no exits (the
/// exit pass is `O(branches x loops)`). Back edges are still keyed. A method
/// with this many loops is generated code, and has no trip hints then.
const LOOP_EXTENTS_MAX_LOOPS: usize = 1024;

impl LoopExtents {
    /// Build the table for `code` (a method's bytecode; trailing padding
    /// bytes are zero, i.e. `nop`, and change nothing).
    pub fn build(code: &[u8]) -> Self {
        if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_PGO_LOOP_KEYS") {
            Self::build_keyed(code)
        } else {
            Self::build_legacy(code)
        }
    }

    fn build_keyed(code: &[u8]) -> Self {
        let mut out = Self::default();
        let code_len = code.len();
        // `detect_loops`' back-edge set, so the key agrees with the planner's.
        let edges: Vec<(usize, usize)> = crate::bytecode_analysis::back_edges(code, code_len)
            .into_iter()
            .filter(|&(_, src)| matches!(code.get(src), Some(0x99..=0xa7 | 0xc6 | 0xc7)))
            .collect();
        let mut last: FxHashMap<usize, usize> = FxHashMap::default();
        for &(header, src) in &edges {
            let entry = last.entry(header).or_insert(src);
            if src > *entry {
                *entry = src;
            }
        }
        for &(header, src) in &edges {
            let Some(&key) = last.get(&header) else {
                continue;
            };
            if key != src {
                if let (Ok(s), Ok(k)) = (u32::try_from(src), u32::try_from(key)) {
                    out.back_edge_keys.insert(s, k);
                }
            }
        }
        // Loops closed by a `goto`: their exits are forward branches. A loop
        // closed by a conditional (a `do`/`while`) leaves by falling through
        // it, which no taken branch shows, and the planner skips it anyway.
        let mut loops: Vec<(usize, usize)> = last
            .iter()
            .filter(|&(_, &l)| code.get(l) == Some(&0xa7))
            .map(|(&h, &l)| (h, l))
            .collect();
        if loops.is_empty() || loops.len() > LOOP_EXTENTS_MAX_LOOPS {
            return out;
        }
        loops.sort_unstable();
        let mut pc = 0usize;
        while pc < code_len {
            let op = code.get(pc).copied().unwrap_or(0);
            if matches!(op, 0x99..=0xa7 | 0xc6 | 0xc7 | 0xc8) {
                if let Some(target) = crate::bytecode_analysis::offset_branch_target(code, pc) {
                    if target > pc {
                        let mut keys: Vec<u32> = Vec::new();
                        for &(header, last_edge) in &loops {
                            if header > pc {
                                break;
                            }
                            if pc < last_edge && last_edge < target {
                                if let Ok(k) = u32::try_from(last_edge) {
                                    keys.push(k);
                                }
                            }
                        }
                        if let (false, Ok(at)) = (keys.is_empty(), u32::try_from(pc)) {
                            out.exits.insert(at, keys.into_boxed_slice());
                        }
                    }
                }
            }
            pc += crate::bytecode_analysis::step(code, pc);
        }
        out
    }

    /// The pre-W2-3 exit rule, table-driven so both rules share one call
    /// site: back edges under their own pc; a taken forward conditional at
    /// `pc` to `t` is an exit of the loop keyed `t - 3` when the three bytes
    /// there are a backward `goto` past `pc` to a header at or before `pc`.
    fn build_legacy(code: &[u8]) -> Self {
        let mut out = Self::default();
        let code_len = code.len();
        let mut pc = 0usize;
        while pc < code_len {
            let op = code.get(pc).copied().unwrap_or(0);
            if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) {
                let goto_pc = crate::bytecode_analysis::offset_branch_target(code, pc)
                    .filter(|&t| t > pc)
                    .and_then(|t| t.checked_sub(3))
                    .filter(|&g| g > pc);
                if let Some(g) = goto_pc {
                    let is_loop = code.get(g) == Some(&0xa7)
                        && crate::bytecode_analysis::offset_branch_target(code, g)
                            .is_some_and(|header| header < g && header <= pc);
                    if let (true, Ok(at), Ok(k)) = (is_loop, u32::try_from(pc), u32::try_from(g)) {
                        out.exits.insert(at, Box::new([k]));
                    }
                }
            }
            pc += crate::bytecode_analysis::step(code, pc);
        }
        out
    }

    /// The pc a back edge at `pc` is recorded under.
    #[inline]
    pub fn back_edge_key(&self, pc: usize) -> usize {
        u32::try_from(pc)
            .ok()
            .and_then(|p| self.back_edge_keys.get(&p))
            .map_or(pc, |&k| k as usize) // Cast: u32 -> usize widens
    }

    /// The keys of the loops a taken forward branch at `branch_pc` leaves
    /// (empty for a branch that leaves none).
    #[inline]
    pub fn exits_at(&self, branch_pc: usize) -> &[u32] {
        match u32::try_from(branch_pc).ok().and_then(|p| self.exits.get(&p)) {
            Some(keys) => keys,
            None => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Replay provenance
// ---------------------------------------------------------------------------

/// Which part of one method's profile came from a **replayed** file rather than
/// from this run's interpreter, and how the live run has since judged it.
///
/// # Why a provenance bit exists at all
///
/// [`crate::profile_store`] can seed this store from a previous process's
/// profile (`CRATONVM_JIT_PROFILE_LOAD`). Without a provenance bit, a seeded
/// count is indistinguishable from an observed one, and three things become
/// impossible: a tiering policy cannot discount evidence it did not witness, a
/// census cannot say whether replay helped or hurt, and nobody can tell a
/// profile that *predicted* the program from one that merely *described* a
/// different run of it.
///
/// # What this is NOT
///
/// It is **not** a licence. Nothing in `jit/src/lib.rs` reads this struct, and
/// nothing may start reading it in order to skip a guard: a replayed receiver
/// class is re-checked by exactly the same `CMP` the live path's seed is
/// re-checked by, because the MIC-seeding path cannot tell the two apart and
/// must not be taught to. The bit is for *accounting* and for *discounting*,
/// never for *trusting*.
///
/// # The fields that move
///
/// [`Self::receivers_confirmed`] and [`Self::receivers_refuted`] are driven by
/// [`MethodProfile::record_receiver`], the one live receiver-recording path.
/// A replayed `(pc, class)` pair sits in `pending_receivers` until the live run
/// says something about that pc:
///
/// * the live run observes the **same** class — *confirmed*. The seeded count
///   stays, because the hint was right.
/// * the live run observes a **different** class at that pc — *refuted*. Every
///   still-pending seed at that pc is subtracted back out of
///   [`MethodProfile::receivers`] and [`Self::shape_contradicted`] is set. The
///   live observation that did the refuting is left in place, so the site
///   re-profiles from live evidence alone.
///
/// The asymmetry is deliberate. Confirmation is cheap to get right and costs
/// nothing if wrong; a contradiction is the signal that this call site's shape
/// has changed between runs, and the conservative response to "my stored
/// evidence disagrees with what I am watching" is to discard the stored
/// evidence, not to average it in.
///
/// # Known limit
///
/// These tallies live inside the method's own profile, so
/// [`ProfileStore::invalidate_class`] takes them with it when a class unloads —
/// a census taken after an unload under-reports both halves. The alternative
/// was a process-global counter pair, which `jit/tests/process_global_statics_ratchet.rs`
/// exists to stop this crate growing, and which would be wrong for a second
/// reason: two VMs in one process would share it. Under-reporting a diagnostic
/// on class unload is the cheaper error.
#[derive(Clone, Debug, Default)]
pub struct ReplayProvenance {
    /// How much of this method's invocation counter was seeded from a replay
    /// rather than earned this run. A tiering policy that wants to treat
    /// replayed and live calls differently needs this number; none does today,
    /// which is why `crate::profile_store` also caps the seed itself.
    pub seeded_invocations: u32,
    /// Distinct `(pc, receiver class)` pairs seeded from the replay.
    pub seeded_receiver_types: u32,
    /// Bytecode offsets whose branch counts were seeded from the replay.
    pub seeded_branch_sites: u32,
    /// Seeded receiver classes the live run has since observed at the same pc.
    pub receivers_confirmed: u32,
    /// Seeded receiver classes discarded because the live run observed a
    /// different class at the same pc first.
    pub receivers_refuted: u32,
    /// Whether any call site in this method has contradicted its replayed
    /// receiver shape. One bit per method, so the census can count *methods*
    /// whose shape was wrong and not only individual types.
    pub shape_contradicted: bool,
    /// `pc -> [(class_id, seeded_count)]` still awaiting a live verdict.
    ///
    /// Private: it is an undo log, and a consumer that reads it would be
    /// reading "what the file said" as if it were "what the program does",
    /// which is the confusion this whole struct exists to prevent.
    pending_receivers: FxHashMap<usize, Vec<(u32, u32)>>,
    /// `pc -> [class_id]` seeds the live run has already confirmed. A second
    /// observation of a confirmed class is not in `pending_receivers` any more,
    /// and without this list it read as a class the replay never named — which
    /// refuted every still-pending seed at a bimorphic site the moment its
    /// first class was seen twice.
    confirmed_receivers: FxHashMap<usize, Vec<u32>>,
    /// `pc -> (taken, not_taken)` the replay added to [`MethodProfile::branches`]
    /// (round 11 wave 19). Branch, call-site and loop seeds have no live
    /// verdict the way a receiver seed does: they are layout and unroll hints
    /// that live traffic outvotes. So a save subtracts all of them
    /// (`crate::profile_store::collect_records`) and writes only what this run
    /// observed, which is how the invocation credit and the unjudged receiver
    /// seeds already age out (r11-tier-profile-replay-residuals item 3).
    seeded_branches: FxHashMap<usize, (u32, u32)>,
    /// `pc -> count` the replay added to [`MethodProfile::call_sites`]. See
    /// [`Self::seeded_branches`].
    seeded_call_sites: FxHashMap<usize, u32>,
    /// `back-edge pc -> (backedge_count, entry_count, total_trips)` the replay
    /// added to [`MethodProfile::loops`]. See [`Self::seeded_branches`].
    seeded_loops: FxHashMap<usize, (u64, u32, u64)>,
}

impl ReplayProvenance {
    /// The replayed `(taken, not_taken)` at branch `pc`; `(0, 0)` when that pc
    /// was not seeded. A save subtracts it from the folded counts.
    pub(crate) fn seeded_branch(&self, pc: usize) -> (u32, u32) {
        self.seeded_branches.get(&pc).copied().unwrap_or((0, 0))
    }

    /// The replayed execution count of the invoke at `pc`; `0` when unseeded.
    pub(crate) fn seeded_call_site(&self, pc: usize) -> u32 {
        self.seeded_call_sites.get(&pc).copied().unwrap_or(0)
    }

    /// The replayed `(backedge_count, entry_count, total_trips)` of the loop
    /// whose back edge is at `pc`; all zero when unseeded.
    pub(crate) fn seeded_loop(&self, pc: usize) -> (u64, u32, u64) {
        self.seeded_loops.get(&pc).copied().unwrap_or((0, 0, 0))
    }

    /// Seeded receiver classes with no live verdict yet: neither confirmed nor
    /// refuted, because the pc has not executed this run.
    ///
    /// Saturating and `u32`, like every other counter in this module.
    pub fn receivers_unjudged(&self) -> u32 {
        self.pending_receivers
            .values()
            .fold(0u32, |acc, v| acc.saturating_add(v.len() as u32))
    }

    /// The replayed count at `(pc, class_id)` that no live observation has
    /// judged yet. A save subtracts it (`crate::profile_store::collect`), so an
    /// unconfirmed seed is not written back at full strength as if it had been
    /// observed and then re-seeded on the next load, never ageing out
    /// (r11-tier-profile-replay-residuals item 3). A confirmed seed is not
    /// pending, so it is saved: the live run vouched for it.
    pub(crate) fn unjudged_seed(&self, pc: usize, class_id: u32) -> u32 {
        self.pending_receivers.get(&pc).map_or(0, |seeds| {
            seeds
                .iter()
                .filter(|&&(c, _)| c == class_id)
                .fold(0u32, |acc, &(_, n)| acc.saturating_add(n))
        })
    }

    /// Record one replayed `(pc, class)` pair as pending a live verdict.
    fn note_pending_receiver(&mut self, pc: usize, class_id: u32, seeded: u32) {
        self.pending_receivers
            .entry(pc)
            .or_default()
            .push((class_id, seeded));
        self.seeded_receiver_types = self.seeded_receiver_types.saturating_add(1);
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
    /// Lock-free branch counters for this method, when one has been allocated.
    ///
    /// The recording path writes HERE, not into [`Self::branches`]; the two are
    /// folded together by [`Self::snapshot`] on the way to a compiler. See
    /// [`BranchCounters`] for why.
    ///
    /// `None` on a snapshot (the fold has already happened) and on any method
    /// that has never had a branch recorded through the fast path.
    pub flat_branches: Option<Arc<BranchCounters>>,
    /// Provenance for the part of this profile that was **replayed** from a
    /// previous run's file rather than observed by this one.
    ///
    /// `None` — the overwhelmingly common case, and the only case at all unless
    /// `CRATONVM_JIT_PROFILE_LOAD` is set — means every number above was
    /// observed live. Boxed so a method that was never seeded pays one null
    /// pointer, and so the live recording path's check is a predictable
    /// `is_some()` on a cold-in-practice branch rather than a walk of a map.
    ///
    /// The four maps above are deliberately NOT split by provenance. A consumer
    /// must see one set of counts, exactly as it did before replay existed: a
    /// consumer that had to remember to add a second source would silently read
    /// a seeded method as unprofiled, which is the same failure
    /// [`Self::snapshot`]'s fold of [`Self::flat_branches`] exists to prevent.
    pub replay: Option<Box<ReplayProvenance>>,
    /// Bytecode length of the method this profile was recorded against, as
    /// stamped by [`Self::note_code_len`]: `0` when no recorder that knows the
    /// length has touched it. The FIRST stamp owns the slot and is never
    /// overwritten; a recorder that disagrees sets
    /// [`Self::code_len_conflict`] instead. `crate::profile_store` writes it
    /// into each record, and the seeding doors drop a record whose length does
    /// not match the live method (r11-tier-profile-replay-residuals item 4).
    code_len: u32,
    /// Whether a recorder with a length other than [`Self::code_len`] has
    /// reached this slot: the slot has seen two versions of the method (item 2
    /// of the same page). [`Self::recorded_code_len`] then answers
    /// [`CODE_LEN_CONFLICT`], so the record is saved but never seeded.
    code_len_conflict: bool,
    /// The loop table of the body the interpreter last asked about, with that
    /// body's bytes ([`ProfileStore::loop_extents_borrowed`]). Built once per
    /// version, shared by every thread; not carried by a snapshot.
    loop_extents: Option<(Arc<[u8]>, Arc<LoopExtents>)>,
    /// Set, under this slot's lock, when [`ProfileStore::invalidate_class`]
    /// removed the slot from the store (its class was unloaded or redefined).
    /// A held [`ReceiverRecorder`] then answers `false` instead of recording
    /// into a slot no compile will read again (round 13 wave 3, lane replay2).
    reclaimed: bool,
}

/// [`MethodProfile`]'s code-length stamp after two recorders disagreed about
/// the method's length. No method is this long (`code_length < 65 536`), so a
/// record carrying it never matches a live method and is never seeded.
pub(crate) const CODE_LEN_CONFLICT: u32 = u32::MAX;

/// Per-method conditional-branch counters, one cell per bytecode offset.
///
/// # The problem
///
/// Branch recording used to cost, at every conditional branch the interpreter
/// executed: a 64-bit fingerprint of `(class_id, name, descriptor)`, a shard
/// `RwLock` read, an index probe, a `MethodKey` comparison, a
/// `parking_lot::Mutex` acquire and an `FxHashMap` entry. That is why
/// `CRATONVM_TIER_PGO` has never shipped on — the module header above says so
/// in as many words: *"a GLOBAL cost paid for a LOCAL benefit"*.
///
/// The workaround was a window that switched profiling on globally when a
/// method was nominated for the optimizing tier and off again when the compile
/// finished, a few milliseconds later. Measured on a mixed probe, that window
/// opened five times in a whole process — so the scheduler's `branch_counts`
/// was empty at essentially every compile, and frequency-driven block layout,
/// branch-polarity selection and every speculation built on top of them were
/// running on static heuristics.
///
/// # The shape
///
/// One `AtomicU32` per bytecode offset per direction, indexed directly by pc.
/// Recording is a bounds-checked index and one relaxed `fetch_add` — no hash,
/// no lock, no map, and no contention beyond the cache line itself. This is
/// HotSpot's MDO in miniature, and it is cheap enough to leave on.
///
/// Indexed by pc rather than by a compacted branch-site table so that recording
/// needs no side lookup at all. The cost is 8 bytes per bytecode of the method,
/// allocated lazily on the first branch recorded — 1.6 KB for a 200-byte
/// method, and nothing at all for a method that never branches or never runs.
///
/// # Saturation
///
/// Every counter in this module saturates rather than wrapping, and this one
/// must too: `record_receiver`'s comment already documents what wrapping costs
/// — at 2^32 observations the majority direction reads as the minority and
/// every decision downstream inverts. A relaxed `fetch_add` cannot saturate on
/// its own, so a counter that has reached the ceiling is pinned back to it. The
/// re-store races benignly against other recorders: the worst outcome is that
/// one increment is lost from a counter that is already pinned at `u32::MAX`.
pub struct BranchCounters {
    taken: Box<[AtomicU32]>,
    not_taken: Box<[AtomicU32]>,
    /// Per-method recording request: set when the method was nominated for its
    /// FIRST compile while still interpreted, so the interpreter can record
    /// THIS method's branches without the process-wide gate
    /// ([`is_profiling_enabled`]) being on. See [`Self::arm`].
    armed: AtomicBool,
}

impl std::fmt::Debug for BranchCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchCounters")
            .field("bytecodes", &self.taken.len())
            .finish()
    }
}

impl BranchCounters {
    /// Counters for a method of `code_len` bytes.
    pub fn new(code_len: usize) -> Self {
        let mk = || (0..code_len).map(|_| AtomicU32::new(0)).collect();
        Self {
            taken: mk(),
            not_taken: mk(),
            armed: AtomicBool::new(false),
        }
    }

    /// Ask for this method's branches to be recorded even while the
    /// process-wide gate is off. Sticky: nothing disarms it.
    ///
    /// The per-method half of the fix for
    /// `docs/known-issues/jit/c2-branch-window-opens-after-the-method-leaves-the-interpreter-20260918.md`.
    /// The global C2 window opens when a method is nominated for the
    /// optimizing tier, which is after it stopped being interpreted, so it
    /// records every OTHER interpreted method and not the nominated one. Arming
    /// the method's own counters at its first nomination (the interpreter's
    /// `Interpreter -> C1` recommendation, when it is still interpreted and
    /// hot) collects exactly the stretch the C2 compile will read. No disarm is
    /// needed for cost: branch recording is interpreter-only, so a method that
    /// has left the interpreter stops paying by itself, and one that deopts
    /// back into it is recording evidence the recompile wants.
    ///
    /// Returns whether this call armed it (`false`: it already was).
    #[inline]
    pub fn arm(&self) -> bool {
        !self.armed.swap(true, Ordering::Relaxed)
    }

    /// Whether [`Self::arm`] was called. One relaxed load.
    #[inline(always)]
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Record one observation. Out-of-range `pc` is dropped rather than
    /// panicking: this is a heuristic on the interpreter's hot path, and a
    /// disagreement between the recorded code length and the pc being executed
    /// must never take the VM down from a dispatch loop.
    ///
    /// # Saturating, with no window where it is not
    ///
    /// This was `if cell.fetch_add(1, Relaxed) == u32::MAX { cell.store(MAX) }`,
    /// which is not a saturating increment: `fetch_add` on a pinned counter
    /// **wraps to 0**, and only the next instruction restores it. Inside that
    /// window a concurrent reader sees `0` — a branch taken four billion times
    /// reading as never-taken, which is not a small error in the direction that
    /// matters — and concurrent writers that slip in restart from `0` and count
    /// up from there for a very long time before re-saturating. The scalar
    /// path, [`super::MethodProfile::record_branch`], has always used
    /// `saturating_add`; this one is the atomic twin of it and must agree.
    ///
    /// `fetch_update` with a saturating closure has no such window: the value
    /// never leaves the cell in a state it should not be in, and the closure
    /// returning `None` at the cap makes a pinned counter a no-op rather than a
    /// retry loop.
    #[inline]
    pub fn record(&self, pc: usize, taken: bool) {
        let bank = if taken { &self.taken } else { &self.not_taken };
        let Some(cell) = bank.get(pc) else { return };
        // `checked_add`, NOT `(n != u32::MAX).then_some(n + 1)`.
        //
        // `then_some` takes its argument by value, so `n + 1` is evaluated
        // whatever the condition says. At the cap that overflows: a debug
        // build panics on the interpreter's hot path, and a release build
        // wraps to `0` — which is precisely the pinned-counter-reads-as-zero
        // defect this function was rewritten to remove, reintroduced by the
        // rewrite. `checked_add` returns `None` at the cap, which is the
        // no-op `fetch_update` wants, and evaluates nothing it must not.
        let _ = cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1));
    }

    /// `(taken, not_taken)` at `pc`; `(0, 0)` for a pc with no observation.
    #[inline]
    pub fn counts(&self, pc: usize) -> (u32, u32) {
        let t = self.taken.get(pc).map_or(0, |c| c.load(Ordering::Relaxed));
        let n = self
            .not_taken
            .get(pc)
            .map_or(0, |c| c.load(Ordering::Relaxed));
        (t, n)
    }

    /// How many bytecodes this was sized for.
    pub fn code_len(&self) -> usize {
        self.taken.len()
    }

    /// Every pc with at least one observation, in pc order.
    pub fn observed(&self) -> impl Iterator<Item = (usize, u32, u32)> + '_ {
        (0..self.taken.len()).filter_map(move |pc| {
            let (t, n) = self.counts(pc);
            (t != 0 || n != 0).then_some((pc, t, n))
        })
    }
}

#[cfg(test)]
mod branch_counter_tests {
    use super::*;

    #[test]
    fn records_both_directions_independently() {
        let c = BranchCounters::new(4);
        c.record(1, true);
        c.record(1, true);
        c.record(1, false);
        assert_eq!(c.counts(1), (2, 1));
        assert_eq!(c.counts(0), (0, 0));
        assert_eq!(c.observed().collect::<Vec<_>>(), vec![(1, 2, 1)]);
    }

    /// A per-method arm is sticky, reports only its first call, and is seen
    /// by every later caller of the slot's own length.
    #[test]
    fn a_method_arm_is_sticky_and_shared_by_the_slots_own_length() {
        let c = BranchCounters::new(4);
        assert!(!c.is_armed());
        assert!(c.arm(), "the first arm arms");
        assert!(!c.arm(), "a second one reports it already was");
        assert!(c.is_armed());

        let mut p = MethodProfile::default();
        let first = p.flat_branches_for(4);
        assert!(!first.is_armed());
        first.arm();
        let again = p.flat_branches_for(4);
        assert!(Arc::ptr_eq(&first, &again), "one array per slot");
        assert!(again.is_armed());
    }

    /// Round 11 wave 14 (r11-tier-profile-replay-residuals item 2): a caller
    /// with another length is another version of the method. It gets counters
    /// of its own, which never reach the slot's, and the slot is marked so
    /// its record never seeds a later run. The owner's handle stays live.
    #[test]
    fn another_versions_length_gets_detached_counters_and_marks_the_slot() {
        let mut p = MethodProfile::default();
        let owner = p.flat_branches_for(8);
        owner.record(2, true);
        assert_eq!(p.recorded_code_len(), 8);

        let other = p.flat_branches_for(64);
        assert!(!Arc::ptr_eq(&owner, &other));
        other.record(40, true);
        other.record(2, false);
        assert_eq!(p.recorded_code_len(), CODE_LEN_CONFLICT);

        // The owner's handle is still the slot's array, not an orphan.
        assert!(Arc::ptr_eq(&owner, &p.flat_branches_for(8)));
        owner.record(2, true);
        let snap = p.snapshot();
        assert_eq!(snap.branches[&2].taken, 2);
        assert_eq!(
            snap.branches[&2].not_taken, 0,
            "the other version's pc 2 is not folded"
        );
        assert!(!snap.branches.contains_key(&40));
        assert_eq!(
            snap.recorded_code_len(),
            CODE_LEN_CONFLICT,
            "a snapshot keeps the mark"
        );
    }

    /// A pc past the end is DROPPED, never a panic: this runs on the
    /// interpreter's dispatch loop, and a disagreement between the recorded
    /// code length and the pc executing must not take the VM down.
    #[test]
    fn an_out_of_range_pc_is_dropped_rather_than_panicking() {
        let c = BranchCounters::new(2);
        c.record(9999, true);
        assert_eq!(c.counts(9999), (0, 0));
        assert!(c.observed().next().is_none());
    }

    /// Saturating, not wrapping. `record_receiver`'s comment states what
    /// wrapping costs — the majority direction reading as the minority — and
    /// this counter is read by the same consumers.
    #[test]
    fn a_counter_at_the_ceiling_stays_there() {
        let c = BranchCounters::new(1);
        c.taken[0].store(u32::MAX, Ordering::Relaxed);
        c.record(0, true);
        c.record(0, true);
        assert_eq!(c.counts(0), (u32::MAX, 0));
    }

    /// A pinned counter never passes through `0` on the way to staying pinned.
    /// The old `fetch_add`-then-restore form wrapped first, so a reader in
    /// between saw a maximally-hot branch as never-taken — and a writer that
    /// slipped in restarted the count from zero.
    #[test]
    fn a_pinned_counter_is_never_momentarily_zero() {
        let c = Arc::new(BranchCounters::new(1));
        c.taken[0].store(u32::MAX, Ordering::Relaxed);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut lowest = u32::MAX;
                while !stop.load(Ordering::Relaxed) {
                    lowest = lowest.min(c.counts(0).0);
                }
                lowest
            })
        };
        for _ in 0..200_000 {
            c.record(0, true);
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            reader.join().expect("reader thread"),
            u32::MAX,
            "a saturated counter must never be observed below its ceiling"
        );
    }

    /// The fold is what every compiler-side reader actually sees, and it must
    /// ADD the two sources rather than let one shadow the other: a method can
    /// legitimately be recorded through both (a frame with a
    /// `CachedBytecodeMethod` uses the fast path, one without does not).
    #[test]
    fn snapshot_adds_the_flat_counters_to_the_map() {
        let mut p = MethodProfile::default();
        p.record_branch(3, true); // map side
        let flat = p.flat_branches_for(8);
        flat.record(3, true); // fast side, same pc
        flat.record(5, false);

        let snap = p.snapshot();
        assert_eq!(snap.branches[&3].taken, 2, "both sources must be counted");
        assert_eq!(snap.branches[&5].not_taken, 1);
        assert!(
            snap.flat_branches.is_none(),
            "a snapshot must not fold twice",
        );
    }

    /// A second-version caller does not disturb what the owner recorded:
    /// `snapshot` still reports every observation made through the slot.
    #[test]
    fn another_version_keeps_the_observations_already_recorded() {
        let mut p = MethodProfile::default();
        p.flat_branches_for(4).record(2, true);
        let _other = p.flat_branches_for(64);
        assert_eq!(p.snapshot().branches[&2].taken, 1);
    }
}

impl MethodProfile {
    /// A compiler-facing copy of this profile, with the lock-free branch
    /// counters folded into [`Self::branches`].
    ///
    /// Every reader of a profile — the scheduler's `branch_counts`, the
    /// branch-polarity hints, the inline planner — sees one map, exactly as
    /// before the fast path existed. That is the whole point of folding here
    /// rather than teaching each of them about two sources: a consumer that
    /// forgot the second one would silently read a method as unprofiled.
    ///
    /// The two sources are ADDED rather than one replacing the other. They can
    /// both be populated for one method: the fast path needs a frame carrying a
    /// `CachedBytecodeMethod`, and a frame without one (the launcher's `main`,
    /// some reflective entries) still records through the map. Adding is right
    /// because each observation was recorded exactly once, into exactly one of
    /// them.
    ///
    /// The result's own `flat_branches` is `None`: it is a snapshot, and a
    /// second fold would double-count.
    pub fn snapshot(&self) -> MethodProfile {
        let mut branches = self.branches.clone();
        if let Some(flat) = &self.flat_branches {
            for (pc, taken, not_taken) in flat.observed() {
                let entry = branches.entry(pc).or_default();
                entry.taken = entry.taken.saturating_add(taken);
                entry.not_taken = entry.not_taken.saturating_add(not_taken);
            }
        }
        MethodProfile {
            branches,
            receivers: self.receivers.clone(),
            loops: self.loops.clone(),
            call_sites: self.call_sites.clone(),
            flat_branches: None,
            // Carried, not dropped: `crate::profile_store`'s outcome census is
            // a walk over `snapshot_all()`, and a snapshot that lost the
            // provenance would report every replayed method as never seeded.
            replay: self.replay.clone(),
            code_len: self.code_len,
            code_len_conflict: self.code_len_conflict,
            loop_extents: None,
            reclaimed: self.reclaimed,
        }
    }

    /// Stamp the bytecode length of the method this profile describes, and
    /// answer whether `code_len` is the slot's own. The first stamp owns the
    /// slot and sticks; a different length later is another version of the
    /// method (a redefined class's obsolete frame and its new version share
    /// one key, since `invalidate_class` keeps `class_id`). That marks the
    /// profile [`CODE_LEN_CONFLICT`] for a save, which a load refuses, and
    /// answers `false`.
    pub(crate) fn note_code_len(&mut self, code_len: usize) -> bool {
        let Ok(len) = u32::try_from(code_len) else {
            self.code_len_conflict = true;
            return false;
        };
        if self.code_len == 0 || self.code_len == len {
            self.code_len = len;
            return true;
        }
        self.code_len_conflict = true;
        false
    }

    /// Whether a map-path recorder whose frame's `code.len()` is
    /// `frame_code_len` may be running this slot's own version
    /// (r11-tier-profile-replay-residuals item 2, the map-path half; round 12
    /// wave 3). Such a frame has no cached method, so its code may or may not
    /// carry `frame::padded_bytecode`'s two trailing bytes: both readings are
    /// accepted, so the slot's own version is never refused. An unstamped slot
    /// accepts anything. Only a length that is neither the stamp nor the stamp
    /// plus two is refused: a frame of another version of the method (an
    /// obsolete frame `obsolete_frames` has not moved yet) whose pcs would
    /// mean other instructions here. Neither stamps nor conflicts the slot;
    /// the recorder does not know which reading is the real length.
    ///
    /// `CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK=0` records such observations
    /// again; read only on the refusal path.
    pub(crate) fn admits_frame_code_len(&self, frame_code_len: usize) -> bool {
        let own = self.code_len as usize; // Cast: u32 -> usize widens
        if self.code_len == 0
            || frame_code_len == own
            || frame_code_len.checked_sub(2) == Some(own)
        {
            return true;
        }
        !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK")
    }

    /// The stamp [`Self::note_code_len`] left: `0` = unknown,
    /// [`CODE_LEN_CONFLICT`] = two versions reached this slot.
    pub(crate) fn recorded_code_len(&self) -> u32 {
        if self.code_len_conflict {
            CODE_LEN_CONFLICT
        } else {
            self.code_len
        }
    }

    /// The lock-free counters for this method, allocating them on first use.
    ///
    /// `code_len` sizes the allocation. A caller whose length is not the
    /// slot's own ([`Self::note_code_len`] answers `false`) is running another
    /// version of the method, and gets counters of its own that are never
    /// stored or folded. The two versions' pcs used to be merged here: the
    /// array regrew to the longer length and drained the other version's
    /// counts into the map, and every other thread's memoised handle to the
    /// old array went dead (r11-tier-profile-replay-residuals item 2).
    ///
    /// Which version owns the slot is whichever stamped it first after the
    /// class's `invalidate_class`. The interpreter's per-thread memo keeps an
    /// obsolete frame on the counters it already holds, so that is usually the
    /// new version. A slot an obsolete frame won is marked conflicted when the
    /// new version arrives, so its record never seeds a later run.
    pub fn flat_branches_for(&mut self, code_len: usize) -> Arc<BranchCounters> {
        if !self.note_code_len(code_len) {
            return Arc::new(BranchCounters::new(code_len));
        }
        match &self.flat_branches {
            Some(c) if c.code_len() >= code_len => Arc::clone(c),
            _ => {
                let fresh = Arc::new(BranchCounters::new(code_len));
                // An old array can be shorter only when a zero-length caller
                // stamped nothing first; the drain below stays as the
                // fail-safe for it.
                // Carry what the old (shorter) array holds into the MAP rather
                // than into the new array. Both are folded together by
                // `snapshot`, so the evidence survives, and draining into the
                // map keeps this path free of any assumption about which cells
                // of the new array a concurrent recorder may already be
                // touching.
                if let Some(old) = self.flat_branches.take() {
                    // A per-method arm is a request about the METHOD, not the
                    // array; the regrown array keeps it.
                    if old.is_armed() {
                        fresh.arm();
                    }
                    for (pc, t, n) in old.observed() {
                        let entry = self.branches.entry(pc).or_default();
                        entry.taken = entry.taken.saturating_add(t);
                        entry.not_taken = entry.not_taken.saturating_add(n);
                    }
                }
                self.flat_branches = Some(Arc::clone(&fresh));
                fresh
            }
        }
    }

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
        // Live-path cost when nothing was replayed — the only case in a default
        // run — is one `Option` discriminant test on a field this function has
        // already brought into cache. The map walk is behind it.
        if self.replay.is_some() {
            self.reconcile_replayed_receiver(pc, class_id);
        }
    }

    /// Judge the replayed receiver seeds at `pc` against one live observation.
    ///
    /// Called only when [`Self::replay`] is `Some`. See [`ReplayProvenance`]
    /// for the confirm/refute rule and why refutation *subtracts* rather than
    /// averages.
    ///
    /// Marked `#[cold]` rather than `#[inline]`: it runs at most once per
    /// seeded `(pc, class)` pair for the life of the process, because both
    /// outcomes remove the pair from the pending list.
    #[cold]
    fn reconcile_replayed_receiver(&mut self, pc: usize, observed: u32) {
        let Some(boxed) = self.replay.as_mut() else {
            return;
        };
        // Through the `Box` once, here, so the disjoint-field borrows below
        // (`pending_receivers` against the two counters) are plain field
        // borrows of one `&mut ReplayProvenance`.
        let replay: &mut ReplayProvenance = &mut **boxed;
        let Some(pending) = replay.pending_receivers.get_mut(&pc) else {
            return;
        };
        // An emptied-but-retained entry means every seed at this pc has already
        // been judged. Returning here — rather than falling into the `None` arm
        // below — is what stops a settled site being re-counted as refuted on
        // every subsequent dispatch.
        if pending.is_empty() {
            return;
        }
        let refuted: Vec<(u32, u32)> = match pending.iter().position(|&(c, _)| c == observed) {
            Some(idx) => {
                pending.swap_remove(idx);
                replay.receivers_confirmed = replay.receivers_confirmed.saturating_add(1);
                replay
                    .confirmed_receivers
                    .entry(pc)
                    .or_default()
                    .push(observed);
                return;
            }
            // A seed at this pc the live run already confirmed: the replay DID
            // name it, so it contradicts nothing.
            None if replay
                .confirmed_receivers
                .get(&pc)
                .is_some_and(|seen| seen.contains(&observed)) =>
            {
                return;
            }
            // Contradiction: the live run saw a class this pc's replay never
            // named. Take the whole pending list — the seeds that survive are
            // the ones this site is no longer entitled to.
            None => std::mem::take(pending),
        };
        replay.receivers_refuted = replay
            .receivers_refuted
            .saturating_add(refuted.len().min(u32::MAX as usize) as u32);
        replay.shape_contradicted = true;
        let Some(site) = self.receivers.get_mut(&pc) else {
            return;
        };
        for (class_id, seeded) in refuted {
            let emptied = match site.get_mut(&class_id) {
                Some(n) => {
                    *n = n.saturating_sub(seeded);
                    *n == 0
                }
                None => false,
            };
            if emptied {
                site.remove(&class_id);
            }
        }
        // The live `+1` recorded by the caller keeps this map non-empty, so the
        // site never regresses to `receiver_summary(pc) == None` ("never
        // recorded here") on account of an undo.
        if site.is_empty() {
            self.receivers.remove(&pc);
        }
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

    /// Record one exit of the loop whose back edge is at `backedge_pc`. See
    /// [`LoopTripProfile::record_exit`].
    #[inline]
    pub fn record_loop_exit(&mut self, backedge_pc: usize) {
        self.loops.entry(backedge_pc).or_default().record_exit();
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

// ---------------------------------------------------------------------------
// Replay seeding
// ---------------------------------------------------------------------------
//
// Everything below this line exists for `crate::profile_store` and for nothing
// else. It is on `MethodProfile` rather than in that module because the fields
// it touches are the ones the live path touches, and the two have to agree
// about saturation: a seed that wrapped where a live record saturates would
// reintroduce, through the back door, exactly the inverted-majority bug
// `record_receiver`'s comment describes.
//
// None of these is `#[inline]` and none is on any hot path: they run once per
// method at VM init, under the same per-method `Mutex` the recorders take.

impl MethodProfile {
    /// Whether any part of this profile was replayed rather than observed.
    pub fn is_from_replay(&self) -> bool {
        self.replay.is_some()
    }

    /// Replay provenance, if this method was seeded. See [`ReplayProvenance`].
    pub fn replay_provenance(&self) -> Option<&ReplayProvenance> {
        self.replay.as_deref()
    }

    /// The provenance record, creating it if this is the first seed.
    fn replay_mut(&mut self) -> &mut ReplayProvenance {
        let slot = self
            .replay
            .get_or_insert_with(|| Box::new(ReplayProvenance::default()));
        &mut **slot
    }

    /// Note that `n` of this method's invocation-counter credit was replayed.
    ///
    /// The counter itself lives in `ProfileStore`'s `invocation_counts` map and
    /// is seeded through [`ProfileStore::add_invocations`]; this records only the
    /// provenance, so the two can drift if a caller does one without the other.
    /// `crate::profile_store::load_into` is the one caller and does both.
    pub fn note_replayed_invocations(&mut self, n: u32) {
        let replay = self.replay_mut();
        replay.seeded_invocations = replay.seeded_invocations.saturating_add(n);
    }

    /// Seed one branch site from a replay.
    ///
    /// Added to whatever is already there, saturating, exactly as
    /// [`Self::record_branch`] would have added the same observations one at a
    /// time. No CONSUMER sees provenance per direction: a branch hint is a
    /// layout choice, a wrong one costs a mis-laid-out block, and nothing
    /// downstream needs to know which observations produced the ratio — the
    /// caller's job is to keep the seed small enough that live observations
    /// can outvote it, which `crate::profile_store::ReplaySeedPolicy` documents
    /// and does. The seeded amounts are still remembered
    /// (`ReplayProvenance::seeded_branch`), so a save writes back only what
    /// this run observed and a seed ages out instead of being re-seeded on
    /// every load (round 11 wave 19).
    pub fn seed_replayed_branch(&mut self, pc: usize, taken: u32, not_taken: u32) {
        if taken == 0 && not_taken == 0 {
            return;
        }
        let entry = self.branches.entry(pc).or_default();
        entry.taken = entry.taken.saturating_add(taken);
        entry.not_taken = entry.not_taken.saturating_add(not_taken);
        let replay = self.replay_mut();
        replay.seeded_branch_sites = replay.seeded_branch_sites.saturating_add(1);
        let seeded = replay.seeded_branches.entry(pc).or_insert((0, 0));
        seeded.0 = seeded.0.saturating_add(taken);
        seeded.1 = seeded.1.saturating_add(not_taken);
    }

    /// Seed one `(call site, receiver class)` observation from a replay, and
    /// record it as awaiting a live verdict.
    ///
    /// `class_id` is **this run's** id for the class, resolved by name by the
    /// caller. Seeding a previous run's raw id would be a wrong-code bug:
    /// `ClassId` is allocated per VM and per run, so last run's `41` names a
    /// different class this run with probability approaching one.
    pub fn seed_replayed_receiver(&mut self, pc: usize, class_id: u32, count: u32) {
        if count == 0 {
            return;
        }
        let site = self.receivers.entry(pc).or_default();
        let cell = site.entry(class_id).or_insert(0);
        *cell = cell.saturating_add(count);
        self.replay_mut().note_pending_receiver(pc, class_id, count);
    }

    /// Seed one loop's trip profile from a replay.
    ///
    /// `entry_count` and `total_trips` are seeded as a pair so the *average*
    /// they encode survives; the average is what `suggests_unroll_factor`
    /// reads, and it is scale-free, so scaling one without the other would
    /// change the hint rather than weaken it.
    pub fn seed_replayed_loop(
        &mut self,
        backedge_pc: usize,
        backedge_count: u64,
        entry_count: u32,
        total_trips: u64,
    ) {
        if backedge_count == 0 && entry_count == 0 {
            return;
        }
        let lp = self.loops.entry(backedge_pc).or_default();
        lp.backedge_count = lp.backedge_count.saturating_add(backedge_count);
        lp.entry_count = lp.entry_count.saturating_add(entry_count);
        lp.total_trips = lp.total_trips.saturating_add(total_trips);
        // The seeded back edges belong to the seeded entries, not to the
        // first live exit (`LoopTripProfile::record_exit`).
        lp.backedges_at_last_exit = lp.backedges_at_last_exit.saturating_add(backedge_count);
        let seeded = self
            .replay_mut()
            .seeded_loops
            .entry(backedge_pc)
            .or_insert((0, 0, 0));
        seeded.0 = seeded.0.saturating_add(backedge_count);
        seeded.1 = seeded.1.saturating_add(entry_count);
        seeded.2 = seeded.2.saturating_add(total_trips);
    }

    /// Seed one kind-agnostic call-site execution count from a replay.
    pub fn seed_replayed_call_site(&mut self, pc: usize, count: u32) {
        if count == 0 {
            return;
        }
        let e = self.call_sites.entry(pc).or_insert(0);
        *e = e.saturating_add(count);
        let seeded = self.replay_mut().seeded_call_sites.entry(pc).or_insert(0);
        *seeded = seeded.saturating_add(count);
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
///
/// Public, with [`loop_work_credit`], so the VM's loop-work nomination
/// (`vm/src/runtime/interpreter.rs`, the frame-pop harvest) subtracts the
/// credit this store applied instead of re-deriving it from a copy of the
/// divisor.
pub const LOOP_WORK_PER_INVOCATION: u32 = 32;

/// The invocation credit [`ProfileStore::add_loop_work`] applies for
/// `iterations` completed back edges -- the one definition of the rounding, so
/// a caller asking "did this credit carry the counter across the threshold"
/// cannot disagree with the store about how large the credit was.
#[inline]
pub const fn loop_work_credit(iterations: u32) -> u32 {
    iterations / LOOP_WORK_PER_INVOCATION
}

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
    counts: parking_lot::RwLock<FxHashMap<u128, AtomicU32>>,
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
/// The packed key is `(class_id << 64) | fingerprint(name, descriptor)` (see
/// `cratonvm_jit_api::invoc_key_parts`), so the low 64 bits are the
/// well-distributed part; the high bits would cluster every method of one
/// class into one shard.
#[inline]
fn invocation_shard_for(packed_key: u128) -> usize {
    (packed_key as u64 as usize) & (INVOCATION_SHARDS - 1)
}

/// Add `n` to an invocation counter cell, pinning at `u32::MAX`.
///
/// A compare-and-swap loop, not `fetch_add` followed by `saturating_add` on the
/// value it returned: that pair pinned the RETURNED value but let the cell
/// itself wrap, so a batched door's 16-call credit or a long loop's work credit
/// could carry a hot method's counter back through zero -- and a method below
/// the threshold again is a method that stops being offered to the JIT.
#[inline]
fn saturating_add_cell(cell: &AtomicU32, n: u32) -> u32 {
    match cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
        (c != u32::MAX).then(|| c.saturating_add(n))
    }) {
        Ok(previous) => previous.saturating_add(n),
        Err(saturated) => saturated,
    }
}

/// Increment an invocation counter cell, preserving the exact
/// `u32::saturating_add(1)` semantics of the pre-sharding implementation.
///
/// # Why not `fetch_add` and restore
///
/// This was `fetch_add(1)` followed by `store(u32::MAX)` when the returned
/// value showed a wrap -- the same shape [`BranchCounters::record`] documents
/// and was rewritten away from. It is not a saturating increment: at the cap
/// `fetch_add` WRAPS THE CELL TO 0 and only the next instruction restores it,
/// so a concurrent incrementer in that window counts up from 0 and returns 1,
/// and a concurrent reader (`snapshot_invocation_counts`) sees a maximally hot
/// method as never invoked. Worse, once pinned EVERY call wrapped and restored
/// again, so the window re-opened on every invocation for the rest of the
/// process. The premise that the cap was "astronomically rare" did not hold
/// either: 2^32 interpreted calls of a method that never compiles (an
/// `ineligible` one, say) is a few hours of a busy server.
///
/// `fetch_update` with `checked_add` never leaves the cell in a state it
/// should not be in, and a pinned cell is a single load and no store.
#[inline]
fn saturating_inc(cell: &AtomicU32) -> u32 {
    match cell.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
        Ok(previous) => previous + 1,
        Err(saturated) => saturated,
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

    /// Profile-replay records (`CRATONVM_JIT_PROFILE_LOAD`) whose class was not
    /// defined yet when the file was loaded, waiting for the VM's
    /// class-definition hook. Owned and driven by [`crate::profile_store`];
    /// empty (one atomic) in every run that does not replay.
    pub(crate) replay_pending: crate::profile_store::ReplayPending,
}

impl ProfileStore {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| ProfileShard::new()),
            invocation_counts: std::array::from_fn(|_| InvocationShard::new()),
            name_index_collisions: std::sync::atomic::AtomicU64::new(0),
            name_index_benign_races: std::sync::atomic::AtomicU64::new(0),
            replay_pending: crate::profile_store::ReplayPending::default(),
        }
    }

    /// Reclaim every profile and warmup counter owned by an unloaded class.
    pub fn invalidate_class(&self, class_id: u32) {
        self.invalidate_where(|cid| cid == class_id);
    }

    /// Batch form of [`Self::invalidate_class`] for a whole unloaded set: ONE
    /// sweep of every shard instead of one per class (gc-common w36-c,
    /// `docs/internal/gc-common-round-20260923/common-w8e-class-unload-sweeps-whole-tables-once-per-class-FIXED-20260926.md`).
    /// Equivalent to calling `invalidate_class` for each member
    /// (`invalidate_classes_matches_a_loop_of_invalidate_class`).
    pub fn invalidate_classes(&self, class_ids: &rustc_hash::FxHashSet<u32>) {
        if class_ids.is_empty() {
            return;
        }
        self.invalidate_where(|cid| class_ids.contains(&cid));
    }

    fn invalidate_where(&self, dead: impl Fn(u32) -> bool) {
        // Sharded by the *low* 64 bits (see `invocation_shard_for`), so a
        // class's methods are spread across every shard — all of them must be
        // swept. Cold path (class unloading), so the full walk is fine.
        for shard in &self.invocation_counts {
            shard
                .counts
                .write()
                .retain(|packed, _| !dead((*packed >> 64) as u32));
        }
        // Removed slots are marked (round 13 wave 3, lane replay2) so a
        // `ReceiverRecorder` a thread still holds for one (the invoke doors'
        // memo, keyed by the INTERNED method name, which a redefined or
        // reloaded class shares) stops writing into it and looks the method
        // up again. Marked after the shard locks are dropped: the slot lock
        // stays a leaf, as `snapshot_all` keeps it.
        let mut removed: Vec<Arc<parking_lot::Mutex<MethodProfile>>> = Vec::new();
        for shard in &self.shards {
            let mut methods = shard.methods.write();
            methods.retain(|key, slot| {
                let keep = !dead(key.class_id);
                if !keep {
                    removed.push(Arc::clone(slot));
                }
                keep
            });
            let mut index = shard.name_index.write();
            index.retain(|_, (key, _)| !dead(key.class_id));
        }
        for slot in removed {
            slot.lock().reclaimed = true;
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
    /// `(class_id << 64) | fingerprint(name, descriptor)`. Returns the new count.
    ///
    /// Called from the interpreter's cached bytecode dispatch to gate JIT compilation
    /// behind a warmup threshold instead of compiling on the second invocation.
    #[inline]
    pub fn increment_invocation(&self, packed_key: u128) -> u32 {
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

    /// `increment_invocation` by `n` at once. The interpreter's virtual fast
    /// door counts on `CachedBytecodeMethod::interp_invocations` (one relaxed
    /// `fetch_add`) and folds the count in here every few calls, so this
    /// store still sees every call for the census while the per-call path no
    /// longer takes a shard lock and a hash lookup.
    pub fn add_invocations(&self, packed_key: u128, n: u32) -> u32 {
        let shard = &self.invocation_counts[invocation_shard_for(packed_key)];
        {
            let read = shard.counts.read();
            if let Some(cell) = read.get(&packed_key) {
                return saturating_add_cell(cell, n);
            }
        }
        let mut write = shard.counts.write();
        match write.get(&packed_key) {
            Some(cell) => saturating_add_cell(cell, n),
            None => {
                write.insert(packed_key, AtomicU32::new(n));
                n
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
    pub fn add_loop_work(&self, packed_key: u128, iterations: u32) -> u32 {
        let credit = loop_work_credit(iterations);
        if credit == 0 {
            return 0;
        }
        let shard = &self.invocation_counts[invocation_shard_for(packed_key)];
        {
            let read = shard.counts.read();
            if let Some(cell) = read.get(&packed_key) {
                return saturating_add_cell(cell, credit);
            }
        }
        let mut write = shard.counts.write();
        match write.get(&packed_key) {
            Some(cell) => saturating_add_cell(cell, credit),
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
    ///
    /// `frame_code_len` is the recording frame's `code.len()`, padded or not
    /// (the map path serves frames with no cached method, whose padding is
    /// not guaranteed). An observation whose frame cannot be the slot's own
    /// version is dropped ([`MethodProfile::admits_frame_code_len`]).
    #[inline]
    pub fn record_branch_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        taken: bool,
        frame_code_len: usize,
    ) {
        if !is_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        let mut p = slot.lock();
        if p.admits_frame_code_len(frame_code_len) {
            p.record_branch(pc, taken);
        }
    }

    /// The lock-free branch counters for one method, allocating them on first
    /// use.
    ///
    /// This is the SLOW half of the fast path, and it is meant to run once per
    /// method rather than once per branch: it pays the fingerprint, the shard
    /// lock and the slot mutex exactly as [`Self::record_branch_borrowed`]
    /// does, and hands back a handle the caller can keep. Every recording
    /// through that handle afterwards is one relaxed `fetch_add`.
    ///
    /// Deliberately NOT gated on [`is_profiling_enabled`]: the caller decides
    /// whether to record, and a caller that already holds the handle must not
    /// have to re-ask this function to find out.
    pub fn branch_counters_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        code_len: usize,
    ) -> Arc<BranchCounters> {
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        let mut p = slot.lock();
        p.flat_branches_for(code_len)
    }

    /// [`Self::branch_counters_borrowed`], then [`BranchCounters::arm`]: ask
    /// for this one method's branches to be recorded from now on, whatever
    /// the process-wide gate says. For the VM's first-nomination door (the
    /// invocation hook's `Interpreter -> C1` recommendation); returns the
    /// handle so the caller can cache it beside the method.
    pub fn arm_branch_counters_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        code_len: usize,
    ) -> Arc<BranchCounters> {
        let counters = self.branch_counters_borrowed(class_id, method_name, descriptor, code_len);
        counters.arm();
        counters
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
    ///
    /// `frame_code_len` as for [`Self::record_branch_borrowed`]: a receiver
    /// seen by a frame of another version of the method (an obsolete frame
    /// `obsolete_frames` has not moved yet) is dropped, so it cannot pick the
    /// new version's guarded-inline receiver (round 13 wave 3, lane replay2;
    /// r11-tier-profile-replay-residuals item 2, the receiver residue).
    #[inline]
    pub fn record_receiver_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        receiver_class_id: u32,
        frame_code_len: usize,
    ) {
        if !is_receiver_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        let mut p = slot.lock();
        if p.admits_frame_code_len(frame_code_len) {
            p.record_receiver(pc, receiver_class_id);
        }
    }

    /// Borrowed-key counterpart of [`record_call_site`](Self::record_call_site).
    ///
    /// Intended to be called from the interpreter's invoke dispatch for EVERY
    /// invoke kind — see [`MethodProfile::call_sites`] for why per-method
    /// invocation counts cannot substitute. `frame_code_len` as for
    /// [`Self::record_branch_borrowed`].
    #[inline]
    pub fn record_call_site_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        pc: usize,
        frame_code_len: usize,
    ) {
        if !is_receiver_profiling_enabled() {
            return;
        }
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        let mut p = slot.lock();
        if p.admits_frame_code_len(frame_code_len) {
            p.record_call_site(pc);
        }
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

    /// Borrowed-key recorder of one loop exit (round 11 wave 19). The
    /// interpreter calls it from a taken forward branch that leaves a loop,
    /// under the loop's key ([`LoopExtents::exits_at`], via
    /// `interpreter::record_loop_exit_for_frame`). Paired with
    /// [`Self::record_backedge_borrowed`] at that `goto`, it gives
    /// `LoopTripProfile::avg_trip_count` a live average, which is what the
    /// single-pass backend's `loop_unroll_hints` reads.
    #[inline]
    pub fn record_loop_exit_borrowed(
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
        slot.lock().record_loop_exit(backedge_pc);
    }

    /// The [`LoopExtents`] of the body `code` of this method, built on the
    /// first call for those bytes and kept in the method's slot. The
    /// interpreter memoises the answer per thread by `code`'s identity, so
    /// this runs about once per thread per body.
    ///
    /// A slot shared by two versions of a redefined method holds the last one
    /// asked about; the table is a pure function of the bytes, so a body with
    /// equal bytes but another allocation reuses it, and another version
    /// rebuilds it.
    pub fn loop_extents_borrowed(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        code: &Arc<[u8]>,
    ) -> Arc<LoopExtents> {
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        let mut p = slot.lock();
        if let Some((have, extents)) = &p.loop_extents {
            if Arc::ptr_eq(have, code) || have[..] == code[..] {
                return Arc::clone(extents);
            }
        }
        let fresh = Arc::new(LoopExtents::build(code));
        p.loop_extents = Some((Arc::clone(code), Arc::clone(&fresh)));
        fresh
    }

    /// Record a completed loop trip count.  Called when a loop exits.
    ///
    /// No production caller: the interpreter has no per-frame trip counter.
    /// The live producer is [`Self::record_loop_exit_borrowed`]; this one and
    /// its `MethodProfile` form serve the tests' hand-built profiles.
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
        // Snapshot: clone branch + receiver + loop maps, folding in the
        // lock-free branch counters (`MethodProfile::snapshot`).
        Some(p.snapshot())
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
                (k, p.snapshot())
            })
            .collect()
    }

    /// Run `f` against one method's live profile slot, creating the slot if this
    /// method has never been profiled.
    ///
    /// # What this is for, and what it is not for
    ///
    /// It exists for `crate::profile_store`'s replay path, which has to write a
    /// whole method's worth of seeds — branches, receivers, loops, call sites —
    /// under **one** acquisition of that method's `Mutex`, and has to be able to
    /// create the slot for a method the interpreter has not reached yet (which
    /// is the entire point of replaying: the seeds must be in place *before*
    /// the method first runs).
    ///
    /// It is deliberately not a general mutation hook, and it is not on any hot
    /// path. Every recorder above goes through a `record_*` method that names
    /// what it records; a caller here can write anything, including numbers no
    /// interpreter hook could have produced. Two rules, both of which
    /// `crate::profile_store` obeys:
    ///
    /// * **Do not call it from the interpreter.** `f` runs with the per-method
    ///   `Mutex` held and the shard lock already released, so it is safe, but a
    ///   per-dispatch caller would pay the fingerprint and both locks that
    ///   [`Self::record_receiver_borrowed`] exists to amortise.
    /// * **Do not use it to write counts nothing can revise.** Everything the
    ///   compiler reads from this store is a heuristic that live observation
    ///   outvotes (`docs/jit/pgo-inlining.md` §3). A seed large enough that no
    ///   realistic live traffic can outvote it converts a hint into a fact, and
    ///   this store has no facts.
    ///
    /// Not gated on [`is_profiling_enabled`]: the gate governs *recording*, and
    /// seeding runs at VM init, before the tiered manager has decided whether
    /// to turn recording on. A seeded profile that the run never adds to is
    /// still a usable hint, and gating here would make replay silently depend
    /// on the order of two unrelated init steps.
    pub fn with_method_profile_mut<R>(
        &self,
        key: &MethodKey,
        f: impl FnOnce(&mut MethodProfile) -> R,
    ) -> R {
        let slot = self.get_or_insert(key);
        let mut profile = slot.lock();
        f(&mut profile)
    }

    /// Snapshot all invocation counts: returns (packed_key, count) pairs.
    pub fn snapshot_invocation_counts(&self) -> Vec<(u128, u32)> {
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

        // r11-tier: both directions near saturation. The saturating `total()`
        // pinned at `u32::MAX` and read this 50/50 branch as usually taken.
        let saturated = BranchCounts {
            taken: 3_900_000_000,
            not_taken: 3_900_000_000,
        };
        assert!(
            !saturated.is_usually_taken(),
            "a 50/50 branch is not biased"
        );
        assert!(!saturated.is_usually_not_taken());
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

    /// A held [`ReceiverRecorder`] must keep writing into the SAME slot the
    /// store hands out — that is the property the invoke fast door's one-entry
    /// memo rests on, and the one that would fail silently if it broke.
    ///
    /// Silently is the point: a stale handle does not error, it DROPS the
    /// records. Dropped receiver records are exactly the biased profile that
    /// made the door return wrong answers
    /// (`testrandommapops-deterministic-1810-null-FIXED-20260904.md`),
    /// so this is pinned by a test rather than by the comment that says slots
    /// are insert-only.
    #[test]
    fn a_held_receiver_recorder_stays_the_stores_slot() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let class_id = 7u32;
            let name: std::sync::Arc<str> = std::sync::Arc::from("hot");
            let desc: std::sync::Arc<str> = std::sync::Arc::from("()V");

            // Take the handle FIRST, the way the memo does on its first call.
            let rec = store.receiver_recorder_borrowed(class_id, &name, &desc);

            // Insert other methods in between, so the handle has to survive
            // whatever the store does to its shards while it is held.
            for other in 0..64u32 {
                let n: std::sync::Arc<str> = std::sync::Arc::from(format!("m{other}"));
                store.record_receiver_borrowed(class_id + 1 + other, &n, &desc, 1, 1, 16);
            }

            // Now interleave the two routes into the same method.
            for _ in 0..10 {
                rec.record(11, 99, 16);
            }
            for _ in 0..5 {
                store.record_receiver_borrowed(class_id, &name, &desc, 11, 99, 16);
            }

            let key = MethodKey {
                class_id,
                method_name: name.clone(),
                descriptor: desc.clone(),
            };
            let profile = store
                .get_profile(&key)
                .expect("the handle and the store must name the same method");
            let seen = profile.receivers[&11][&99];
            assert_eq!(
                seen, 15,
                "handle-recorded and store-recorded observations must land in \
                 one slot; a stale handle would show only the store's 5"
            );
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
        // share a shard, since `invocation_shard_for` masks the low 64 bits.
        // This is the case most likely to expose a lost update.
        let key_a: u128 = (1u128 << 64) | 0xDEAD_BEEF;
        let key_b: u128 = (2u128 << 64) | 0xDEAD_BEEF;
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
        let key: u128 = 0x0BAD_C0DE;
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

    /// A pinned invocation cell is never observed below the cap, and no
    /// concurrent incrementer is ever handed a restarted count. The old
    /// `fetch_add`-then-restore form wrapped the cell to 0 on EVERY call once
    /// pinned, so both a reader and a racing incrementer could see ~0.
    #[test]
    fn a_pinned_invocation_counter_is_never_momentarily_restarted() {
        let store = Arc::new(ProfileStore::new());
        let key: u128 = (9u128 << 64) | 0x5A7_0000;
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard.counts.write().insert(key, AtomicU32::new(u32::MAX));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut lowest = u32::MAX;
                while !stop.load(Ordering::Relaxed) {
                    for (k, n) in store.snapshot_invocation_counts() {
                        if k == key {
                            lowest = lowest.min(n);
                        }
                    }
                }
                lowest
            })
        };
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let mut lowest = u32::MAX;
                    for _ in 0..20_000 {
                        lowest = lowest.min(store.increment_invocation(key));
                    }
                    lowest
                })
            })
            .collect();
        for w in writers {
            assert_eq!(
                w.join().expect("writer"),
                u32::MAX,
                "an incrementer was handed a restarted count"
            );
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(
            reader.join().expect("reader"),
            u32::MAX,
            "a saturated invocation counter must never be observed below its cap"
        );
    }

    /// The batched doors must pin the CELL, not only the value they return:
    /// `fetch_add` let the cell wrap while the returned value saturated, so
    /// the next read restarted a hot method's warmup from almost zero.
    #[test]
    fn batched_invocation_credit_saturates_the_cell() {
        let store = ProfileStore::new();
        let key: u128 = (3u128 << 64) | 0x00C0_FFEE;
        {
            let shard = &store.invocation_counts[invocation_shard_for(key)];
            shard
                .counts
                .write()
                .insert(key, AtomicU32::new(u32::MAX - 3));
        }
        assert_eq!(store.add_invocations(key, 16), u32::MAX);
        assert_eq!(
            store.add_invocations(key, 16),
            u32::MAX,
            "the cell itself stayed pinned instead of wrapping"
        );
        assert_eq!(store.add_loop_work(key, u32::MAX), u32::MAX);
        assert_eq!(store.increment_invocation(key), u32::MAX);
    }

    /// `add_loop_work` credits exactly `loop_work_credit(iterations)`, so the
    /// VM's "did THIS credit cross the threshold" test (the total minus that
    /// credit) names the one credit that crossed, whatever the divisor is.
    #[test]
    fn loop_work_credits_exactly_the_published_credit() {
        let store = ProfileStore::new();
        let key: u128 = (5u128 << 64) | 0x10AD_3A7E;
        assert_eq!(loop_work_credit(LOOP_WORK_PER_INVOCATION - 1), 0);
        assert_eq!(store.add_loop_work(key, LOOP_WORK_PER_INVOCATION - 1), 0);
        let mut total = 0u32;
        for iterations in [LOOP_WORK_PER_INVOCATION * 3 + 7, 300, 1_000, 31, 64] {
            let before = total;
            total = store.add_loop_work(key, iterations).max(total);
            assert_eq!(
                total.saturating_sub(loop_work_credit(iterations)),
                before,
                "the credit applied for {iterations} iterations is the published one"
            );
        }
    }

    /// Overloads must not share a counter. The old key folded name and
    /// descriptor into 32 bits with `31 * h`, where short strings collide by
    /// construction (`"Aa"` and `"BB"` hash alike).
    #[test]
    fn invocation_keys_separate_overloads_and_keep_the_class_id() {
        let a = cratonvm_jit_api::invoc_key_parts(7, "m", "(IJ)V");
        let b = cratonvm_jit_api::invoc_key_parts(7, "m", "(JI)V");
        let c = cratonvm_jit_api::invoc_key_parts(7, "mI", "(J)V");
        let d = cratonvm_jit_api::invoc_key_parts(7, "Aa", "()V");
        let e = cratonvm_jit_api::invoc_key_parts(7, "BB", "()V");
        assert_ne!(a, b);
        assert_ne!(
            a, c,
            "the name/descriptor boundary is part of the fingerprint"
        );
        assert_ne!(d, e);
        assert_eq!((a >> 64) as u32, 7, "the class id stays in the high half");
        let store = ProfileStore::new();
        store.increment_invocation(a);
        assert_eq!(
            store.increment_invocation(b),
            1,
            "an overload starts its own count"
        );
    }

    /// `invalidate_class` must sweep *every* shard: because the shard index
    /// comes from the low 64 bits, one class's methods are spread across all
    /// of them. A single-shard sweep would leak counters for an unloaded
    /// class and let a recycled `class_id` inherit stale warmup state.
    #[test]
    fn invalidate_class_sweeps_all_shards() {
        let store = ProfileStore::new();
        // 256 methods of class 7 — with 64 shards this reliably populates
        // many distinct shards.
        for m in 0..256u128 {
            store.increment_invocation((7u128 << 64) | m);
        }
        // A second class that must survive the sweep.
        for m in 0..256u128 {
            store.increment_invocation((9u128 << 64) | m);
        }
        assert_eq!(store.snapshot_invocation_counts().len(), 512);

        store.invalidate_class(7);

        let remaining = store.snapshot_invocation_counts();
        assert_eq!(remaining.len(), 256, "class 7 counters should all be gone");
        assert!(
            remaining.iter().all(|(k, _)| (*k >> 64) as u32 == 9),
            "only class 9 counters should remain"
        );
    }

    /// gc-common w36-c: the batch form leaves exactly what a loop of the
    /// single form leaves -- in the invocation counters, the profile map and
    /// the name index -- for the empty set, a set naming an absent class, and
    /// a mixed set.
    #[test]
    fn invalidate_classes_matches_a_loop_of_invalidate_class() {
        fn fill() -> ProfileStore {
            let store = ProfileStore::new();
            for class in 1..=6u32 {
                for m in 0..40u128 {
                    store.increment_invocation(((class as u128) << 64) | m);
                }
                for m in 0..5 {
                    let name: Arc<str> = Arc::from(format!("m{m}").as_str());
                    let desc: Arc<str> = Arc::from("()V");
                    // Populates both `methods` and `name_index`.
                    let _ = store.get_or_insert_borrowed(class, &name, &desc);
                    let _ = store.get_or_insert(&MethodKey {
                        class_id: class,
                        method_name: Arc::from(format!("o{m}").as_str()),
                        descriptor: Arc::clone(&desc),
                    });
                }
            }
            store
        }
        fn state(store: &ProfileStore) -> (Vec<u128>, Vec<(u32, String)>, Vec<u32>) {
            let mut counts: Vec<u128> = store
                .snapshot_invocation_counts()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            counts.sort_unstable();
            let mut methods = Vec::new();
            let mut index = Vec::new();
            for shard in &store.shards {
                for key in shard.methods.read().keys() {
                    methods.push((
                        key.class_id,
                        format!("{}{}", key.method_name, key.descriptor),
                    ));
                }
                for (key, _) in shard.name_index.read().values() {
                    index.push(key.class_id);
                }
            }
            methods.sort();
            index.sort_unstable();
            (counts, methods, index)
        }
        let sets: [&[u32]; 4] = [&[], &[99], &[2], &[1, 3, 6, 99]];
        for set in sets {
            let looped = fill();
            for &id in set {
                looped.invalidate_class(id);
            }
            let batched = fill();
            batched.invalidate_classes(&set.iter().copied().collect());
            let (a, b) = (state(&looped), state(&batched));
            assert_eq!(a, b, "set {set:?}");
            assert!(!a.2.is_empty(), "the name index was populated");
            for &id in set {
                assert!(b.0.iter().all(|k| (*k >> 64) as u32 != id));
                assert!(b.1.iter().all(|(c, _)| *c != id));
                assert!(b.2.iter().all(|c| *c != id));
            }
        }
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
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2)); // 8..=32 trips → 2x (R12-2)
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
        // Round 12 wave 2 (R12-2): a long loop is the one unrolling pays
        // for, so avg 100 > 32 → 4x (it used to be no hint).
        assert_eq!(ltp.suggests_unroll_factor(8), Some(4));
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

    /// Tier proposal R12-2: short loops get no copies (`Some(1)`, which the
    /// planner reads as "do not unroll"), long ones the most, and a loop with
    /// back edges but no recorded exit gives no hint at all. The legacy
    /// mapping behind `CRATONVM_JIT_PGO_UNROLL_LEGACY_FACTOR` is unchanged.
    #[test]
    fn the_unroll_factor_grows_with_the_trip_average() {
        let new = |avg: f64, entries: u32| unroll_factor_for(1_000, entries, avg, 8, false);
        assert_eq!(new(1.0, 10), Some(1));
        assert_eq!(new(3.0, 10), Some(1));
        assert_eq!(new(7.9, 10), Some(1));
        assert_eq!(new(8.0, 10), Some(2));
        assert_eq!(new(32.0, 10), Some(2));
        assert_eq!(new(33.0, 10), Some(4));
        assert_eq!(new(10_000.0, 10), Some(4));
        assert_eq!(new(0.0, 0), None, "no exit seen: no average");
        assert_eq!(unroll_factor_for(99, 10, 64.0, 8, false), None, "cold");
        assert_eq!(unroll_factor_for(1_000, 10, 64.0, 2, false), Some(2), "capped");
        let legacy = |avg: f64| unroll_factor_for(1_000, 10, avg, 8, true);
        assert_eq!(legacy(1.0), None);
        assert_eq!(legacy(3.0), Some(4));
        assert_eq!(legacy(20.0), Some(2));
        assert_eq!(legacy(64.0), None);
        assert_eq!(legacy(200.0), None);
    }

    /// Round 11 wave 19: an exit charges the back edges since the previous
    /// exit as its trips, so back edges plus exits alone give the average
    /// `suggests_unroll_factor` reads; replayed back edges are not charged to
    /// the first live exit.
    #[test]
    fn a_loop_exit_charges_the_back_edges_since_the_previous_exit() {
        let mut ltp = LoopTripProfile::default();
        for trips in [6u32, 10, 8] {
            for _ in 0..trips {
                ltp.record_backedge();
            }
            ltp.record_exit();
        }
        assert_eq!((ltp.entry_count, ltp.total_trips), (3, 24));
        assert!((ltp.avg_trip_count() - 8.0).abs() < 0.01);
        // Hot enough once there are 100 back edges.
        for _ in 0..12 {
            for _ in 0..8 {
                ltp.record_backedge();
            }
            ltp.record_exit();
        }
        assert_eq!(ltp.backedge_count, 120);
        assert_eq!(ltp.suggests_unroll_factor(8), Some(2));

        let mut seeded = MethodProfile::default();
        seeded.seed_replayed_loop(9, 1_000, 10, 1_000);
        for _ in 0..4 {
            seeded.record_backedge(9);
        }
        seeded.record_loop_exit(9);
        let lp = &seeded.loops[&9];
        assert_eq!(
            (lp.entry_count, lp.total_trips),
            (11, 1_004),
            "the live exit is charged its own 4 back edges, not the seed's 1 000"
        );
    }

    /// `while (i < n) { if (x) { i++; continue; } i += 2; }` as javac lays it
    /// out: the `continue` is a second backward `goto` to the header (pc 12),
    /// in front of the `if`'s join (pc 15).
    const WHILE_CONTINUE: [u8; 22] = [
        0x1b, 0x1c, 0xa2, 0x00, 0x13, // 0: iload_1 iload_2 if_icmpge 21
        0x1d, 0x99, 0x00, 0x09, // 5: iload_3 ifeq 15
        0x84, 0x01, 0x01, 0xa7, 0xff, 0xf4, // 9: iinc; 12: goto 0 (continue)
        0x84, 0x01, 0x02, 0xa7, 0xff, 0xee, // 15: iinc; 18: goto 0 (closing)
        0xb1, // 21: return
    ];

    #[test]
    fn a_continue_is_keyed_by_its_loops_last_back_edge_and_its_if_is_no_exit() {
        let keyed = LoopExtents::build_keyed(&WHILE_CONTINUE);
        assert_eq!(keyed.back_edge_key(12), 18, "the continue counts toward the loop");
        assert_eq!(keyed.back_edge_key(18), 18);
        assert_eq!(keyed.exits_at(2), &[18], "the header's test leaves the loop");
        assert!(keyed.exits_at(6).is_empty(), "the if's branch to its join stays inside");
        // The pre-W2-3 rule, kept behind CRATONVM_JIT_PGO_LOOP_KEYS=0, charged
        // the `if` as an exit of a loop keyed by the continue's goto.
        let legacy = LoopExtents::build_legacy(&WHILE_CONTINUE);
        assert_eq!(legacy.back_edge_key(12), 12);
        assert_eq!(legacy.exits_at(6), &[12]);
        assert_eq!(legacy.exits_at(2), &[18]);
        // Trailing padding (`Frame::code`'s two zero bytes) changes nothing.
        let mut padded = WHILE_CONTINUE.to_vec();
        padded.extend_from_slice(&[0, 0]);
        let keyed_padded = LoopExtents::build_keyed(&padded);
        assert_eq!(keyed_padded.back_edge_key(12), 18);
        assert_eq!(keyed_padded.exits_at(2), &[18]);
    }

    #[test]
    fn a_break_is_an_exit_of_the_loop_it_leaves() {
        // for (;;) { if (x) break; i++; }
        let code: [u8; 14] = [
            0x1d, 0x99, 0x00, 0x06, // 0: iload_3; 1: ifeq 7
            0xa7, 0x00, 0x09, // 4: goto 13 (break)
            0x84, 0x01, 0x01, // 7: iinc
            0xa7, 0xff, 0xf6, // 10: goto 0
            0xb1, // 13: return
        ];
        let keyed = LoopExtents::build_keyed(&code);
        assert_eq!(keyed.exits_at(4), &[10]);
        assert!(keyed.exits_at(1).is_empty());
        assert!(LoopExtents::build_legacy(&code).exits_at(4).is_empty());
    }

    /// r11-tier-profile-replay-residuals item 2, the map-path half: an owned
    /// frame's length is accepted padded or not, and only another version's
    /// length is refused.
    #[test]
    fn a_map_path_record_from_another_versions_frame_is_dropped() {
        let mut p = MethodProfile::default();
        assert!(p.admits_frame_code_len(40), "an unstamped slot accepts anything");
        assert!(p.note_code_len(40));
        assert!(p.admits_frame_code_len(40), "the unpadded own length");
        assert!(p.admits_frame_code_len(42), "the padded own length");
        assert!(!p.admits_frame_code_len(58), "another version's padded length");
        assert!(!p.admits_frame_code_len(0));
        assert_eq!(p.recorded_code_len(), 40, "a refusal marks nothing");
    }

    /// Round 13 wave 3 (lane replay2): the receiver recorders apply the
    /// map-path frame-length rule, on both the store route and a held
    /// [`ReceiverRecorder`] (the invoke doors' memo), so an obsolete frame
    /// cannot vote for the new version's guarded-inline receiver.
    #[test]
    fn a_receiver_from_another_versions_frame_is_dropped() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let name: Arc<str> = Arc::from("run");
            let desc: Arc<str> = Arc::from("()V");
            let slot = store.get_or_insert_borrowed(9, &name, &desc);
            assert!(slot.lock().note_code_len(40));
            let rec = store.receiver_recorder_borrowed(9, &name, &desc);
            rec.record(5, 77, 42); // own version, padded
            rec.record(5, 77, 58); // another version's frame: dropped
            store.record_receiver_borrowed(9, &name, &desc, 5, 77, 40); // own, unpadded
            store.record_receiver_borrowed(9, &name, &desc, 5, 77, 58); // dropped
            let seen = slot.lock().receivers[&5][&77];
            assert_eq!(seen, 2, "only the slot's own version's frames record");
            assert_eq!(slot.lock().recorded_code_len(), 40, "a refusal marks nothing");
        });
    }

    /// Round 13 wave 3 (lane replay2): a held [`ReceiverRecorder`] whose slot
    /// `invalidate_class` removed (class unloaded or redefined, which keeps
    /// the class id and, interned, the name pointers the doors' memo compares)
    /// answers `false` and records nothing; the holder's fresh lookup gets
    /// the store's new slot, which the next compile reads.
    #[test]
    fn a_recorder_of_a_reclaimed_slot_says_so() {
        with_profiling_enabled(|| {
            let store = ProfileStore::new();
            let name: Arc<str> = Arc::from("run");
            let desc: Arc<str> = Arc::from("()V");
            let rec = store.receiver_recorder_borrowed(3, &name, &desc);
            assert!(rec.record(4, 50, 16));
            store.invalidate_class(3);
            assert!(!rec.record(4, 51, 16), "the slot was reclaimed");
            let fresh = store.receiver_recorder_borrowed(3, &name, &desc);
            assert!(fresh.record(4, 52, 16));
            let key = MethodKey {
                class_id: 3,
                method_name: name.clone(),
                descriptor: desc.clone(),
            };
            let profile = store.get_profile(&key).expect("the new slot");
            assert_eq!(profile.receivers[&4].get(&52), Some(&1));
            assert!(profile.receivers[&4].get(&51).is_none());
            assert!(profile.receivers[&4].get(&50).is_none(), "the old version's slot is gone");
        });
    }

    #[test]
    fn loop_extents_are_built_once_per_body_bytes() {
        let store = ProfileStore::new();
        let name: Arc<str> = Arc::from("run");
        let desc: Arc<str> = Arc::from("()V");
        let a: Arc<[u8]> = Arc::from(&WHILE_CONTINUE[..]);
        let b: Arc<[u8]> = Arc::from(&WHILE_CONTINUE[..]);
        let first = store.loop_extents_borrowed(7, &name, &desc, &a);
        let equal_bytes = store.loop_extents_borrowed(7, &name, &desc, &b);
        assert!(Arc::ptr_eq(&first, &equal_bytes), "equal bytes reuse the table");
        let mut other = WHILE_CONTINUE.to_vec();
        other[20] = 0xf1; // the closing goto now targets pc 3: another version
        let c: Arc<[u8]> = Arc::from(other);
        let rebuilt = store.loop_extents_borrowed(7, &name, &desc, &c);
        assert!(!Arc::ptr_eq(&first, &rebuilt));
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
                store.increment_invocation((i as u128) << 64);
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

    /// Round 12 wave 1 (r11-tier-profile-replay-residuals item 5): one
    /// observation is not a dominant receiver; the floor's worth is.
    #[test]
    fn dominant_receiver_needs_a_minimum_sample() {
        if !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR")
        {
            return; // the kill switch is set in this environment
        }
        let mut one = ReceiverCounts::default();
        one.insert(7, 1);
        assert_eq!(dominant_receiver(&one, 80), None);
        let mut just_under = ReceiverCounts::default();
        just_under.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS - 1);
        assert_eq!(dominant_receiver(&just_under, 80), None);
        let mut enough = ReceiverCounts::default();
        enough.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS);
        assert_eq!(dominant_receiver(&enough, 80), Some(7));
        // The floor is on the site's total, not the top class's count.
        let mut spread = ReceiverCounts::default();
        spread.insert(7, DOMINANT_RECEIVER_MIN_OBSERVATIONS - 2);
        spread.insert(8, 2);
        assert_eq!(dominant_receiver(&spread, 80), Some(7));
    }

    #[test]
    fn dominant_receiver_of_an_all_zero_profile_is_none() {
        let mut counts = ReceiverCounts::default();
        counts.insert(1, 0);
        assert_eq!(dominant_receiver(&counts, 1), None);
    }

    /// Tier proposal R12-5: the one evidence type answers exactly what each of
    /// the three policies it replaces answers, on a grid of shapes.
    #[test]
    fn receiver_evidence_agrees_with_the_three_policies_it_replaces() {
        let shapes: [&[(u32, u32)]; 9] = [
            &[(7, 10)],
            &[(7, 31), (9, 1)],
            &[(7, 29), (9, 3)],
            &[(7, 950), (9, 50)],
            &[(9, 350), (7, 600), (3, 50)],
            &[(4, 100), (2, 100), (3, 100)],
            &[(1, 200), (2, 200)],
            &[(5, u32::MAX), (6, 1)],
            &[(8, 0)],
        ];
        for shape in shapes {
            let counts: ReceiverCounts = shape.iter().copied().collect();
            let ev = ReceiverEvidence::of(&counts);
            assert_eq!(ev.seed_a_guarded_cache(80), dominant_receiver(&counts, 80));
            let mut p = MethodProfile::default();
            p.receivers.insert(11, counts.clone());
            // Round 12 wave 4 routed both trap-guard predicates and
            // `classify_receiver_shape` through this type, so the formulas
            // they used before are kept here: without them the comparisons
            // below would compare the type with itself.
            // Round 12 wave 5 (W4-4): a saturated site's `Supports` is now
            // `Silent`; every other answer is the legacy one.
            let fidelity_rule = !ev.summary().is_exact()
                && cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY",
                );
            let legacy_guard = |class_id: u32| {
                let total: u32 = counts.values().copied().fold(0u32, u32::saturating_add);
                if total < crate::MIN_GUARDED_RECEIVER_EVIDENCE {
                    return GuardEvidence::Silent;
                }
                let hits = u64::from(counts.get(&class_id).copied().unwrap_or(0));
                let pct = u64::from(crate::MIN_GUARDED_RECEIVER_PCT);
                if hits * 100 >= u64::from(total) * pct {
                    if fidelity_rule {
                        return GuardEvidence::Silent;
                    }
                    GuardEvidence::Supports
                } else {
                    GuardEvidence::Rejects
                }
            };
            for &(class_id, _) in shape {
                let verdict = ev.guard_evidence(
                    class_id,
                    crate::MIN_GUARDED_RECEIVER_EVIDENCE,
                    crate::MIN_GUARDED_RECEIVER_PCT,
                );
                assert_eq!(verdict, legacy_guard(class_id), "{shape:?} guard {class_id}");
                assert_eq!(
                    verdict == GuardEvidence::Supports,
                    crate::receiver_profile_supports_guard(Some(&p), 11, class_id),
                    "{shape:?} supports {class_id}"
                );
                assert_eq!(
                    verdict == GuardEvidence::Rejects,
                    crate::receiver_profile_rejects_guard(Some(&p), 11, class_id),
                    "{shape:?} rejects {class_id}"
                );
            }
            let s = ev.summary();
            // The sort `classify_receiver_shape` used before wave 4.
            let mut ranked: Vec<(u32, u32)> = shape.to_vec();
            ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            assert_eq!(s.top, ranked.first().copied(), "{shape:?} top");
            assert_eq!(s.second, ranked.get(1).copied(), "{shape:?} second");
            match crate::classify_receiver_shape(Some(&counts)) {
                crate::ReceiverShape::Monomorphic { class_id, observations } => {
                    assert_eq!(s.top.map(|t| t.0), Some(class_id));
                    assert_eq!(s.observations, observations);
                }
                crate::ReceiverShape::Bimorphic { class_ids, observations } => {
                    assert_eq!([s.top.map(|t| t.0), s.second.map(|t| t.0)], class_ids.map(Some));
                    assert_eq!(s.observations, observations);
                }
                crate::ReceiverShape::Saturated { types, .. } => {
                    assert!(!s.is_exact());
                    assert_eq!(s.types, types);
                }
                crate::ReceiverShape::Megamorphic { types, observations }
                | crate::ReceiverShape::Cold { types, observations } => {
                    assert_eq!((s.types, s.observations), (types, observations));
                }
                crate::ReceiverShape::Unprofiled => assert_eq!(s.observations, 0),
            }
        }
    }

    /// Tier proposal W4-4 (round 12 wave 5): a pinned counter no longer
    /// supports a trap-on-miss guard, while a saturated site's rejection
    /// stands (the true total is at least the clamped one).
    #[test]
    fn a_saturated_profile_never_supports_a_trap_on_miss_guard() {
        if !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_GUARD_EVIDENCE_FIDELITY",
        ) {
            return;
        }
        let min = crate::MIN_GUARDED_RECEIVER_EVIDENCE;
        let pct = crate::MIN_GUARDED_RECEIVER_PCT;
        let counts = |pairs: &[(u32, u32)]| -> ReceiverCounts { pairs.iter().copied().collect() };
        // String pinned beside a billion other receivers: the true share is
        // about 81 %, the clamped reading 100 %.
        let pinned = counts(&[(5, u32::MAX), (6, 1_000_000_000)]);
        let ev = ReceiverEvidence::of(&pinned);
        assert!(!ev.summary().is_exact());
        assert_eq!(ev.guard_evidence(5, min, pct), GuardEvidence::Silent);
        assert_eq!(ev.guard_evidence(6, min, pct), GuardEvidence::Rejects);
        // Saturated through the total alone: 3e9 of a clamped 4.29e9 is
        // under 90 %, and so of the true 6e9.
        let total_only = counts(&[(1, 3_000_000_000), (2, 3_000_000_000)]);
        let ev = ReceiverEvidence::of(&total_only);
        assert_eq!(ev.guard_evidence(1, min, pct), GuardEvidence::Rejects);
        // An exact profile is unchanged.
        let exact = counts(&[(5, 950), (6, 50)]);
        let ev = ReceiverEvidence::of(&exact);
        assert_eq!(ev.guard_evidence(5, min, pct), GuardEvidence::Supports);
        assert_eq!(ev.guard_evidence(6, min, pct), GuardEvidence::Rejects);
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
    /// the property the deleted `pgo::ReceiverTypeProfile` (cap 8) did *not*
    /// have, and the reason the two were never interchangeable evidence.
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

/// A borrowed handle to ONE method's profile slot, so a caller that records
/// against the same method many times in a row pays the lookup once.
///
/// [`ProfileStore::record_receiver_borrowed`] costs a hash pass over
/// `(class_id, method_name, descriptor)` plus two lock acquisitions plus an
/// `Arc` clone. That is right once per method and far too much once per CALL —
/// and the monomorphic invoke fast door records once per call, where it
/// measured **+44% CPU** on interpreted dispatch
/// (`testrandommapops-deterministic-1810-null-FIXED-20260904.md`).
///
/// The profile key is the CALLER's method, which cannot change while its frame
/// is live, so one handle serves the whole frame.
///
/// **Why caching the slot is sound:** the store removes a slot only in
/// [`ProfileStore::invalidate_class`] (class unload and redefinition), and it
/// marks the slot as it goes; [`Self::record`] then answers `false` and the
/// holder looks the method up again. Otherwise a slot handed out once stays
/// the slot the store keeps handing out, and a held handle keeps writing
/// where the lookup would. (This used to say the store had no remove at all,
/// while `invalidate_class` removed slots, so a stale handle silently DROPPED
/// records, which are exactly the biased profile that made the door return
/// wrong answers. Round 13 wave 3, lane replay2.)
#[derive(Clone)]
pub struct ReceiverRecorder(std::sync::Arc<parking_lot::Mutex<MethodProfile>>);

impl ReceiverRecorder {
    /// Record one receiver observation — the same effect as the last lines of
    /// [`ProfileStore::record_receiver_borrowed`] (the frame-length check
    /// included), without the lookup.
    ///
    /// `false`, recording nothing, once the store has removed this handle's
    /// slot ([`ProfileStore::invalidate_class`]: the class was unloaded or
    /// redefined): the holder must look the method up again. Before round 13
    /// wave 3 (lane replay2) a held handle kept writing into the removed slot,
    /// so a hot caller of a redefined class lost its new receiver profile for
    /// as long as its memo held.
    #[inline]
    pub fn record(&self, pc: usize, receiver_class_id: u32, frame_code_len: usize) -> bool {
        let mut p = self.0.lock();
        if p.reclaimed {
            return false;
        }
        if p.admits_frame_code_len(frame_code_len) {
            p.record_receiver(pc, receiver_class_id);
        }
        true
    }
}

impl ProfileStore {
    /// Resolve a method's profile slot ONCE and hand back a reusable handle.
    /// For callers that record repeatedly against one method; see
    /// [`ReceiverRecorder`].
    pub fn receiver_recorder_borrowed(
        &self,
        class_id: u32,
        method_name: &std::sync::Arc<str>,
        descriptor: &std::sync::Arc<str>,
    ) -> ReceiverRecorder {
        ReceiverRecorder(self.get_or_insert_borrowed(class_id, method_name, descriptor))
    }
}
