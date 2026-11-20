use super::*;

/// The IV local used throughout; local 2 is the usual limit local.
const IV: usize = 1;
const N: usize = 2;

fn mk(init: IntRange, stride: Stride, cmp: ExitCmp, bound: BoundSource) -> CountedLoop {
    CountedLoop {
        header_pc: 0,
        back_edge_pc: 0,
        iv: AffineIv {
            local: IV,
            init,
            stride,
        },
        cmp,
        bound,
        bound_range: IntRange::unknown(),
        form: LoopForm::PreTested,
        modified_locals: 1u64 << IV,
        heap_stable: true,
        has_other_exit: false,
    }
}

fn guards(p: &BoundsProof) -> Vec<PreheaderGuard> {
    match p {
        BoundsProof::Guarded(g) => g.clone(),
        other => panic!("expected Guarded, got {other:?}"),
    }
}

fn refusal(p: &BoundsProof) -> RefusalReason {
    match p {
        BoundsProof::Refused(r) => *r,
        other => panic!("expected Refused, got {other:?}"),
    }
}

fn bound_term(b: BoundSource, addend: i32) -> SymBound {
    SymBound {
        base: BoundTerm::Bound(b),
        addend,
    }
}

fn entry_term(addend: i32) -> SymBound {
    SymBound {
        base: BoundTerm::IvEntry(IV),
        addend,
    }
}

// -- lattice ------------------------------------------------------------

#[test]
fn lattice_join_meet_and_emptiness() {
    let a = IntRange::new(0, 10);
    let b = IntRange::new(5, 20);
    assert_eq!(a.join(b), IntRange::new(0, 20));
    assert_eq!(a.meet(b), IntRange::new(5, 10));
    // Disjoint facts about one value are a contradiction, not a guess.
    assert!(IntRange::new(0, 3).meet(IntRange::new(9, 12)).is_empty());
    assert!(IntRange::new(1, 0).is_empty());
    assert!(IntRange::unknown().is_unknown());
    assert!(!IntRange::Empty.is_unknown());
    assert_eq!(IntRange::constant(7).as_constant(), Some(7));
    assert!(IntRange::unknown().contains_all(a));
    assert!(!a.contains_all(IntRange::unknown()));
}

#[test]
fn lattice_arithmetic_refuses_to_pretend_wrapping_did_not_happen() {
    let hi = IntRange::new(i32::MAX - 1, i32::MAX);
    // Exact addition has no answer in `int`.
    assert_eq!(hi.add_no_wrap(IntRange::constant(1)), None);
    // The wrapping model is sound but useless — never a tight lie.
    assert!(hi.add(IntRange::constant(1)).is_unknown());
    // In range, both agree.
    assert_eq!(
        IntRange::new(0, 10).add_no_wrap(IntRange::constant(5)),
        Some(IntRange::new(5, 15))
    );
    // Negative scale swaps the endpoints; i32::MIN has no negation.
    assert_eq!(
        IntRange::new(-2, 3).scale_no_wrap(-4),
        Some(IntRange::new(-12, 8))
    );
    assert_eq!(IntRange::constant(i32::MIN).neg_no_wrap(), None);
    // min/max are elementwise.
    assert_eq!(
        IntRange::new(0, 100).min_with(IntRange::new(-5, 20)),
        IntRange::new(-5, 20)
    );
    assert_eq!(
        IntRange::new(0, 100).max_with(IntRange::new(-5, 20)),
        IntRange::new(0, 100)
    );
    assert!(IntRange::array_length().definitely_non_negative());
}

/// `IntRange::Empty` is not non-negative, for the same reason
/// `range_analysis::Range`'s bottom is not: the only thing a caller does
/// with the answer is drop a bounds check, and at `Empty` there is no index
/// that was checked. The vacuous twin this replaces answered `true`.
#[test]
fn an_empty_int_range_is_not_definitely_non_negative() {
    assert!(!IntRange::Empty.definitely_non_negative());
    assert!(IntRange::new(0, 0).definitely_non_negative());
    assert!(!IntRange::new(-1, 9).definitely_non_negative());
    assert!(!IntRange::unknown().definitely_non_negative());
}

// -- ascending, exclusive vs inclusive ----------------------------------

#[test]
fn ascending_exclusive_local_bound_needs_length_ge_bound() {
    // for (i = ?; i < n; i++) a[i]
    let l = mk(
        IntRange::unknown(),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p),
        [
            PreheaderGuard::NonNegative(entry_term(0)),
            // `length >= n`: the exclusive comparator's -1 and the "one
            // past the last index" +1 cancel exactly.
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
        ]
    );
    // No wrap guard: the last executed index is at most `n - 1`, so the
    // next value `n` is representable for every `n`.
    assert_eq!(
        l.iv_span(&RangeEnv::new()).unwrap().overflow,
        OverflowModel::NoWrapProven
    );
}

