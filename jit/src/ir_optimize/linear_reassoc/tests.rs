use super::*;
use crate::ir::IrBuilder;

/// `int f(int a, int b) { return a + b; }`: a graph with two `int`
/// parameters whose returned value the tests replace.
fn two_param_graph() -> (Graph, NodeId, NodeId, NodeId) {
    // iload_0; iload_1; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0xac];
    let g = IrBuilder::new(2, 2).build(&code, code.len()).expect("build");
    let ret = g.nodes[g.exit as usize].inputs[1];
    let add = &g.nodes[ret as usize];
    assert_eq!(add.op, Op::Add);
    let (a, b) = (add.inputs[0], add.inputs[1]);
    (g, ret, a, b)
}

fn k(g: &mut Graph, v: i64, ty: IrType) -> NodeId {
    g.add(Op::Const(v), ty, vec![], None)
}

fn bin(g: &mut Graph, op: Op, ty: IrType, a: NodeId, b: NodeId) -> NodeId {
    g.add(op, ty, vec![a, b], None)
}

/// Evaluate `id` over the two parameters, Java `int` semantics.
fn eval_int(g: &Graph, id: NodeId, a: NodeId, b: NodeId, va: i32, vb: i32) -> i32 {
    if id == a {
        return va;
    }
    if id == b {
        return vb;
    }
    let n = &g.nodes[id as usize];
    let x = |s: usize| eval_int(g, n.inputs[s], a, b, va, vb);
    match n.op {
        // Cast: an `int` constant is its low 32 bits.
        Op::Const(v) => v as i32,
        Op::Add => x(0).wrapping_add(x(1)),
        Op::Sub => x(0).wrapping_sub(x(1)),
        Op::Mul => x(0).wrapping_mul(x(1)),
        Op::Neg => x(0).wrapping_neg(),
        // Cast: JLS 15.19 masks the count.
        Op::Shl => x(0).wrapping_shl((x(1) & 31) as u32),
        ref op => panic!("unexpected {op:?} at n{id}"),
    }
}

/// Non-constant nodes the returned value reaches, parameters excluded.
fn reached_ops(g: &Graph, id: NodeId, a: NodeId, b: NodeId) -> usize {
    if id == a || id == b {
        return 0;
    }
    let n = &g.nodes[id as usize];
    if matches!(n.op, Op::Const(_)) {
        return 0;
    }
    1 + n
        .inputs
        .as_slice()
        .iter()
        .map(|&i| reached_ops(g, i, a, b))
        .sum::<usize>()
}

fn ret_of(g: &Graph) -> NodeId {
    g.nodes[g.exit as usize].inputs[1]
}

const VALUES: [i32; 7] = [0, 1, -1, 7, i32::MAX, i32::MIN, 0x1234_5678];

/// `acc + i + (i+1) + (i+2) + (i+3)` (the unrolled accumulation of the
/// page) becomes `acc + (i << 2) + 6`: three nodes for seven, same value
/// at every input, wrap-around included.
#[test]
fn j27_loopcg_an_unrolled_accumulation_becomes_acc_plus_4i_plus_6() {
    let (mut g, ret, acc, i) = two_param_graph();
    let ty = IrType::Int;
    let mut top = bin(&mut g, Op::Add, ty, acc, i);
    for c in 1..=3 {
        let kc = k(&mut g, c, ty);
        let ic = bin(&mut g, Op::Add, ty, i, kc);
        top = bin(&mut g, Op::Add, ty, top, ic);
    }
    g.replace_all_uses(ret, top);
    g.kill(ret);
    let before: Vec<i32> = VALUES
        .iter()
        .flat_map(|&x| VALUES.iter().map(move |&y| (x, y)))
        .map(|(x, y)| eval_int(&g, ret_of(&g), acc, i, x, y))
        .collect();
    assert_eq!(reached_ops(&g, ret_of(&g), acc, i), 7);
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census.trees, 1, "{census:?}");
    assert_eq!(census.removed, 7);
    assert_eq!(census.added, 3);
    assert_eq!(reached_ops(&g, ret_of(&g), acc, i), 3);
    let after: Vec<i32> = VALUES
        .iter()
        .flat_map(|&x| VALUES.iter().map(move |&y| (x, y)))
        .map(|(x, y)| eval_int(&g, ret_of(&g), acc, i, x, y))
        .collect();
    assert_eq!(before, after);
    let top = &g.nodes[ret_of(&g) as usize];
    assert_eq!(top.op, Op::Add);
    assert_eq!(
        g.nodes[top.inputs[1] as usize].op,
        Op::Const(6),
        "the constant is pulled outermost"
    );
}

/// `((x + 3) + 3) + 3) + 3` is `x + 12`.
#[test]
fn j27_loopcg_a_constant_chain_folds_to_one_add() {
    let (mut g, ret, x, y) = two_param_graph();
    let ty = IrType::Int;
    let mut top = x;
    for _ in 0..4 {
        let kc = k(&mut g, 3, ty);
        top = bin(&mut g, Op::Add, ty, top, kc);
    }
    g.replace_all_uses(ret, top);
    g.kill(ret);
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census.trees, 1);
    let r = &g.nodes[ret_of(&g) as usize];
    assert_eq!(r.op, Op::Add);
    assert_eq!(r.inputs[0], x);
    assert_eq!(g.nodes[r.inputs[1] as usize].op, Op::Const(12));
    for &v in &VALUES {
        assert_eq!(eval_int(&g, ret_of(&g), x, y, v, 0), v.wrapping_add(12));
    }
}

