// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;
use crate::ir::{CmpOp, Graph, IrType, MemKind, Op, SafepointSnapshot};

fn invoke_info(class: &'static str, method: &'static str, desc: &'static str) -> usize {
    // LEAK(intentional): test-only; read by `ir_call_box_value_of` / `ir_call_hash_code`.
    Box::leak(Box::new(JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor: desc,
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as *const JitInvokeInfo as usize
}

#[test]
fn spliced_integer_diamond_folds_field_load_and_retires_empty_diamond_in_ea() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let x = g.add(Op::Param(0), IrType::Int, vec![], Some(0));
    let low = g.add(Op::Const(-128), IrType::Int, vec![], Some(1));
    let high = g.add(Op::Const(127), IrType::Int, vec![], Some(1));
    let off = g.add(Op::Const(16), IrType::Int, vec![], Some(1));

    let cmp_lo = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![x, low], Some(2));
    let if_lo = g.add(Op::If, IrType::Control, vec![ctrl, cmp_lo], Some(2));
    let in_lo = g.add(Op::Proj(0), IrType::Control, vec![if_lo], Some(2));
    let miss_lo = g.add(Op::Proj(1), IrType::Control, vec![if_lo], Some(2));

    let cmp_hi = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![x, high], Some(3));
    let if_hi = g.add(Op::If, IrType::Control, vec![in_lo, cmp_hi], Some(3));
    let cache_ctrl = g.add(Op::Proj(0), IrType::Control, vec![if_hi], Some(3));
    let miss_hi = g.add(Op::Proj(1), IrType::Control, vec![if_hi], Some(3));

    let static_arr = g.add(
        Op::LoadStatic {
            class_id: 10,
            field_index: 0,
            type_tag: b'[',
            is_volatile: false,
        },
        IrType::Ref,
        vec![cache_ctrl, mem],
        Some(4),
    );
    let idx = g.add(Op::Sub, IrType::Int, vec![x, low], Some(4));
    let cache_load = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![cache_ctrl, static_arr, static_arr, idx],
        Some(4),
    );

    let miss_merge = g.add(Op::Merge, IrType::Control, vec![miss_lo, miss_hi], Some(5));
    let new_box = g.add(
        Op::New {
            class_id: 11,
            num_fields: 1,
        },
        IrType::Ref,
        vec![miss_merge, mem],
        Some(5),
    );
    let store_val = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![miss_merge, new_box, new_box, off, x],
        Some(5),
    );

    let join = g.add(
        Op::Merge,
        IrType::Control,
        vec![cache_ctrl, miss_merge],
        Some(6),
    );
    let join_mem = g.add(
        Op::Phi,
        IrType::Memory,
        vec![join, cache_load, store_val],
        Some(6),
    );
    let box_phi = g.add(
        Op::Phi,
        IrType::Ref,
        vec![join, cache_load, new_box],
        Some(6),
    );

    let read_val = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![join, join_mem, box_phi, off],
        Some(8),
    );
    let after = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![join, read_val],
        Some(10),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![join, after], Some(12));
    g.box_class_ids = (Some(11), None);
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(SafepointSnapshot::new(
        10,
        vec![box_phi],
        vec![read_val],
        vec![],
    ));

    assert!(detect_spliced_box_diamond(&g, box_phi).is_some());
    assert_eq!(fold_spliced_box_field_loads_and_unboxes(&mut g), 1);
    assert_eq!(g.nodes[read_val as usize].op, Op::Dead);
    assert_eq!(g.nodes[after as usize].inputs[1], join_mem);
    assert_eq!(g.safepoints[0].stack[0], x);

    let mut sr_map = None;
    let mut pinned = std::collections::HashSet::new();
    let ea_folded = ir_fold_box_phi_webs(&mut g, &mut sr_map, &mut pinned);
    assert_eq!(ea_folded, 1);
    assert_eq!(g.nodes[box_phi as usize].op, Op::Dead);
    assert_eq!(g.nodes[new_box as usize].op, Op::Dead);
    assert_eq!(g.nodes[store_val as usize].op, Op::Dead);
    assert_eq!(g.nodes[cache_load as usize].op, Op::Dead);
    assert_eq!(g.nodes[join as usize].op, Op::Dead);
    assert!(pinned.contains(&x));
    let sr = sr_map.expect("snapshot recipe created");
    let key = g.safepoints[0].locals[0];
    let info = sr
        .objects
        .get(&key)
        .expect("VirtualObjectInfo for snapshot");
    assert_eq!(
        info.array_element_type,
        Some(crate::deopt::AUTO_BOX_INTEGER_ATYPE)
    );
    assert_eq!(info.field_values, vec![Some(x)]);
}

