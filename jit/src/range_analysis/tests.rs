use super::*;
use crate::ir::{Inputs, UseLists};
use crate::scev::IntRange;

const W: IntWidth = IntWidth::W32;
const L: IntWidth = IntWidth::W64;

fn r(lo: i64, hi: i64) -> Range {
    Range::exact(W, lo, hi)
}

// ---- the overflow argument, first ------------------------------------

/// **The mistake this module exists to prevent.** In mathematical
/// integers `i + 1 > i`. On a 32-bit machine integer it is false exactly
/// once, at `Integer.MAX_VALUE`, and an analysis that misses that deletes
/// a real bounds check.
///
/// The lattice must therefore answer `top` — not `[MIN+1, MAX+1]`, and not
/// a silently clamped `[MIN+1, MAX]` — for `unknown_int + 1`.
#[test]
fn add_one_to_top_is_top_not_a_shifted_interval() {
    let top = Range::top(W);
    let sum = top.add(Range::constant(W, 1));
    assert!(sum.is_top(), "unknown + 1 must be top, got {sum:?}");
    assert!(
        sum.contains_all(Range::constant(W, i32::MIN as i64)),
        "the wrapped result IS Integer.MIN_VALUE; the range must admit it"
    );
    // And the exact form refuses rather than degrading.
    assert_eq!(top.add_no_wrap(Range::constant(W, 1)), None);
}

/// The same trap one step further out: a range that *ends* at
/// `Integer.MAX_VALUE` still wraps.
#[test]
fn add_wraps_at_the_upper_endpoint_only() {
    // [0, MAX-1] + 1 = [1, MAX] — representable, so exact.
    let ok = r(0, i32::MAX as i64 - 1).add_no_wrap(Range::constant(W, 1));
    assert_eq!(ok, Some(r(1, i32::MAX as i64)));
    // [0, MAX] + 1 is not.
    assert_eq!(
        r(0, i32::MAX as i64).add_no_wrap(Range::constant(W, 1)),
        None
    );
    assert!(r(0, i32::MAX as i64).add(Range::constant(W, 1)).is_top());
}

/// `-Integer.MIN_VALUE` is `Integer.MIN_VALUE`, not `2^31`.
#[test]
fn neg_of_min_wraps() {
    assert_eq!(Range::constant(W, i32::MIN as i64).neg_no_wrap(), None);
    assert!(Range::constant(W, i32::MIN as i64).neg().is_top());
    assert_eq!(r(-5, 5).neg(), r(-5, 5));
    assert_eq!(r(1, 5).neg(), r(-5, -1));
}

/// A 64-bit range is not an `int` range, and the two must never be mixed
/// silently: a cross-width join degrades to top rather than producing an
/// interval in the wrong domain.
#[test]
fn width_mismatch_degrades_to_top() {
    let i = r(0, 10);
    let l = Range::exact(L, 0, 10);
    assert!(i.join(l).is_top());
    assert!(i.add(l).is_top());
    assert_eq!(i.meet(l), i, "meet cannot narrow across widths");
    assert!(!i.contains_all(l));
    // 64-bit ranges do not convert into the `int`-only scev lattice.
    assert_eq!(l.to_scev(), None);
}

// ---- lattice laws ----------------------------------------------------

#[test]
fn join_is_commutative_associative_and_absorbing() {
    let a = r(-3, 4);
    let b = r(2, 9);
    let c = r(-100, -50);
    assert_eq!(a.join(b), b.join(a));
    assert_eq!(a.join(b).join(c), a.join(b.join(c)));
    assert_eq!(a.join(a), a);
    assert_eq!(a.join(Range::empty(W)), a);
    assert!(a.join(Range::top(W)).is_top());
    assert_eq!(a.join(b), r(-3, 9));
}

#[test]
fn meet_is_commutative_associative_and_can_contradict() {
    let a = r(-3, 4);
    let b = r(2, 9);
    assert_eq!(a.meet(b), b.meet(a));
    assert_eq!(a.meet(b), r(2, 4));
    assert_eq!(a.meet(a), a);
    assert_eq!(a.meet(Range::top(W)), a);
    assert!(a.meet(Range::empty(W)).is_empty());
    // A genuine contradiction is bottom, never a pick-one-side.
    assert!(r(0, 3).meet(r(7, 9)).is_empty());
}

