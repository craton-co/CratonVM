use super::*;

/// `Param(0): ty` and `op(param, Const(k))` (or swapped), returning the
/// graph, the param and the node.
fn shape(op: Op, ty: IrType, k: i64, const_first: bool) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let p = g.add(Op::Param(0), ty, vec![start], None);
    let const_ty = if ty == IrType::Ref { IrType::Long } else { ty };
    let c = g.add(Op::Const(k), const_ty, vec![], None);
    let inputs = if const_first { vec![c, p] } else { vec![p, c] };
    let n = g.add(op, ty, inputs, None);
    (g, p, n)
}

#[test]
fn j27_remat_an_int_add_of_a_constant_is_base_plus_addend() {
    let (g, p, n) = shape(Op::Add, IrType::Int, 7, false);
    assert_eq!(
        remat_add_of(&g, n),
        Some(RematAdd { base: p, addend: 7, wide: false })
    );
    // `Add` commutes: `7 + x` is the same remat.
    let (g, p, n) = shape(Op::Add, IrType::Int, 7, true);
    assert_eq!(
        remat_add_of(&g, n),
        Some(RematAdd { base: p, addend: 7, wide: false })
    );
}

#[test]
fn j27_remat_a_sub_negates_with_the_wrap_of_its_width() {
    let (g, p, n) = shape(Op::Sub, IrType::Int, 5, false);
    assert_eq!(remat_add_of(&g, n).map(|r| (r.base, r.addend)), Some((p, -5)));
    // `x - MIN` is `x + MIN` modulo 2^32: the negation wraps, it does not
    // decline and it does not widen.
    let (g, _, n) = shape(Op::Sub, IrType::Int, i64::from(i32::MIN), false);
    assert_eq!(remat_add_of(&g, n).map(|r| r.addend), Some(i64::from(i32::MIN)));
    let (g, p, n) = shape(Op::Sub, IrType::Long, i64::MIN, false);
    assert_eq!(
        remat_add_of(&g, n),
        Some(RematAdd { base: p, addend: i64::MIN, wide: true })
    );
    let (g, _, n) = shape(Op::Add, IrType::Long, 1 << 40, false);
    assert_eq!(remat_add_of(&g, n).map(|r| (r.addend, r.wide)), Some((1 << 40, true)));
}

#[test]
fn j27_remat_refuses_a_ref_a_constant_minuend_and_other_ops() {
    // A `Ref` is never recomputed, whatever its op.
    let (g, _, n) = shape(Op::Add, IrType::Ref, 16, false);
    assert_eq!(remat_add_of(&g, n), None);
    // `c - x` is not `x + c'`.
    let (g, _, n) = shape(Op::Sub, IrType::Int, 5, true);
    assert_eq!(remat_add_of(&g, n), None);
    let (g, _, n) = shape(Op::Mul, IrType::Int, 3, false);
    assert_eq!(remat_add_of(&g, n), None);
    // `x + y` has no constant to fold.
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let y = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let n = g.add(Op::Add, IrType::Int, vec![x, y], None);
    assert_eq!(remat_add_of(&g, n), None);
}

#[test]
fn j27_remat_refuses_a_base_of_another_type() {
    // An `int` add whose operand is a `long` is not a shape the builder
    // makes; if it ever appears, the remat must not guess a width.
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let x = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let c = g.add(Op::Const(1), IrType::Int, vec![], None);
    let n = g.add(Op::Add, IrType::Int, vec![x, c], None);
    assert_eq!(remat_add_of(&g, n), None);
}
