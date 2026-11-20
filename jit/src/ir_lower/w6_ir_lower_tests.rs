// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

fn mk_lowerer(g: Graph) -> (&'static Graph, Lowerer<'static>) {
    let g: &'static Graph = Box::leak(Box::new(g));
    let sched: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(g)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(g, sched, None)));
    let lo = tests::lowerer_over(g, sched, plan, 4096);
    (g, lo)
}

#[test]
fn layout_guard_lone_allows_multiple_readonly_reads_and_rejects_two_stores() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let v1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let _ld1 = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        vec![ctrl, mem, obj, f0],
        Some(10),
    );
    let _len = g.add(Op::ArrayLength, IrType::Int, vec![ctrl, mem, obj], Some(10));
    let _al = g.add(
        Op::ArrayLoad(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, obj, zero],
        Some(10),
    );
    let _gd = g.add(
        Op::Guard { bci: 10 },
        IrType::Control,
        vec![ctrl, v1],
        Some(10),
    );
    let s0 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, obj, f0, v1],
        Some(20),
    );
    let _s1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, s0, obj, f0, v1],
        Some(20),
    );
    let lone_pcs = layout_guard_lone_pcs(&g);
    assert!(lone_pcs.contains(&10), "read-only cluster at pc 10 is lone");
    assert!(!lone_pcs.contains(&20), "two stores at pc 20 are not lone");
    let lone_sites = layout_guard_lone_sites(&g);
    assert!(lone_sites.contains(&(10, None)));
    assert!(!lone_sites.contains(&(20, None)));
}

#[test]
fn exact_class_non_null_bytes_omit_null_test() {
    let b = exact_class_test_flags_non_null_bytes(0x1234_5678);
    assert_eq!(b.len(), 13);
    assert_eq!(b[0], 0xF6);
    assert_eq!(b[4], 0x75, "JNE skip CMP on non-plain object");
    assert_eq!(b[5], 7);
    assert_eq!(&b[6..9], &[0x81, 0x78, 0x00]);
    assert_eq!(&b[9..13], &0x1234_5678u32.to_le_bytes());
}

#[test]
fn const_true_guard_cannot_trap_and_array_known_non_boolean_classifies_byte_arrays() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let len = g.add(Op::Const(16), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let g_true = g.add(
        Op::Guard { bci: 5 },
        IrType::Control,
        vec![ctrl, one],
        Some(5),
    );
    let g_false = g.add(
        Op::Guard { bci: 6 },
        IrType::Control,
        vec![ctrl, zero],
        Some(6),
    );
    assert!(guard_const_true_cannot_trap(&g, &g.nodes[g_true as usize]));
    assert!(!guard_const_true_cannot_trap(
        &g,
        &g.nodes[g_false as usize]
    ));
    assert!(node_cannot_deopt(&g, &g.nodes[g_true as usize]));
    assert!(!node_cannot_deopt(&g, &g.nodes[g_false as usize]));

    let byte_arr = g.add(
        Op::NewArray {
            element_type: 8,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        None,
    );
    let bool_arr = g.add(
        Op::NewArray {
            element_type: 4,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        None,
    );
    let phi_bytes = g.add(Op::Phi, IrType::Ref, vec![ctrl, byte_arr, byte_arr], None);
    let phi_mixed = g.add(Op::Phi, IrType::Ref, vec![ctrl, byte_arr, bool_arr], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, phi_bytes], None);
    let (_, lo) = mk_lowerer(g);
    assert!(lo.array_known_non_boolean(byte_arr));
    assert!(lo.array_known_non_boolean(phi_bytes));
    assert!(!lo.array_known_non_boolean(bool_arr));
    assert!(!lo.array_known_non_boolean(phi_mixed));
}

#[test]
fn final_instanceof_class_id_and_drop_unused_const_homes() {
    let (ptr, len) = crate::intern_typecheck_target_with_finality("w6/FinalBox", Some(99), true);
    let op = Op::InstanceOf {
        name_ptr: ptr as usize,
        name_len: len,
        seen_cell: 0,
        header_only: false,
    };
    assert_eq!(Lowerer::final_instanceof_class_id(&op), Some(99));

    let (aptr, alen) =
        crate::intern_typecheck_target_with_finality("[Lw6/FinalBox;", Some(100), true);
    let aop = Op::InstanceOf {
        name_ptr: aptr as usize,
        name_len: alen,
        seen_cell: 0,
        header_only: false,
    };
    assert_eq!(Lowerer::final_instanceof_class_id(&aop), None);

    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let p0 = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let k7 = g.add(Op::Const(7), IrType::Int, vec![], None);
    let null_c = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let sum = g.add(Op::Add, IrType::Int, vec![p0, k7], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    let _cmp = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![null_c, null_c], None);
    let (_, mut lo) = mk_lowerer(g);
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.elide_excluded = vec![false; lo.graph.nodes.len()];
    lo.phi_copy_regs_on = true;
    lo.const_imm_on = true;
    lo.mir_mode = MirMode::Off;
    lo.drop_guaranteed_unused_const_homes();
    assert!(lo.is_dropped_const(k7));
    assert!(lo.is_dropped_const(null_c));
}

#[test]
fn final_instanceof_branch_fuses_and_proves_exact_class_and_checkcast_outlines() {
    let (ptr, len) = crate::intern_typecheck_target_with_finality("w6/FinalElem", Some(177), true);
    let mut g = Graph::empty(32);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let inst = g.add(
        Op::InstanceOf {
            name_ptr: ptr as usize,
            name_len: len,
            seen_cell: 0,
            header_only: false,
        },
        IrType::Int,
        vec![ctrl, mem, obj],
        Some(4),
    );
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cmp = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![inst, zero], None);
    let br = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(5));
    let p_true = g.add(Op::Proj(0), IrType::Control, vec![br], None);
    let p_false = g.add(Op::Proj(1), IrType::Control, vec![br], None);
    let cast = g.add(
        Op::CheckCast {
            name_ptr: ptr as usize,
            name_len: len,
        },
        IrType::Ref,
        vec![p_true, mem, obj],
        Some(8),
    );
    let _r1 = g.add(Op::Return, IrType::Void, vec![p_true, cast], None);
    let _r2 = g.add(Op::Return, IrType::Void, vec![p_false, obj], None);
    g.exit = _r1;

    let (_, mut lo) = mk_lowerer(g);
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.defined_nodes = vec![false; lo.graph.nodes.len()];
    lo.elide_excluded = vec![false; lo.graph.nodes.len()];
    lo.prepare_fusion_tables();
    assert!(
        lo.fused_cmp[inst as usize],
        "final InstanceOf fuses into If(Cmp)"
    );
    assert!(lo.fused_cmp[cmp as usize], "Cmp(Ne, InstanceOf, 0) fuses");
    let true_blk = lo.schedule.node_to_block[cast as usize];
    assert!(lo.dominating_instanceof_proves_non_null(obj, true_blk));
    assert_eq!(lo.dominating_exact_class(obj, true_blk), Some(177));
    lo.cur_lower_block = true_blk;
    lo.alloc_slot(obj);
    let cast_slot = lo.alloc_slot(cast);
    assert!(lo.try_elide_proven_checkcast(cast, obj, cast_slot, false, 177));
}

