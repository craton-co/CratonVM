// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Unit tests for `OpCostRows.sync` loop monitor block coarsening (`VthreadBoundaryTest`
//! pre-enter guard admission and 16x coarsening of tiny single-monitor bodies).

use super::*;
use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, SafepointSnapshot, NO_NODE};

fn build_opcost_sync_graph(invariant_lock: bool) -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::empty(1);
    g.direct_static_fields = vec![(9, 0), (9, 1)];
    g.static_final_non_null.insert((9, 0));
    g.trusted_final_static_fields = vec![(9, 0)];

    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);

    let hoisted_lock = if invariant_lock {
        Some(g.add(
            Op::LoadStatic {
                class_id: 9,
                field_index: 0,
                type_tag: b'L',
                is_volatile: false,
            },
            IrType::Ref,
            vec![entry, mem0],
            Some(2),
        ))
    } else {
        None
    };

    let header = g.add(Op::Merge, IrType::Control, vec![entry, NO_NODE], Some(4));
    let mem_phi = g.add(
        Op::Phi,
        IrType::Memory,
        vec![header, mem0, NO_NODE],
        Some(4),
    );
    let i_phi = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(4));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![i_phi, n], Some(6));
    let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(6));
    let body_entry = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let loop_exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);

    let lock_read = hoisted_lock.unwrap_or_else(|| {
        g.add(
            Op::LoadStatic {
                class_id: 9,
                field_index: 0,
                type_tag: b'L',
                is_volatile: false,
            },
            IrType::Ref,
            vec![body_entry, mem_phi],
            Some(9),
        )
    });

    let vtest = g.add(
        Op::VthreadBoundaryTest { offset: 8, mask: 1 },
        IrType::Int,
        vec![body_entry],
        Some(12),
    );
    let vguard = g.add(
        Op::Guard { bci: 12 },
        IrType::Void,
        vec![body_entry, vtest],
        Some(12),
    );
    let enter_mem = if invariant_lock { mem_phi } else { lock_read };
    let enter = g.add(
        Op::MonitorEnter,
        IrType::Memory,
        vec![body_entry, enter_mem, lock_read],
        Some(12),
    );
    let cnt_load = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 1,
            type_tag: b'I',
            is_volatile: false,
        },
        IrType::Int,
        vec![body_entry, enter],
        Some(15),
    );
    let cnt_next = g.add(Op::Add, IrType::Int, vec![cnt_load, one], Some(18));
    let cnt_store = g.add(
        Op::StoreStatic {
            class_id: 9,
            field_index: 1,
            type_tag: b'I',
            is_volatile: false,
        },
        IrType::Memory,
        vec![body_entry, cnt_load, cnt_next],
        Some(19),
    );
    let m_exit = g.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![body_entry, cnt_store, lock_read],
        Some(22),
    );
    let i_next = g.add(Op::Add, IrType::Int, vec![i_phi, one], Some(25));

    assert!(g.set_input(header, 1, body_entry));
    assert!(g.set_input(mem_phi, 2, m_exit));
    assert!(g.set_input(i_phi, 2, i_next));

    let ret_read = g.add(
        Op::LoadStatic {
            class_id: 9,
            field_index: 1,
            type_tag: b'I',
            is_volatile: false,
        },
        IrType::Int,
        vec![loop_exit, mem_phi],
        Some(30),
    );
    g.exit = g.add(
        Op::Return,
        IrType::Void,
        vec![loop_exit, ret_read],
        Some(33),
    );

    g.push_safepoint(SafepointSnapshot::new(
        12,
        vec![n, i_phi, lock_read],
        vec![lock_read],
        vec![],
    ));
    let vguard_si = (g.safepoints.len() - 1) as u32;
    assert!(g.set_node_frame_snapshot(vguard, vguard_si));
    g.push_safepoint(SafepointSnapshot::new(
        15,
        vec![n, i_phi, lock_read],
        vec![lock_read],
        vec![lock_read],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        19,
        vec![n, i_phi, lock_read],
        vec![cnt_next],
        vec![lock_read],
    ));
    g.push_safepoint(SafepointSnapshot::new(
        22,
        vec![n, i_phi, lock_read],
        vec![lock_read],
        vec![lock_read],
    ));

    (g, header, vtest, vguard)
}