/// Monotonicity of join and meet, checked exhaustively over a small grid.
/// `a ⊑ a'` must imply `a ⊔ b ⊑ a' ⊔ b` and `a ⊓ b ⊑ a' ⊓ b`.
#[test]
fn join_and_meet_are_monotone() {
    let pts: Vec<Range> = {
        let mut v = vec![Range::empty(W)];
        for lo in -3..=3i64 {
            for hi in lo..=3i64 {
                v.push(r(lo, hi));
            }
        }
        v
    };
    for a in &pts {
        for ap in &pts {
            if !ap.contains_all(*a) {
                continue; // only pairs with a ⊑ a'
            }
            for b in &pts {
                assert!(
                    ap.join(*b).contains_all(a.join(*b)),
                    "join not monotone: {a:?} ⊑ {ap:?} but {:?} ⋢ {:?}",
                    a.join(*b),
                    ap.join(*b)
                );
                assert!(
                    ap.meet(*b).contains_all(a.meet(*b)),
                    "meet not monotone: {a:?} ⊑ {ap:?} but {:?} ⋢ {:?}",
                    a.meet(*b),
                    ap.meet(*b)
                );
            }
        }
    }
}

/// Arithmetic must be monotone too, or the fixpoint's "inputs only grow"
/// termination argument is worthless.
#[test]
fn add_is_monotone_and_over_approximates_pointwise() {
    let small = r(0, 2);
    let big = r(-1, 5);
    assert!(big.contains_all(small));
    assert!(big.add(r(1, 1)).contains_all(small.add(r(1, 1))));
    // Pointwise soundness: every concrete sum is admitted.
    let sum = r(3, 7).add(r(-2, 4));
    for x in 3..=7i64 {
        for y in -2..=4i64 {
            assert!(
                sum.contains_all(Range::constant(W, x + y)),
                "{x} + {y} not admitted by {sum:?}"
            );
        }
    }
}

#[test]
fn widen_throws_outward_endpoints_to_the_extremes() {
    let prev = r(0, 10);
    // Growing upward only: the lower endpoint stays put.
    let w1 = prev.widen(r(0, 11));
    assert_eq!(w1, r(0, i32::MAX as i64));
    // Growing downward only.
    let w2 = prev.widen(r(-1, 10));
    assert_eq!(w2, r(i32::MIN as i64, 10));
    // Not growing at all: unchanged.
    assert_eq!(prev.widen(r(2, 8)), prev);
    // Bottom is the identity in both directions.
    assert_eq!(Range::empty(W).widen(prev), prev);
    assert_eq!(prev.widen(Range::empty(W)), prev);
}

/// Widening must saturate: repeatedly widening against a range that keeps
/// growing has to reach top in a bounded number of steps, or the fixpoint
/// never terminates.
#[test]
fn widen_saturates_in_bounded_steps() {
    let mut acc = r(0, 0);
    let mut steps = 0;
    for i in 1..100i64 {
        let next = acc.join(r(-i, i));
        let widened = acc.widen(next);
        steps += 1;
        if widened == acc {
            break;
        }
        acc = widened;
        if acc.is_top() {
            break;
        }
    }
    assert!(acc.is_top(), "widening did not saturate, ended at {acc:?}");
    assert!(steps <= 4, "widening took {steps} steps to saturate");
}

// ---- the narrowing transfer functions --------------------------------

/// `hash & (n - 1)`: the mask is non-negative, so the result is in
/// `[0, mask]` no matter how wild the hash is. This is the fact that makes
/// a hash-table index provable.
#[test]
fn and_with_a_non_negative_mask_bounds_the_result() {
    let hash = Range::top(W);
    assert_eq!(hash.and(Range::constant(W, 15)), r(0, 15));
    assert_eq!(Range::constant(W, 15).and(hash), r(0, 15));
    // Both non-negative: the tighter of the two upper bounds wins.
    assert_eq!(r(0, 100).and(r(0, 7)), r(0, 7));
    // Both possibly negative: nothing is known.
    assert!(hash.and(Range::top(W)).is_top());
    // Both entirely negative: the sign bit survives.
    let neg = r(i32::MIN as i64, -1);
    assert_eq!(neg.and(neg), neg);
}

#[test]
fn or_and_xor_are_modelled_only_for_non_negative_operands() {
    assert_eq!(r(0, 5).or(r(0, 9)), r(0, 15));
    assert_eq!(r(4, 5).or(r(2, 3)), r(4, 7));
    assert_eq!(r(0, 5).xor(r(0, 9)), r(0, 15));
    assert!(r(-1, 5).or(r(0, 9)).is_top());
    assert!(r(-1, 5).xor(r(0, 9)).is_top());
}

#[test]
fn shifts_respect_the_jvms_count_mask() {
    // `x << 32` shifts an int by 0, not by 32.
    assert_eq!(r(1, 1).shl(Range::constant(W, 32)), r(1, 1));
    assert_eq!(r(1, 1).shl(Range::constant(W, 33)), r(2, 2));
    // A long shifts by the low 6 bits.
    let l1 = Range::constant(L, 1);
    assert_eq!(l1.shl(Range::constant(W, 64)), l1);
    assert_eq!(l1.shl(Range::constant(W, 65)), Range::constant(L, 2));
    // A shift that overflows the width wraps: top.
    assert!(r(1, 1).shl(Range::constant(W, 31)).is_top());
    // An unknown count proves nothing about a left shift.
    assert!(r(1, 1).shl(Range::top(W)).is_top());
}

