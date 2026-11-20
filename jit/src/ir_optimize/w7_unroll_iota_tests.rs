// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::{CmpOp, Graph, IrType, Op, NO_NODE};

#[test]
fn w7_iota_reduction_yields_to_vector_loop() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero_i = g.add(Op::Const(0), IrType::Int, vec![], None);
    let zero_l = g.add(Op::Const(0), IrType::Long, vec![], None);
    let c1000 = g.add(Op::Const(1000), IrType::Int, vec![], None);
    let one_i = g.add(Op::Const(1), IrType::Int, vec![], None);

    let region = g.add(Op::Region, IrType::Control, vec![c0, NO_NODE], Some(6));
    let iv = g.add(Op::Phi, IrType::Int, vec![region, zero_i, NO_NODE], Some(6));
    let acc = g.add(
        Op::Phi,
        IrType::Long,
        vec![region, zero_l, NO_NODE],
        Some(6),
    );
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n], Some(6));
    let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(6));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(9));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(20));

    let biased = g.add(Op::Add, IrType::Int, vec![iv, c1000], Some(10));
    let wide = g.add(Op::I2L, IrType::Long, vec![biased], Some(12));
    let next_acc = g.add(Op::Add, IrType::Long, vec![acc, wide], Some(14));
    let next_iv = g.add(Op::Add, IrType::Int, vec![iv, one_i], Some(16));
    g.set_input(region, 1, stay);
    g.set_input(iv, 2, next_iv);
    g.set_input(acc, 2, next_acc);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(20));

    assert!(iota_reduction_candidate(&g));
    let allowed = headers_left_to_unroll(&g, None, &[region]);
    assert!(!allowed.contains(&region));
    assert!(!unroll_in_yielding_iota(&mut g, None, true));
}
