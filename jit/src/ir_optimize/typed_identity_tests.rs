use super::*;

fn graph_with_start() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    (g, start)
}

/// `d - d` is NaN for NaN and ±Infinity, so it must not fold to zero.
/// `static boolean finite(double d) { return d - d == 0.0; }` returned true
/// for NaN when it did.
#[test]
fn a_floating_point_self_subtraction_is_not_folded() {
    let (mut g, start) = graph_with_start();
    let d = g.add(Op::Param(0), IrType::Double, vec![start], None);
    let sub = g.add(Op::Sub, IrType::Double, vec![d, d], None);
    let user = g.add(Op::Neg, IrType::Double, vec![sub], None);

    let _ = algebraic_simplify(&mut g);

    assert_eq!(g.nodes[user as usize].inputs[0], sub);
    assert_ne!(g.nodes[sub as usize].op, Op::Dead);
}

/// `x - x` on a `long` folds to a `Long` zero, not to whichever `Const(0)`
/// the arena happens to hold first — here the `Ref`-typed null literal.
#[test]
fn a_long_self_subtraction_folds_to_a_long_zero() {
    let (mut g, start) = graph_with_start();
    let _null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let l = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let sub = g.add(Op::Sub, IrType::Long, vec![l, l], None);
    let user = g.add(Op::Neg, IrType::Long, vec![sub], None);

    let _ = algebraic_simplify(&mut g);

    let zero = g.nodes[user as usize].inputs[0];
    assert_eq!(g.nodes[zero as usize].op, Op::Const(0));
    assert_eq!(g.nodes[zero as usize].ty, IrType::Long);
}

#[test]
fn an_int_self_xor_folds_to_an_int_zero() {
    let (mut g, start) = graph_with_start();
    let _long_zero = g.add(Op::Const(0), IrType::Long, vec![], None);
    let i = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let xor = g.add(Op::Xor, IrType::Int, vec![i, i], None);
    let user = g.add(Op::Neg, IrType::Int, vec![xor], None);

    let _ = algebraic_simplify(&mut g);

    let zero = g.nodes[user as usize].inputs[0];
    assert_eq!(g.nodes[zero as usize].op, Op::Const(0));
    assert_eq!(g.nodes[zero as usize].ty, IrType::Int);
}

/// A `Cmp` result is always `Int`; the width must come from the operands.
/// `1L << 32 != 0L` compared as truncated ints is `0 != 0`.
#[test]
fn a_compare_of_long_constants_folds_at_64_bits() {
    let (mut g, _start) = graph_with_start();
    let a = g.add(Op::Const(1i64 << 32), IrType::Long, vec![], None);
    let b = g.add(Op::Const(0), IrType::Long, vec![], None);
    let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![a, b], None);
    assert_eq!(try_fold(&g.nodes, ne), Some(1));
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![b, a], None);
    assert_eq!(try_fold(&g.nodes, lt), Some(1));
}