#[test]
fn ascending_inclusive_is_accepted_with_length_gt_bound_and_a_wrap_guard() {
    // for (i = ?; i <= n; i++) a[i] — the shape `bce.rs` refuses on the
    // static path and hides behind an off-by-default env flag on the
    // speculative one.
    let l = mk(
        IntRange::unknown(),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Local(N),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p),
        [
            // `n != Integer.MAX_VALUE`, derived rather than special-cased.
            PreheaderGuard::AtMost {
                term: bound_term(BoundSource::Local(N), 0),
                limit: i32::MAX - 1,
            },
            PreheaderGuard::NonNegative(entry_term(0)),
            // `length >= n + 1`, i.e. strictly greater than the bound.
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 1)),
        ]
    );
    assert_eq!(
        l.iv_span(&RangeEnv::new()).unwrap().overflow,
        OverflowModel::NoWrapGuarded
    );
}

#[test]
fn ascending_exclusive_constant_bound_with_known_length_is_static() {
    // for (i = 0; i < 16; i++) a[i] with `a = new int[16]`.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Const(16),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::constant(16),
        &RangeEnv::new(),
    );
    assert_eq!(p, BoundsProof::Static);
    // Twin: one element shorter and the same loop must not be static.
    let p2 = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::constant(15),
        &RangeEnv::new(),
    );
    assert!(matches!(p2, BoundsProof::Refused(_)), "got {p2:?}");
}

// -- descending ---------------------------------------------------------

#[test]
fn descending_inclusive_zero_bound_is_bounded_by_the_entry_value() {
    // for (i = n - 1; i >= 0; i--) a[i]
    let l = mk(
        IntRange::unknown(),
        Stride::Const(-1),
        ExitCmp::Lt,
        BoundSource::Const(0),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    // The `>= 0` half is free: the comparator itself pins the low end.
    // The high end is the entry value, so `length > i_entry`.
    assert_eq!(guards(&p), [PreheaderGuard::LengthAtLeast(entry_term(1))]);
    assert_eq!(l.direction(), Direction::Decreasing);
}

#[test]
fn descending_exclusive_zero_bound_still_pins_the_low_end() {
    // for (i = n; i > 0; i--) a[i - 1]
    let l = mk(
        IntRange::unknown(),
        Stride::Const(-1),
        ExitCmp::Le,
        BoundSource::Const(0),
    );
    let span = l.iv_span(&RangeEnv::new()).unwrap();
    // Runs while `i > 0`, so every executed value is at least 1.
    assert_eq!(span.span.numeric, IntRange::new(1, i32::MAX));
    let p = l.prove_index_in_bounds(
        &IndexExpr::shifted(IV, -1),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(guards(&p), [PreheaderGuard::LengthAtLeast(entry_term(0))]);
}

#[test]
fn descending_stride_must_match_the_comparator() {
    // A positive stride under a low-side exit is not a counted loop.
    let bad = mk(
        IntRange::constant(10),
        Stride::Const(1),
        ExitCmp::Le,
        BoundSource::Const(0),
    );
    assert_eq!(
        bad.iv_span(&RangeEnv::new()),
        Err(RefusalReason::DirectionMismatch)
    );
    // Twin: the same loop counting the right way is provable.
    let good = mk(
        IntRange::constant(10),
        Stride::Const(-1),
        ExitCmp::Le,
        BoundSource::Const(0),
    );
    assert!(good.iv_span(&RangeEnv::new()).is_ok());
}

#[test]
fn zero_stride_is_never_a_counted_loop() {
    let l = mk(
        IntRange::constant(0),
        Stride::Const(0),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::ZeroStride));
}

// -- non-unit strides ---------------------------------------------------

#[test]
fn non_unit_ascending_stride_widens_the_wrap_headroom() {
    // for (i = 0; i < n; i += 3): last index <= n-1, next value n+2.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(3),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p),
        [
            PreheaderGuard::AtMost {
                term: bound_term(BoundSource::Local(N), 0),
                limit: i32::MAX - 2,
            },
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
        ]
    );
    // Twin: knowing the limit is small discharges the wrap obligation
    // statically, and only the length guard survives.
    let mut small = l.clone();
    small.bound_range = IntRange::new(0, 1000);
    let p2 = small.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p2),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(N),
            0
        ))]
    );
    assert_eq!(
        small.iv_span(&RangeEnv::new()).unwrap().overflow,
        OverflowModel::NoWrapProven
    );
}

