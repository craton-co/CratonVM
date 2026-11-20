use super::*;
use crate::ir::CmpOp;

/// `for (k = 0; k < trips; k++) acc += (k & 1) == 0 ? a : b; return acc;`
/// by hand: header `H` at pc 2 with φs `k`, `acc`; test `If(k < trips)`;
/// the body branches on `(k & 1) == 0` and joins at `M`, the latch.
fn diamond_loop(trips: i64) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let n = g.add(Op::Const(trips), IrType::Int, vec![], None);
    let h = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(2));
    let k = g.add(Op::Phi, IrType::Int, vec![h, zero, NO_NODE], Some(2));
    let acc = g.add(Op::Phi, IrType::Int, vec![h, zero, NO_NODE], Some(2));
    let test = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, n], Some(5));
    let iff = g.add(Op::If, IrType::Control, vec![h, test], Some(5));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(5));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(5));
    let bit = g.add(Op::And, IrType::Int, vec![k, one], Some(9));
    let even = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![bit, zero], Some(11));
    let br = g.add(Op::If, IrType::Control, vec![stay, even], Some(11));
    let t_arm = g.add(Op::Proj(0), IrType::Control, vec![br], Some(11));
    let f_arm = g.add(Op::Proj(1), IrType::Control, vec![br], Some(11));
    let m = g.add(Op::Merge, IrType::Control, vec![t_arm, f_arm], Some(18));
    let x = g.add(Op::Phi, IrType::Int, vec![m, a, b], Some(18));
    let acc_next = g.add(Op::Add, IrType::Int, vec![acc, x], Some(19));
    let k_next = g.add(Op::Add, IrType::Int, vec![k, one], Some(21));
    assert!(g.set_input(h, 1, m));
    assert!(g.set_input(k, 2, k_next));
    assert!(g.set_input(acc, 2, acc_next));
    let ret = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(30));
    g.exit = ret;
    (g, h, ret)
}


/// The diamond loop is unrolled into straight-line copies, and after the
/// cleanup folds each copy's branch the return reads `a + b + a + b`.
#[test]
fn r24_c_a_branching_constant_loop_is_fully_unrolled() {
    let _ = super::super::take_unroll_stranded_frames();
    let (mut g, h, ret) = diamond_loop(4);
    let n = unroll_cfg_loops(&mut g);
    assert_eq!(n, 1);
    assert_eq!(g.nodes[h as usize].op, Op::Dead, "the header is gone");
    super::super::prune_constant_ifs(&mut g);
    super::super::scalar_cleanup(&mut g);
    super::super::prune_constant_ifs(&mut g);
    super::super::scalar_cleanup(&mut g);
    assert!(
        !g.nodes.iter().any(|n| n.op == Op::If),
        "every copy's branch folded on its constant index"
    );
    // The value: an Add tree over the parameters, `a` twice and `b` twice
    // (one per copy, each copy's arm chosen by its constant index).
    let mut leaves: Vec<NodeId> = Vec::new();
    let mut stack = vec![g.nodes[ret as usize].inputs[1]];
    while let Some(v) = stack.pop() {
        match g.nodes[v as usize].op {
            Op::Add => stack.extend(g.nodes[v as usize].inputs.iter().copied()),
            Op::Param(_) => leaves.push(v),
            ref other => panic!("unexpected {other:?} in the folded sum"),
        }
    }
    let count = |want: u32| {
        leaves
            .iter()
            .filter(|&&l| matches!(g.nodes[l as usize].op, Op::Param(i) if i as u32 == want))
            .count()
    };
    assert_eq!((count(0), count(1)), (2, 2), "a + b + a + b");
    let stranded = super::super::take_unroll_stranded_frames();
    assert!(stranded.flat.contains(&2), "the header's frame is stranded");    }