#[test]
fn opcost_sync_static_final_lock_with_vthread_guard_coarsens_x16() {
    for invariant_lock in [false, true] {
        let (mut g, header, vtest, vguard) = build_opcost_sync_graph(invariant_lock);
        let plan = plan_loop_block_coarsening(&g, header)
            .unwrap_or_else(|why| panic!("invariant_lock={invariant_lock} refused: {why:?}"));
        assert_eq!(plan.factor, 16, "invariant_lock={invariant_lock}");
        assert_eq!(plan.pre, vec![vguard]);
        assert!(plan.pre_float.contains(&vtest));
        assert!(!plan.pre_float.contains(&plan.body_entry));
        assert_eq!(coarsen_loop_sync_windows(&mut g), 1);

        let store_count = g
            .nodes
            .iter()
            .filter(|n| matches!(n.op, Op::StoreStatic { .. }))
            .count();
        // 1 in the slow tail loop + 16 in the coarsened fast window.
        assert_eq!(store_count, 17, "invariant_lock={invariant_lock}");
    }
}

#[test]
fn opcost_sync_static_final_mem_phi_token_coarsens_x16() {
    // Trusted static final `LoadStatic` in `body_entry` whose `MonitorEnter` passes
    // `e_tok == mem_phi` (as produced by `getstatic` when `static_final_non_null` holds).
    let (mut g, header, vtest, vguard) = build_opcost_sync_graph(false);
    let enter_id = g
        .nodes
        .iter()
        .position(|n| n.op == Op::MonitorEnter)
        .expect("enter") as NodeId;
    let mem_phi = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        .expect("mem_phi") as NodeId;
    g.set_input(enter_id, 1, mem_phi);

    let plan = plan_loop_block_coarsening(&g, header)
        .unwrap_or_else(|why| panic!("mem_phi static lock refused: {why:?}"));
    assert_eq!(plan.factor, 16);
    assert_eq!(plan.monitor, BlockMonitor::Static);
    assert!(plan.trusted_final);
    assert_eq!(plan.pre, vec![vguard]);
    assert!(plan.pre_float.contains(&vtest));
    assert_eq!(coarsen_loop_sync_windows(&mut g), 1);
}

#[test]
fn opcost_sync_indy_root_ref_hoisted_by_licm_and_coarsens_x16() {
    let (mut g, header, _vtest, _vguard) = build_opcost_sync_graph(false);
    let body_entry = g.nodes[header as usize].inputs[1];
    let enter_id = g
        .nodes
        .iter()
        .position(|n| n.op == Op::MonitorEnter)
        .expect("enter") as NodeId;
    let exit_id = g
        .nodes
        .iter()
        .position(|n| n.op == Op::MonitorExit)
        .expect("exit") as NodeId;
    let mem_phi = g
        .nodes
        .iter()
        .position(|n| n.op == Op::Phi && n.ty == IrType::Memory)
        .expect("mem_phi") as NodeId;
    let old_lock = g.nodes[enter_id as usize].inputs[2];
    let root_ref = g.add(
        Op::IndyRootRef {
            slot_addr: 0xABCD_0000,
        },
        IrType::Ref,
        vec![body_entry],
        Some(9),
    );
    g.const_ref_meta.insert(
        0xABCD_0000,
        crate::ir::ConstRefMeta {
            non_null: true,
            exact_class_id: Some(1),
            prim_array_tag: None,
            array_length: None,
            static_field: Some((9, 0)),
        },
    );
    g.set_input(enter_id, 1, mem_phi);
    g.replace_all_uses(old_lock, root_ref);
    g.kill(old_lock);

    let pre = g.nodes[header as usize].inputs[0];
    let outcome = super::plant_null_base_load_predicates::licm_outcome(&mut g);
    assert!(outcome.moved, "LICM must hoist Op::IndyRootRef");
    assert_eq!(
        g.nodes[root_ref as usize].inputs,
        vec![pre],
        "IndyRootRef must move to loop preheader"
    );
    assert_eq!(coarsen_loop_sync_windows(&mut g), 1);
}