#[test]
fn negative_non_unit_stride_counts_down() {
    // for (i = 10; i > 0; i -= 3) → 10, 7, 4, 1 → 4 trips.
    let l = mk(
        IntRange::constant(10),
        Stride::Const(-3),
        ExitCmp::Le,
        BoundSource::Const(0),
    );
    assert_eq!(
        l.trip_count(&RangeEnv::new()).and_then(|t| t.exact()),
        Some(4)
    );
    assert_eq!(
        l.iv_span(&RangeEnv::new()).unwrap().span.numeric,
        IntRange::new(1, 10)
    );
}

// -- bound provenance ---------------------------------------------------

#[test]
fn bound_from_a_constant_local_field_and_array_length_all_work() {
    for bound in [
        BoundSource::Const(64),
        BoundSource::Local(N),
        BoundSource::ArrayLength(3),
        BoundSource::Field {
            cp_index: 7,
            receiver_local: Some(0),
        },
        BoundSource::Field {
            cp_index: 9,
            receiver_local: None,
        },
    ] {
        let l = mk(
            IntRange::constant(0),
            Stride::Const(1),
            ExitCmp::Ge,
            bound.clone(),
        );
        let p = l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new(),
        );
        assert!(
            matches!(p, BoundsProof::Guarded(_) | BoundsProof::Static),
            "bound {bound:?} should be provable, got {p:?}"
        );
    }
}

#[test]
fn a_field_bound_needs_a_heap_stable_body() {
    let field = BoundSource::Field {
        cp_index: 7,
        receiver_local: Some(0),
    };
    let mut l = mk(IntRange::constant(0), Stride::Const(1), ExitCmp::Ge, field);
    // MUST REFUSE: the body can store to the field between the pre-header
    // guard and a later iteration's exit test.
    l.heap_stable = false;
    assert_eq!(
        l.iv_span(&RangeEnv::new()),
        Err(RefusalReason::BoundNotInvariant)
    );
    // MUST ACCEPT twin.
    l.heap_stable = true;
    assert!(l.iv_span(&RangeEnv::new()).is_ok());
}

#[test]
fn a_bound_local_the_body_writes_is_refused() {
    let mut l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    // MUST REFUSE: raising `n` inside the loop makes a pre-header
    // `length >= n` guard stale on a later trip.
    l.modified_locals |= 1u64 << N;
    assert_eq!(
        l.iv_span(&RangeEnv::new()),
        Err(RefusalReason::BoundNotInvariant)
    );
    // MUST ACCEPT twin.
    l.modified_locals = 1u64 << IV;
    assert!(l.iv_span(&RangeEnv::new()).is_ok());
}

#[test]
fn min_and_max_bounds_fold_through_the_lattice() {
    // for (i = 0; i < Math.min(16, n); i++) into a 16-element array: the
    // numeric side alone finishes the proof, no guard at all.
    let min_bound = BoundSource::Min(
        Box::new(BoundSource::Const(16)),
        Box::new(BoundSource::Local(N)),
    );
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        min_bound,
    );
    assert_eq!(
        l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::constant(16),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    );
    // `Math.max(0, n)` gives a non-negative limit, which is enough to
    // prove the *low* end of the span without any guard.
    let max_bound = BoundSource::Max(
        Box::new(BoundSource::Const(0)),
        Box::new(BoundSource::Local(N)),
    );
    let l2 = mk(
        IntRange::unknown(),
        Stride::Const(-1),
        ExitCmp::Lt,
        max_bound,
    );
    let span = l2.iv_span(&RangeEnv::new()).unwrap();
    assert!(span.span.numeric.definitely_non_negative());
    assert!(span.guards.is_empty());

    // Twin: without the `max`, the same descending loop can reach `n`,
    // which may be negative — so the proof asks for the guard instead of
    // assuming the limit is sane.
    let l3 = mk(
        IntRange::unknown(),
        Stride::Const(-1),
        ExitCmp::Lt,
        BoundSource::Local(N),
    );
    assert!(!l3
        .iv_span(&RangeEnv::new())
        .unwrap()
        .span
        .numeric
        .definitely_non_negative());
    let p3 = l3.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p3),
        [
            // `n` must not sit at Integer.MIN_VALUE, or `n - 1` underflows.
            PreheaderGuard::AtLeast {
                term: bound_term(BoundSource::Local(N), 0),
                limit: i32::MIN + 1,
            },
            PreheaderGuard::NonNegative(bound_term(BoundSource::Local(N), 0)),
            PreheaderGuard::LengthAtLeast(entry_term(1)),
        ]
    );
}

// -- the overflow model -------------------------------------------------

