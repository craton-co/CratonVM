use super::*;

fn graph_with_start() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    (g, start)
}

/// What `id` reads as after one `algebraic_simplify`, seen through a user
/// added for the purpose: `id + p` for a fresh parameter `p`, which no
/// identity touches (a `Neg` user would itself be a W2-6 candidate).
fn after_simplify(g: &mut Graph, id: NodeId, ty: IrType) -> NodeId {
    let p = g.add(Op::Param(7), ty, vec![], None);
    let user = g.add(Op::Add, ty, vec![id, p], None);
    let _ = algebraic_simplify(g);
    g.nodes[user as usize].inputs[0]
}

#[test]
fn division_by_minus_one_becomes_a_negation_at_both_widths() {
    for ty in [IrType::Int, IrType::Long] {
        let (mut g, start) = graph_with_start();
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let x = g.add(Op::Param(0), ty, vec![start], None);
        let m1 = g.add(Op::Const(-1), ty, vec![], None);
        let zero = g.add(Op::Const(0), ty, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![m1, zero], None);
        let guard = g.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, cond], None);
        let div = g.add(Op::Div, ty, vec![x, m1, guard], Some(4));
        let r = after_simplify(&mut g, div, ty);
        assert_eq!(g.nodes[r as usize].op, Op::Neg, "{ty:?}");
        assert_eq!(g.nodes[r as usize].ty, ty);
        assert_eq!(g.nodes[r as usize].inputs.as_slice(), &[x][..]);
        assert_eq!(g.nodes[r as usize].bytecode_pc, Some(4));
        assert_eq!(g.nodes[div as usize].op, Op::Dead);
        // The guard outlives the division, and goes on its own evidence.
        assert_eq!(g.nodes[guard as usize].op, Op::Guard { bci: 4 });
        assert!(fold_constants(&mut g));
        assert!(eliminate_trivial_guards(&mut g));
        assert_eq!(g.nodes[guard as usize].op, Op::Dead);
    }
}

/// The int view of a `-1` held as its unsigned 32-bit pattern is still -1.
#[test]
fn an_int_minus_one_is_read_at_int_width() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let m1 = g.add(Op::Const(0xFFFF_FFFF), IrType::Int, vec![], None);
    let mul = g.add(Op::Mul, IrType::Int, vec![m1, x], None);
    let r = after_simplify(&mut g, mul, IrType::Int);
    assert_eq!(g.nodes[r as usize].op, Op::Neg);
    // ...but the same bits are not -1 as a `long`.
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let c = g.add(Op::Const(0xFFFF_FFFF), IrType::Long, vec![], None);
    let mul = g.add(Op::Mul, IrType::Long, vec![x, c], None);
    assert_eq!(after_simplify(&mut g, mul, IrType::Long), mul);
}

#[test]
fn zero_minus_x_and_x_times_minus_one_are_negations() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let z = g.add(Op::Const(0), IrType::Long, vec![], None);
    let sub = g.add(Op::Sub, IrType::Long, vec![z, x], None);
    let r = after_simplify(&mut g, sub, IrType::Long);
    assert_eq!(g.nodes[r as usize].op, Op::Neg);
    assert_eq!(g.nodes[r as usize].inputs.as_slice(), &[x][..]);

    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let m1 = g.add(Op::Const(-1), IrType::Int, vec![], None);
    let mul = g.add(Op::Mul, IrType::Int, vec![x, m1], None);
    let r = after_simplify(&mut g, mul, IrType::Int);
    assert_eq!(g.nodes[r as usize].op, Op::Neg);
}

#[test]
fn a_double_negation_and_a_widen_narrow_pair_are_the_operand() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let n1 = g.add(Op::Neg, IrType::Int, vec![x], None);
    let n2 = g.add(Op::Neg, IrType::Int, vec![n1], None);
    assert_eq!(after_simplify(&mut g, n2, IrType::Int), x);

    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let wide = g.add(Op::I2L, IrType::Long, vec![x], None);
    let narrow = g.add(Op::L2I, IrType::Int, vec![wide], None);
    assert_eq!(after_simplify(&mut g, narrow, IrType::Int), x);

    // `(long)(int) l` is NOT `l`: the narrowing loses the high half.
    let (mut g, start) = graph_with_start();
    let l = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let narrow = g.add(Op::L2I, IrType::Int, vec![l], None);
    let wide = g.add(Op::I2L, IrType::Long, vec![narrow], None);
    assert_eq!(after_simplify(&mut g, wide, IrType::Long), wide);
}

/// `0.0 - 0.0` is `+0.0` and `-(0.0)` is `-0.0`: no FP rewrite, even with
/// an integer-shaped zero operand.
#[test]
fn floating_point_is_never_rewritten() {
    let (mut g, start) = graph_with_start();
    let d = g.add(Op::Param(0), IrType::Double, vec![start], None);
    let z = g.add(Op::Const(0), IrType::Double, vec![], None);
    let sub = g.add(Op::Sub, IrType::Double, vec![z, d], None);
    assert_eq!(after_simplify(&mut g, sub, IrType::Double), sub);
    let n1 = g.add(Op::Neg, IrType::Double, vec![d], None);
    let n2 = g.add(Op::Neg, IrType::Double, vec![n1], None);
    assert_eq!(after_simplify(&mut g, n2, IrType::Double), n2);
}

