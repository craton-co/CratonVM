// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
//! ACCEPT21-1 (j27 wave 7, lane T1ACCEPT; book
//! `docs/internal/proposals/jit/tier1/jit-r21-accept-proposals.md`): the
//! DECISION half of the measuring acceptance gate. Pure data and pure
//! functions; nothing here reads a switch, a clock or the cache.
//!
//! The measuring gate publishes the optimizing body beside the single-pass
//! one, routes a sample of entries to each ([`route_entry`]), times both
//! ([`MeasureWindow::record`]), and keeps the faster ([`window_outcome`]);
//! the loser is retired through the existing supersede / not-entrant path.
//! TIERMGR21-4 (`jit-r21-tiermgr-proposals.md`, `tiered/accept_timing.rs`)
//! built the per-VM timer slot, queue and idle-tick census line; the side
//! slot a refused body is published to and the clock read around the call at
//! the caller-side door are VM dispatch work NOT built yet (they need a build
//! and the hot call door). This module is what that door feeds, in whatever
//! unit it measures (ns, or TSC ticks), the same unit on both arms; a decided
//! window hands TIERMGR21-4 its sample through [`timing_sample`].
//!
//! State is per method and per VM: a [`MeasureWindow`] belongs in the
//! per-VM tier manager's method state (e.g. `Option<Box<MeasureWindow>>` on
//! `tiered::MethodState`), never in a process global (AGENTS.md; the jit
//! statics ratchet counts every `static`). The OSR door uses the same window
//! keyed by its loop header.
//!
//! Noise: in-JVM timings on the test hosts swing ~3x between repetitions.
//! The window therefore (a) interleaves the arms call by call, so both see
//! the same load, (b) compares MEDIANS of batch means, so one batch with a GC
//! pause or a P/E-core migration in it cannot decide, and (c) calls anything
//! within [`TIE_MARGIN_PERCENT`] a tie, which keeps the evidence gate's own
//! answer: a measurement only ever overrides the gate when it is clear.

/// One entry in this many goes to the single-pass body during a window; the
/// rest go to the optimizing body (the proposal's "1 in 8").
pub const DEFAULT_ROUTE_PERIOD: u32 = 8;

/// Calls timed per batch; a batch's mean is one sample of its arm.
pub const BATCH_CALLS: u32 = 64;

/// Batches each arm needs before the window decides.
pub const MIN_BATCHES: usize = 5;

/// Batches an arm keeps; later calls on a full arm are not recorded. Odd, so
/// a full arm has a true median.
pub const MAX_BATCHES: usize = 9;

/// Medians closer than this percentage of the slower one are a tie.
pub const TIE_MARGIN_PERCENT: u64 = 10;

/// Which body an entry runs during a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasureArm {
    /// The single-pass (C1) body the gate compares against.
    SinglePass,
    /// The optimizing (IR) body on trial.
    Optimizing,
}

/// The body entry number `entry_count` (the counter the single-pass prologue
/// already bumps, `crate::entry_counter`) runs: every `period`-th entry is
/// [`MeasureArm::SinglePass`]. A `period` below 2 samples nothing: every entry
/// is optimizing (a period of 1 would starve the arm on trial).
pub fn route_entry(entry_count: u64, period: u32) -> MeasureArm {
    if period >= 2 && entry_count % u64::from(period) == 0 {
        MeasureArm::SinglePass
    } else {
        MeasureArm::Optimizing
    }
}

/// One arm's timings: up to [`MAX_BATCHES`] batch means plus the batch being
/// filled. Fixed size, no allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArmTimes {
    batch_means: [u64; MAX_BATCHES],
    batches: usize,
    open_sum: u64,
    open_calls: u32,
}

impl ArmTimes {
    /// Record one call that took `elapsed`. A call that trapped or threw is
    /// the caller's to drop: its time measures the deopt, not the body.
    pub fn record(&mut self, elapsed: u64) {
        if self.is_full() {
            return;
        }
        self.open_sum = self.open_sum.saturating_add(elapsed);
        self.open_calls += 1;
        if self.open_calls >= BATCH_CALLS {
            self.batch_means[self.batches] = self.open_sum / u64::from(self.open_calls);
            self.batches += 1;
            self.open_sum = 0;
            self.open_calls = 0;
        }
    }

    /// Closed batches so far.
    pub fn batches(&self) -> usize {
        self.batches
    }