#[test]
fn fused_add_i2l_plans_and_emits_without_rcx_copy() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let acc = g.add(Op::Param(0), IrType::Long, vec![start], None);
    let x = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let wid = g.add(Op::I2L, IrType::Long, vec![x], None);
    let sum = g.add(Op::Add, IrType::Long, vec![acc, wid], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, sum], None);
    let (_, mut lo) = mk_lowerer(g);
    lo.mir_mode = MirMode::Off;
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.defined_nodes = vec![false; lo.graph.nodes.len()];
    lo.elide_excluded = vec![false; lo.graph.nodes.len()];
    lo.prepare_fusion_tables();
    assert!(lo.fused_i2l[wid as usize]);
    lo.alloc_slot(acc);
    lo.alloc_slot(x);
    let before = lo.buf.pos();
    let slot = lo.alloc_slot(sum);
    assert!(lo.try_emit_fused_add_i2l(sum, slot));
    assert!(lo.buf.pos() > before);
}

#[test]
fn armed_newarray_sinks_map_and_clears_to_slow_path() {
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let len = g.add(Op::Const(8), IrType::Int, vec![], None);
    let arr = g.add(
        Op::NewArray {
            element_type: 10, // T_INT
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, len],
        Some(12),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, arr], None);
    let (_, mut lo) = mk_lowerer(g);
    lo.newarray = 0x1234;
    lo.zgc_note_tlab_object = 0x5678;
    lo.frame_record = 0x9abc;
    lo.reloc_emit_on = true;
    lo.context_slot_off = 32;
    lo.shadow_thread_slot_off = 16;
    lo.tlab_cursor_off = 8;
    lo.tlab_end_off = 16;
    lo.sp_id_slot_off = 24;
    lo.phi_copy_scratch_slot_off = 40;
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.defined_nodes = vec![false; lo.graph.nodes.len()];
    lo.alloc_slot(len);
    lo.dead_ref_clear_site = Some((0, 12, arr));
    assert!(lo.alloc_site_armed(arr));
    assert!(
        lo.try_lower_newarray_map_sink(arr, len, 10, 0),
        "sink failed: map_sink={} inline_tlab={} zgc={} zgc_ann={} zgc_sink={}",
        ir_newarray_map_sink_enabled(),
        ir_inline_tlab_enabled(),
        cratonvm_types::jit_tlab_registration_required(),
        crate::x64::zgc_inline_array_announce_enabled(),
        ir_alloc_map_sink_zgc_enabled(),
    );
}