#[test]
fn arithmetic_shift_right_never_wraps() {
    assert_eq!(r(-16, 16).shr(Range::constant(W, 2)), r(-4, 4));
    assert_eq!(r(-1, -1).shr(Range::constant(W, 31)), r(-1, -1));
    // Unknown count: the value can only move toward 0 / -1.
    assert_eq!(r(5, 40).shr(Range::top(W)), r(0, 40));
    assert_eq!(r(-40, -5).shr(Range::top(W)), r(-40, -1));
    assert_eq!(r(-40, 40).shr(Range::top(W)), r(-40, 40));
}

#[test]
fn unsigned_shift_right_of_a_negative_value_is_non_negative() {
    // (-1 >>> 1) == 0x7FFFFFFF
    assert_eq!(
        Range::constant(W, -1).ushr(Range::constant(W, 1)),
        r(0, i32::MAX as i64)
    );
    // A non-negative shiftee keeps its exact bounds.
    assert_eq!(r(8, 16).ushr(Range::constant(W, 3)), r(1, 2));
    // Count 0 is the identity — including for a negative value, which is
    // why an unknown count must be top.
    assert_eq!(
        Range::constant(W, -1).ushr(Range::constant(W, 0)),
        Range::constant(W, -1)
    );
    assert!(Range::constant(W, -1).ushr(Range::top(W)).is_top());
}

#[test]
fn rem_follows_the_dividend_sign_and_bounds_by_the_divisor() {
    // A non-negative dividend gives a non-negative remainder.
    assert_eq!(Range::top(W).rem(Range::constant(W, 8)), r(-7, 7));
    assert_eq!(r(0, 1000).rem(Range::constant(W, 8)), r(0, 7));
    // `Integer.MIN_VALUE % -1` is 0 in Java, not an overflow.
    assert_eq!(
        Range::constant(W, i32::MIN as i64).rem(Range::constant(W, -1)),
        r(0, 0)
    );
    // Division by a divisor that can only be zero never produces a value.
    assert!(r(0, 5).rem(Range::constant(W, 0)).is_empty());
    // |a % b| <= |a| too.
    assert_eq!(r(0, 3).rem(Range::constant(W, 100)), r(0, 3));
}

#[test]
fn div_by_a_constant_is_exact_and_min_over_minus_one_wraps() {
    assert_eq!(r(-10, 10).div(Range::constant(W, 3)), r(-3, 3));
    assert_eq!(r(-10, 10).div(Range::constant(W, -3)), r(-3, 3));
    assert!(Range::constant(W, i32::MIN as i64)
        .div(Range::constant(W, -1))
        .is_top());
    assert!(r(0, 5).div(Range::constant(W, 0)).is_empty());
}

#[test]
fn conversions_truncate_rather_than_intersect() {
    // Already inside the target type: exact.
    assert_eq!(r(0, 100).i2b(), r(0, 100));
    // Outside it: the whole target type, NOT an intersection (`300 as byte`
    // is 44, which an intersection would have wrongly excluded).
    assert_eq!(r(0, 300).i2b(), r(-128, 127));
    assert!(r(0, 300).i2b().contains_all(Range::constant(W, 44)));
    assert_eq!(r(0, 300).i2c(), r(0, 300));
    assert_eq!(r(-1, 5).i2c(), r(0, 65535));
    assert_eq!(r(-1, 5).i2s(), r(-1, 5));
    // i2l is exact; l2i truncates.
    assert_eq!(r(-5, 5).i2l(), Range::exact(L, -5, 5));
    assert_eq!(Range::exact(L, -5, 5).l2i(), r(-5, 5));
    assert!(Range::exact(L, 0, 1i64 << 40).l2i().is_top());
}

/// Bottom is not non-negative, whatever the logic of vacuous truth says.
///
/// The one property `definitely_non_negative` exists for: a consumer turns
/// "every value is >= 0" into a deleted bounds check, and at bottom there
/// is no value to have been checked. The vacuous predicate this replaces
/// answered `true` here.
#[test]
fn bottom_is_not_definitely_non_negative() {
    assert!(!Range::empty(W).definitely_non_negative());
    assert!(Range::empty(W).is_empty());
    // …while a range that admits values and starts at or above zero is.
    assert!(r(0, 0).definitely_non_negative());
    assert!(r(1, 7).definitely_non_negative());
    assert!(!r(-1, 7).definitely_non_negative());
    assert!(!Range::top(W).definitely_non_negative());
}