    /// No more calls are recorded.
    pub fn is_full(&self) -> bool {
        self.batches >= MAX_BATCHES
    }

    /// The (lower) median of the closed batch means, `None` before the first.
    pub fn median(&self) -> Option<u64> {
        let n = self.batches;
        if n == 0 {
            return None;
        }
        let mut means = self.batch_means;
        means[..n].sort_unstable();
        Some(means[(n - 1) / 2])
    }
}

/// What a window has measured so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasuredVerdict {
    /// An arm has fewer than [`MIN_BATCHES`] batches.
    Pending,
    /// The optimizing body's median is faster by at least the margin.
    OptimizingFaster,
    /// The single-pass body's median is faster by at least the margin.
    SinglePassFaster,
    /// Within [`TIE_MARGIN_PERCENT`].
    Tie,
}

impl MeasuredVerdict {
    /// The census word.
    pub fn word(self) -> &'static str {
        match self {
            MeasuredVerdict::Pending => "pending",
            MeasuredVerdict::OptimizingFaster => "c2-faster",
            MeasuredVerdict::SinglePassFaster => "c1-faster",
            MeasuredVerdict::Tie => "tie",
        }
    }
}

/// Compare two medians (`c1` single-pass, `c2` optimizing).
pub fn compare_medians(c1: u64, c2: u64) -> MeasuredVerdict {
    let slower = c1.max(c2);
    let diff = c1.abs_diff(c2);
    if diff.saturating_mul(100) < slower.saturating_mul(TIE_MARGIN_PERCENT) || slower == 0 {
        MeasuredVerdict::Tie
    } else if c2 < c1 {
        MeasuredVerdict::OptimizingFaster
    } else {
        MeasuredVerdict::SinglePassFaster
    }
}

/// Both arms' timings for one method (or one OSR loop header).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeasureWindow {
    /// The single-pass arm (`c1=` in the census).
    pub single_pass: ArmTimes,
    /// The optimizing arm (`c2=` in the census).
    pub optimizing: ArmTimes,
}

impl MeasureWindow {
    /// Record one call of `arm` that took `elapsed`.
    pub fn record(&mut self, arm: MeasureArm, elapsed: u64) {
        match arm {
            MeasureArm::SinglePass => self.single_pass.record(elapsed),
            MeasureArm::Optimizing => self.optimizing.record(elapsed),
        }
    }

    /// The verdict once both arms have [`MIN_BATCHES`] batches.
    pub fn verdict(&self) -> MeasuredVerdict {
        if self.single_pass.batches() < MIN_BATCHES || self.optimizing.batches() < MIN_BATCHES {
            return MeasuredVerdict::Pending;
        }
        match (self.single_pass.median(), self.optimizing.median()) {
            (Some(c1), Some(c2)) => compare_medians(c1, c2),
            _ => MeasuredVerdict::Pending,
        }
    }
}

/// What the tier manager does when a window reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowOutcome {
    /// Keep routing; the window is still open.
    Undecided,
    /// Keep the optimizing body; retire the single-pass one.
    KeepOptimizing,
    /// Keep the single-pass body; retire the optimizing one.
    KeepSinglePass,
}

impl WindowOutcome {
    /// The census word.
    pub fn word(self) -> &'static str {
        match self {
            WindowOutcome::Undecided => "undecided",
            WindowOutcome::KeepOptimizing => "keep-c2",
            WindowOutcome::KeepSinglePass => "keep-c1",
        }
    }
}

/// The one decision: a clear measurement wins; a tie keeps the body the
/// evidence gate (with its self-recursion exemptions) chose, `gate_kept` =
/// it chose the optimizing body.
pub fn window_outcome(gate_kept: bool, verdict: MeasuredVerdict) -> WindowOutcome {
    match verdict {
        MeasuredVerdict::Pending => WindowOutcome::Undecided,
        MeasuredVerdict::OptimizingFaster => WindowOutcome::KeepOptimizing,
        MeasuredVerdict::SinglePassFaster => WindowOutcome::KeepSinglePass,
        MeasuredVerdict::Tie if gate_kept => WindowOutcome::KeepOptimizing,
        MeasuredVerdict::Tie => WindowOutcome::KeepSinglePass,
    }
}