#[test]
fn inclusive_loop_at_integer_max_value_must_refuse() {
    // MUST REFUSE: `for (i = 0; i <= n; i++)` with `n == Integer.MAX_VALUE`
    // wraps `i` negative while the exit test keeps passing. No pre-header
    // guard can rescue it — every admissible `n` wraps.
    let mut l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Local(N),
    );
    l.bound_range = IntRange::constant(i32::MAX);
    assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::IvMayWrap));
    assert_eq!(
        refusal(&l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        RefusalReason::IvMayWrap
    );

    // MUST ACCEPT twin: the identical loop with a limit known to be small
    // needs no wrap guard at all.
    let mut ok = l.clone();
    ok.bound_range = IntRange::new(0, 1000);
    let proven = ok.iv_span(&RangeEnv::new()).unwrap();
    assert_eq!(proven.overflow, OverflowModel::NoWrapProven);
    assert_eq!(
        guards(&ok.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(N),
            1
        ))]
    );
}

#[test]
fn descending_loop_at_integer_min_value_must_refuse() {
    // MUST REFUSE: `for (i = ?; i >= n; i--)` with `n == Integer.MIN_VALUE`
    // underflows on the step that follows the last executed iteration.
    let mut l = mk(
        IntRange::unknown(),
        Stride::Const(-1),
        ExitCmp::Lt,
        BoundSource::Local(N),
    );
    l.bound_range = IntRange::constant(i32::MIN);
    assert_eq!(l.iv_span(&RangeEnv::new()), Err(RefusalReason::IvMayWrap));
    // MUST ACCEPT twin.
    l.bound_range = IntRange::new(0, 10);
    assert!(l.iv_span(&RangeEnv::new()).is_ok());
}

#[test]
fn index_expression_overflow_is_refused_not_wrapped() {
    // MUST REFUSE: `a[2 * i]` where `i` can reach `Integer.MAX_VALUE - 1`.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let doubled = IndexExpr {
        iv_local: IV,
        scale: 2,
        offset: 0,
    };
    assert_eq!(
        l.index_span(&doubled, &RangeEnv::new()),
        Err(RefusalReason::IndexMayWrap)
    );
    // MUST ACCEPT twin: a limit small enough that `2 * i` cannot wrap.
    let mut small = l.clone();
    small.bound_range = IntRange::new(0, 100);
    let span = small.index_span(&doubled, &RangeEnv::new()).unwrap();
    assert_eq!(span.span.numeric, IntRange::new(0, 198));
    assert_eq!(
        guards(&small.prove_index_in_bounds(
            &doubled,
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        [PreheaderGuard::LengthAtLeast(SymBound::constant(199))]
    );
}

#[test]
fn shifted_index_moves_the_length_requirement_by_the_same_amount() {
    // `for (i = 0; i < n; i++) a[i + 1]` needs `length >= n + 1`.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(
        guards(&l.prove_index_in_bounds(
            &IndexExpr::shifted(IV, 1),
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(N),
            1
        ))]
    );
    // A negative displacement can push the index below the array base, so
    // the low half stops being free.
    let p = l.prove_index_in_bounds(
        &IndexExpr::shifted(IV, -1),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(refusal(&p), RefusalReason::IndexMayBeNegative);
}

#[test]
fn an_index_on_another_local_is_not_this_loops_business() {
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(
        l.index_span(&IndexExpr::identity(9), &RangeEnv::new()),
        Err(RefusalReason::NotTheInductionVariable)
    );
}

// -- runtime strides ----------------------------------------------------

#[test]
fn a_runtime_stride_is_admitted_only_behind_a_sign_and_magnitude_guard() {
    // MUST ACCEPT (guarded): `for (j = i; j < n; j += step)` — the Sieve
    // inner loop shape.
    let l = mk(
        IntRange::unknown(),
        Stride::Variable(4),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p),
        [
            PreheaderGuard::StrideInRange {
                local: 4,
                headroom: bound_term(BoundSource::Local(N), -1),
            },
            PreheaderGuard::NonNegative(entry_term(0)),
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
        ]
    );
    assert_eq!(l.direction(), Direction::Unknown);

    // MUST REFUSE: the same runtime stride under a decreasing exit test.
    // A guard proving `step >= 0` would contradict the direction, and a
    // negative-step guard has no upper witness to hang the length on.
    let down = mk(
        IntRange::unknown(),
        Stride::Variable(4),
        ExitCmp::Le,
        BoundSource::Const(0),
    );
    assert_eq!(
        down.iv_span(&RangeEnv::new()),
        Err(RefusalReason::UnknownStride)
    );
}

// -- loop form ----------------------------------------------------------