#[test]
fn standalone_value_of_call_in_snapshot_folds_in_box_phi_webs() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let x = g.add(Op::Param(0), IrType::Int, vec![], Some(0));
    let bi = invoke_info("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
    let boxed = g.add(
        Op::Call { info_ptr: bi },
        IrType::Ref,
        vec![ctrl, mem, x],
        Some(2),
    );
    let after = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![ctrl, boxed],
        Some(5),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, x], Some(6));
    g.entry = start;
    g.exit = ret;
    g.push_safepoint(SafepointSnapshot::new(4, vec![boxed], vec![], vec![]));

    assert!(is_box_web_seed(&g, &g.nodes[boxed as usize]));
    let mut sr_map = None;
    let mut pinned = std::collections::HashSet::new();
    let folded = ir_fold_box_phi_webs(&mut g, &mut sr_map, &mut pinned);
    assert_eq!(folded, 1);
    assert_eq!(g.nodes[boxed as usize].op, Op::Dead);
    assert_eq!(g.nodes[after as usize].inputs[1], mem);
    assert!(pinned.contains(&x));
    let sr = sr_map.expect("VirtualObjectInfo for standalone Call(valueOf)");
    let key = g.safepoints[0].locals[0];
    assert_eq!(
        sr.objects[&key].array_element_type,
        Some(crate::deopt::AUTO_BOX_INTEGER_ATYPE)
    );
}

#[test]
fn non_box_single_field_diamond_is_not_folded() {
    let mut g = Graph::empty(0);
    g.box_class_ids = (Some(11), Some(12));
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let x = g.add(Op::Param(0), IrType::Int, vec![], Some(0));
    let lim = g.add(Op::Const(10), IrType::Int, vec![], Some(1));
    let off = g.add(Op::Const(16), IrType::Int, vec![], Some(1));
    let cmp = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![x, lim], Some(2));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(2));
    let c_arm = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(2));
    let n_arm = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(2));
    let static_arr = g.add(
        Op::LoadStatic {
            class_id: 10,
            field_index: 0,
            type_tag: b'[',
            is_volatile: false,
        },
        IrType::Ref,
        vec![c_arm, mem],
        Some(3),
    );
    let cache_load = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![c_arm, static_arr, static_arr, x],
        Some(3),
    );
    let new_obj = g.add(
        Op::New {
            class_id: 999,
            num_fields: 1,
        },
        IrType::Ref,
        vec![n_arm, mem],
        Some(4),
    );
    let _store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![n_arm, new_obj, new_obj, off, x],
        Some(4),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![c_arm, n_arm], Some(5));
    let phi = g.add(
        Op::Phi,
        IrType::Ref,
        vec![join, cache_load, new_obj],
        Some(5),
    );
    g.entry = start;
    g.exit = join;
    assert!(detect_spliced_box_diamond(&g, phi).is_none());
}