#[test]
fn dead_add_remat_and_call_free_loop_header() {
    // Test find_dead_add_remat_deopt_node when two unrolled copies share a bytecode_pc,
    // and verify is_call_free_gc_free_loop_header on a pure load/add loop.
    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx0 = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let f0 = g.add(Op::Const(16), IrType::Long, vec![], None);
    let loop_h = g.add(Op::Region, IrType::Control, vec![ctrl0], Some(4));
    let phi_i = g.add(Op::Phi, IrType::Int, vec![loop_h, idx0], Some(4));
    let ld0 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![loop_h, mem0, obj, f0],
        Some(10),
    );
    let ld1 = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![loop_h, mem0, obj, f0],
        Some(10),
    );
    let next_i = g.add(Op::Add, IrType::Int, vec![phi_i, one], Some(12));
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![ld1, zero], Some(14));
    let if_n = g.add(Op::If, IrType::Control, vec![loop_h, cond], Some(14));
    let t_p = g.add(Op::Proj(0), IrType::Control, vec![if_n], None);
    let f_p = g.add(Op::Proj(1), IrType::Control, vec![if_n], None);
    g.nodes[loop_h as usize].inputs.push(t_p);
    g.nodes[phi_i as usize].inputs.push(next_i);
    g.nodes[ld0 as usize].frame_snapshot = Some(5);
    g.nodes[ld1 as usize].frame_snapshot = Some(7);
    g.exit = g.add(Op::Return, IrType::Void, vec![f_p, ld0], None);

    let (_, mut lo) = mk_lowerer(g);
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.defined_nodes = vec![false; lo.graph.nodes.len()];
    // Unrolled copy: two trapping loads share target_bci=10, sp_indices points at ld1's snapshot (7).
    let found = lo.find_dead_add_remat_deopt_node(&[next_i, ld0, ld1], 0, 10, &[7]);
    assert_eq!(found, Some((2, ld1)));
    // Call-free GC-free loop header accepts an IrBuilder int loop with no calls.
    let code: [u8; 21] = [
        0x03, 0x3c, // 0: s = 0
        0x03, 0x3d, // 2: i = 0
        0x1c, 0x1a, 0xa2, 0x00, 0x0d, // 4: if (i >= n) goto 19
        0x1b, 0x1c, 0x82, 0x3c, // 9: s ^= i (not closed-formable)
        0x84, 0x02, 0x01, // 13: i++
        0xa7, 0xff, 0xf4, // 16: goto 4
        0x1b, 0xac, // 19: return s
    ];
    let mut builder = crate::ir::IrBuilder::new(1, 4);
    builder.set_param_types(&[IrType::Int]);
    let mut g2 = builder.build(&code, code.len()).expect("IR build");
    crate::ir_optimize::optimize(&mut g2);
    let (_, lo2) = mk_lowerer(g2);
    let diag: Vec<_> = lo2
        .schedule
        .blocks
        .iter()
        .enumerate()
        .map(|(b, blk)| {
            let ops: Vec<_> = blk
                .nodes
                .iter()
                .map(|&n| format!("{:?}", lo2.graph.nodes[n as usize].op))
                .collect();
            (
                b,
                blk.predecessors.clone(),
                blk.terminator
                    .map(|t| format!("{:?}", lo2.graph.nodes[t as usize].op)),
                ops,
            )
        })
        .collect();
    assert!(
        (1..lo2.schedule.blocks.len()).any(|b| lo2.is_call_free_gc_free_loop_header(b)),
        "expected a call-free gc-free loop header among {} blocks: {:?}",
        lo2.schedule.blocks.len(),
        diag
    );
}

#[test]
fn swap_loop_compiles_cleanly() {
    let code = [
        0x03, 0x3e, 0x1d, 0x1c, 0xa2, 0x00, 0x0d, 0x1b, 0x1a, 0x3c, 0x3b, 0x84, 0x03, 0x01, 0xa7,
        0xff, 0xf4, 0x1a, 0x1b, 0x64, 0xac,
    ];
    let builder = crate::ir::IrBuilder::new(3, 4);
    let mut g = builder.build(&code, code.len()).expect("IR build");
    crate::ir_optimize::optimize(&mut g);
    let sched = crate::ir_schedule::schedule(&g);
    let helpers = JitRuntimeHelpers::default();
    let cm = lower(&g, &sched, 3, 4, &helpers);
    assert!(
        cm.is_some(),
        "swap loop failed: bail={:?}",
        last_lower_bail()
    );
}
