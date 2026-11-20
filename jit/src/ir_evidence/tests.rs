use super::*;

/// A nested IR attempt on the same thread must not leave its exit for the
/// outer one. The outer attempt below declines in the pipeline AFTER a
/// nested attempt was refused by the acceptance gate; without the
/// save/restore the outer one would read `Acceptance` and the funnel would
/// file a pipeline decline as a quality refusal.
#[test]
fn a_nested_attempt_does_not_leak_its_exit_into_the_outer_one() {
    let outer = arm_ir_exit_mark();
    {
        let inner = arm_ir_exit_mark();
        mark_ir_exit(AdmissionExit::Acceptance);
        assert_eq!(take_ir_exit_mark(inner), AdmissionExit::Acceptance);
    }
    assert_eq!(take_ir_exit_mark(outer), AdmissionExit::Pipeline);

    let outer = arm_ir_exit_mark();
    {
        let inner = arm_ir_exit_mark();
        assert_eq!(take_ir_exit_mark(inner), AdmissionExit::Pipeline);
    }
    mark_ir_exit(AdmissionExit::NodeCap);
    assert_eq!(take_ir_exit_mark(outer), AdmissionExit::NodeCap);
}

/// Every exit has a row, zeroes included, in funnel order.
#[test]
fn the_funnel_reports_every_exit() {
    let names: Vec<&str> = admission_funnel().into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        AdmissionExit::ALL
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>()
    );
    assert!(admission_funnel_line().starts_with("optimizing requests="));
}

/// Drain any residue a sibling test left on this thread.
fn drain() {
    while take().is_some() {}
}

/// The armed/un-armed distinction is the whole safety argument, so it is
/// the first thing tested: an un-armed `note` must not leak into the next
/// compile on the same thread.
#[test]
fn an_unarmed_note_is_ignored_and_does_not_leak() {
    drain();
    note(Transform::Inlined);
    assert_eq!(take(), None, "a note with no armed compile records nothing");
    begin_compile();
    assert_eq!(
        take(),
        Some(CompileRecord::default()),
        "an armed compile that did nothing reads as an empty record, not None",
    );
}

/// A compile that begins INSIDE another must not disarm it. With one cell
/// instead of a stack, the H2 JDBC workload reported `unjudged=190` -- the
/// outer compile read `None` and the gate waved it through without judging.
#[test]
fn a_nested_compile_does_not_disarm_the_outer_one() {
    drain();
    begin_compile();
    note(Transform::Inlined);
    // ...an inner compile starts and finishes...
    begin_compile();
    note(Transform::GuardElided);
    let inner = take().expect("the inner compile is armed");
    assert_eq!(
        describe(inner.bits),
        "guard-elided",
        "the inner compile must see ONLY its own transforms",
    );
    let outer = take().expect("the outer compile must still be armed");
    assert_eq!(
        describe(outer.bits),
        "inlined",
        "the outer compile must keep its own bits across a nested one",
    );
    assert_eq!(take(), None, "and the stack is empty again");
}

#[test]
fn bits_accumulate_and_describe() {
    drain();
    begin_compile();
    note(Transform::Inlined);
    note(Transform::GuardElided);
    let rec = take().expect("armed");
    assert!(is_worth_publishing(rec));
    let d = describe(rec.bits);
    assert!(d.contains("inlined") && d.contains("guard-elided"), "{d}");
}

/// `Unrolled` and `Licm` alone are NOT evidence, because the single-pass
/// backend has both. This is the judgment the module header states, pinned
/// so that changing it is a deliberate edit rather than a drift.
#[test]
fn a_transform_the_baseline_tier_also_has_is_not_evidence() {
    drain();
    begin_compile();
    note(Transform::Unrolled);
    note(Transform::Licm);
    let rec = take().expect("armed");
    assert!(
        !is_worth_publishing(rec),
        "the baseline tier unrolls 4x and hoists; doing the same is not a \
         reason to replace its body",
    );
}

/// THE REGRESSION THIS GATE MISSED. `Inlined` is on the evidence list, so
/// the bitset alone accepts this body -- and the body is slower, because
/// the splice left behind a call that resolves by name on every execution.
/// Measured shape: `Objects.checkIndex` spliced into `ArrayList.get`,
/// 897 ms against a single-pass 338 ms.
#[test]
fn a_transform_that_made_the_body_slower_is_refused() {
    drain();
    begin_compile();
    note(Transform::Inlined);
    note_blind_dispatch_in_splice();
    let rec = take().expect("armed");
    assert!(
        is_worth_publishing_bits(rec.bits),
        "the transform list alone still accepts it -- that is the bug",
    );
    assert!(
        rec.added_ns_per_execution() > 0,
        "one name resolution against one saved frame is a regression",
    );
    assert!(
        !is_worth_publishing(rec),
        "the priced judgment must refuse a body its own transform made slower",
    );
}

/// The refusal is a PRICE, not a veto on the opcode: enough saved frames
/// pay for a resolution, and the gate must let that through rather than
/// treating any blind dispatch as fatal.
#[test]
fn enough_saved_frames_pay_for_a_resolution() {
    drain();
    begin_compile();
    note(Transform::Inlined);
    note_blind_dispatch_in_splice();
    // 171 ns of regression against 4 ns a frame: 43 frames is the turn.
    for _ in 0..50 {
        note(Transform::Inlined);
    }
    let rec = take().expect("armed");
    assert!(rec.added_ns_per_execution() <= 0, "{rec:?}");
    assert!(
        is_worth_publishing(rec),
        "a paid-for trade is still publishable"
    );
}