#[test]
fn real_jdk_indy_root_ref_box_diamond_with_null_guard_and_dead_splice_chain_folds() {
    let mut g = Graph::empty(1);
    g.box_class_ids = (Some(11), Some(12));
    let start = g.add(Op::Start, IrType::Control, vec![], Some(0));
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], Some(0));
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], Some(0));
    let x = g.add(Op::Param(0), IrType::Int, vec![start], Some(0));
    let lim = g.add(Op::Const(127), IrType::Int, vec![], Some(1));
    let off = g.add(Op::Const(16), IrType::Int, vec![], Some(1));
    let cmp = g.add(Op::Cmp(CmpOp::Le), IrType::Int, vec![x, lim], Some(2));
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cmp], Some(2));
    let c_arm = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(2));
    let n_arm = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(2));

    // `--real-jdk`: `IntegerCache.cache` loaded as `Op::IndyRootRef`.
    let cache_arr = g.add(
        Op::IndyRootRef {
            slot_addr: 0xFACE_0001,
        },
        IrType::Ref,
        vec![c_arm],
        Some(3),
    );
    let cache_load = g.add(
        Op::ArrayLoad(MemKind::Ref),
        IrType::Ref,
        vec![c_arm, mem, cache_arr, x],
        Some(3),
    );
    let new_obj = g.add(
        Op::New {
            class_id: 11,
            num_fields: 1,
        },
        IrType::Ref,
        vec![n_arm, mem],
        Some(4),
    );
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![n_arm, new_obj, new_obj, off, x],
        Some(4),
    );
    let join = g.add(Op::Merge, IrType::Control, vec![c_arm, n_arm], Some(5));
    let mem_phi = g.add(Op::Phi, IrType::Memory, vec![join, mem, store], Some(5));
    let phi = g.add(
        Op::Phi,
        IrType::Ref,
        vec![join, cache_load, new_obj],
        Some(5),
    );

    // Null check and `b.hashCode()` field load on the diamond `phi`.
    let zero_ref = g.add(Op::Const(0), IrType::Ref, vec![], Some(6));
    let ne_cmp = g.add(
        Op::Cmp(CmpOp::Ne),
        IrType::Int,
        vec![phi, zero_ref],
        Some(6),
    );
    let null_guard = g.add(
        Op::Guard { bci: 6 },
        IrType::Void,
        vec![join, ne_cmp],
        Some(6),
    );
    let hc_load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![join, mem_phi, phi, off],
        Some(7),
    );
    let ret = g.add(Op::Return, IrType::Void, vec![join, hc_load], Some(8));
    g.exit = ret;

    // An unconsulted `splice_state` snapshot naming `new_obj` (e.g. inside `Integer.valueOf`).
    let mut splice_sp = SafepointSnapshot::new(99, vec![new_obj], vec![], vec![]);
    splice_sp.splice_scope = Some(std::sync::Arc::new(crate::ir::SpliceScopeChain {
        scopes: vec![
            crate::ir::SpliceScopeFrame {
                class_id: 0,
                method_key: "java/lang/Integer.valueOf:(I)Ljava/lang/Integer;".to_string(),
                bci: 5,
                code_len: 32,
                max_locals: 1,
                locals_len: 0,
                stack_len: 0,
            },
            crate::ir::SpliceScopeFrame {
                class_id: 0,
                method_key: String::new(),
                bci: 5,
                code_len: 0,
                max_locals: 0,
                locals_len: 0,
                stack_len: 0,
            },
        ],
    }));
    splice_sp.post_call_only = true;
    g.push_safepoint(splice_sp);

    let mut sr_map = None;
    let mut pinned = std::collections::HashSet::new();
    let folded = ir_fold_box_phi_webs(&mut g, &mut sr_map, &mut pinned);
    assert!(
        folded >= 1,
        "box diamond with IndyRootRef cache, null guard, and unconsulted splice_state must fold"
    );
    assert_eq!(g.nodes[phi as usize].op, Op::Dead);
    assert_eq!(g.nodes[new_obj as usize].op, Op::Dead);
    assert_eq!(g.nodes[null_guard as usize].op, Op::Dead);
    assert_eq!(g.nodes[ret as usize].inputs[1], x);
}
