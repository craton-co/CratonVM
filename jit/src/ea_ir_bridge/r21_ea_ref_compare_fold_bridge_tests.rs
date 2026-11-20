use super::*;
use crate::ir::{CmpOp, Graph, IrType, Op};

/// `a = new T(); b = new T(); if (a == b) ...` (or `!=`) with both
/// confined: the compare folds to the constant and the `If` reads it.
fn two_fresh(cc: CmpOp) -> (Graph, ir::NodeId, ir::NodeId, ir::NodeId) {
    let mut g = Graph::empty(12);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let fresh = |g: &mut Graph, pc: usize| {
        g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            Some(pc),
        )
    };
    let a = fresh(&mut g, 0);
    let b = fresh(&mut g, 4);
    let cmp = g.add(Op::Cmp(cc), IrType::Int, vec![a, b], Some(8));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(8));
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl], Some(11));
    g.entry = start;
    g.exit = ret;
    (g, a, cmp, iff)
}

#[test]
fn a_compare_of_two_confined_allocations_folds_to_its_constant() {
    for (cc, want) in [(CmpOp::Eq, 0), (CmpOp::Ne, 1)] {
        let (mut g, a, cmp, iff) = two_fresh(cc);
        let (ea, id_map) = escape_analysis_from_ir(&g);
        assert_eq!(ea.nodes[id_map[cmp as usize]].op, escape_analysis::Op::RefCompare);
        let folds = escape_analysis::confined_ref_compare_folds_for_tests(&ea);
        assert_eq!(folds, vec![(id_map[cmp as usize], id_map[a as usize])]);
        assert_eq!(ir_fold_confined_ref_compares(&mut g, &id_map, &folds), 1);
        assert_eq!(g.nodes[cmp as usize].op, Op::Dead);
        let cond = g.nodes[iff as usize].inputs[1];
        assert_eq!(g.nodes[cond as usize].op, Op::Const(want));
    }
}

#[test]
fn nothing_folds_without_a_proof() {
    let (mut g, _a, cmp, _iff) = two_fresh(CmpOp::Eq);
    let (_ea, id_map) = escape_analysis_from_ir(&g);
    assert_eq!(ir_fold_confined_ref_compares(&mut g, &id_map, &[]), 0);
    assert!(matches!(g.nodes[cmp as usize].op, Op::Cmp(CmpOp::Eq)));
}
