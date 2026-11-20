use super::*;
use crate::ir::{IrType, NO_NODE};
use crate::ir_schedule;

/// The induction variable's start.
#[derive(Clone, Copy)]
enum Start {
    Zero,
    N,
    /// `Sub(a.length, k)`.
    LengthSub(i64),
    /// `Add(a.length, k)`.
    LengthAdd(i64),
    Length,
}

/// A limit the test compares the induction variable against.
#[derive(Clone, Copy)]
enum Limit {
    N,
    /// `Sub(a.length, k)`.
    LengthSub(i64),
}

/// The test that keeps the loop running, on the `If`'s TRUE edge.
#[derive(Clone, Copy)]
enum Stay {
    Below(Limit),
    AtMost(Limit),
    AtLeast(i64),
}

/// `[Guard(n <pre> a.length);] for (i = start; <stay>; i += step)
/// a[i] = 1;`, the body on the true edge (also the back edge). Returns
/// whether the store's bounds check is proven away by the range pass.
fn proved(pre: Option<CmpOp>, start: Start, stay: Stay, step: i64) -> bool {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = s;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let m0 = g.add(Op::Proj(1), IrType::Memory, vec![s], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![s], None);
    let n = g.add(Op::Param(1), IrType::Int, vec![s], None);
    if let Some(op) = pre {
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let cmp = g.add(Op::Cmp(op), IrType::Int, vec![n, len], None);
        let _ = g.add(Op::Guard { bci: 0 }, IrType::Void, vec![c0, cmp], Some(0));
    }
    let length_minus = |g: &mut Graph, k: i64, sub: bool| {
        let len = g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None);
        let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
        let op = if sub { Op::Sub } else { Op::Add };
        g.add(op, IrType::Int, vec![len, kc], None)
    };
    let init = match start {
        Start::Zero => g.add(Op::Const(0), IrType::Int, vec![], None),
        Start::N => n,
        Start::LengthSub(k) => length_minus(&mut g, k, true),
        Start::LengthAdd(k) => length_minus(&mut g, k, false),
        Start::Length => g.add(Op::ArrayLength, IrType::Int, vec![c0, m0, arr], None),
    };
    let limit = |g: &mut Graph, l: Limit| match l {
        Limit::N => n,
        Limit::LengthSub(k) => length_minus(g, k, true),
    };
    let region = g.add(Op::Merge, IrType::Control, vec![c0], None);
    let iv = g.add(Op::Phi, IrType::Int, vec![region, init], None);
    let stride = g.add(Op::Const(step), IrType::Int, vec![], None);
    let next = g.add(Op::Add, IrType::Int, vec![iv, stride], None);
    g.nodes[iv as usize].inputs.push(next);
    let cmp = match stay {
        Stay::Below(l) => {
            let x = limit(&mut g, l);
            g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, x], None)
        }
        Stay::AtMost(l) => {
            let x = limit(&mut g, l);
            g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![iv, x], None)
        }
        Stay::AtLeast(c) => {
            let x = g.add(Op::Const(c), IrType::Int, vec![], None);
            g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, x], None)
        }
    };
    let iff = g.add(Op::If, IrType::Control, vec![region, cmp], None);
    let body = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
    g.nodes[region as usize].inputs.push(body);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let st = g.add(
        Op::ArrayStore(MemKind::Int),
        IrType::Void,
        vec![body, m0, arr, iv, one],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, iv], Some(1));
    let sched = ir_schedule::schedule(&g);
    let e = analyze(&g, &sched);
    e.bounds_elided(st) && e.bounds_range_proved(st)
}

/// `for (i = 0; i <= n; i++) a[i]` is in bounds only when `n < a.length`:
/// the non-strict `n <= a.length` leaves `a[a.length]`.
#[test]
fn an_inclusive_loop_needs_a_strict_fact() {
    let up_to_n = Stay::AtMost(Limit::N);
    assert!(proved(Some(CmpOp::Lt), Start::Zero, up_to_n, 1));
    assert!(!proved(Some(CmpOp::Le), Start::Zero, up_to_n, 1));
    assert!(!proved(None, Start::Zero, up_to_n, 1));
}

/// `i <= a.length - 1` and `i < a.length - 1` need no fact at all; a
/// subtraction of zero is not below the length.
#[test]
fn a_limit_of_length_minus_k_is_below_the_length() {
    assert!(proved(
        None,
        Start::Zero,
        Stay::AtMost(Limit::LengthSub(1)),
        1
    ));
    assert!(proved(
        None,
        Start::Zero,
        Stay::Below(Limit::LengthSub(1)),
        1
    ));
    assert!(!proved(
        None,
        Start::Zero,
        Stay::AtMost(Limit::LengthSub(0)),
        1
    ));
}

/// `for (i = a.length - 1; i >= 0; i--) a[i]`, the same from
/// `a.length + (-1)`, and from a start the pre-header guard puts below the
/// length, by any negative step.
#[test]
fn a_down_count_from_below_the_length_is_in_bounds() {
    let to_zero = Stay::AtLeast(0);
    assert!(proved(None, Start::LengthSub(1), to_zero, -1));
    assert!(proved(None, Start::LengthAdd(-1), to_zero, -1));
    assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -1));
    assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -2));
}

/// The down-count negatives: a start AT the length, a non-strict fact on
/// the start, no fact, a test that admits `-1`, and a step that is not
/// negative. Each one can index out of range.
#[test]
fn a_down_count_that_can_leave_the_array_keeps_its_check() {
    let to_zero = Stay::AtLeast(0);
    assert!(!proved(None, Start::Length, to_zero, -1));
    assert!(!proved(Some(CmpOp::Le), Start::N, to_zero, -1));
    assert!(!proved(None, Start::N, to_zero, -1));
    assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(-1), -1));
    assert!(!proved(None, Start::LengthSub(1), to_zero, 1));
    assert!(!proved(None, Start::LengthSub(1), to_zero, 0));
}

/// `CRATONVM_JIT_IR_BCE_RANGE=0` drops the wave-8 facts with the rest.
#[test]
fn the_range_switch_drops_the_wave8_facts() {
    let _off = RangeForce::off();
    assert!(!proved(
        Some(CmpOp::Lt),
        Start::Zero,
        Stay::AtMost(Limit::N),
        1
    ));
    assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(0), -1));
}

/// Wave 9 (mid proposal W8-1): the down count's `i >= 0` half is the loop
/// test's own edge fact (`non_negative_at`). With default flags the down
/// counts are proved and the negatives keep their checks. The arm is an
/// edge fact, so `CRATONVM_JIT_IR_RANGE_EDGES=0` withdraws it with every
/// other one (that switch must restore the pre-narrowing answer), and it
/// does not stand alone without the SSA range lattice either: the upper
/// half of a down count's proof comes from the lattice.
#[test]
fn a_down_count_is_proved_by_its_loop_test_edge() {
    let to_zero = Stay::AtLeast(0);
    assert!(proved(None, Start::LengthSub(1), to_zero, -1));
    assert!(proved(Some(CmpOp::Lt), Start::N, to_zero, -2));
    assert!(!proved(None, Start::LengthSub(1), Stay::AtLeast(-1), -1));
    assert!(!proved(None, Start::Length, to_zero, -1));
    assert!(!proved(Some(CmpOp::Le), Start::N, to_zero, -1));
}