#[test]
fn a_post_tested_loop_folds_its_untested_first_iteration_into_the_span() {
    // MUST REFUSE: `do { a[i] } while (i++ < n)` with an unknown entry
    // value — the first access is not covered by any test.
    let mut l = mk(
        IntRange::unknown(),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    l.form = LoopForm::PostTested;
    assert_eq!(
        l.iv_span(&RangeEnv::new()),
        Err(RefusalReason::UnboundedEntry)
    );
    // MUST ACCEPT twin: a known entry value gives the extra witness.
    l.iv.init = IntRange::constant(0);
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    assert_eq!(
        guards(&p),
        [
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
            // The untested first iteration indexes with the entry value —
            // known to be 0 here, so the length must simply be non-empty.
            PreheaderGuard::LengthAtLeast(SymBound::constant(1)),
        ]
    );
}

// -- trip counts --------------------------------------------------------

#[test]
fn trip_counts_cover_both_inclusivities_and_both_directions() {
    let env = RangeEnv::new();
    let up = |init: i32, bound: i32, stride: i32, cmp: ExitCmp| {
        mk(
            IntRange::constant(init),
            Stride::Const(stride),
            cmp,
            BoundSource::Const(bound),
        )
        .trip_count(&env)
        .and_then(|t| t.exact())
    };
    assert_eq!(up(0, 10, 1, ExitCmp::Ge), Some(10)); // i < 10
    assert_eq!(up(0, 10, 1, ExitCmp::Gt), Some(11)); // i <= 10
    assert_eq!(up(0, 10, 3, ExitCmp::Ge), Some(4)); // 0,3,6,9
    assert_eq!(up(10, 0, -1, ExitCmp::Le), Some(10)); // i > 0
    assert_eq!(up(10, 0, -1, ExitCmp::Lt), Some(11)); // i >= 0
}

#[test]
fn a_zero_trip_loop_is_proved_zero_trip_and_needs_no_check() {
    // MUST ACCEPT (trivially): `for (i = 5; i < 5; i++) a[i]` never runs,
    // so there is no access to guard.
    let l = mk(
        IntRange::constant(5),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Const(5),
    );
    let tc = l.trip_count(&RangeEnv::new()).unwrap();
    assert!(tc.is_zero());
    assert!(l.iv_span(&RangeEnv::new()).unwrap().span.numeric.is_empty());
    assert_eq!(
        l.prove_index_in_bounds(
            &IndexExpr::identity(IV),
            IntRange::constant(0),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    );
    // Twin: one more element of range and the loop really does run.
    let runs = mk(
        IntRange::constant(4),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Const(5),
    );
    assert_eq!(
        runs.trip_count(&RangeEnv::new()).and_then(|t| t.exact()),
        Some(1)
    );
}

#[test]
fn a_loop_that_can_wrap_has_no_finite_maximum_trip_count() {
    let env = RangeEnv::new();
    // MUST REFUSE a finite max: `for (i = 0; i <= MAX; i++)` never ends.
    let forever = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Const(i32::MAX),
    );
    let t = forever.trip_count(&env).expect("min survives a wrap");
    assert_eq!(t.max, u64::MAX);
    assert_eq!(t.exact(), None);
    assert_eq!(t.min, i32::MAX as u64 + 1);
    // MUST ACCEPT twin: one below the edge and the count is exact again.
    let ends = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Const(i32::MAX - 1),
    );
    assert_eq!(
        ends.trip_count(&env).and_then(|t| t.exact()),
        Some(i32::MAX as u64)
    );
    // Descending, the same edge at MIN.
    let down = mk(
        IntRange::constant(0),
        Stride::Const(-2),
        ExitCmp::Lt,
        BoundSource::Const(i32::MIN + 1),
    );
    assert_eq!(down.trip_count(&env).map(|t| t.max), Some(u64::MAX));
}

#[test]
fn a_limit_the_body_rewrites_has_no_trip_count() {
    let mut l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert!(l.trip_count(&RangeEnv::new()).is_some());
    l.modified_locals |= 1u64 << N;
    assert_eq!(l.trip_count(&RangeEnv::new()), None);
}

#[test]
fn an_unknown_entry_value_gives_a_trip_count_range_not_a_number() {
    let mut l = mk(
        IntRange::new(0, 4),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    l.bound_range = IntRange::new(8, 10);
    let tc = l.trip_count(&RangeEnv::new()).unwrap();
    assert_eq!(tc.min, 4); // init 4, bound 8 → 4,5,6,7
    assert_eq!(tc.max, 10); // init 0, bound 10 → 0..9
    assert_eq!(tc.exact(), None);
}

#[test]
fn a_limit_that_is_the_guarded_arrays_own_length_needs_no_guard() {
    // for (i = 0; i < a.length; i++) a[i] — the shape `bce.rs` spends a
    // whole-method `arraylength` provenance pass to prove.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::ArrayLength(0),
    );
    assert_eq!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(IV),
            Some(0),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    );
    // MUST NOT generalise to another array: `out[i]` under a guard on
    // `a.length` keeps its check — that is the multi-array out-of-bounds
    // store the provenance pass exists to prevent.
    assert_eq!(
        guards(&l.prove_index_in_bounds_of(
            &IndexExpr::identity(IV),
            Some(5),
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::ArrayLength(0),
            0
        ))]
    );
    // Inclusive over the same array is a real off-by-one: it keeps a guard
    // (one that can never pass) rather than inheriting the tautology.
    let incl = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::ArrayLength(0),
    );
    assert_eq!(
        guards(&incl.prove_index_in_bounds_of(
            &IndexExpr::identity(IV),
            Some(0),
            IntRange::array_length(),
            &RangeEnv::new()
        )),
        [
            PreheaderGuard::AtMost {
                term: bound_term(BoundSource::ArrayLength(0), 0),
                limit: i32::MAX - 1,
            },
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::ArrayLength(0), 1)),
        ]
    );
}

