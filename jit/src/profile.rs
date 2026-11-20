// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
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
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
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
            && true
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

    /// Round 16 (lane mega): the polymorphic generalisation of
    /// [`Self::bimorphic_pair`]: the top `max_classes` (at most) classes, best
    /// first (descending count, ties by ascending class id, the summary's
    /// order), when the site has at least `min_samples` observations, its top
    /// class holds under `max_top_pct` percent (the one-class guarded splice
    /// owns the rest), every named class holds at least `min_each_pct`
    /// percent, and together they hold at least `min_total_pct` percent. At
    /// least THREE classes are named: a pair is [`Self::bimorphic_pair`]'s.
    /// Class id 0 (unknown) ends the list. No fidelity check, as for the pair:
    /// a hint whose miss edge is the call.
    pub fn polymorphic_set(
        &self,
        min_samples: u32,
        max_classes: usize,
        max_top_pct: u32,
        min_each_pct: u32,
        min_total_pct: u32,
    ) -> Option<Vec<u32>> {
        let s = &self.summary;
        if s.observations < min_samples || s.observations == 0 || max_classes < 3 {
            return None;
        }
        if s.top_holds_at_least_pct(max_top_pct) {
            return None;
        }
        let mut ranked: Vec<(u32, u32)> = self.counts.iter().map(|(&c, &n)| (c, n)).collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let total = u64::from(s.observations);
        let mut chosen: Vec<u32> = Vec::new();
        let mut held: u64 = 0;
        for (class_id, count) in ranked {
            if chosen.len() >= max_classes
                || class_id == 0
                || !s.holds_at_least_pct(count, min_each_pct)
            {
                break;
            }
            chosen.push(class_id);
            held += u64::from(count);
            // In `u64`: counts near `u32::MAX` must not wrap.
            if chosen.len() >= 3 && held * 100 >= total * u64::from(min_total_pct) {
                return Some(chosen);
            }
        }
        None
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
        let trips = self
            .backedge_count
            .saturating_sub(self.backedges_at_last_exit);
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
            false,
        )    }}

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
        1usize    } else if avg <= 32.0 {
        2    } else {
        4    };
    Some(factor.min(max_factor))}

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
        {
            Self::build_keyed(code)
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
        match u32::try_from(branch_pc)
            .ok()
            .and_then(|p| self.exits.get(&p))
        {
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
/// `call_sites` has two sources (corrected round nio20 wave 2, lane L3; the
/// paragraph that stood here named four recorders in `invoke.rs` that no
/// longer exist):
///
/// * the SLOW paths' `ProfileStore::record_call_site_borrowed`
///   (`dispatch_static.rs`, `dispatch_virtual.rs`, `invoke.rs`): reached on
///   an invoke-cache miss / resolution only, so a hot site reads 1-2 there;
/// * the per-execution fast-path cells, [`BranchCounters::record_call`],
///   which the interpreter's four invoke arms (`execute_frame_from_index`,
///   `record_call_for_frame`) bump under the branch profile's own gate (the
///   process-wide [`is_profiling_enabled`] or the method's arm), and which
///   [`Self::snapshot`] folds in. Every invoke kind, so a virtual site whose
///   method was armed answers [`CallSiteEvidence::Direct`] with a frequency.
///
/// Use [`Self::call_site_count`], which unifies these with the receiver
/// observations and states which one answered.
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
    /// nio21 wave 16, lane L4: an inline-cache profile seed already asked for
    /// this method's one recompile ([`ProfileStore::claim_ic_seed_recompile`]).
    /// Lives with the slot, so a redefinition's `invalidate_class` (which
    /// removes the slot) re-opens it for the new version.
    ic_seed_recompile_claimed: bool,
    ic_seed_budget_held: bool,
    ic_seed_site_claims: u8,
    /// JIT r20 wave 11, lane pProf (W15L2-P3): the lock-free receiver /
    /// call-site rows a held [`ReceiverRecorder`] writes into, allocated by
    /// [`ProfileStore::receiver_recorder_borrowed`] when
    /// `CRATONVM_JIT_PROF_RECEIVER_ROWS` is on. Folded into
    /// [`Self::receivers`] / [`Self::call_sites`] by [`Self::snapshot`], like
    /// [`Self::flat_branches`]. `None` on a snapshot and on every slot no
    /// recorder handle was made for.
    receiver_rows: Option<Arc<ReceiverRows>>,
    /// j27 wave 5 (lane TRAPPROF, `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT`, opt-in):
    /// the pcs a branch guard's trap fired at ([`ProfileStore::note_trap`]),
    /// HotSpot's per-bci MDO trap bit. Both prune feeds keep a branch whose pc
    /// is here ([`Self::branch_trapped`]; HotSpot's `too_many_traps(bci,
    /// Reason_unstable_if)`), whatever its counts say. Set before the eager
    /// re-queue, so the recompile sees it even when it reads the profile
    /// before the resumed interpreter records the edge. Carried by a
    /// snapshot; removed with the slot on redefinition. Empty with the switch
    /// off.
    branch_trap_pcs: rustc_hash::FxHashSet<usize>,
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
    /// Round nio20 wave 2, lane L3 (page
    /// `r20-per-site-invoke-counts-are-recorded-on-slow-paths-only-FIXED-20261007.md`):
    /// executions of the invoke instruction at each pc, recorded by the
    /// interpreter's invoke arms on their FAST paths (every kind), under the
    /// same gate as the branch cells ([`Self::record_call`]). Folded into
    /// [`MethodProfile::call_sites`] by [`MethodProfile::snapshot`]. Disjoint
    /// from the two branch banks by construction: an invoke pc is never a
    /// conditional branch's.
    calls: Box<[AtomicU32]>,
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

/// Opt-in native branch observations from single-pass bodies, including C1.
/// Per-branch atomic updates dominate arithmetic loops; enable only when their
/// profile benefit outweighs that cost. Artifact-owned banks remain per VM.
pub fn single_pass_branch_recording_enabled() -> bool {
    true
}

impl BranchCounters {
    /// Counters for a method of `code_len` bytes.
    pub fn new(code_len: usize) -> Self {
        let mk = || (0..code_len).map(|_| AtomicU32::new(0)).collect();
        Self {
            taken: mk(),
            not_taken: mk(),
            calls: mk(),
            armed: AtomicBool::new(false),
        }
    }

    /// Record one execution of the invoke instruction at `pc` (round nio20
    /// wave 2, lane L3). Saturating and out-of-range-tolerant exactly like
    /// [`Self::record`].
    #[inline]
    pub fn record_call(&self, pc: usize) {
        let Some(cell) = self.calls.get(pc) else {
            return;
        };
        let _ = cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1));
    }

    /// Executions recorded at invoke `pc`; `0` for none.
    #[inline]
    pub fn call_count(&self, pc: usize) -> u32 {
        self.calls.get(pc).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Every invoke pc with at least one recorded execution, in pc order.
    pub fn observed_calls(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        (0..self.calls.len()).filter_map(move |pc| {
            let n = self.call_count(pc);
            (n != 0).then_some((pc, n))
        })
    }

    /// Ask for this method's branches to be recorded even while the
    /// process-wide gate is off. Sticky: nothing disarms it.
    ///
    /// The per-method half of the fix for
    /// `docs/feature-designs/jit-osr-entry-metadata.md`.
    /// The global C2 window opens when a method is nominated for the
    /// optimizing tier, which is after it stopped being interpreted, so it
    /// records every OTHER interpreted method and not the nominated one. Arming
    /// the method's own counters at its first nomination (the interpreter's
    /// `Interpreter -> C1` recommendation, when it is still interpreted and
    /// hot) collects exactly the stretch the C2 compile will read. No disarm is
    /// needed for interpreter cost: a method that leaves the interpreter stops
    /// paying there, and a deopt records evidence the recompile wants. The
    /// opt-in single-pass profiler records into the same bank independently.
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
        let _ = cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1));
    }

    /// Stable atomic cell addresses for native observations. Generated code uses
    /// atomic RMWs only; its artifact must retain the owning Arc.
    pub(crate) fn native_branch_cells(&self, pc: usize) -> Option<(usize, usize)> {
        Some((
            self.taken.get(pc)?.as_ptr() as usize,
            self.not_taken.get(pc)?.as_ptr() as usize,
        ))
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
mod branch_counter_tests;

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
        let mut call_sites = self.call_sites.clone();
        if let Some(flat) = &self.flat_branches {
            for (pc, taken, not_taken) in flat.observed() {
                let entry = branches.entry(pc).or_default();
                entry.taken = entry.taken.saturating_add(taken);
                entry.not_taken = entry.not_taken.saturating_add(not_taken);
            }
            // Round nio20 wave 2, lane L3: the fast-path invoke counters, added
            // like the branches (the slow-path `record_call_site*` rows are a
            // handful per site and stay counted beside them).
            for (pc, n) in flat.observed_calls() {
                let e = call_sites.entry(pc).or_insert(0);
                *e = e.saturating_add(n);
            }
        }
        let mut receivers = self.receivers.clone();
        // JIT r20 wave 11, lane pProf (W15L2-P3): the held recorders' lock-free
        // rows, added like the flat counters (each observation was recorded
        // into exactly one of the rows or the maps).
        if let Some(rows) = &self.receiver_rows {
            rows.fold_into(&mut receivers, &mut call_sites);
        }
        MethodProfile {
            branches,
            receivers,
            loops: self.loops.clone(),
            call_sites,
            flat_branches: None,
            // Carried, not dropped: `crate::profile_store`'s outcome census is
            // a walk over `snapshot_all()`, and a snapshot that lost the
            // provenance would report every replayed method as never seeded.
            replay: self.replay.clone(),
            code_len: self.code_len,
            code_len_conflict: self.code_len_conflict,
            loop_extents: None,
            reclaimed: self.reclaimed,
            ic_seed_recompile_claimed: self.ic_seed_recompile_claimed,
            ic_seed_budget_held: self.ic_seed_budget_held,
            ic_seed_site_claims: self.ic_seed_site_claims,
            receiver_rows: None,
            branch_trap_pcs: self.branch_trap_pcs.clone(),
        }
    }

    /// j27 wave 5 (lane TRAPPROF): whether a branch guard trapped at `pc`
    /// ([`Self::branch_trap_pcs`]). The prune feeds' trap-bit test.
    pub fn branch_trapped(&self, pc: usize) -> bool {
        self.branch_trap_pcs.contains(&pc)
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
            // W15L2-P3: the lock-free rows apply the same frame-length rule
            // and read the stamp without the lock.
            if let Some(rows) = &self.receiver_rows {
                rows.code_len.store(len, Ordering::Relaxed);
            }
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
        if self.code_len == 0 || frame_code_len == own || frame_code_len.checked_sub(2) == Some(own)
        {
            return true;        }
        !true    }

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
                    for (pc, n) in old.observed_calls() {
                        let e = self.call_sites.entry(pc).or_insert(0);
                        *e = e.saturating_add(n);
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
        self.record_receiver_n(pc, class_id, 1);
    }

    /// [`Self::record_receiver`] for `n` calls at once: a sampled record from
    /// the invoke doors' coalescing stands for every call it skipped.
    #[inline]
    pub fn record_receiver_n(&mut self, pc: usize, class_id: u32, n: u32) {
        let entry = self.receivers.entry(pc).or_default();
        let count = entry.entry(class_id).or_insert(0);
        *count = count.saturating_add(n);
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
        self.record_call_site_n(pc, 1);
    }

    /// [`Self::record_call_site`] for `n` executions at once.
    #[inline]
    pub fn record_call_site_n(&mut self, pc: usize, n: u32) {
        let e = self.call_sites.entry(pc).or_insert(0);
        *e = e.saturating_add(n);
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
        // W15L2-P3: a replayed slot reconciles every live receiver against
        // its seeds under the lock, so its held recorders stop using the
        // lock-free rows (what they hold already stays folded in).
        if let Some(rows) = &self.receiver_rows {
            rows.locked_only.store(true, Ordering::Relaxed);
        }
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
    match cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
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
    match cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1)) {
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
///
/// nio21 wave 15 L2: Fx, not std's `DefaultHasher` (SipHash-1-3). This is
/// on the interpreter's per-call profile path (`get_or_insert`, and through
/// [`shard_and_fingerprint_for_borrowed`] `get_or_insert_borrowed`), where the
/// `perf` profile of `Nio20JitRows rlock` / `Nio21UnsafeVolatile lock` put
/// `Sip13Rounds` at ~3% of the whole run. Nothing persists or compares these
/// values across processes: they pick a shard and key the in-memory
/// `name_index`, whose every hit is verified against the full key, so a
/// weaker hash can only cost a collision fall-through, never a wrong slot.
/// MUST hash exactly what [`shard_and_fingerprint_for_borrowed`] hashes, in
/// the same order (`MethodKey`'s derived `Hash` is `class_id`, then the two
/// strings), so both paths land on one shard.
#[inline]
fn shard_for_key(key: &MethodKey) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    key.hash(&mut h);
    shard_of_fingerprint(h.finish())
}