#[test]
fn array_length_and_mem_kinds() {
    assert_eq!(Range::array_length(), r(0, i32::MAX as i64));
    assert!(Range::array_length().definitely_non_negative());
    assert_eq!(Range::of_mem_kind(MemKind::Byte), Some(r(-128, 127)));
    assert_eq!(Range::of_mem_kind(MemKind::Char), Some(r(0, 65535)));
    assert_eq!(Range::of_mem_kind(MemKind::Short), Some(r(-32768, 32767)));
    assert_eq!(Range::of_mem_kind(MemKind::Int), None);
    assert_eq!(Range::of_mem_kind(MemKind::Ref), None);
}

#[test]
fn scev_conversion_round_trips() {
    let a = r(-7, 19);
    let s = a.to_scev().expect("32-bit range converts");
    assert_eq!(Range::from_scev(s), a);
    assert!(Range::from_scev(IntRange::unknown()).is_top());
    assert!(Range::from_scev(IntRange::empty()).is_empty());
    assert_eq!(
        Range::array_length().to_scev(),
        Some(IntRange::array_length())
    );
}

// ---- the sea-of-nodes evaluator --------------------------------------

fn empty_graph() -> Graph {
    let mut g = Graph::empty(0);
    g.exit = 0;
    g
}

fn push(g: &mut Graph, op: Op, ty: IrType, inputs: Vec<NodeId>) -> NodeId {
    // Cast: dense ids assigned from 0, far below `NO_NODE`.
    let id = g.nodes.len() as NodeId;
    g.nodes.push(crate::ir::Node {
        op,
        ty,
        inputs: Inputs::from(inputs),
        bytecode_pc: None,
        frame_snapshot: None,
        volatile_access: false,
    });
    id
}

#[test]
fn graph_folds_constants_and_narrows_array_length() {
    let mut g = empty_graph();
    let c3 = push(&mut g, Op::Const(3), IrType::Int, vec![]);
    let c4 = push(&mut g, Op::Const(4), IrType::Int, vec![]);
    let sum = push(&mut g, Op::Add, IrType::Int, vec![c3, c4]);
    let len = push(&mut g, Op::ArrayLength, IrType::Int, vec![]);
    let masked = push(&mut g, Op::And, IrType::Int, vec![len, c3]);
    let ranges = analyze_graph(&g);
    assert!(ranges.converged());
    assert_eq!(ranges.of(sum).as_constant(), Some(7));
    assert_eq!(ranges.of(len), r(0, i32::MAX as i64));
    assert_eq!(ranges.of(masked), r(0, 3));
}

#[test]
fn graph_leaves_unmodelled_producers_at_top() {
    let mut g = empty_graph();
    let p = push(&mut g, Op::Param(0), IrType::Int, vec![]);
    let c1 = push(&mut g, Op::Const(1), IrType::Int, vec![]);
    let sum = push(&mut g, Op::Add, IrType::Int, vec![p, c1]);
    let ranges = analyze_graph(&g);
    assert!(ranges.converged());
    assert!(ranges.of(p).is_top(), "a parameter is an unknown");
    assert!(
        ranges.of(sum).is_top(),
        "unknown + 1 stays unknown — the wrap case again"
    );
}

#[test]
fn graph_narrows_a_byte_array_load() {
    let mut g = empty_graph();
    let load = push(&mut g, Op::ArrayLoad(MemKind::Byte), IrType::Int, vec![]);
    let cload = push(&mut g, Op::Load(MemKind::Char), IrType::Int, vec![]);
    let iload = push(&mut g, Op::ArrayLoad(MemKind::Int), IrType::Int, vec![]);
    let ranges = analyze_graph(&g);
    assert_eq!(ranges.of(load), r(-128, 127));
    assert_eq!(ranges.of(cload), r(0, 65535));
    assert!(ranges.of(iload).is_top());
}

/// A φ whose inputs disagree is their hull; a φ feeding itself through an
/// increment is the cyclic case widening exists for. The point of the test
/// is that it **terminates** and lands on a sound answer, not that the
/// answer is tight.
#[test]
fn graph_phi_cycle_terminates_and_stays_sound() {
    let mut g = empty_graph();
    let region = push(&mut g, Op::Region, IrType::Control, vec![]);
    let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
    let one = push(&mut g, Op::Const(1), IrType::Int, vec![]);
    // phi = φ(0, phi + 1) — the id of the Add is not known yet, so patch
    // the edge after both exist.
    let phi = push(&mut g, Op::Phi, IrType::Int, vec![region, zero, zero]);
    let inc = push(&mut g, Op::Add, IrType::Int, vec![phi, one]);
    g.nodes[phi as usize].inputs = Inputs::from(vec![region, zero, inc]);

    let ranges = analyze_graph(&g);
    assert!(ranges.converged(), "the widened φ cycle must settle");
    let p = ranges.of(phi);
    assert!(
        p.contains_all(Range::constant(W, 0)),
        "0 is the entry value and must be admitted, got {p:?}"
    );
    assert!(
        p.contains_all(Range::constant(W, 1)),
        "1 is reachable after one increment, got {p:?}"
    );
    // Soundness demands the wrapped value be admitted too, because nothing
    // here bounds the trip count. Widening delivers exactly that.
    assert!(
        p.contains_all(Range::constant(W, i32::MIN as i64)),
        "an unbounded counter wraps; the range must admit it, got {p:?}"
    );
}