/// Whether the gate's answer agrees with the measurement -- the number step 1
/// of ACCEPT21-1 asks for ("how often the gate is wrong and in which
/// direction"): `agree`, `gate-wrong-refuse` (refused a faster body),
/// `gate-wrong-accept` (kept a slower body), `tie` or `pending`.
pub fn gate_agreement(gate_kept: bool, verdict: MeasuredVerdict) -> &'static str {
    match verdict {
        MeasuredVerdict::Pending => "pending",
        MeasuredVerdict::Tie => "tie",
        MeasuredVerdict::OptimizingFaster if gate_kept => "agree",
        MeasuredVerdict::OptimizingFaster => "gate-wrong-refuse",
        MeasuredVerdict::SinglePassFaster if gate_kept => "gate-wrong-accept",
        MeasuredVerdict::SinglePassFaster => "agree",
    }
}

/// The measured census line, printed under the existing
/// `CRATONVM_DBG_ACCEPT_CENSUS` channel (`ir_evidence::accept_census_enabled`)
/// once a window decides. `gate_word` is `ir_evidence::evidence_gate_word`'s
/// answer; the `[accept-census]` prefix keeps step 1's greps matching and the
/// `measured` word tells the two line kinds apart.
pub fn measured_census_line(
    method: &str,
    gate_word: &str,
    gate_kept: bool,
    window: &MeasureWindow,
) -> String {
    let median = |arm: &ArmTimes| arm.median().map_or_else(|| "-".to_string(), |m| m.to_string());
    let verdict = window.verdict();
    format!(
        "[accept-census] measured {method} c1={} c2={} batches={}/{} verdict={} gate={gate_word} \
         agreement={} outcome={}",
        median(&window.single_pass),
        median(&window.optimizing),
        window.single_pass.batches(),
        window.optimizing.batches(),
        verdict.word(),
        gate_agreement(gate_kept, verdict),
        window_outcome(gate_kept, verdict).word(),
    )
}

