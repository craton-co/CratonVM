// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::optimize;
use super::strip_mine::{strip_mine_enabled, strip_mine_osr_enabled};
use crate::ir::{CmpOp, Graph, IrType, MemKind, NodeId, Op, SafepointSnapshot, NO_NODE};
use crate::ir_lower::{ir_drop_unreachable_homes_enabled, op_cannot_deopt};
use crate::ir_schedule::schedule;

/// `OpCostRows.arith`:
/// `static long arith(int n) { long s = 1; for (int i = 0; i < n; i++) s = s * 31 + i; return s; }`
///
/// With wide pure unrolling, unreachable-home dropping, and counted-loop
/// strip-mining enabled, `arith` unrolls 16x into `main` + `post`, `main` is
/// strip-mined so its inner latch skips the per-trip safepoint poll, and the
/// whole optimized graph stays trap-free.
#[test]
fn opcost_arith_wide_pure_unrolls_and_strip_mines() {
    assert!(
        strip_mine_enabled(),
        "strip_mine must be enabled by default"
    );
    assert!(
        !strip_mine_osr_enabled(),
        "strip_mine_osr stays off so post loops keep their shape"
    );
    assert!(
        ir_drop_unreachable_homes_enabled(),
        "ir_drop_unreachable_homes must be enabled by default"
    );

    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let s0 = g.add(Op::Const(1), IrType::Long, vec![], None);
    let i0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let k31 = g.add(Op::Const(31), IrType::Long, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(4));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, i0, NO_NODE], Some(4));
    let s_phi = g.add(Op::Phi, IrType::Long, vec![header, s0, NO_NODE], Some(4));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(6));
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(6));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let mul = g.add(Op::Mul, IrType::Long, vec![s_phi, k31], Some(12));
    let i_long = g.add(Op::I2L, IrType::Long, vec![i_phi], Some(14));
    let s_next = g.add(Op::Add, IrType::Long, vec![mul, i_long], Some(15));
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(17));

    assert!(g.set_input(header, 1, stay));
    assert!(g.set_input(i_phi, 2, i_next));
    assert!(g.set_input(s_phi, 2, s_next));
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, s_phi], Some(22));

    // Realistic javac operand-stack snapshot naming `s * 31` mid-body.
    g.push_safepoint(SafepointSnapshot::new(
        4,
        vec![n, s_phi, i_phi],
        vec![],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        12,
        vec![n, s_phi, i_phi],
        vec![mul],
        vec![],
    ));

    optimize(&mut g);

    assert_eq!(
        g.partially_unrolled_headers.len(),
        2,
        "expected [post, main] headers: {:?}",
        g.partially_unrolled_headers
    );
    let main_header = g
        .partially_unrolled_headers
        .iter()
        .copied()
        .find(|&h| g.nodes[h as usize].bytecode_pc.is_none())
        .expect("main loop header without bci");
    assert_eq!(
        g.strip_mined_inner_headers,
        vec![main_header],
        "the 16x wide pure main loop must be strip-mined"
    );

    let main_i_phi = g
        .nodes
        .iter()
        .find(|nd| nd.op == Op::Phi && nd.ty == IrType::Int && nd.input_opt(0) == Some(main_header))
        .expect("main loop induction phi");
    let step_node = &g.nodes[main_i_phi.inputs[2] as usize];
    assert_eq!(step_node.op, Op::Add);
    let has_stride_16 = step_node
        .inputs
        .iter()
        .any(|&inp| g.node_opt(inp).is_some_and(|c| c.op == Op::Const(16)));
    assert!(
        has_stride_16,
        "arith must unroll 16x under unroll_wide_pure: step inputs {:?}",
        step_node.inputs
    );

    assert!(
        g.nodes
            .iter()
            .filter(|nd| nd.op != Op::Dead)
            .all(|nd| op_cannot_deopt(&nd.op)),
        "optimized arith graph must remain trap-free so unreachable homes drop"
    );

    let sched = schedule(&g);
    let inner_latches = (0..sched.blocks.len())
        .filter(|&b| super::latch_enters_only_strip_inner(&g, &sched, b))
        .count();
    assert_eq!(
        inner_latches, 1,
        "strip-mined inner main latch must skip the per-trip safepoint poll"
    );
}

fn build_put_int_loop(accum_op: Op) -> (Graph, NodeId) {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let field_idx = g.add(Op::Const(0), IrType::Int, vec![], None);

    let hold = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 1,
            type_tag: b'L',
            is_volatile: false,
        },
        IrType::Ref,
        vec![entry, mem0],
        Some(0),
    );
    g.static_final_non_null.insert((9, 1));

    let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(5));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(5));
    let m_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![header, mem0, NO_NODE],
        Some(5),
    );
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(7));
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(7));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![stay, m_phi, hold, field_idx],
        Some(12),
    );
    g.field_decl_class.insert(load, 9);
    let next_val = g.add(accum_op, IrType::Int, vec![load, i_phi], Some(14));
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![stay, m_phi, hold, field_idx, next_val],
        Some(15),
    );
    g.field_decl_class.insert(store, 9);
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(18));

    assert!(g.set_input(header, 1, stay));
    assert!(g.set_input(i_phi, 2, i_next));
    assert!(g.set_input(m_phi, 2, store));

    let after_load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![exit, m_phi, hold, field_idx],
        Some(24),
    );
    g.field_decl_class.insert(after_load, 9);
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, after_load], Some(28));

    g.push_safepoint(SafepointSnapshot::new(
        5,
        vec![n, hold, i_phi],
        vec![],
        vec![],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        12,
        vec![n, hold, i_phi],
        vec![hold, load],
        vec![],
    ));

    (g, header)
}

