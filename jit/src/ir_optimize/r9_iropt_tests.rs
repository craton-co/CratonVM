use super::*;
use crate::ir::SafepointSnapshot;

fn graph_with_start() -> (Graph, NodeId) {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    (g, start)
}

fn unary(op: Op, ty: IrType, in_ty: IrType, v: i64) -> Option<i64> {
    let (mut g, _) = graph_with_start();
    let c = g.add(Op::Const(v), in_ty, vec![], None);
    let n = g.add(op, ty, vec![c], None);
    try_fold(&g.nodes, n)
}

#[test]
fn integer_conversions_fold_by_their_jls_rules() {
    assert_eq!(unary(Op::I2L, IrType::Long, IrType::Int, -1), Some(-1));
    assert_eq!(
        unary(Op::L2I, IrType::Int, IrType::Long, 0x1_8000_0000),
        Some(i32::MIN as i64),
        "l2i keeps the low 32 bits and sign-extends them"
    );
    assert_eq!(unary(Op::I2B, IrType::Int, IrType::Int, 0x1FF), Some(-1));
    assert_eq!(
        unary(Op::I2C, IrType::Int, IrType::Int, -1),
        Some(0xFFFF),
        "i2c zero-extends"
    );
    assert_eq!(
        unary(Op::I2S, IrType::Int, IrType::Int, 0x18000),
        Some(-32768)
    );
    // A conversion to floating point is never folded by the integer folder.
    assert_eq!(unary(Op::I2D, IrType::Double, IrType::Int, 3), None);
}

#[test]
fn lcmp_folds_to_the_sign_of_the_difference_at_64_bits() {
    let (mut g, _) = graph_with_start();
    let big = g.add(Op::Const(1i64 << 40), IrType::Long, vec![], None);
    let small = g.add(Op::Const(-5), IrType::Long, vec![], None);
    let gt = g.add(Op::LCmp, IrType::Int, vec![big, small], None);
    let lt = g.add(Op::LCmp, IrType::Int, vec![small, big], None);
    let eq = g.add(Op::LCmp, IrType::Int, vec![big, big], None);
    assert_eq!(try_fold(&g.nodes, gt), Some(1));
    assert_eq!(try_fold(&g.nodes, lt), Some(-1));
    assert_eq!(try_fold(&g.nodes, eq), Some(0));
}

/// What `id` reads as after one `algebraic_simplify`, observed through a
/// user added for the purpose.
fn simplified_through_user(g: &mut Graph, id: NodeId, ty: IrType) -> NodeId {
    let user = g.add(Op::Neg, ty, vec![id], None);
    let _ = algebraic_simplify(g);
    g.nodes[user as usize].inputs[0]
}

#[test]
fn a_shift_count_the_jls_masks_to_zero_is_the_identity() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let c32 = g.add(Op::Const(32), IrType::Int, vec![], None);
    let shl = g.add(Op::Shl, IrType::Int, vec![x, c32], None);
    assert_eq!(simplified_through_user(&mut g, shl, IrType::Int), x);

    // The same count on a `long` is a real shift.
    let (mut g, start) = graph_with_start();
    let l = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let c32 = g.add(Op::Const(32), IrType::Int, vec![], None);
    let lshl = g.add(Op::Shl, IrType::Long, vec![l, c32], None);
    assert_eq!(simplified_through_user(&mut g, lshl, IrType::Long), lshl);

    // ...and 64 is the identity there.
    let (mut g, start) = graph_with_start();
    let l = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let c64 = g.add(Op::Const(64), IrType::Int, vec![], None);
    let lshr = g.add(Op::UShr, IrType::Long, vec![l, c64], None);
    assert_eq!(simplified_through_user(&mut g, lshr, IrType::Long), l);
}

#[test]
fn integer_division_and_remainder_by_one() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let div = g.add(Op::Div, IrType::Int, vec![x, one], None);
    assert_eq!(simplified_through_user(&mut g, div, IrType::Int), x);

    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let m1 = g.add(Op::Const(-1), IrType::Long, vec![], None);
    let rem = g.add(Op::Rem, IrType::Long, vec![x, m1], None);
    let z = simplified_through_user(&mut g, rem, IrType::Long);
    assert_eq!(g.nodes[z as usize].op, Op::Const(0));
    assert_eq!(g.nodes[z as usize].ty, IrType::Long);
}

