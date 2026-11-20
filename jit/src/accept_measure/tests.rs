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