/// `OpCostRows.putInt`:
/// - Linear `p.x += i` promotes `p.x` and then retires to closed form `O(1)`
///   (`[]` remaining loops).
/// - Non-linear `p.x ^= i` promotes `p.x`, unrolls 16x into `[post, main]`,
///   and strip-mines the `main` loop's per-trip safepoint poll.
#[test]
fn opcost_put_int_promotes_unrolls_reassociates_and_strip_mines() {
    let (mut g, header) = build_put_int_loop(Op::Add);
    optimize(&mut g);

    assert_eq!(g.promoted_cell_header_bcis, vec![5]);
    assert!(!g.promoted_cell_writebacks.is_empty());
    assert!(
        g.partially_unrolled_headers.is_empty(),
        "linear p.x += i must retire to closed form O(1): {:?}",
        g.partially_unrolled_headers
    );
    assert!(
        g.strip_mined_inner_headers.is_empty(),
        "retired linear p.x += i leaves no loop to strip-mine"
    );
    assert_eq!(
        g.nodes[header as usize].op,
        Op::Dead,
        "linear p.x += i loop header must be retired and eliminated"
    );
}

/// Non-linear `p.x ^= i`: promotes `p.x` into a header `Phi` accumulator with an
/// exit write-back, unrolls 16x into `[post, main]`, and strip-mines `main`.
#[test]
fn opcost_put_int_nonlinear_xor_promotes_unrolls_and_strip_mines() {
    let (mut g, _header) = build_put_int_loop(Op::Xor);
    optimize(&mut g);

    assert_eq!(g.promoted_cell_header_bcis, vec![5]);
    assert!(!g.promoted_cell_writebacks.is_empty());
    assert_eq!(
        g.partially_unrolled_headers.len(),
        2,
        "promoted non-linear putInt loop must unroll into [post, main]: {:?}",
        g.partially_unrolled_headers
    );
    let main_header = g
        .partially_unrolled_headers
        .iter()
        .copied()
        .find(|&h| g.nodes[h as usize].bytecode_pc.is_none())
        .expect("main loop header");
    assert_eq!(g.strip_mined_inner_headers, vec![main_header]);

    // No field store remains inside the main loop; only the conditional exit write-back exists.
    let main_phis: Vec<NodeId> = g
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, nd)| {
            nd.op == Op::Phi && nd.ty != IrType::Memory && nd.input_opt(0) == Some(main_header)
        })
        .map(|(id, _)| id as NodeId)
        .collect();
    assert_eq!(main_phis.len(), 2, "expected [iv, acc] phis on main header");

    let iv_phi = main_phis
        .iter()
        .copied()
        .find(|&p| {
            let back = g.nodes[p as usize].inputs[2];
            g.node_opt(back).is_some_and(|bn| {
                bn.op == Op::Add
                    && bn
                        .inputs
                        .iter()
                        .any(|&x| g.node_opt(x).is_some_and(|c| c.op == Op::Const(16)))
            })
        })
        .expect("main induction variable stepping by 16");
    let acc_phi = main_phis
        .into_iter()
        .find(|&p| p != iv_phi)
        .expect("promoted accumulator phi on main header");
    let acc_back = g.nodes[acc_phi as usize].inputs[2];
    assert_eq!(
        g.nodes[acc_back as usize].op,
        Op::Xor,
        "expected non-linear Op::Xor accumulator back-edge on main loop"
    );
}

/// Linear constant-step promoted loops (`static-counter-long`:
/// `for (int i = 0; i < n; i++) COUNTER += 3L;`) must still retire to closed
/// form in `loop_retire` before `unroll_promoted_loops` runs.
#[test]
fn opcost_static_counter_still_retires_before_promoted_unroll() {
    let mut g = Graph::empty(1);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let three = g.add(Op::Const(3), IrType::Long, vec![], None);

    let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(2));
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
    let m_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![header, mem0, NO_NODE],
        Some(2),
    );
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(4));
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
    let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let get = Op::LoadStatic {
        class_id: 9,
        field_index: 0,
        type_tag: b'J',
        is_volatile: false,
    };
    let load = g.add(get.clone(), IrType::Long, vec![stay, m_phi], Some(5));
    let sum = g.add(Op::Add, IrType::Long, vec![load, three], Some(7));
    let store = g.add(
        Op::StoreStatic {
            class_id: 9,
            field_index: 0,
            type_tag: b'J',
            is_volatile: false,
        },
        IrType::Memory,
        vec![stay, m_phi, sum],
        Some(8),
    );
    let next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(10));

    assert!(g.set_input(header, 1, stay));
    assert!(g.set_input(i_phi, 2, next));
    assert!(g.set_input(m_phi, 2, store));

    let after = g.add(get, IrType::Long, vec![exit, m_phi], Some(20));
    g.exit = g.add(Op::Return, IrType::Void, vec![exit, after], Some(24));
    g.push_safepoint(SafepointSnapshot::new(2, vec![n, i_phi], vec![], vec![]));
    g.direct_static_fields = vec![(9, 0)];

    optimize(&mut g);

    assert!(
        g.partially_unrolled_headers.is_empty(),
        "static-counter-long must retire instead of unrolling: {:?}",
        g.partially_unrolled_headers
    );
    assert!(
        g.strip_mined_inner_headers.is_empty(),
        "retired static-counter-long leaves no loop to strip-mine"
    );
    assert_eq!(
        g.nodes[header as usize].op,
        Op::Dead,
        "loop header must be retired and eliminated"
    );
}