/// `x % 1.0` is the fractional part of `x`, not zero.
#[test]
fn a_floating_point_remainder_by_one_is_not_simplified() {
    let (mut g, start) = graph_with_start();
    let d = g.add(Op::Param(0), IrType::Double, vec![start], None);
    // An integer-shaped `Const(1)` operand, so that the TYPE guard is what
    // refuses, not the absence of a `ConstF` match.
    let one = g.add(Op::Const(1), IrType::Double, vec![], None);
    let rem = g.add(Op::Rem, IrType::Double, vec![d, one], None);
    assert_eq!(simplified_through_user(&mut g, rem, IrType::Double), rem);
}

#[test]
fn a_self_compare_folds_and_a_self_and_is_the_operand() {
    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let le = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![x, x], None);
    let r = simplified_through_user(&mut g, le, IrType::Int);
    assert_eq!(g.nodes[r as usize].op, Op::Const(1));
    assert_eq!(g.nodes[r as usize].ty, IrType::Int);

    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let lt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![x, x], None);
    let r = simplified_through_user(&mut g, lt, IrType::Int);
    assert_eq!(g.nodes[r as usize].op, Op::Const(0));

    let (mut g, start) = graph_with_start();
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let and = g.add(Op::And, IrType::Int, vec![x, x], None);
    assert_eq!(simplified_through_user(&mut g, and, IrType::Int), x);
}

/// A self-compare of a floating-point operand is NaN-sensitive and must
/// never fold, even if one were ever spelled as `Op::Cmp`.
#[test]
fn a_self_compare_of_a_double_is_not_folded() {
    let (mut g, start) = graph_with_start();
    let d = g.add(Op::Param(0), IrType::Double, vec![start], None);
    let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![d, d], None);
    assert_eq!(simplified_through_user(&mut g, eq, IrType::Int), eq);
}

#[test]
fn a_guard_on_a_folded_true_condition_is_removed_and_its_token_stripped() {
    let (mut g, start) = graph_with_start();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let x = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let ten = g.add(Op::Const(10), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![ten, zero], None);
    let guard = g.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, cond], None);
    // The guarded arity: `[a, b, guard]`.
    let rem = g.add(Op::Rem, IrType::Int, vec![x, ten, guard], None);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, rem], None);
    g.exit = ret;

    assert!(fold_constants(&mut g), "`10 != 0` folds");
    assert!(eliminate_trivial_guards(&mut g));
    assert_eq!(g.nodes[guard as usize].op, Op::Dead);
    assert_eq!(
        g.nodes[rem as usize].inputs.as_slice(),
        &[x, ten][..],
        "the token edge is cut back to the unguarded arity, never left dangling"
    );
}

#[test]
fn a_guard_that_can_fire_is_kept() {
    let (mut g, start) = graph_with_start();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    // The uncommon-trap shape: a guard on constant zero always deopts.
    let trap = g.add(Op::Guard { bci: 7 }, IrType::Void, vec![ctrl, zero], None);
    // A constant whose low half is zero is read at the width the lowering
    // may test, and kept.
    let wide = g.add(Op::Const(1i64 << 32), IrType::Int, vec![], None);
    let wide_guard = g.add(Op::Guard { bci: 8 }, IrType::Void, vec![ctrl, wide], None);
    assert!(!eliminate_trivial_guards(&mut g));
    assert_ne!(g.nodes[trap as usize].op, Op::Dead);
    assert_ne!(g.nodes[wide_guard as usize].op, Op::Dead);
}

#[test]
fn a_null_test_of_a_fresh_allocation_or_the_receiver_folds() {
    let (mut g, start) = graph_with_start();
    g.receiver_param = Some(0);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let this = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let other = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let obj = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let ne_this = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![this, null], None);
    let eq_obj = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![null, obj], None);
    let ne_other = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![other, null], None);
    assert_eq!(null_compare_answer(&g, ne_this), Some(1));
    assert_eq!(null_compare_answer(&g, eq_obj), Some(0));
    assert_eq!(
        null_compare_answer(&g, ne_other),
        None,
        "a non-receiver parameter may be null"
    );
}