/// A φ with a merge edge only (no value inputs) and a φ with a dangling
/// value edge both have to answer something sound rather than panic.
#[test]
fn graph_tolerates_malformed_phis() {
    let mut g = empty_graph();
    let region = push(&mut g, Op::Region, IrType::Control, vec![]);
    let bare = push(&mut g, Op::Phi, IrType::Int, vec![region]);
    let dangling = push(
        &mut g,
        Op::Phi,
        IrType::Int,
        vec![region, crate::ir::NO_NODE],
    );
    let ranges = analyze_graph(&g);
    assert!(ranges.of(bare).is_empty(), "no inputs means no values");
    assert!(ranges.of(dangling).is_top(), "an unknown edge is unknown");
}

#[test]
fn graph_out_of_range_query_is_top() {
    let g = empty_graph();
    let ranges = analyze_graph(&g);
    assert_eq!(ranges.len(), 0);
    assert!(ranges.of(7).is_top());
}

// ---- B3: the narrowing primitives ------------------------------------

/// The plus/minus one at the extremes of the width.
///
/// `x < [MIN, MIN]` has no solution. Saying so must not be computed by
/// evaluating `MIN - 1`, which in a release build wraps silently to `MAX`
/// and inverts the bound into "x can be anything" -- the one arithmetic
/// slip in this family that turns a refusal into permission.
#[test]
fn narrowing_at_the_width_extremes_does_not_wrap() {
    for w in [W, L] {
        let bottom = Range::constant(w, w.min());
        let top_v = Range::constant(w, w.max());

        assert!(
            Range::top(w).narrow_lt(bottom).is_empty(),
            "nothing is below the smallest representable value ({w:?})",
        );
        assert!(
            Range::top(w).narrow_gt(top_v).is_empty(),
            "nothing is above the largest representable value ({w:?})",
        );
        assert_eq!(
            Range::top(w).narrow_le(bottom),
            bottom,
            "<= MIN leaves exactly MIN ({w:?})",
        );
        assert_eq!(
            Range::top(w).narrow_ge(top_v),
            top_v,
            ">= MAX leaves exactly MAX ({w:?})",
        );
    }
}

/// The direction rule: a bound is read off the FAR endpoint of the other
/// operand.
///
/// `x < [3, 9]` admits `x == 8`, because the comparison only has to hold
/// for SOME value the right-hand side can take. Reading the near endpoint
/// would give `[MIN, 2]` and drop five values that genuinely occur -- the
/// unsound direction, and the one that reads as a tighter, better analysis.
#[test]
fn a_bound_comes_from_the_far_endpoint_of_the_other_operand() {
    let x = Range::top(W);
    let other = r(3, 9);
    assert_eq!(x.narrow_lt(other), Range::exact(W, i32::MIN as i64, 8));
    assert_eq!(x.narrow_le(other), Range::exact(W, i32::MIN as i64, 9));
    assert_eq!(x.narrow_gt(other), Range::exact(W, 4, i32::MAX as i64));
    assert_eq!(x.narrow_ge(other), Range::exact(W, 3, i32::MAX as i64));
}

/// An interval lattice has no hole, so `!=` proves something only against a
/// constant sitting on an endpoint.
#[test]
fn inequality_only_moves_an_endpoint() {
    assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 0)), r(1, 10));
    assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 10)), r(0, 9));
    // A value in the middle cannot be excised.
    assert_eq!(r(0, 10).narrow_ne(Range::constant(W, 5)), r(0, 10));
    // A non-constant right-hand side proves nothing at all.
    assert_eq!(r(0, 10).narrow_ne(r(0, 0).join(r(3, 3))), r(0, 10));
    // The singleton that excludes itself is bottom, not the same singleton.
    assert!(r(7, 7).narrow_ne(Range::constant(W, 7)).is_empty());
}

/// Widths are not comparable across the lattice, and refusing to narrow is
/// the safe answer.
#[test]
fn a_width_mismatch_narrows_nothing() {
    let i = Range::top(W);
    let l = Range::constant(L, 0);
    assert_eq!(i.narrow_lt(l), i);
    assert_eq!(i.narrow_ge(l), i);
    assert_eq!(i.narrow_by(CmpOp::Gt, l), i);
}