/// Subtractions, negations, constant multiplies and shifts: the rebuilt
/// value equals the tree's at every input.
#[test]
fn j27_loopcg_mixed_linear_trees_keep_their_value() {
    let (mut g, ret, a, b) = two_param_graph();
    let ty = IrType::Int;
    // t = ((a * 5 - b) + -(a << 3)) - (b * -4) + (7 - a)
    let c5 = k(&mut g, 5, ty);
    let a5 = bin(&mut g, Op::Mul, ty, a, c5);
    let t1 = bin(&mut g, Op::Sub, ty, a5, b);
    let c3 = k(&mut g, 3, IrType::Int);
    let a8 = bin(&mut g, Op::Shl, ty, a, c3);
    let na8 = g.add(Op::Neg, ty, vec![a8], None);
    let t2 = bin(&mut g, Op::Add, ty, t1, na8);
    let cm4 = k(&mut g, -4, ty);
    let bm4 = bin(&mut g, Op::Mul, ty, b, cm4);
    let t3 = bin(&mut g, Op::Sub, ty, t2, bm4);
    let c7 = k(&mut g, 7, ty);
    let sa = bin(&mut g, Op::Sub, ty, c7, a);
    let top = bin(&mut g, Op::Add, ty, t3, sa);
    g.replace_all_uses(ret, top);
    g.kill(ret);
    let mut before = Vec::new();
    for &x in &VALUES {
        for &y in &VALUES {
            before.push(eval_int(&g, ret_of(&g), a, b, x, y));
        }
    }
    // a*(5 - 8 - 1) + b*(-1 + 4) + 7 = -4a + 3b + 7.
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census.trees, 1, "{census:?}");
    let mut after = Vec::new();
    for &x in &VALUES {
        for &y in &VALUES {
            after.push(eval_int(&g, ret_of(&g), a, b, x, y));
        }
    }
    assert_eq!(before, after);
    assert!(reached_ops(&g, ret_of(&g), a, b) < 9);
}

/// An intermediate read twice is a leaf of both readers, and a tree no
/// smaller rebuilt is left exactly as it was.
#[test]
fn j27_loopcg_a_shared_intermediate_and_a_one_node_step_are_left_alone() {
    let (mut g, ret, a, b) = two_param_graph();
    let ty = IrType::Int;
    let c1 = k(&mut g, 1, ty);
    let shared = bin(&mut g, Op::Add, ty, a, c1);
    // top = shared * shared is not linear; each of its operands is the
    // top of a one-node tree `a + 1`, which nothing shrinks.
    let top = bin(&mut g, Op::Mul, ty, shared, shared);
    g.replace_all_uses(ret, top);
    g.kill(ret);
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census, LinearReassocCensus::default());
    assert_eq!(g.nodes[ret_of(&g) as usize].inputs.as_slice(), &[shared, shared]);
    let _ = b;
}

/// A snapshot-named intermediate stays a leaf (and alive): the builder's
/// own `iadd` is on the operand stack of no later bci here, so name it.
#[test]
fn j27_loopcg_a_snapshot_named_intermediate_is_a_leaf() {
    let (mut g, ret, a, b) = two_param_graph();
    let ty = IrType::Int;
    let c1 = k(&mut g, 1, ty);
    let inner = bin(&mut g, Op::Add, ty, a, c1);
    let c2 = k(&mut g, 2, ty);
    let top = bin(&mut g, Op::Add, ty, inner, c2);
    g.replace_all_uses(ret, top);
    g.kill(ret);
    g.push_safepoint(crate::ir::SafepointSnapshot::new(0, vec![inner], vec![], vec![]));
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census.trees, 0, "{census:?}");
    assert_eq!(g.nodes[inner as usize].op, Op::Add);
    let _ = b;
}

/// `long`: the coefficient of `x << 63` wraps to `Long.MIN_VALUE`.
#[test]
fn j27_loopcg_long_shift_coefficients_wrap_at_64_bits() {
    let (mut g, ret, a, _b) = two_param_graph();
    let ty = IrType::Long;
    let x = g.add(Op::I2L, ty, vec![a], None);
    let c63 = k(&mut g, 63, IrType::Int);
    let s = bin(&mut g, Op::Shl, ty, x, c63);
    let t = bin(&mut g, Op::Add, ty, s, s);
    let c9 = k(&mut g, 9, ty);
    let top = bin(&mut g, Op::Add, ty, t, c9);
    let l2i = g.add(Op::L2I, IrType::Int, vec![top], None);
    g.replace_all_uses(ret, l2i);
    g.kill(ret);
    // `s` is read twice, so it is a leaf; `t + 9` holds two linear nodes
    // and rebuilds as `(s << 1) + 9`, two nodes: no gain, untouched.
    let census = reassociate_linear_trees(&mut g);
    assert_eq!(census.trees, 0, "{census:?}");
    assert_eq!(classify(&g, s).map(|(l, _)| l), Some(Lin::Scale(x, i64::MIN)));
}

