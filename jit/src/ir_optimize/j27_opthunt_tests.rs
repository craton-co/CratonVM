// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane OPTHUNT): tests for the optimizing tier's passes, written
//! while hunting miscompiles in `ir_optimize.rs`.

use super::resolve_ref_points_to;
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};

/// A loop header `Merge` and three reference phis on it, each of whose two
/// value inputs are the other two phis: refs rotated under a branch in a
/// loop, after the trivial-phi folds left only the cycle.
fn phi_triangle() -> (Graph, [NodeId; 3]) {
    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let header = g.add(Op::Merge, IrType::Control, vec![entry, entry], Some(2));
    let phi = |g: &mut Graph| g.add(Op::Phi, IrType::Ref, vec![header, NO_NODE, NO_NODE], None);
    let (a, b, c) = (phi(&mut g), phi(&mut g), phi(&mut g));
    for (p, x, y) in [(a, b, c), (b, c, a), (c, a, b)] {
        assert!(g.set_input(p, 1, x));
        assert!(g.set_input(p, 2, y));
    }
    (g, [a, b, c])
}

/// The phi cycle has no allocation or parameter behind it, so its points-to
/// set is unknown. Before the walk stopped at the first unknown input it
/// visited a full binary tree to the depth cap (about 2^33 calls) to say
/// so; this test then ran for minutes instead of microseconds.
#[test]
fn j27_opthunt_a_phi_cycle_with_two_phi_inputs_resolves_unknown_at_once() {
    let (g, [a, b, c]) = phi_triangle();
    for p in [a, b, c] {
        assert!(resolve_ref_points_to(&g, p, 0).allocs.is_none());
    }
}

/// The early stop keeps the answer: a phi of a parameter and a fresh
/// allocation is that allocation plus a pre-existing ref, and one unknown
/// input (a load) still poisons the set wherever it sits.
#[test]
fn j27_opthunt_points_to_answers_are_unchanged_by_the_early_stop() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let fresh = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![entry, mem],
        Some(1),
    );
    let field = g.add(Op::Const(0), IrType::Int, vec![], None);
    let loaded = g.add(
        Op::Load(crate::ir::MemKind::Ref),
        IrType::Ref,
        vec![entry, mem, p, field],
        Some(3),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![entry, entry], Some(5));
    let known = g.add(Op::Phi, IrType::Ref, vec![join, p, fresh], None);
    let pt = resolve_ref_points_to(&g, known, 0);
    let allocs = pt.allocs.expect("a param and a new are both known");
    assert_eq!(allocs.len(), 1);
    assert!(allocs.contains(&fresh));
    assert!(pt.has_pre);
    for inputs in [vec![join, loaded, fresh], vec![join, fresh, loaded]] {
        let phi = g.add(Op::Phi, IrType::Ref, inputs, None);
        assert!(resolve_ref_points_to(&g, phi, 0).allocs.is_none());
    }
}