/// `negate` and `swapped` are different functions, pinned here because
/// `apply_condition` picks between them and `Eq`/`Ne` are fixed points of
/// the second -- a test on equality alone would not notice them confused.
#[test]
fn negate_and_swapped_are_not_the_same_function() {
    assert_eq!(CmpOp::Lt.negate(), CmpOp::Ge);
    assert_eq!(CmpOp::Lt.swapped(), CmpOp::Gt);
    assert_eq!(CmpOp::Le.negate(), CmpOp::Gt);
    assert_eq!(CmpOp::Le.swapped(), CmpOp::Ge);
    for op in [CmpOp::Eq, CmpOp::Ne] {
        assert_eq!(op.swapped(), op, "{op:?} is its own mirror");
        assert_ne!(op.negate(), op, "{op:?} is not its own negation");
    }
    // And the property that makes `swapped` correct at all.
    for op in [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ] {
        assert_eq!(op.swapped().swapped(), op, "{op:?} mirrors back");
        assert_eq!(op.negate().negate(), op, "{op:?} negates back");
    }
}

// ---- B3: the graph walk ----------------------------------------------

/// `if (p >= 0) { use p }`, scheduled, then asked at each block.
///
/// Returns `(graph, schedule, param, true_block, false_block)`.
fn sign_guarded_graph() -> (Graph, Schedule, NodeId, usize, usize) {
    let mut g = empty_graph();
    let start = push(&mut g, Op::Start, IrType::Control, vec![]);
    g.entry = start;
    let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
    let p = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
    let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
    let cmp = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
    let iff = push(&mut g, Op::If, IrType::Control, vec![c0, cmp]);
    let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
    let f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
    g.exit = push(&mut g, Op::Return, IrType::Void, vec![t, p]);
    let sched = crate::ir_schedule::schedule(&g);
    let tb = sched.node_to_block[t as usize];
    let fb = sched.node_to_block[f as usize];
    (g, sched, p, tb, fb)
}

/// The headline: a value with no shape at all is narrowed by the edge.
#[test]
fn an_if_edge_narrows_its_operand_in_the_block_it_guards() {
    let (g, sched, p, tb, fb) = sign_guarded_graph();
    let pr = analyze_graph_pathwise(&g, &sched);
    assert!(pr.converged());

    assert!(
        pr.of(p).is_top(),
        "the whole-method answer for a bare Param is still top -- the \
         refinement must not leak back into it",
    );
    assert!(
        pr.at(p, tb).definitely_non_negative(),
        "on the true edge of `p >= 0`, p is non-negative",
    );
    assert_eq!(
        pr.at(p, fb),
        Range::exact(W, i32::MIN as i64, -1),
        "and on the false edge it is negative",
    );
    assert_eq!(pr.census().blocks_with_facts, 2);
}

/// An unknown block index, and a node nothing narrowed, both fall back to
/// the whole-method answer rather than to something invented.
#[test]
fn an_unknown_block_answers_the_whole_method_range() {
    let (g, sched, p, _tb, _fb) = sign_guarded_graph();
    let pr = analyze_graph_pathwise(&g, &sched);
    assert_eq!(pr.at(p, 9_999), pr.of(p));
    assert_eq!(pr.at(12_345, 0), pr.of(12_345));
}

/// A block a branch does not dominate must carry no fact from it. Here the
/// two arms of an `if` merge, and at the merge `p` can be anything again --
/// both a negative and a non-negative value reach it.
///
/// The mechanism is `entry_edge_condition`'s `Op::Proj` destructure: the
/// merge block is headed by `Op::Merge`, so it never reaches the fact-
/// recording path at all. It is NOT the `predecessors.len() == 1` test --
/// deleting that leaves this test passing, which is recorded at that
/// function rather than left for the next person to rediscover.
#[test]
fn a_merge_of_both_arms_carries_no_fact_from_either() {
    let mut g = empty_graph();
    let start = push(&mut g, Op::Start, IrType::Control, vec![]);
    g.entry = start;
    let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
    let p = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
    let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
    let cmp = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
    let iff = push(&mut g, Op::If, IrType::Control, vec![c0, cmp]);
    let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
    let f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
    let m = push(&mut g, Op::Merge, IrType::Control, vec![t, f]);
    g.exit = push(&mut g, Op::Return, IrType::Void, vec![m, p]);
    let sched = crate::ir_schedule::schedule(&g);
    let mb = sched.node_to_block[m as usize];

    let pr = analyze_graph_pathwise(&g, &sched);
    assert!(
        pr.at(p, mb).is_top(),
        "at a merge of both arms p is unconstrained again, got {:?}",
        pr.at(p, mb),
    );
}