/// The shard a 64-bit profile fingerprint belongs to. Fx's low bits are its
/// weakest (the last step is a multiply, whose low bits see only the low bits
/// of its input), so the fingerprint is multiplied by an odd constant and the
/// TOP bits taken: they depend on every input bit, whatever the `rustc-hash`
/// version's `finish` does.
#[inline]
fn shard_of_fingerprint(fingerprint: u64) -> usize {
    // Cast: the shift leaves at most `log2(PROFILE_SHARDS)` bits.
    ((fingerprint.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - PROFILE_SHARDS.trailing_zeros()))
        as usize)
        & (PROFILE_SHARDS - 1)
}

/// Compute the shard index and 64-bit fingerprint for a borrowed
/// `(class_id, method_name, descriptor)` triple in a single hash pass.
/// The fingerprint is used as the `name_index` key, and it also picks the
/// shard ([`shard_of_fingerprint`]) — so a fingerprint hit on shard `s` is
/// guaranteed to belong to the same shard as a `MethodKey`-hash lookup for the
/// same triple in the slow path (`shard_for_key`, same hasher, same order;
/// pinned by `fx_shard_paths_agree`).
#[inline]
fn shard_and_fingerprint_for_borrowed(
    class_id: u32,
    method_name: &str,
    descriptor: &str,
) -> (usize, u64) {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    class_id.hash(&mut h);
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    let fingerprint = h.finish();
    (shard_of_fingerprint(fingerprint), fingerprint)
}

