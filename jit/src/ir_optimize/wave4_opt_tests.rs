// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::SafepointSnapshot;

#[test]
fn final_typecheck_rewrites_if_cmp_eq_exact_class_is_zero_and_swaps_projs() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
    let exact = g.add(
        Op::ExactClassIs { class_id: 42 },
        IrType::Int,
        vec![obj],
        Some(1),
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let neg = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![exact, zero], Some(2));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, neg], Some(2));
    let miss_proj = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(2));
    let hit_proj = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(2));
    let join = g.add(
        Op::Merge,
        IrType::Control,
        vec![miss_proj, hit_proj],
        Some(3),
    );
    let c10 = g.add(Op::Const(10), IrType::Int, vec![], None);
    let c20 = g.add(Op::Const(20), IrType::Int, vec![], None);
    let phi = g.add(Op::Phi, IrType::Int, vec![join, c10, c20], Some(3));
    g.exit = g.add(Op::Return, IrType::Void, vec![join, phi], Some(4));

    let changed = final_typecheck_fold::final_typecheck_fold(&mut g);
    assert_eq!(changed, 1);
    assert_eq!(g.nodes[iff as usize].inputs[1], exact);
    assert_eq!(g.nodes[miss_proj as usize].op, Op::Proj(1));
    assert_eq!(g.nodes[hit_proj as usize].op, Op::Proj(0));
    assert_eq!(g.nodes[neg as usize].op, Op::Dead);
}

#[test]
fn final_typecheck_folds_ne_zero_and_keeps_snapshot_cond_intact() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], Some(0));
    let exact = g.add(
        Op::ExactClassIs { class_id: 77 },
        IrType::Int,
        vec![obj],
        Some(1),
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![zero, exact], Some(2));
    let eq = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![zero, exact], Some(3));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, eq], Some(3));
    let p0 = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(3));
    let p1 = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(3));
    let join = g.add(Op::Merge, IrType::Control, vec![p0, p1], Some(4));
    g.push_safepoint(SafepointSnapshot::new(3, vec![eq], vec![], vec![]));
    g.exit = g.add(Op::Return, IrType::Void, vec![join, ne], Some(5));

    let changed = final_typecheck_fold::final_typecheck_fold(&mut g);
    assert_eq!(
        changed, 1,
        "Cmp(Ne, 0, ExactClassIs) should fold to ExactClassIs"
    );
    assert_eq!(g.nodes[ne as usize].op, Op::Dead);
    assert_eq!(g.nodes[g.exit as usize].inputs[1], exact);
    // `eq` is referenced by a safepoint snapshot, so `If(eq)` must NOT be rewritten.
    assert_eq!(g.nodes[iff as usize].inputs[1], eq);
    assert_eq!(g.nodes[p0 as usize].op, Op::Proj(0));
    assert_eq!(g.nodes[p1 as usize].op, Op::Proj(1));
}

#[test]
fn fold_range_redundant_masks_drops_char_mask_on_bounded_nonneg_sum() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let i = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k15 = g.add(Op::Const(15), IrType::Int, vec![], None);
    let ka = g.add(Op::Const(i64::from(b'a')), IrType::Int, vec![], None);
    let kffff = g.add(Op::Const(0xFFFF), IrType::Int, vec![], None);
    let masked_i = g.add(Op::And, IrType::Int, vec![i, k15], None);
    let sum = g.add(Op::Add, IrType::Int, vec![masked_i, ka], None);
    let char_mask = g.add(Op::And, IrType::Int, vec![sum, kffff], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, char_mask], None);

    assert!(fold_range_redundant_masks(&mut g));
    assert_eq!(g.nodes[char_mask as usize].op, Op::Dead);
    assert_eq!(g.nodes[g.exit as usize].inputs[1], sum);
}

#[test]
fn fold_range_redundant_masks_rejects_negative_or_unbounded_masks() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let i = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k15 = g.add(Op::Const(15), IrType::Int, vec![], None);
    let ka = g.add(Op::Const(i64::from(b'a')), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let kneg16 = g.add(Op::Const(-16), IrType::Int, vec![], None);
    let kfffe = g.add(Op::Const(0xFFFE), IrType::Int, vec![], None);
    let kffff = g.add(Op::Const(0xFFFF), IrType::Int, vec![], None);

    let masked_i = g.add(Op::And, IrType::Int, vec![i, k15], None);
    let sum = g.add(Op::Add, IrType::Int, vec![masked_i, ka], None);
    let neg_lo = g.add(Op::Sub, IrType::Int, vec![masked_i, one], None);

    let and_neg_mask = g.add(Op::And, IrType::Int, vec![sum, kneg16], None);
    let and_hole_mask = g.add(Op::And, IrType::Int, vec![sum, kfffe], None);
    let and_neg_lo = g.add(Op::And, IrType::Int, vec![neg_lo, kffff], None);
    let and_unbounded = g.add(Op::And, IrType::Int, vec![i, kffff], None);

    let s1 = g.add(
        Op::Add,
        IrType::Int,
        vec![and_neg_mask, and_hole_mask],
        None,
    );
    let s2 = g.add(Op::Add, IrType::Int, vec![and_neg_lo, and_unbounded], None);
    let total = g.add(Op::Add, IrType::Int, vec![s1, s2], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, total], None);

    assert!(!fold_range_redundant_masks(&mut g));
    assert_eq!(g.nodes[and_neg_mask as usize].op, Op::And);
    assert_eq!(g.nodes[and_hole_mask as usize].op, Op::And);
    assert_eq!(g.nodes[and_neg_lo as usize].op, Op::And);
    assert_eq!(g.nodes[and_unbounded as usize].op, Op::And);
}