/// A loop counter that starts negative must not be reported non-negative
/// inside the body.
///
/// `for (i = -5; i < n; i++)`: the body runs with `i == -5`, and the only
/// thing `i < n` proves about `i` there is an UPPER bound. Its lower bound
/// must still admit `-5`, or the consumer deletes a bounds check the first
/// iteration needs.
///
/// This pins the outcome, and it is the outcome `refine`'s phi exclusion
/// exists to protect -- but it does NOT pin that exclusion: removing the
/// exclusion leaves this passing, because recomputing the phi joins its
/// `init` input back in and `-5` survives anyway. Said plainly here because
/// the test was written expecting to catch that, and an A/B showed it does
/// not.
#[test]
fn a_loop_phi_keeps_a_lower_bound_that_admits_its_entry_value() {
    let mut g = empty_graph();
    let start = push(&mut g, Op::Start, IrType::Control, vec![]);
    g.entry = start;
    let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
    let n = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
    let init = push(&mut g, Op::Const(-5), IrType::Int, vec![]);
    let one = push(&mut g, Op::Const(1), IrType::Int, vec![]);
    // Region(entry, backedge); Phi(region, init, next)
    let region = push(&mut g, Op::Region, IrType::Control, vec![c0, c0]);
    let phi = push(&mut g, Op::Phi, IrType::Int, vec![region, init, 0]);
    let next = push(&mut g, Op::Add, IrType::Int, vec![phi, one]);
    g.nodes[phi as usize].inputs = Inputs::from(vec![region, init, next]);
    let cmp = push(&mut g, Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi, n]);
    let iff = push(&mut g, Op::If, IrType::Control, vec![region, cmp]);
    let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
    let _f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
    g.nodes[region as usize].inputs = Inputs::from(vec![c0, t]);
    g.exit = push(&mut g, Op::Return, IrType::Void, vec![t, phi]);
    let sched = crate::ir_schedule::schedule(&g);
    let tb = sched.node_to_block[t as usize];

    let pr = analyze_graph_pathwise(&g, &sched);
    let body = pr.at(phi, tb);
    assert!(
        !body.definitely_non_negative(),
        "the loop counter starts at -5 and the body runs with it; any \
         analysis answering non-negative here would delete a bounds check \
         the first iteration needs. Got {body:?}",
    );
    match body.lo() {
        None => {} // bottom is not a claim about a value; nothing to check
        Some(lo) => assert!(
            lo <= -5,
            "the phi's lower bound must still admit its entry value, got {lo}",
        ),
    }
}

/// A refinement result is never ABOVE the flow-insensitive answer.
///
/// The overlay is only ever allowed to narrow. An entry above the global
/// range would be a second, weaker opinion about the same value, and `at`
/// would then disagree with `of` in the permissive direction.
#[test]
fn every_narrowed_entry_is_below_the_whole_method_answer() {
    let (g, sched, _p, _tb, _fb) = sign_guarded_graph();
    let pr = analyze_graph_pathwise(&g, &sched);
    for b in 0..sched.blocks.len() {
        for id in 0..g.nodes.len() as NodeId {
            let global = pr.of(id);
            let local = pr.at(id, b);
            assert!(
                global.contains_all(local),
                "node {id} in block {b}: {local:?} is not inside {global:?}",
            );
        }
    }
}

/// `if (p_i >= 0)` for `i` in `0..n`, each false edge to one exit merge.
/// Returns `(graph, schedule, params, true_blocks, false_blocks)`.
fn guard_chain(n: usize) -> (Graph, Schedule, Vec<NodeId>, Vec<usize>, Vec<usize>) {
    let mut g = empty_graph();
    let start = push(&mut g, Op::Start, IrType::Control, vec![]);
    g.entry = start;
    let mut ctrl = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
    let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
    let (mut ps, mut ts, mut fs) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..n {
        let p = push(&mut g, Op::Param(i as u16), IrType::Int, vec![start]);
        let cmp = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
        let iff = push(&mut g, Op::If, IrType::Control, vec![ctrl, cmp]);
        let t = push(&mut g, Op::Proj(0), IrType::Control, vec![iff]);
        let f = push(&mut g, Op::Proj(1), IrType::Control, vec![iff]);
        ps.push(p);
        ts.push(t);
        fs.push(f);
        ctrl = t;
    }
    let mut exits = fs.clone();
    exits.push(ctrl);
    let m = push(&mut g, Op::Merge, IrType::Control, exits);
    g.exit = push(&mut g, Op::Return, IrType::Void, vec![m, zero]);
    let sched = crate::ir_schedule::schedule(&g);
    let tb = ts
        .iter()
        .map(|&t| sched.node_to_block[t as usize])
        .collect();
    let fb = fs
        .iter()
        .map(|&f| sched.node_to_block[f as usize])
        .collect();
    (g, sched, ps, tb, fb)
}

