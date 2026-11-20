// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

#[test]
fn plans_pure_iv_reduction_and_i2l_of_int_expr() {
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let n_param = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k_param = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let zero_l = g.add(Op::Const(0), IrType::Long, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let hdr = g.add(Op::Region, IrType::Control, vec![ctrl0, ctrl0], Some(10));
    let iv = g.add(Op::Phi, IrType::Int, vec![hdr, zero, zero], Some(10));
    let acc = g.add(Op::Phi, IrType::Long, vec![hdr, zero_l, zero_l], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, n_param], Some(10));
    let iff = g.add(Op::If, IrType::Control, vec![hdr, cond], Some(10));
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(12));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(20));

    let iv_plus_k = g.add(Op::Add, IrType::Int, vec![iv, k_param], Some(12));
    let wide = g.add(Op::I2L, IrType::Long, vec![iv_plus_k], Some(12));
    let acc_next = g.add(Op::Add, IrType::Long, vec![acc, wide], Some(12));
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(14));
    g.set_input(hdr, 1, back);
    g.set_input(iv, 2, iv_next);
    g.set_input(acc, 2, acc_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(20));

    let schedule = crate::ir_schedule::schedule(&g);
    let opts = VecOpts {
        reduce: true,
        widen: true,
        scoped_store: true,
        ..VecOpts::default()
    };
    let (plans, refusals) = plan_vector_loops_with(
        &g,
        &schedule,
        &|c| crate::ir_lower::ctrl_block_of(&g, &schedule, c),
        opts,
    );
    assert!(refusals.is_empty(), "unexpected refusals: {refusals:?}");
    assert_eq!(plans.len(), 1);
    let p = &plans[0];
    assert!(p.bases.is_empty());
    assert!(p.iota);
    assert_eq!(p.kind, VecKind::Long);
    assert_eq!(
        p.value,
        VExpr::I2L(Box::new(VExpr::Bin(
            VOp::Add,
            Box::new(VExpr::Iota),
            Box::new(VExpr::Inv(0))
        )))
    );

    let regs = StepRegs {
        inv_regs: &[4, 5, 6],
        acc: Some(3),
        nan: None,
        temps: &[0, 1, 2],
    };
    let code = step_code(p, &regs, 0).expect("step_code emits VPMOVSXDQ");
    assert!(
        code.windows(4)
            .any(|w| w[0] == 0xC4 && (w[1] & 0x1F) == 2 && (w[2] & 0x07) == 5 && w[3] == 0x25),
        "expected VPMOVSXDQ ymm, xmm encoding"
    );
}