/// A decided window as TIERMGR21-4's sample (`crate::tiered::AcceptTimingSample`),
/// so a VM timer installed with `TieredCompilationManager::set_accept_timing_hook`
/// can hand over what routed real entries measured instead of replaying calls
/// itself (a replay runs the method's side effects again, off a Java thread:
/// see `docs/internal/jit/j27-accept-timing-replay-harness-reruns-side-effects-FIXED-20261029.md`).
/// Each arm's median per-call time over [`BATCH_CALLS`] calls; `None` while
/// the window is pending.
pub fn timing_sample(window: &MeasureWindow) -> Option<crate::tiered::AcceptTimingSample> {
    if window.verdict() == MeasuredVerdict::Pending {
        return None;
    }
    let calls = u64::from(BATCH_CALLS);
    Some(crate::tiered::AcceptTimingSample {
        single_pass_ns: window.single_pass.median()?.saturating_mul(calls),
        optimizing_ns: window.optimizing.median()?.saturating_mul(calls),
        calls: BATCH_CALLS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill `batches` whole batches of `arm` at `ns` per call.
    fn fill(w: &mut MeasureWindow, arm: MeasureArm, batches: usize, ns: u64) {
        for _ in 0..batches * BATCH_CALLS as usize {
            w.record(arm, ns);
        }
    }

    #[test]
    fn one_entry_in_the_period_goes_to_the_single_pass_body() {
        let single = (0..800u64)
            .filter(|&n| route_entry(n, DEFAULT_ROUTE_PERIOD) == MeasureArm::SinglePass)
            .count();
        assert_eq!(single, 100);
        assert_eq!(route_entry(0, 1), MeasureArm::Optimizing, "period 1 samples nothing");
        assert_eq!(route_entry(0, 0), MeasureArm::Optimizing, "period 0 samples nothing");
    }

    #[test]
    fn a_batch_closes_at_its_call_count_with_its_mean() {
        let mut a = ArmTimes::default();
        for i in 0..BATCH_CALLS - 1 {
            a.record(u64::from(i % 2) * 20);
        }
        assert_eq!(a.batches(), 0);
        assert_eq!(a.median(), None);
        a.record(20);
        assert_eq!(a.batches(), 1);
        assert_eq!(a.median(), Some(10));
    }

    #[test]
    fn a_full_arm_records_nothing_more() {
        let mut w = MeasureWindow::default();
        fill(&mut w, MeasureArm::Optimizing, MAX_BATCHES, 7);
        assert!(w.optimizing.is_full());
        fill(&mut w, MeasureArm::Optimizing, 3, 1_000_000);
        assert_eq!(w.optimizing.batches(), MAX_BATCHES);
        assert_eq!(w.optimizing.median(), Some(7));
    }

    #[test]
    fn the_window_waits_for_both_arms() {
        let mut w = MeasureWindow::default();
        fill(&mut w, MeasureArm::Optimizing, MAX_BATCHES, 10);
        fill(&mut w, MeasureArm::SinglePass, MIN_BATCHES - 1, 100);
        assert_eq!(w.verdict(), MeasuredVerdict::Pending);
        fill(&mut w, MeasureArm::SinglePass, 1, 100);
        assert_eq!(w.verdict(), MeasuredVerdict::OptimizingFaster);
    }

    /// One pathological batch (a GC pause) on the faster arm does not flip
    /// the verdict: medians, not means.
    #[test]
    fn one_slow_batch_does_not_decide() {
        let mut w = MeasureWindow::default();
        fill(&mut w, MeasureArm::SinglePass, MIN_BATCHES, 100);
        fill(&mut w, MeasureArm::Optimizing, 1, 50_000);
        fill(&mut w, MeasureArm::Optimizing, MIN_BATCHES - 1, 60);
        assert_eq!(w.verdict(), MeasuredVerdict::OptimizingFaster);
    }

    #[test]
    fn medians_within_the_margin_are_a_tie() {
        assert_eq!(compare_medians(100, 91), MeasuredVerdict::Tie);
        assert_eq!(compare_medians(100, 90), MeasuredVerdict::OptimizingFaster);
        assert_eq!(compare_medians(90, 100), MeasuredVerdict::SinglePassFaster);
        assert_eq!(compare_medians(0, 0), MeasuredVerdict::Tie);
        assert_eq!(compare_medians(u64::MAX, 1), MeasuredVerdict::OptimizingFaster);
    }

    #[test]
    fn a_clear_measurement_overrides_the_gate_and_a_tie_keeps_it() {
        use MeasuredVerdict::*;
        use WindowOutcome::*;
        assert_eq!(window_outcome(false, OptimizingFaster), KeepOptimizing);
        assert_eq!(window_outcome(true, SinglePassFaster), KeepSinglePass);
        assert_eq!(window_outcome(true, Tie), KeepOptimizing);
        assert_eq!(window_outcome(false, Tie), KeepSinglePass);
        assert_eq!(window_outcome(true, Pending), Undecided);
    }

    #[test]
    fn agreement_names_the_direction_the_gate_was_wrong_in() {
        use MeasuredVerdict::*;
        assert_eq!(gate_agreement(false, OptimizingFaster), "gate-wrong-refuse");
        assert_eq!(gate_agreement(true, SinglePassFaster), "gate-wrong-accept");
        assert_eq!(gate_agreement(true, OptimizingFaster), "agree");
        assert_eq!(gate_agreement(false, SinglePassFaster), "agree");
        assert_eq!(gate_agreement(false, Tie), "tie");
        assert_eq!(gate_agreement(true, Pending), "pending");
    }

    /// The handover to TIERMGR21-4's line reads back the window's medians.
    #[test]
    fn a_decided_window_is_a_timing_sample_of_its_medians() {
        let mut w = MeasureWindow::default();
        fill(&mut w, MeasureArm::SinglePass, MIN_BATCHES, 120);
        assert_eq!(timing_sample(&w), None, "pending");
        fill(&mut w, MeasureArm::Optimizing, MIN_BATCHES, 80);
        let s = timing_sample(&w).expect("decided");
        assert_eq!((s.per_call(s.single_pass_ns), s.per_call(s.optimizing_ns)), (120, 80));
        assert_eq!(s.faster(), "c2");
    }

    #[test]
    fn the_measured_census_line() {
        let mut w = MeasureWindow::default();
        assert_eq!(
            measured_census_line("C.m()V", "refuse-no-evidence", false, &w),
            "[accept-census] measured C.m()V c1=- c2=- batches=0/0 verdict=pending \
             gate=refuse-no-evidence agreement=pending outcome=undecided"
        );
        fill(&mut w, MeasureArm::SinglePass, MIN_BATCHES, 120);
        fill(&mut w, MeasureArm::Optimizing, MIN_BATCHES, 80);
        assert_eq!(
            measured_census_line("C.m()V", "refuse-no-evidence", false, &w),
            "[accept-census] measured C.m()V c1=120 c2=80 batches=5/5 verdict=c2-faster \
             gate=refuse-no-evidence agreement=gate-wrong-refuse outcome=keep-c2"
        );
    }
}