#[cfg(test)]
mod w15_l2_fx_shard_tests;

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

    /// nio21 wave 16, lane L4: recompiles the inline-cache profile seed has
    /// claimed in this VM ([`Self::claim_ic_seed_recompile`]), against
    /// [`IC_SEED_RECOMPILE_BUDGET`].
    ic_seed_recompiles: std::sync::atomic::AtomicU32,

    /// JIT r20 wave 11, lane pProf (W15L2-P3):
    /// `CRATONVM_JIT_PROF_RECEIVER_ROWS`, read once per VM. When on, a
    /// [`ReceiverRecorder`] records into the slot's lock-free
    /// [`ReceiverRows`] instead of taking the slot's mutex per call.
    receiver_rows_on: bool,
    /// `CRATONVM_DBG_PROF_ROWS`: one `[prof-rows]` line per slot whose rows
    /// [`Self::receiver_recorder_borrowed`] made (W15L2-P3's positive control).
    receiver_rows_dbg: bool,
    /// JIT round 21 wave 4, lane ic (`r21-receiver-profiling-gate-is-per-process`):
    /// whether THIS VM records receiver rows and call-site counts. The process
    /// gate ([`is_receiver_profiling_enabled`]) then only says "some VM in the
    /// process records" ([`ProfileStore::receivers_recording`] asks both; the
    /// master gate still records on its own, wave 7).
    /// `true` for a fresh store (a store no VM configured records as before);
    /// the VM's init sets its own answer ([`ProfileStore::set_receivers_on`]).
    receivers_on: std::sync::atomic::AtomicBool,
    store_id: u64,
    generation: std::sync::atomic::AtomicU64,
}

static NEXT_PROFILE_STORE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// `CRATONVM_JIT_PROFILE_RECEIVERS_PER_VM` (default ON; `0` restores the
/// process-wide store of each VM's answer, under which a VM with no profile
/// consumer switched receiver recording off for every other VM). JIT round 21
/// wave 4, lane ic. Read once per VM, at its init.
pub fn receivers_per_vm_enabled() -> bool {
    true}