/// `p = new P(); p.x = 5; if (rare) return p.x;` with `rare` pruned to an
/// uncommon trap: no `Load` of `p` survives in the graph, but the deopt
/// hands `p` to the interpreter, which reads `p.x`. The store must stay.
#[test]
fn a_store_into_an_allocation_a_safepoint_names_is_not_write_only() {
    let (mut g, start) = graph_with_start();
    let alloc = g.add(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        IrType::Ref,
        vec![start],
        None,
    );
    let five = g.add(Op::Const(5), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Void,
        vec![alloc, five],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![start], None);
    g.exit = ret;
    g.safepoints
        .push(SafepointSnapshot::new(9, vec![alloc], vec![], Vec::new()));

    eliminate_dead_stores(&mut g);

    assert_eq!(
        g.nodes[store as usize].op,
        Op::Store(MemKind::Int),
        "a deopt can rebuild a frame holding the allocation, so its field \
         store is observable"
    );
}

#[test]
fn gvn_merges_a_commutative_op_across_operand_order() {
    let (mut g, start) = graph_with_start();
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let ab = g.add(Op::Add, IrType::Int, vec![a, b], None);
    let ba = g.add(Op::Add, IrType::Int, vec![b, a], None);
    let user = g.add(Op::Sub, IrType::Int, vec![ab, ba], None);
    gvn(&mut g);
    assert_eq!(g.nodes[ba as usize].op, Op::Dead, "`b + a` is `a + b`");
    assert_eq!(g.nodes[user as usize].inputs.as_slice(), &[ab, ab][..]);
}

/// Of two guards testing one condition at one program point, the one
/// nothing reads folds away (its unclaimed frame dropped) and the one with
/// a user stays; with users on both, the later folds into the earlier. A
/// guard another predicate planter owns stays.
#[test]
fn identical_guards_fold_into_one() {
    let (mut g, start) = graph_with_start();
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let o = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let cond = g.add(Op::ExactClassIs { class_id: 6 }, IrType::Int, vec![o], None);
    let frame = |g: &mut Graph| {
        g.push_safepoint(SafepointSnapshot::new(31, vec![o], vec![], Vec::new()));
        (g.safepoints.len() - 1) as u32
    };
    let guard = |g: &mut Graph, si: u32| {
        let id = g.add(Op::Guard { bci: 31 }, IrType::Void, vec![ctrl, cond], Some(31));
        assert!(g.set_node_frame_snapshot(id, si));
        id
    };
    let (f1, f2) = (frame(&mut g), frame(&mut g));
    let (g1, g2) = (guard(&mut g, f1), guard(&mut g, f2));
    let user = g.add(Op::Proj(0), IrType::Control, vec![g2], None);
    assert_eq!(merge_identical_guards(&mut g), 1);
    assert_eq!(g.nodes[g1 as usize].op, Op::Dead, "the unread guard folds");
    assert_eq!(g.nodes[user as usize].inputs.as_slice(), &[g2][..]);
    assert_eq!(g.safepoints.len(), 1, "the folded guard's frame is dropped");

    let f3 = frame(&mut g);
    let g3 = guard(&mut g, f3);
    let user3 = g.add(Op::Proj(0), IrType::Control, vec![g3], None);
    assert_eq!(merge_identical_guards(&mut g), 1, "both read: the later folds");
    assert_eq!(g.nodes[g3 as usize].op, Op::Dead);
    assert_eq!(g.nodes[user3 as usize].inputs.as_slice(), &[g2][..]);

    let f4 = frame(&mut g);
    let g4 = guard(&mut g, f4);
    g.predicate_guard_kinds.push((g4, 1));
    assert_eq!(merge_identical_guards(&mut g), 0, "another planter's guard stays");
    assert_ne!(g.nodes[g4 as usize].op, Op::Dead);
}

#[test]
fn gvn_keeps_operand_order_where_it_matters() {
    // `a - b` and `b - a` are different values.
    let (mut g, start) = graph_with_start();
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let ab = g.add(Op::Sub, IrType::Int, vec![a, b], None);
    let ba = g.add(Op::Sub, IrType::Int, vec![b, a], None);
    let lt_ab = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![a, b], None);
    let lt_ba = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![b, a], None);
    // An FP add commutes in value but not in NaN payload bits.
    let x = g.add(Op::Param(2), IrType::Double, vec![start], None);
    let y = g.add(Op::Param(3), IrType::Double, vec![start], None);
    let xy = g.add(Op::Add, IrType::Double, vec![x, y], None);
    let yx = g.add(Op::Add, IrType::Double, vec![y, x], None);
    gvn(&mut g);
    for n in [ab, ba, lt_ab, lt_ba, xy, yx] {
        assert_ne!(g.nodes[n as usize].op, Op::Dead, "node {n} must survive");
    }
}