/// Round 11 wave 5: facts are stored once per establishing block and
/// found by dominator-subtree interval. A chain of guards on DIFFERENT
/// values is the shape that used to copy every inherited fact into every
/// block below; this pins that the answers did not change: a guard's
/// fact reaches everything below its true edge, its false edge sees the
/// negation, and nothing leaks into a sibling or out through the merge.
#[test]
fn a_chain_of_guards_on_different_values_keeps_every_fact_where_it_holds() {
    let n = 64;
    let (g, sched, ps, tb, fb) = guard_chain(n);
    let pr = analyze_graph_pathwise(&g, &sched);
    assert!(pr.converged());
    assert_eq!(pr.census().blocks_with_facts, 2 * n);
    let non_neg = r(0, i32::MAX as i64);
    let neg = r(i32::MIN as i64, -1);
    for i in 0..n {
        for (j, &p) in ps.iter().enumerate() {
            let (at_t, at_f) = (pr.at(p, tb[i]), pr.at(p, fb[i]));
            if j < i {
                assert_eq!(at_t, non_neg, "p{j} below guard {i}'s true edge");
                assert_eq!(at_f, non_neg, "p{j} below guard {i}'s false edge");
            } else if j == i {
                assert_eq!(at_t, non_neg, "p{i} on its own true edge");
                assert_eq!(at_f, neg, "p{i} on its own false edge");
            } else {
                assert!(at_t.is_top(), "p{j} is not tested by guard {i}");
                assert!(at_f.is_top(), "p{j} is not tested above guard {i}");
            }
        }
    }
    let exit = sched.node_to_block[g.nodes[g.exit as usize].inputs[0] as usize];
    for &p in &ps {
        assert!(
            pr.at(p, exit).is_top(),
            "the merge of every exit knows nothing"
        );
    }
}

/// A deeper guard on the SAME value overrides the outer fact in its own
/// subtree only: `if (p >= 0) { if (p < 10) A else B } `.
#[test]
fn a_nested_fact_on_the_same_value_overrides_only_below_it() {
    let mut g = empty_graph();
    let start = push(&mut g, Op::Start, IrType::Control, vec![]);
    g.entry = start;
    let c0 = push(&mut g, Op::Proj(0), IrType::Control, vec![start]);
    let p = push(&mut g, Op::Param(0), IrType::Int, vec![start]);
    let zero = push(&mut g, Op::Const(0), IrType::Int, vec![]);
    let ten = push(&mut g, Op::Const(10), IrType::Int, vec![]);
    let c1 = push(&mut g, Op::Cmp(CmpOp::Ge), IrType::Int, vec![p, zero]);
    let if1 = push(&mut g, Op::If, IrType::Control, vec![c0, c1]);
    let t1 = push(&mut g, Op::Proj(0), IrType::Control, vec![if1]);
    let f1 = push(&mut g, Op::Proj(1), IrType::Control, vec![if1]);
    let c2 = push(&mut g, Op::Cmp(CmpOp::Lt), IrType::Int, vec![p, ten]);
    let if2 = push(&mut g, Op::If, IrType::Control, vec![t1, c2]);
    let t2 = push(&mut g, Op::Proj(0), IrType::Control, vec![if2]);
    let f2 = push(&mut g, Op::Proj(1), IrType::Control, vec![if2]);
    let m = push(&mut g, Op::Merge, IrType::Control, vec![f1, t2, f2]);
    g.exit = push(&mut g, Op::Return, IrType::Void, vec![m, p]);
    let sched = crate::ir_schedule::schedule(&g);
    let b = |c: NodeId| sched.node_to_block[c as usize];
    let pr = analyze_graph_pathwise(&g, &sched);
    assert_eq!(pr.at(p, b(t1)), r(0, i32::MAX as i64));
    assert_eq!(pr.at(p, b(t2)), r(0, 9));
    assert_eq!(pr.at(p, b(f2)), r(10, i32::MAX as i64));
    assert_eq!(pr.at(p, b(f1)), r(i32::MIN as i64, -1));
    assert!(pr.at(p, b(m)).is_top());
}

/// A non-converged fixpoint records no facts at all: every base range is
/// top, so there is nothing to narrow from and nothing that could be
/// narrowed to something wrong.
#[test]
fn a_refused_fixpoint_records_no_facts() {
    let (g, sched, p, tb, _fb) = sign_guarded_graph();
    let pr = analyze_graph_pathwise(&g, &sched);
    // Positive control first -- this fixture DOES converge and DOES narrow,
    // so the assertion below is about the refusal and not about the shape.
    assert!(pr.converged() && !pr.at(p, tb).is_top());

    let refused = PathRanges::flow_insensitive(GraphRanges {
        ranges: vec![Range::top(W); g.nodes.len()],
        converged: false,
    });
    assert!(!refused.converged());
    for b in 0..sched.blocks.len() {
        assert_eq!(refused.at(p, b), refused.of(p));
    }
}