/// The answer [`ProfileStore::receivers_recording`] gives from its three
/// inputs: `master` (`PROFILING_ENABLED`: `CRATONVM_TIER_PGO`,
/// `CRATONVM_TIER_PGO_ALWAYS`, or the C2 branch window), `process_gate`
/// (`RECEIVER_PROFILING_ENABLED`: some VM in the process wants receivers) and
/// `store_on` (this VM's own answer, `ProfileStore::set_receivers_on`).
///
/// The master gate records everything, receivers included, regardless of
/// `CRATONVM_TIER_PGO_RECEIVERS` -- the answer before JIT round 21 wave 4
/// (`master || process_gate`), on which the optimizing tier's type profile
/// leans while the C2 window is open. Only the receiver gate is per VM: a VM
/// that does not want receivers no longer records because ANOTHER VM switched
/// the process gate on (`r21-receiver-profiling-gate-is-per-process-20261018`).
/// JIT round 21 wave 7, lane ic (r21-review-w45-vm-findings item 3).
#[inline(always)]
pub fn receivers_recording_answer(master: bool, process_gate: bool, store_on: bool) -> bool {
    master || (process_gate && store_on)
}

/// The most recompiles the inline-cache profile seed may ask for in one VM
/// (nio21 wave 16, lane L4). One per method already; this bounds a workload
/// that compiles thousands of starved loop bodies (a framework's startup) to a
/// fixed number of extra optimizing compiles.
pub const IC_SEED_RECOMPILE_BUDGET: u32 = 512;