/// A blind dispatch in the method's OWN code is not this compile's doing --
/// an unbindable or megamorphic site resolves by name in either tier -- so
/// it must not be charged here. Only `note_blind_dispatch_in_splice` counts.
#[test]
fn only_the_splices_own_resolutions_are_charged() {
    drain();
    begin_compile();
    note(Transform::Inlined);
    let rec = take().expect("armed");
    assert_eq!(rec.blind_dispatches_in_splice, 0);
    assert!(
        rec.added_ns_per_execution() < 0,
        "a splice that left no resolution behind is a straight saving",
    );
    assert!(is_worth_publishing(rec));
}

/// Round 11 (irlower): a lowering RETRIED after a code-buffer overflow
/// rolls the blind-dispatch charge back to what it was before the first
/// attempt, so the discarded attempt's resolutions are not charged twice.
/// One splice left one resolution behind and saved 43 frames: exactly
/// paid for (-1 ns). Charged twice it read +170 ns and was refused.
#[test]
fn a_retried_lowering_is_charged_its_blind_dispatches_once() {
    drain();
    assert_eq!(
        blind_dispatch_charge(),
        None,
        "nothing armed, nothing to read"
    );
    restore_blind_dispatch_charge(5); // a no-op when un-armed
    assert_eq!(take(), None);

    begin_compile();
    for _ in 0..43 {
        note(Transform::Inlined);
    }
    let before = blind_dispatch_charge().expect("armed");
    assert_eq!(before, 0);
    note_blind_dispatch_in_splice(); // the discarded attempt
    restore_blind_dispatch_charge(before); // the retry's roll-back
    note_blind_dispatch_in_splice(); // the retry
    let rec = take().expect("armed");
    assert_eq!(rec.blind_dispatches_in_splice, 1);
    assert!(rec.added_ns_per_execution() <= 0, "{rec:?}");
    assert!(is_worth_publishing(rec));
}


/// JIT round 17 wave 3 (lane irlower, IRFEED17-2 / IRFEED17-1): a FULL
/// unroll is recorded apart from the bitset and is evidence unless its
/// switch is `=0`; `describe_record` names every out-of-bitset count and
/// renders a record without them exactly as `describe` does.



/// `TRANSFORM_CENSUS` is an array of `ALL.len()` counters indexed by the
/// discriminant, from `note` — which runs on every compile thread. A
/// variant added to the enum but not to `ALL` would index past the end and
/// PANIC there, and one inserted out of order would count under another
/// transform's name. Pin both.
#[test]
fn every_transform_indexes_its_own_census_row() {
    for (i, t) in Transform::ALL.iter().enumerate() {
        assert_eq!(*t as usize, i, "{t:?} is out of place in Transform::ALL");
    }
    assert_eq!(
        Transform::MainPostUnrolled as usize + 1,
        Transform::ALL.len(),
        "the last variant must be the last entry of ALL",
    );
}

/// A plumbing mistake must cost an unfiltered publish, not a disabled tier.
#[test]
fn an_unrecorded_compile_is_accepted_and_counted() {
    drain();
    let before = census().3;
    // Only meaningful under the default policy; another test may have
    // latched a different one into the `OnceLock`.
    if accept_policy() == AcceptPolicy::Evidence {
        assert!(accept(None), "an unjudged compile must be accepted");
        assert_eq!(census().3, before + 1, "and counted as unjudged");
    }
}

/// Round 9 wave 2, `ir-refusal-memo-outlives-the-inputs-it-judged`: the
/// gate's refusal is judged against the inputs of its generation, a sticky
/// refusal is not, and an input-stamped one becomes final after
/// `MAX_INPUT_REFUSALS` generations.
///
/// Reads the entries and `memo_still_refuses` directly rather than
/// `IrRefusalMemo::already_refused`, which also depends on the
/// process-wide policy latch; other tests may bump the generation
/// concurrently, which only widens the gaps this test relies on.
#[test]
fn an_input_stamped_refusal_expires_when_the_inputs_move_and_a_sticky_one_does_not() {
    let memo = IrRefusalMemo::default();
    let stamped = 0x5EED_0009_0002_0001u64;
    let sticky = 0x5EED_0009_0002_0002u64;
    let entry = |k: u64| memo.entry(k).expect("the refusal was recorded");
    let note_method_refused_for_inputs = |k: u64| memo.note_refused_for_inputs(k);
    let note_method_refused = |k: u64| memo.note_refused(k);

    note_method_refused_for_inputs(stamped);
    note_method_refused(sticky);
    let g0 = entry(stamped).generation;
    assert!(
        memo_still_refuses(&entry(stamped), g0),
        "refused under its own inputs"
    );
    assert!(
        !memo_still_refuses(&entry(stamped), g0.wrapping_add(1)),
        "offered again once the inputs moved",
    );
    assert!(
        memo_still_refuses(&entry(sticky), g0.wrapping_add(1)),
        "a sticky (site-trap) refusal never expires",
    );

    // A repeat under the SAME generation is not a second refusal...
    let before = entry(stamped).refusals;
    note_method_refused_for_inputs(stamped);
    if inputs_generation() == g0 {
        assert_eq!(entry(stamped).refusals, before);
    }
    // ...and refusals under MAX_INPUT_REFUSALS distinct generations are
    // final.
    while entry(stamped).refusals < MAX_INPUT_REFUSALS {
        bump_inputs_generation();
        note_method_refused_for_inputs(stamped);
    }
    let last = entry(stamped).generation;
    assert!(
        memo_still_refuses(&entry(stamped), last.wrapping_add(7)),
        "refused under {MAX_INPUT_REFUSALS} generations: final",
    );

    // An input-stamped note never downgrades a sticky entry.
    bump_inputs_generation();
    note_method_refused_for_inputs(sticky);
    assert!(entry(sticky).sticky);
}
