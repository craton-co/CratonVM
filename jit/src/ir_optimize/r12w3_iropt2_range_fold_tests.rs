use super::*;

/// `return (p <op> k)`-shaped graph: `(g, ret, cmp)` where `lhs` is built
/// by `build` from the parameter.
fn compare_graph(
    ty: IrType,
    build: impl FnOnce(&mut Graph, NodeId) -> NodeId,
    cc: CmpOp,
    k: i64,
) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let p = g.add(Op::Param(0), ty, vec![start], None);
    let lhs = build(&mut g, p);
    let kn = g.add(Op::Const(k), ty, vec![], None);
    let cmp = g.add(Op::Cmp(cc), IrType::Int, vec![lhs, kn], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, cmp], None);
    g.exit = ret;
    (g, ret, cmp)
}

fn masked(mask: i64) -> impl FnOnce(&mut Graph, NodeId) -> NodeId {
    move |g: &mut Graph, p: NodeId| {
        let m = g.add(Op::Const(mask), IrType::Int, vec![], None);
        g.add(Op::And, IrType::Int, vec![p, m], None)
    }
}

fn returned(g: &Graph, ret: NodeId) -> Op {
    g.nodes[g.nodes[ret as usize].inputs[1] as usize].op.clone()
}

#[test]
fn a_masked_value_below_its_mask_bound_is_decided() {
    let (mut g, ret, cmp) = compare_graph(IrType::Int, masked(0x7f), CmpOp::Lt, 0x80);
    assert!(fold_range_decided_compares(&mut g));
    assert_eq!(g.nodes[cmp as usize].op, Op::Dead);
    assert_eq!(returned(&g, ret), Op::Const(1));

    // `== 0x80` can never hold.
    let (mut g, ret, _) = compare_graph(IrType::Int, masked(0x7f), CmpOp::Eq, 0x80);
    assert!(fold_range_decided_compares(&mut g));
    assert_eq!(returned(&g, ret), Op::Const(0));

    // `>= 0` always holds; `> 0` does not decide (the mask admits 0).
    let (mut g, ret, _) = compare_graph(IrType::Int, masked(0xff), CmpOp::Ge, 0);
    assert!(fold_range_decided_compares(&mut g));
    assert_eq!(returned(&g, ret), Op::Const(1));
    let (mut g, _, cmp) = compare_graph(IrType::Int, masked(0xff), CmpOp::Gt, 0);
    assert!(!fold_range_decided_compares(&mut g));
    assert!(matches!(g.nodes[cmp as usize].op, Op::Cmp(CmpOp::Gt)));
}

/// The must-differ twin: a wider mask straddles the bound, and an unmasked
/// parameter is top.
#[test]
fn a_compare_the_ranges_straddle_is_kept() {
    let (mut g, _, cmp) = compare_graph(IrType::Int, masked(0xff), CmpOp::Lt, 0x80);
    assert!(!fold_range_decided_compares(&mut g));
    assert!(matches!(g.nodes[cmp as usize].op, Op::Cmp(CmpOp::Lt)));
    let (mut g, _, cmp) = compare_graph(IrType::Int, |_, p| p, CmpOp::Lt, i64::from(i32::MAX));
    assert!(!fold_range_decided_compares(&mut g));
    assert!(matches!(g.nodes[cmp as usize].op, Op::Cmp(_)));
}

#[test]
fn a_long_compare_is_decided_at_its_own_width() {
    // `(l >>> 60) <= 15`.
    let (mut g, ret, _) = compare_graph(
        IrType::Long,
        |g, p| {
            let c = g.add(Op::Const(60), IrType::Int, vec![], None);
            g.add(Op::UShr, IrType::Long, vec![p, c], None)
        },
        CmpOp::Le,
        15,
    );
    assert!(fold_range_decided_compares(&mut g));
    assert_eq!(returned(&g, ret), Op::Const(1));
    // `(l >>> 1) <= Integer.MAX_VALUE` is NOT decided: a long shift keeps 63 bits.
    let (mut g, _, cmp) = compare_graph(
        IrType::Long,
        |g, p| {
            let c = g.add(Op::Const(1), IrType::Int, vec![], None);
            g.add(Op::UShr, IrType::Long, vec![p, c], None)
        },
        CmpOp::Le,
        i64::from(i32::MAX),
    );
    assert!(!fold_range_decided_compares(&mut g));
    assert!(matches!(g.nodes[cmp as usize].op, Op::Cmp(_)));
}

/// A divide-by-zero guard on a divisor the lattice keeps away from zero
/// becomes trivial and goes.
#[test]
fn a_division_guard_on_a_nonzero_range_is_removed() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let three = g.add(Op::Const(3), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let low = g.add(Op::And, IrType::Int, vec![p, three], None);
    let d = g.add(Op::Add, IrType::Int, vec![low, one], None); // [1, 4]
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![d, zero], None);
    let guard = g.add(
        Op::Guard { bci: 5 },
        IrType::Void,
        vec![ctrl, cond],
        Some(5),
    );
    let div = g.add(Op::Div, IrType::Int, vec![p, d, guard], Some(5));
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, div], None);
    g.exit = ret;
    assert!(fold_range_decided_compares(&mut g));
    assert!(eliminate_trivial_guards(&mut g));
    assert_eq!(g.nodes[guard as usize].op, Op::Dead);
    assert_eq!(g.nodes[div as usize].inputs.as_slice(), &[p, d][..]);
}

/// A reference compare is never decided here, even against null.
#[test]
fn a_reference_compare_is_not_a_candidate() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let r = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let cmp = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![r, null], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, cmp], None);
    g.exit = ret;
    assert!(!fold_range_decided_compares(&mut g));
}


#[test]
fn range_decides_refuses_bottom_and_mixed_widths() {
    use crate::range_analysis::{IntWidth, Range};
    let w = IntWidth::W32;
    assert_eq!(
        range_decides(CmpOp::Lt, Range::empty(w), Range::constant(w, 5)),
        None
    );
    assert_eq!(
        range_decides(
            CmpOp::Lt,
            Range::exact(w, 0, 1),
            Range::constant(IntWidth::W64, 5)
        ),
        None
    );
    assert_eq!(
        range_decides(CmpOp::Ne, Range::constant(w, 5), Range::constant(w, 5)),
        Some(0)
    );
    assert_eq!(
        range_decides(CmpOp::Le, Range::exact(w, 0, 5), Range::exact(w, 5, 9)),
        Some(1)
    );
    assert_eq!(
        range_decides(CmpOp::Lt, Range::exact(w, 0, 5), Range::exact(w, 5, 9)),
        None
    );
}
