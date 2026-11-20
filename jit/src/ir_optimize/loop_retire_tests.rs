// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir_verify::verify_graph_at_phase;

fn affine_loop_graph(
    widen: bool,
    sub_acc: bool,
    offset_k: Option<(i64, bool)>,
    wrap_i2l_inner: bool,
) -> Graph {
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let iv0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let bound = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let acc_ty = if widen { IrType::Long } else { IrType::Int };
    let p0 = g.add(Op::Param(2), acc_ty, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let hdr = g.add(Op::Region, IrType::Control, vec![ctrl0, ctrl0], Some(10));
    let iv = g.add(Op::Phi, IrType::Int, vec![hdr, iv0, iv0], Some(10));
    let acc = g.add(Op::Phi, acc_ty, vec![hdr, p0, p0], Some(10));
    let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![iv, bound], Some(10));
    let iff = g.add(Op::If, IrType::Control, vec![hdr, cond], Some(10));
    let back = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(12));
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let term = if widen && !wrap_i2l_inner {
        let wide_iv = g.add(Op::I2L, IrType::Long, vec![iv], Some(12));
        match offset_k {
            Some((k, sub_k)) => {
                let kc = g.add(Op::Const(k), IrType::Long, vec![], None);
                g.add(
                    if sub_k { Op::Sub } else { Op::Add },
                    IrType::Long,
                    vec![wide_iv, kc],
                    Some(12),
                )
            }
            None => wide_iv,
        }
    } else {
        let term_i = match offset_k {
            Some((k, sub_k)) => {
                let kc = g.add(Op::Const(k), IrType::Int, vec![], None);
                g.add(
                    if sub_k { Op::Sub } else { Op::Add },
                    IrType::Int,
                    vec![iv, kc],
                    Some(12),
                )
            }
            None => iv,
        };
        if widen {
            g.add(Op::I2L, IrType::Long, vec![term_i], Some(12))
        } else {
            term_i
        }
    };
    let acc_next = g.add(
        if sub_acc { Op::Sub } else { Op::Add },
        acc_ty,
        vec![acc, term],
        Some(12),
    );
    let iv_next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(14));
    g.set_input(hdr, 1, back);
    g.set_input(iv, 2, iv_next);
    g.set_input(acc, 2, acc_next);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, acc], Some(20));
    g
}

#[test]
fn retires_affine_iv_int_and_widened_long_accumulators() {
    for (widen, sub_acc, off) in [
        (false, false, None),
        (false, true, Some((5, false))),
        (true, false, None),
        (true, false, Some((3, true))),
    ] {
        let mut g = affine_loop_graph(widen, sub_acc, off, false);
        let retired = retire_accumulator_loops(&mut g, None);
        assert_eq!(
            retired, 1,
            "expected retirement for widen={widen} sub={sub_acc} off={off:?}"
        );
        assert!(!g.nodes.iter().any(|n| n.op == Op::Phi));
        assert!(g.nodes.iter().any(|n| n.op == Op::UShr));
        assert!(verify_graph_at_phase(&g, "post_optimize").is_ok());
    }
    // `I2L(iv + k)` with `k > 1` wraps in 32-bit int before widening and must not retire.
    let mut g_wrap = affine_loop_graph(true, false, Some((3, false)), true);
    assert_eq!(retire_accumulator_loops(&mut g_wrap, None), 0);

    // `I2L(iv + 1)` under `iv < bound` never wraps `i32` and MUST retire (`lambda`).
    let mut g_plus1 = affine_loop_graph(true, false, Some((1, false)), true);
    assert_eq!(retire_accumulator_loops(&mut g_plus1, None), 1);
    assert!(!g_plus1.nodes.iter().any(|n| n.op == Op::Phi));
    assert!(verify_graph_at_phase(&g_plus1, "post_optimize").is_ok());

    // `I2L(iv - 1)` with unknown `iv_init` may wrap at `i32::MIN` and must not retire,
    // whereas with `iv_init = 0` it cannot wrap and retires cleanly.
    let mut g_sub1_param = affine_loop_graph(true, false, Some((1, true)), true);
    assert_eq!(retire_accumulator_loops(&mut g_sub1_param, None), 0);

    let mut g_sub1_zero = affine_loop_graph(true, false, Some((1, true)), true);
    let zero = g_sub1_zero.add(Op::Const(0), IrType::Int, vec![], None);
    let iv_phi = g_sub1_zero
        .nodes
        .iter()
        .position(|n| n.op == Op::Phi && n.ty == IrType::Int)
        .expect("iv phi") as NodeId;
    g_sub1_zero.set_input(iv_phi, 1, zero);
    assert_eq!(retire_accumulator_loops(&mut g_sub1_zero, None), 1);
    assert!(!g_sub1_zero.nodes.iter().any(|n| n.op == Op::Phi));
    assert!(verify_graph_at_phase(&g_sub1_zero, "post_optimize").is_ok());
}

#[test]
fn post_box_fold_cleanup_retires_unboxed_affine_loop() {
    let mut g = affine_loop_graph(true, false, Some((1, false)), false);
    let mut rep = 0usize;
    post_box_fold_cleanup(&mut g, &mut rep);
    assert_eq!(rep, 1);
    assert!(!g.nodes.iter().any(|n| n.op == Op::Phi));
    assert!(verify_graph_at_phase(&g, "post_optimize").is_ok());
}