// -- nesting ------------------------------------------------------------

#[test]
fn an_inner_bound_that_is_the_outer_iv_inherits_the_outer_range() {
    // for (i = 0; i < n; i++)
    //     for (j = 0; j <= i; j++) a[j]
    let outer = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let inner = CountedLoop {
        header_pc: 10,
        back_edge_pc: 20,
        iv: AffineIv {
            local: 3,
            init: IntRange::constant(0),
            stride: Stride::Const(1),
        },
        cmp: ExitCmp::Gt, // inclusive: `j <= i`
        bound: BoundSource::Local(IV),
        bound_range: IntRange::unknown(),
        form: LoopForm::PreTested,
        modified_locals: 1u64 << 3,
        heap_stable: true,
        has_other_exit: false,
    };

    // Without the nest, the inclusive comparator needs the
    // `i != Integer.MAX_VALUE` wrap guard.
    let bare = inner.iv_span(&RangeEnv::new()).unwrap();
    assert_eq!(bare.overflow, OverflowModel::NoWrapGuarded);
    assert_eq!(
        bare.guards,
        vec![PreheaderGuard::AtMost {
            term: bound_term(BoundSource::Local(IV), 0),
            limit: i32::MAX - 1,
        }]
    );

    // Inside the nest, the outer loop's proven range for `i` is
    // `[0, MAX - 1]`, which discharges the wrap obligation statically.
    let env = RangeEnv::new().with_local(N, IntRange::unknown());
    let nested_env = env.with_loop_iv(&outer);
    assert_eq!(nested_env.local(IV), IntRange::new(0, i32::MAX - 1));
    let nested = inner.iv_span(&nested_env).unwrap();
    assert_eq!(nested.overflow, OverflowModel::NoWrapProven);
    assert!(nested.guards.is_empty());
    // The inner index still needs `length > i`, which is the whole point
    // of an inclusive inner bound.
    assert_eq!(
        guards(&inner.prove_index_in_bounds(
            &IndexExpr::identity(3),
            IntRange::array_length(),
            &nested_env
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(IV),
            1
        ))]
    );
}

#[test]
fn a_bounded_outer_range_makes_the_inner_loop_static() {
    // for (i = 0; i < 8; i++) for (j = 0; j <= i; j++) a[j], a.length 16.
    let outer = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Const(8),
    );
    let inner = CountedLoop {
        header_pc: 10,
        back_edge_pc: 20,
        iv: AffineIv {
            local: 3,
            init: IntRange::constant(0),
            stride: Stride::Const(1),
        },
        cmp: ExitCmp::Gt,
        bound: BoundSource::Local(IV),
        bound_range: IntRange::unknown(),
        form: LoopForm::PreTested,
        modified_locals: 1u64 << 3,
        heap_stable: true,
        has_other_exit: false,
    };
    let env = RangeEnv::new().with_loop_iv(&outer);
    assert_eq!(env.local(IV), IntRange::new(0, 7));
    assert_eq!(
        inner.prove_index_in_bounds(&IndexExpr::identity(3), IntRange::constant(16), &env),
        BoundsProof::Static
    );
}

// -- minimum trip count -------------------------------------------------

fn tc_guards(p: &TripCountProof) -> Vec<PreheaderGuard> {
    match p {
        TripCountProof::Guarded(g) => g.clone(),
        other => panic!("expected Guarded, got {other:?}"),
    }
}

