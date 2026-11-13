// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::IrBuilder;

#[test]
fn a_successful_epoch_guard_also_covers_entry_reference_parameters() {
    // int f(A a, B b, int x) { int y=a.x; Object later=b.ref; return y+b.y+x; }
    let code = [
        0x2a, 0xb4, 0, 1, 0x2b, 0xb4, 0, 2, 0x57, 0x2b, 0xb4, 0, 3, 0x60, 0x1c, 0x60, 0xac,
    ];
    let mut b = IrBuilder::new(3, 3);
    b.set_param_types(&[IrType::Ref, IrType::Ref, IrType::Int]);
    b.set_field_info(HashMap::from([
        (1, (0, b'I')),
        (5, (1, b'L')),
        (10, (2, b'I')),
    ]));
    let g = Box::leak(Box::new(
        b.build(&code, code.len())
            .expect("parameter and later-reference graph"),
    ));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
    let param = |n| {
        g.nodes
            .iter()
            .position(|node| node.op == Op::Param(n))
            .unwrap() as NodeId
    };
    let [a, b, int] = [param(0), param(1), param(2)];
    let first = g
        .nodes
        .iter()
        .position(|node| node.bytecode_pc == Some(1) && node.op == Op::Load(MemKind::Int))
        .unwrap() as NodeId;
    let later = g
        .nodes
        .iter()
        .position(|node| {
            node.bytecode_pc == Some(5)
                && node.ty == IrType::Ref
                && node.op == Op::Load(MemKind::Ref)
        })
        .unwrap() as NodeId;
    let second = g
        .nodes
        .iter()
        .position(|node| node.bytecode_pc == Some(10) && node.op == Op::Load(MemKind::Int))
        .unwrap() as NodeId;
    for (deopt, can_trap) in [(false, true), (true, false), (true, true)] {
        let mut lo = super::tests::lowerer_over(g, schedule, plan, 16384);
        lo.layout_guard_deopt_on = deopt;
        lo.layout_guard_lone_pcs.insert(1);
        lo.layout_guard_lone_sites = None;
        lo.cur_def = Some(first);
        lo.cur_bci = 1;
        lo.cur_node_pc = Some(1);
        for p in [a, b, int] {
            lo.alloc_slot(p);
        }
        let block = lo.field_proof_block().unwrap();
        assert!(
            lo.field_site_may_trap(1, true),
            "fixture must support the real deopting guard"
        );
        let rejoin = lo.emit_field_layout_guard(a, 1, can_trap);
        assert!(lo.take_latched_bailout().is_none());
        if !deopt || !can_trap {
            assert!(
                rejoin.is_some(),
                "a helper rejoin establishes no global witness"
            );
            assert!(lo.layout_proven.is_empty());
            continue;
        }
        assert!(rejoin.is_none());
        assert!(
            lo.field_proof_holds(&lo.layout_proven, a, block, false),
            "the base proof is the premise"
        );
        assert!(
            lo.field_proof_holds(&lo.layout_proven, b, block, false),
            "an entry Ref already existed before the successful epoch read"
        );
        assert!(
            !lo.field_proof_holds(&lo.layout_proven, later, block, false),
            "a later field read could obtain an object published after a replacement"
        );
        assert!(
            !lo.layout_proven.iter().any(|&(node, _)| node == int),
            "an integer parameter is not a receiver proof"
        );
        let before = lo.buf.pos();
        lo.cur_def = Some(second);
        lo.cur_bci = 10;
        lo.cur_node_pc = Some(10);
        assert!(lo.emit_field_layout_guard(b, 10, true).is_none());
        assert_eq!(
            lo.buf.pos(),
            before,
            "the second entry receiver needs no repeated epoch compare"
        );
        assert!(
            lo.compact_proven.is_empty(),
            "epoch validity says nothing about compactness or nullness"
        );
    }
}

#[test]
fn dominating_exact_class_and_str_coder_load_skip_redundant_compact_checks() {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    let c = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![s], None);
    let cls = g.add(Op::ExactClassIs { class_id: 42 }, IrType::Int, vec![recv], Some(1));
    let _gd = g.add(Op::Guard { bci: 1 }, IrType::Void, vec![c, cls], Some(1));
    let use_node = g.add(Op::Return, IrType::Void, vec![c, recv], Some(2));
    g.exit = use_node;
    let g = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
    let mut lo = super::tests::lowerer_over(g, schedule, plan, 16384);
    lo.cur_def = Some(use_node);
    let blk = schedule.node_to_block[use_node as usize];
    assert_eq!(lo.dominating_exact_class(recv, blk), Some(42));

    let layout = crate::StringFieldLayout {
        value_field_index: 0,
        value_compact_offset: 8,
        value_compact_is_narrow: false,
        value_legacy_offset: 16,
        hash_field_index: 1,
        hash_compact_offset: 12,
        hash_legacy_offset: 24,
        has_coder: true,
        coder_field_index: 2,
        coder_compact_offset: 16,
        coder_legacy_offset: 32,
        coder_compact_is_byte: true,
        string_class_id: 42,
        builder: None,
        array_list: None,
        hash_is_zero_compact_offset: None,
        locale_language: None,
    };
    let compact_test = [
        0xF6,
        0x40,
        cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8,
        cratonvm_types::GC_FLAG_COMPACT,
    ];

    let start_unproven = lo.buf.pos();
    lo.emit_str_coder_load(&layout, false);
    let bytes_unproven = lo.buf.as_slice()[start_unproven..].to_vec();
    assert!(bytes_unproven.windows(compact_test.len()).any(|w| w == compact_test.as_slice()));

    let start_proven = lo.buf.pos();
    lo.emit_str_coder_load(&layout, true);
    let bytes_proven = lo.buf.as_slice()[start_proven..].to_vec();
    assert!(bytes_proven.len() < bytes_unproven.len());
    assert!(!bytes_proven.windows(compact_test.len()).any(|w| w == compact_test.as_slice()));
}

#[test]
fn small_scalar_fill_emits_gpr_stores_without_vzeroupper() {
    let mut g = Graph::empty(0);
    let s = g.add(Op::Start, IrType::Control, vec![], None);
    let c = g.add(Op::Proj(0), IrType::Control, vec![s], None);
    let val = g.add(Op::Const(-1), IrType::Int, vec![], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![c, val], Some(0));
    let g = Box::leak(Box::new(g));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan = Box::leak(Box::new(plan_slots(g, schedule, None)));
    let mut lo = super::tests::lowerer_over(g, schedule, plan, 16384);
    let start = lo.buf.pos();
    lo.emit_small_scalar_fill(val, 4);
    let emitted = &lo.buf.as_slice()[start..];
    let store_count = emitted.windows(4).filter(|w| w == &[0x44, 0x89, 0x4C, 0x90]).count();
    assert_eq!(store_count, 3, "1..=3 element unroll emits three i32 GPR stores");
    assert!(!emitted.windows(3).any(|w| w == &[0xC5, 0xF8, 0x77]), "must not emit VZEROUPPER");
}