impl ProfileStore {
    pub fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| ProfileShard::new()),
            invocation_counts: std::array::from_fn(|_| InvocationShard::new()),
            name_index_collisions: std::sync::atomic::AtomicU64::new(0),
            name_index_benign_races: std::sync::atomic::AtomicU64::new(0),
            replay_pending: crate::profile_store::ReplayPending::default(),
            ic_seed_recompiles: std::sync::atomic::AtomicU32::new(0),
            receiver_rows_on: true,
            receiver_rows_dbg: false,
            receivers_on: std::sync::atomic::AtomicBool::new(true),
            store_id: NEXT_PROFILE_STORE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Set whether this VM records receiver rows and call-site counts (JIT
    /// round 21 wave 4, lane ic; see the `receivers_on` field).
    pub fn set_receivers_on(&self, on: bool) {
        self.receivers_on
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this VM records receiver rows and call-site counts: the master
    /// gate (`CRATONVM_TIER_PGO` / the C2 branch window), OR the process
    /// receiver gate AND this store's own answer
    /// ([`receivers_recording_answer`]). What every receiver / call-site
    /// recorder of this store, and the interpreter's invoke doors before they
    /// pay for a record, ask.
    #[inline]
    pub fn receivers_recording(&self) -> bool {
        receivers_recording_answer(
            PROFILING_ENABLED.load(Ordering::Relaxed),
            RECEIVER_PROFILING_ENABLED.load(Ordering::Relaxed),
            self.receivers_on.load(Ordering::Relaxed),
        )
    }

    /// nio21 wave 16, lane L4 (`crate::IcProfileSeedSite`): record that the
    /// inline cache at `pc` of `key` served a receiver of exactly `class_id`,
    /// as `count` receiver observations -- unless the row already holds that
    /// many for the class (idempotent per class). `count` is
    /// [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] at the VM's call site: the
    /// cache saw the class, and every class it saw counts alike, so one class
    /// reads monomorphic, two bimorphic, and so on. `false` when profiling is
    /// off or nothing changed.
    pub fn seed_ic_observed_receiver(
        &self,
        key: &MethodKey,
        pc: usize,
        class_id: u32,
        count: u32,
    ) -> bool {
        if !self.receivers_recording() || count == 0 {
            return false;
        }
        let slot = self.get_or_insert(key);
        let mut p = slot.lock();
        if p.reclaimed {
            return false;
        }
        const MAX_IC_SEED_SITE_CLAIMS_PER_METHOD: u8 = 4;
        let rows_site_total = p
            .receiver_rows
            .as_ref()
            .map_or(0, |rows| rows.site_receiver_total(pc));
        let map_site_total = p.receivers.get(&pc).map_or(0u32, |m| {
            m.values().fold(0u32, |acc, &v| acc.saturating_add(v))
        });
        let site_total_before = rows_site_total.saturating_add(map_site_total);
        // W15L2-P3: what the lock-free rows hold for the class counts as
        // already recorded, so the seed tops the row's TOTAL up to `count`.
        let in_rows = p
            .receiver_rows
            .as_ref()
            .map_or(0, |rows| rows.receiver_count(pc, class_id));
        let cell = p
            .receivers
            .entry(pc)
            .or_default()
            .entry(class_id)
            .or_insert(0);
        let total = cell.saturating_add(in_rows);
        if total >= count {
            return false;
        }
        *cell = cell.saturating_add(count - total);
        if site_total_before < count && p.ic_seed_site_claims < MAX_IC_SEED_SITE_CLAIMS_PER_METHOD {
            p.ic_seed_recompile_claimed = false;
        }
        true
    }

    /// nio21 wave 16, lane L4: claim the recompile an inline-cache profile
    /// seed may ask for `key`, within the VM's [`IC_SEED_RECOMPILE_BUDGET`].
    /// Charges at most one VM budget unit per method profile slot while
    /// allowing distinct newly-seeded starved call sites in the same method
    /// (up to `MAX_IC_SEED_SITE_CLAIMS_PER_METHOD`) to request a follow-up C2
    /// retry if an earlier site's background compile snapshotted before the
    /// later site executed.
    pub fn claim_ic_seed_recompile(&self, key: &MethodKey) -> bool {
        use std::sync::atomic::Ordering;
        if !self.receivers_recording() {
            return false;
        }
        let slot = self.get_or_insert(key);
        let mut p = slot.lock();
        if p.reclaimed || p.ic_seed_recompile_claimed {
            return false;
        }
        if !p.ic_seed_budget_held {
            let spent = self
                .ic_seed_recompiles
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    (n < IC_SEED_RECOMPILE_BUDGET).then_some(n + 1)
                })
                .is_err();
            if spent {
                return false;
            }
            p.ic_seed_budget_held = true;
        }
        p.ic_seed_recompile_claimed = true;
        p.ic_seed_site_claims = p.ic_seed_site_claims.saturating_add(1);
        true
    }

    /// j27 HANGHUNT (`CRATONVM_JIT_IR_IC_SEED_CLAIM_ON_ENQUEUE`, default ON): hand
    /// back a [`Self::claim_ic_seed_recompile`] whose C2 retry was refused, so
    /// the claim is spent only by a recompile that will run. Returns the VM
    /// budget unit too when no earlier site claim remains on the method.
    pub fn release_ic_seed_recompile(&self, key: &MethodKey) {
        use std::sync::atomic::Ordering;
        let slot = self.get_or_insert(key);
        let mut p = slot.lock();
        if !std::mem::take(&mut p.ic_seed_recompile_claimed) {
            return;
        }
        p.ic_seed_site_claims = p.ic_seed_site_claims.saturating_sub(1);
        if p.ic_seed_site_claims == 0 && std::mem::take(&mut p.ic_seed_budget_held) {
            let _ = self
                .ic_seed_recompiles
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
    }

    /// Reclaim every profile and warmup counter owned by an unloaded class.
    pub fn invalidate_class(&self, class_id: u32) {
        self.invalidate_where(|cid| cid == class_id);
    }

    /// Batch form of [`Self::invalidate_class`] for a whole unloaded set: ONE
    /// sweep of every shard instead of one per class (gc-common w36-c,
    /// `docs/feature-designs/jit-ir-optimizations.md`).
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
        if !removed.is_empty() {
            self.generation.fetch_add(1, Ordering::Release);
        }
        for slot in removed {
            let mut p = slot.lock();
            p.reclaimed = true;
            // W15L2-P3: the held recorders' lock-free path reads this one.
            if let Some(rows) = &p.receiver_rows {
                rows.reclaimed.store(true, Ordering::Release);
            }
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

    /// The invocation count recorded for `packed_key` so far; 0 for a method that
    /// has not been called. Read-only: a shared read lock and one relaxed load;
    /// it never inserts.
    ///
    /// A door that counts on the method's own `interp_invocations` folds it in
    /// here every few calls (`invoke_fast.rs` `INVOCATION_SYNC_EVERY`), so this
    /// lags the true count by that interval: a non-zero answer means "has run",
    /// a zero answer means "has not run through a counting door" (a first call
    /// through the general dispatch path is counted here at once).
    ///
    /// Callers: the eager callee chain's warmth gate
    /// (`CRATONVM_JIT_EAGER_CHAIN_MIN_INVOCATIONS`, `DirectBindRefusal::EagerChainCold`),
    /// and JIT round 18 wave 9 (lane eagercold): the optimizing door's eager
    /// static-callee compile, so it does not compile a callee that has never run.
    pub fn invocation_count(&self, packed_key: u128) -> u32 {
        let shard = &self.invocation_counts[invocation_shard_for(packed_key)];
        let read = shard.counts.read();
        read.get(&packed_key)
            .map_or(0, |cell| cell.load(Ordering::Relaxed))
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
        const TLS_PROFILE_SLOTS: usize = 2048;
        struct TlsProfileSlot {
            store_id: u64,
            generation: u64,
            class_id: u32,
            name: Arc<str>,
            desc: Arc<str>,
            profile: Arc<parking_lot::Mutex<MethodProfile>>,
        }
        const EMPTY_PROF: Option<TlsProfileSlot> = None;
        thread_local! {
            static TLS_PROFILE_CACHE: std::cell::UnsafeCell<[Option<TlsProfileSlot>; TLS_PROFILE_SLOTS]> =
                const { std::cell::UnsafeCell::new([EMPTY_PROF; TLS_PROFILE_SLOTS]) };
        }
        let gen = self.generation.load(Ordering::Acquire);
        let idx = ((class_id as usize)
            ^ (Arc::as_ptr(method_name) as *const u8 as usize >> 4)
            ^ (Arc::as_ptr(descriptor) as *const u8 as usize >> 5))
            & (TLS_PROFILE_SLOTS - 1);
        if let Some(hit) = TLS_PROFILE_CACHE.with(|cell| {
            // SAFETY: the masked index is in range and this thread-local access is non-reentrant.
            let slot = unsafe { (&*cell.get()).get_unchecked(idx) };
            if let Some(ref slot) = slot {
                if slot.store_id == self.store_id
                    && slot.generation == gen
                    && slot.class_id == class_id
                    && Arc::ptr_eq(&slot.name, method_name)
                    && Arc::ptr_eq(&slot.desc, descriptor)
                {
                    return Some(Arc::clone(&slot.profile));
                }
            }
            None
        }) {
            return hit;
        }
        let slot = self.get_or_insert_borrowed_slow(class_id, method_name, descriptor);
        TLS_PROFILE_CACHE.with(|cell| {
            // SAFETY: the masked index is in range and this thread-local access is non-reentrant.
            unsafe {
                *(&mut *cell.get()).get_unchecked_mut(idx) = Some(TlsProfileSlot {
                    store_id: self.store_id,
                    generation: gen,
                    class_id,
                    name: Arc::clone(method_name),
                    desc: Arc::clone(descriptor),
                    profile: Arc::clone(&slot),
                });
            }
        });
        slot
    }

    #[inline(never)]
    fn get_or_insert_borrowed_slow(
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
        // is `methods` > `name_index` (see `docs/feature-designs/jit-local-exception-handlers.md`). All
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
        let mut saw_fingerprint_hit = false;
        let cached: Option<Arc<parking_lot::Mutex<MethodProfile>>> = {
            let idx = shard.name_index.read();
            match idx.get(&fingerprint) {
                Some((cached_key, slot))
                    if cached_key.class_id == class_id
                        && (Arc::ptr_eq(&cached_key.method_name, method_name)
                            || cached_key.method_name.as_ref() == method_name.as_ref())
                        && (Arc::ptr_eq(&cached_key.descriptor, descriptor)
                            || cached_key.descriptor.as_ref() == descriptor.as_ref()) =>
                {
                    Some(Arc::clone(slot))
                }
                Some(_) => {
                    saw_fingerprint_hit = true;
                    None // collision — handled below
                }
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
        if saw_fingerprint_hit {
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
                    && (Arc::ptr_eq(&cached_key.method_name, method_name)
                        || cached_key.method_name.as_ref() == method_name.as_ref())
                    && (Arc::ptr_eq(&cached_key.descriptor, descriptor)
                        || cached_key.descriptor.as_ref() == descriptor.as_ref())
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
        if !self.receivers_recording() {
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
        if !self.receivers_recording() {
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

/// j27 wave 5 (lane TRAPPROF; page
/// `j27-trap-recompile-reads-the-profile-before-the-resume-records-FIXED-20261029.md`):
/// `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT`, **default OFF** (`1` turns it on). A
/// speculation-policy change that needs a host A/B before it is flipped. Read
/// once per trap, only for a branch guard's reason.
pub fn prof_branch_trap_bit_enabled() -> bool {
    true
}

/// Whether a trap of `reason` can be a pruned branch's guard failing: the
/// never-taken prune's `UnstableIf` and the always-taken prune's
/// `UncommonTrap`. Another guard of the same reason at a branch pc only costs
/// that branch its prune, which is HotSpot's direction too (a bci's trap state
/// holding a second reason reads as `Reason_many`).
fn trap_sets_branch_bit(reason: crate::deopt::DeoptReason) -> bool {
    matches!(
        reason,
        crate::deopt::DeoptReason::UnstableIf | crate::deopt::DeoptReason::UncommonTrap
    )
}

impl ProfileStore {
    /// j27 wave 5 (lane TRAPPROF, `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT`): set
    /// the trap bit of `(method, bci)` ([`MethodProfile::branch_trapped`]).
    /// The VM calls it where the trap is charged
    /// (`SharedVm::record_deoptimization`), which is before the trap evicts
    /// the body and re-queues the recompile, so no compile admitted by that
    /// trap can read the profile without the bit. HotSpot sets the MDO trap
    /// bit in `uncommon_trap_inner` the same way, before the recompile.
    ///
    /// Nothing with the switch off, for another reason, for an unidentified
    /// class (`class_id` 0 names no slot the compile reads) or for the
    /// superseded-guard sentinel (`bci == u32::MAX`).
    pub fn note_trap(
        &self,
        class_id: u32,
        method_name: &Arc<str>,
        descriptor: &Arc<str>,
        reason: crate::deopt::DeoptReason,
        bci: u32,
    ) {
        if class_id == 0
            || bci == u32::MAX
            || !trap_sets_branch_bit(reason)
            || !prof_branch_trap_bit_enabled()
        {
            return;
        }
        let Ok(pc) = usize::try_from(bci) else {
            return;
        };
        let key = MethodKey {
            class_id,
            method_name: Arc::clone(method_name),
            descriptor: Arc::clone(descriptor),
        };
        self.get_or_insert(&key).lock().branch_trap_pcs.insert(pc);    }}

impl Default for ProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

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
pub struct ReceiverRecorder(
    std::sync::Arc<parking_lot::Mutex<MethodProfile>>,
    /// JIT r20 wave 11, lane pProf (W15L2-P3): the slot's lock-free rows, when
    /// `CRATONVM_JIT_PROF_RECEIVER_ROWS` is on. Every observation they cannot
    /// take (a full table, a replayed slot, another version's frame) falls
    /// back to the mutex path below, so the switch at `=0` is today's code.
    Option<std::sync::Arc<ReceiverRows>>,
);

/// Receiver rows per call site of [`ReceiverRows`] (HotSpot's
/// `TypeProfileWidth` is 2; the polymorphic guarded splice names up to 4).
const RECEIVER_ROW_WIDTH: usize = 4;

/// One call site of [`ReceiverRows`]: its pc, its execution count, and up to
/// [`RECEIVER_ROW_WIDTH`] `(class, count)` rows.
struct ReceiverSiteRow {
    /// `pc + 1`; `0` = free. Claimed once by CAS and never changed again.
    pc: AtomicU32,
    /// Executions recorded through [`ReceiverRecorder::record_call_site`].
    calls: AtomicU32,
    /// `1 << 32 | class_id`; `0` = free. Claimed in order, once, by CAS, and
    /// never changed again, so one class has at most one row per site.
    classes: [std::sync::atomic::AtomicU64; RECEIVER_ROW_WIDTH],
    counts: [AtomicU32; RECEIVER_ROW_WIDTH],
}

impl ReceiverSiteRow {
    fn new() -> Self {
        Self {
            pc: AtomicU32::new(0),
            calls: AtomicU32::new(0),
            classes: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
            counts: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }
}

/// JIT r20 wave 11, lane pProf (W15L2-P3, book
/// `docs/feature-designs/jit-ir-optimizations.md`): a method
/// slot's lock-free receiver and call-site counters, HotSpot's
/// `ReceiverTypeData` shape -- a small open-addressed table of call sites keyed
/// by pc, each with a fixed number of `(class, count)` rows.
///
/// [`ReceiverRecorder::record`] used to take the slot's `parking_lot::Mutex`
/// for every interpreted virtual call while receiver profiling was on; with
/// the rows it is a few relaxed loads and one saturating RMW. The counts are
/// lossless (`fetch_update`) and saturate like every counter in this module.
///
/// # Exactness
///
/// * Every observation lands in exactly ONE place: the rows, or (when the rows
///   cannot take it) the slot's maps under the mutex. [`MethodProfile::snapshot`]
///   adds the two, so every reader (the planners, the megamorphic decisions,
///   the profile save) sees each class with its full count, and the type
///   count stays exact -- a class the rows have no room for is recorded in
///   the map, never dropped and never merged into an "other" bucket.
/// * [`Self::reclaimed`] mirrors [`MethodProfile::reclaimed`]; it is set under
///   the slot lock by `invalidate_where`, so a held recorder answers `false`
///   from the next call on, exactly as the mutex path does. A record that read
///   it as clear just before is ordered before the reclaim, as a mutex record
///   that won the lock just before would have been.
/// * A replayed slot ([`Self::locked_only`]) and a frame of another version of
///   the method (the frame-length rule, [`Self::code_len`]) take the mutex
///   path, which applies the replay reconciliation and the
///   `CRATONVM_JIT_PROFILE_FRAME_LEN_CHECK` rule unchanged.
pub struct ReceiverRows {
    reclaimed: AtomicBool,
    locked_only: AtomicBool,
    /// Mirror of [`MethodProfile::code_len`] (`0` = unstamped), stored by
    /// [`MethodProfile::note_code_len`] under the slot lock.
    code_len: AtomicU32,
    sites: Box<[ReceiverSiteRow]>,
}

/// What the lock-free rows did with one observation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowRecord {
    /// Recorded.
    Done,
    /// The slot was reclaimed: the holder must look the method up again.
    Reclaimed,
    /// Not taken: record it under the slot's mutex instead.
    Locked,
}

impl ReceiverRows {
    /// Rows for a method whose stamped bytecode length is `code_len` (`0`:
    /// unknown). An invoke is at least three bytes, so `code_len / 8` sites
    /// (rounded up to a power of two, 8..=64) covers a method whose every
    /// other instruction is a call; extra sites fall back to the mutex.
    fn new(code_len: u32, locked_only: bool) -> Self {
        let wanted = if code_len == 0 {
            16
        } else {
            (code_len as usize)
                .div_ceil(8)
                .next_power_of_two()
                .clamp(8, 64) // Cast: u32 -> usize widens
        };
        Self {
            reclaimed: AtomicBool::new(false),
            locked_only: AtomicBool::new(locked_only),
            code_len: AtomicU32::new(code_len),
            sites: (0..wanted).map(|_| ReceiverSiteRow::new()).collect(),
        }
    }

    /// The site row of `pc`, claiming a free one; `None` when the table is
    /// full or `pc` does not fit the key.
    #[inline]
    fn site(&self, pc: usize) -> Option<&ReceiverSiteRow> {
        let key = u32::try_from(pc).ok()?.checked_add(1)?;
        let n = self.sites.len();
        let start = pc & (n - 1);
        for i in 0..n {
            let row = &self.sites[(start + i) & (n - 1)];
            let seen = row.pc.load(Ordering::Acquire);
            if seen == key {
                return Some(row);
            }
            if seen == 0 {
                match row
                    .pc
                    .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => return Some(row),
                    Err(now) if now == key => return Some(row),
                    Err(_) => {}
                }
            }
        }
        None
    }

    /// The site row of `pc` if one was claimed; never claims.
    fn find_site(&self, pc: usize) -> Option<&ReceiverSiteRow> {
        let key = u32::try_from(pc).ok()?.checked_add(1)?;
        let n = self.sites.len();
        let start = pc & (n - 1);
        for i in 0..n {
            let row = &self.sites[(start + i) & (n - 1)];
            match row.pc.load(Ordering::Acquire) {
                seen if seen == key => return Some(row),
                0 => return None,
                _ => {}
            }
        }
        None
    }

    /// The gates every lock-free record passes first: reclaimed, replayed,
    /// another version's frame.
    #[inline]
    fn gate(&self, frame_code_len: usize) -> Option<RowRecord> {
        if self.reclaimed.load(Ordering::Acquire) {
            return Some(RowRecord::Reclaimed);
        }
        if self.locked_only.load(Ordering::Relaxed) {
            return Some(RowRecord::Locked);
        }
        let own = self.code_len.load(Ordering::Relaxed) as usize; // Cast: u32 -> usize widens
        if own != 0 && frame_code_len != own && frame_code_len.checked_sub(2) != Some(own) {
            return Some(RowRecord::Locked);
        }
        None
    }

    #[inline]
    fn try_record_receiver(&self, pc: usize, class_id: u32, frame_code_len: usize, n: u32) -> RowRecord {
        if let Some(out) = self.gate(frame_code_len) {
            return out;
        }
        let Some(site) = self.site(pc) else {
            return RowRecord::Locked;
        };
        let key = (1u64 << 32) | u64::from(class_id);
        for (k, cell) in site.classes.iter().enumerate() {
            let mut seen = cell.load(Ordering::Acquire);
            if seen == 0 {
                seen = match cell.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => key,
                    Err(now) => now,
                };
            }
            if seen == key {
                if site.counts[k].load(Ordering::Relaxed) < 1_000_000 {
                    site.counts[k].fetch_add(n, Ordering::Relaxed);
                }
                return RowRecord::Done;
            }
        }
        RowRecord::Locked
    }

    #[inline]
    fn try_record_call_site(&self, pc: usize, frame_code_len: usize, n: u32) -> RowRecord {
        if let Some(out) = self.gate(frame_code_len) {
            return out;
        }
        let Some(site) = self.site(pc) else {
            return RowRecord::Locked;
        };
        if site.calls.load(Ordering::Relaxed) < 1_000_000 {
            site.calls.fetch_add(n, Ordering::Relaxed);
        }
        RowRecord::Done
    }

    /// Total receiver observations across all classes in the rows at `pc`.
    fn site_receiver_total(&self, pc: usize) -> u32 {
        let Some(site) = self.find_site(pc) else {
            return 0;
        };
        site.classes
            .iter()
            .zip(site.counts.iter())
            .fold(0u32, |acc, (c, n)| {
                if c.load(Ordering::Acquire) == 0 {
                    acc
                } else {
                    acc.saturating_add(n.load(Ordering::Relaxed))
                }
            })
    }

    /// What the rows hold for `class_id` at `pc`.
    fn receiver_count(&self, pc: usize, class_id: u32) -> u32 {
        let key = (1u64 << 32) | u64::from(class_id);
        let Some(site) = self.find_site(pc) else {
            return 0;
        };
        site.classes
            .iter()
            .position(|c| c.load(Ordering::Acquire) == key)
            .map_or(0, |k| site.counts[k].load(Ordering::Relaxed))
    }

    /// Add every non-zero row to the maps (a row claimed but not yet counted
    /// reads `0` and is skipped, so a snapshot never shows a class observed
    /// zero times).
    fn fold_into(
        &self,
        receivers: &mut FxHashMap<usize, ReceiverCounts>,
        call_sites: &mut FxHashMap<usize, u32>,
    ) {
        for site in self.sites.iter() {
            let key = site.pc.load(Ordering::Acquire);
            if key == 0 {
                continue;
            }
            let pc = (key - 1) as usize; // Cast: u32 -> usize widens
            let calls = site.calls.load(Ordering::Relaxed);
            if calls != 0 {
                let e = call_sites.entry(pc).or_insert(0);
                *e = e.saturating_add(calls);
            }
            for (k, cell) in site.classes.iter().enumerate() {
                let c = cell.load(Ordering::Acquire);
                if c == 0 {
                    continue;
                }
                let n = site.counts[k].load(Ordering::Relaxed);
                if n == 0 {
                    continue;
                }
                let class_id = c as u32; // Cast: the low half is the class id
                let e = receivers
                    .entry(pc)
                    .or_default()
                    .entry(class_id)
                    .or_insert(0);
                *e = e.saturating_add(n);
            }
        }
    }
}

impl MethodProfile {
    /// The slot's lock-free rows, allocating them on first use (W15L2-P3).
    fn receiver_rows_handle(&mut self) -> Arc<ReceiverRows> {
        if let Some(rows) = &self.receiver_rows {
            return Arc::clone(rows);
        }
        let rows = Arc::new(ReceiverRows::new(self.code_len, self.replay.is_some()));
        if self.reclaimed {
            rows.reclaimed.store(true, Ordering::Release);
        }
        self.receiver_rows = Some(Arc::clone(&rows));
        rows
    }
}

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
        self.record_n(pc, receiver_class_id, frame_code_len, 1)
    }

    /// [`Self::record`] for `n` calls at once (a sampled record that stands
    /// for the calls the invoke doors' coalescing skipped), so the evidence
    /// bars ([`crate::MIN_GUARDED_RECEIVER_EVIDENCE`]) still count calls.
    #[inline]
    pub fn record_n(&self, pc: usize, receiver_class_id: u32, frame_code_len: usize, n: u32) -> bool {
        if let Some(rows) = &self.1 {
            match rows.try_record_receiver(pc, receiver_class_id, frame_code_len, n) {
                RowRecord::Done => return true,
                RowRecord::Reclaimed => return false,
                RowRecord::Locked => {}
            }
        }
        let mut p = self.0.lock();
        if p.reclaimed {
            return false;
        }
        if p.admits_frame_code_len(frame_code_len) {
            p.record_receiver_n(pc, receiver_class_id, n);
        }
        true
    }

    /// Record one execution of the invoke at `pc` — the same effect as the
    /// last lines of [`ProfileStore::record_call_site_borrowed`], without the
    /// lookup. `false` exactly as [`Self::record`]: the slot was reclaimed and
    /// the holder must look the method up again (nio21 wave 15 L2).
    #[inline]
    pub fn record_call_site(&self, pc: usize, frame_code_len: usize) -> bool {
        self.record_call_site_n(pc, frame_code_len, 1)
    }

    /// [`Self::record_call_site`] for `n` executions at once.
    #[inline]
    pub fn record_call_site_n(&self, pc: usize, frame_code_len: usize, n: u32) -> bool {
        if let Some(rows) = &self.1 {
            match rows.try_record_call_site(pc, frame_code_len, n) {
                RowRecord::Done => return true,
                RowRecord::Reclaimed => return false,
                RowRecord::Locked => {}
            }
        }
        let mut p = self.0.lock();
        if p.reclaimed {
            return false;
        }
        if p.admits_frame_code_len(frame_code_len) {
            p.record_call_site_n(pc, n);
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
        let slot = self.get_or_insert_borrowed(class_id, method_name, descriptor);
        // W15L2-P3: the rows are made once per slot, under its lock, and
        // shared by every thread's handle.
        let rows = self.receiver_rows_on.then(|| {
            let mut p = slot.lock();
            let fresh = p.receiver_rows.is_none();
            let rows = p.receiver_rows_handle();
            if fresh && self.receiver_rows_dbg {
                eprintln!(
                    "[prof-rows] class={class_id} {method_name}{descriptor} sites={} locked_only={}",
                    rows.sites.len(),
                    rows.locked_only.load(Ordering::Relaxed)
                );
            }
            rows
        });
        ReceiverRecorder(slot, rows)
    }
}

#[cfg(test)]
mod r16_mega_polymorphic_set_tests;

#[cfg(test)]
mod w15l2_p3_receiver_rows_tests;

#[cfg(test)]
mod r21_ic_receivers_per_vm_tests;

#[cfg(test)]
mod r21_ic_receivers_env_table_tests;