fn tc_refusal(p: &TripCountProof) -> RefusalReason {
    match p {
        TripCountProof::Refused(r) => *r,
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn an_unknown_limit_yields_a_trip_count_guard_instead_of_a_refusal() {
    // for (i = 0; i < n; i++) — the commonest loop in Java, and the one
    // the vectorization gate reported as its single largest refusal source.
    let env = RangeEnv::new();
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    // The fact that made every profitability floor refuse: nothing at
    // compile time rules out a zero-trip loop.
    assert_eq!(l.trip_count(&env).unwrap().min, 0);

    // `trip >= 4` is now one pre-header compare: `n >= 4`. The exclusive
    // comparator's -1 and the "+1 because the entry value is itself an
    // executed iteration" cancel exactly, so the witness is bare `n`.
    assert_eq!(
        tc_guards(&l.prove_trip_count_at_least(4, &env)),
        [PreheaderGuard::TripCountAtLeast {
            term: bound_term(BoundSource::Local(N), 0),
            minimum: 4,
        }]
    );

    // The witness folds in the entry value and the comparator, so a
    // consumer never has to know either. `for (i = 3; i <= n; i++)` runs
    // `n - 2` times, so `trip >= 4` is `n - 2 >= 4`.
    let shifted = mk(
        IntRange::constant(3),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Local(N),
    );
    assert_eq!(
        tc_guards(&shifted.prove_trip_count_at_least(4, &env)),
        [PreheaderGuard::TripCountAtLeast {
            term: bound_term(BoundSource::Local(N), -2),
            minimum: 4,
        }]
    );

    // A partly-known entry value uses its HIGHEST admissible value — the
    // fewest iterations the loop can run. Conservative, never optimistic.
    let ranged = mk(
        IntRange::new(0, 5),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(
        tc_guards(&ranged.prove_trip_count_at_least(2, &env)),
        [PreheaderGuard::TripCountAtLeast {
            term: bound_term(BoundSource::Local(N), -5),
            minimum: 2,
        }]
    );
}

#[test]
fn a_provable_trip_count_emits_no_guard_at_all() {
    let env = RangeEnv::new();
    // for (i = 0; i < 16; i++) — 16 trips, known at compile time.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Const(16),
    );
    assert_eq!(l.trip_count(&env).and_then(|t| t.exact()), Some(16));
    for minimum in [0u64, 1, 2, 4, 8, 16] {
        assert_eq!(
            l.prove_trip_count_at_least(minimum, &env),
            TripCountProof::Static,
            "trip >= {minimum} is a compile-time fact here"
        );
    }
    // MUST REFUSE twin: a demand the loop provably cannot meet is a
    // refusal, not a guard that always fails.
    assert_eq!(
        tc_refusal(&l.prove_trip_count_at_least(17, &env)),
        RefusalReason::UnusableBound
    );

    // A runtime limit whose range is known also settles statically.
    let mut ranged = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    ranged.bound_range = IntRange::new(8, 10);
    assert_eq!(
        ranged.prove_trip_count_at_least(8, &env),
        TripCountProof::Static
    );
    // One more than the proven floor, and it becomes a runtime question.
    assert_eq!(
        tc_guards(&ranged.prove_trip_count_at_least(9, &env)),
        [PreheaderGuard::TripCountAtLeast {
            term: bound_term(BoundSource::Local(N), 0),
            minimum: 9,
        }]
    );
}

#[test]
fn a_trip_count_minimum_is_refused_where_no_witness_is_expressible() {
    let env = RangeEnv::new();

    // Non-unit stride: the count needs a division `SymBound` cannot spell.
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(0),
                Stride::Const(4),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::UnsupportedLoopForm
    );
    // Decreasing: the count is `entry - limit + 1`, and `SymBound` cannot
    // negate its base term.
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(100),
                Stride::Const(-1),
                ExitCmp::Le,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::UnsupportedLoopForm
    );
    // Unknown entry value: nothing to subtract the limit from.
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::unknown(),
                Stride::Const(1),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::UnboundedEntry
    );
    // Runtime stride, zero stride, direction mismatch — the same verdicts
    // `iv_span` gives, so the two proofs never disagree about the cause.
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(0),
                Stride::Variable(4),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::UnknownStride
    );
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(0),
                Stride::Const(0),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::ZeroStride
    );
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(0),
                Stride::Const(-1),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(4, &env)
        ),
        RefusalReason::DirectionMismatch
    );
    // A post-tested loop's first iteration is unconditional; `trip_count`
    // refuses it and so does this.
    let mut post = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    post.form = LoopForm::PostTested;
    assert_eq!(
        tc_refusal(&post.prove_trip_count_at_least(4, &env)),
        RefusalReason::UnsupportedLoopForm
    );
    // A limit the body can change would make the pre-header check stale.
    let mut mutable_bound = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    mutable_bound.modified_locals |= 1u64 << N;
    assert_eq!(
        tc_refusal(&mutable_bound.prove_trip_count_at_least(4, &env)),
        RefusalReason::BoundNotInvariant
    );
    // "At least zero times" is not a claim about a loop, so it holds even
    // for the ones every other verdict refuses.
    assert_eq!(
        post.prove_trip_count_at_least(0, &env),
        TripCountProof::Static
    );
    // A demand no `int` trip count could reach.
    assert_eq!(
        tc_refusal(
            &mk(
                IntRange::constant(0),
                Stride::Const(1),
                ExitCmp::Ge,
                BoundSource::Local(N),
            )
            .prove_trip_count_at_least(u64::from(u32::MAX) + 1, &env)
        ),
        RefusalReason::UnusableBound
    );
}

#[test]
fn the_new_guard_shape_never_leaks_into_an_existing_proof() {
    // The whole point of adding a variant is that no existing guard list
    // changes. These are the exact lists the bounds/span tests above
    // assert, restated here so a future producer that starts attaching a
    // trip-count obligation to `iv_span` fails one focused test.
    let env = RangeEnv::new();
    let unknown_entry = mk(
        IntRange::unknown(),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    let p = unknown_entry.prove_index_in_bounds(
        &IndexExpr::identity(IV),
        IntRange::array_length(),
        &env,
    );
    assert_eq!(
        guards(&p),
        [
            PreheaderGuard::NonNegative(entry_term(0)),
            PreheaderGuard::LengthAtLeast(bound_term(BoundSource::Local(N), 0)),
        ]
    );

    let inclusive = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Gt,
        BoundSource::Local(N),
    );
    assert_eq!(
        inclusive.iv_span(&env).unwrap().guards,
        [PreheaderGuard::AtMost {
            term: bound_term(BoundSource::Local(N), 0),
            limit: i32::MAX - 1,
        }]
    );

    let variable_stride = mk(
        IntRange::constant(0),
        Stride::Variable(4),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(
        variable_stride.iv_span(&env).unwrap().guards,
        [PreheaderGuard::StrideInRange {
            local: 4,
            headroom: bound_term(BoundSource::Local(N), -1),
        }]
    );

    // No proof in this module mints the new shape; only the explicit
    // trip-count request does.
    for l in [&unknown_entry, &inclusive, &variable_stride] {
        let mut every: Vec<PreheaderGuard> =
            l.iv_span(&env).map(|p| p.guards).unwrap_or_default();
        if let BoundsProof::Guarded(g) =
            l.prove_index_in_bounds(&IndexExpr::identity(IV), IntRange::array_length(), &env)
        {
            every.extend(g);
        }
        assert!(
            !every
                .iter()
                .any(|g| matches!(g, PreheaderGuard::TripCountAtLeast { .. })),
            "a bounds/span proof minted a trip-count guard: {every:?}"
        );
    }
}

// -- naming the guarded array -------------------------------------------

#[test]
fn an_array_length_expression_discharges_the_same_tautology_as_a_slot() {
    let env = RangeEnv::new();
    // for (i = 0; i < a.length; i++) a[i], with `a` in local 0.
    let l = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::ArrayLength(0),
    );
    let idx = IndexExpr::identity(IV);
    // The slot-keyed form is exactly the expression-keyed form applied to
    // `ArrayLength(slot)` — including the `None` case, which is what keeps
    // every existing caller byte-identical.
    for slot in [None, Some(0usize), Some(5usize)] {
        let denoted = slot.map(BoundSource::ArrayLength);
        assert_eq!(
            l.prove_index_in_bounds_of(&idx, slot, IntRange::array_length(), &env),
            l.prove_index_in_bounds_of_array(
                &idx,
                denoted.as_ref(),
                IntRange::array_length(),
                &env
            ),
            "slot {slot:?} must delegate unchanged"
        );
    }

    // The reason the overload exists: a limit that IS this array's length
    // but is spelled as something other than `ArrayLength(slot)` — a local
    // the caller has proven holds `a.length`, or an IR node. The slot form
    // cannot express it and pays a guard; the expression form discharges it.
    let via_local = mk(
        IntRange::constant(0),
        Stride::Const(1),
        ExitCmp::Ge,
        BoundSource::Local(N),
    );
    assert_eq!(
        guards(&via_local.prove_index_in_bounds_of(
            &idx,
            Some(0),
            IntRange::array_length(),
            &env
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(N),
            0
        ))]
    );
    assert_eq!(
        via_local.prove_index_in_bounds_of_array(
            &idx,
            Some(&BoundSource::Local(N)),
            IntRange::array_length(),
            &env
        ),
        BoundsProof::Static
    );
    // MUST NOT generalise: naming a limit that is *not* this array's
    // length keeps the guard, which is the multi-array out-of-bounds store
    // the whole shortcut is fenced against.
    assert_eq!(
        guards(&via_local.prove_index_in_bounds_of_array(
            &idx,
            Some(&BoundSource::Local(N + 1)),
            IntRange::array_length(),
            &env
        )),
        [PreheaderGuard::LengthAtLeast(bound_term(
            BoundSource::Local(N),
            0
        ))]
    );
}
