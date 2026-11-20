// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#![cfg(all(target_arch = "x86_64", any(unix, windows)))]

use cratonvm_jit::deopt::DespecRegistry;
use cratonvm_jit::ir::{
    with_guard_token, with_licensing_guard, CmpOp, ConstFoldTables, Graph, IrBuilder, IrType,
    MemKind, NodeId, Op, SafepointSnapshot,
};
use cratonvm_jit::ir_lower::lower;
use cratonvm_jit::ir_optimize::{optimize_with, OptimizeContext};
use cratonvm_jit::ir_schedule::schedule;
use cratonvm_jit::ir_verify::verify_graph_at_phase;
use cratonvm_jit::{
    enter_ir_collection_layouts_scope, ir_collection_layouts_cached, ir_collection_layouts_current,
    ir_collection_layouts_for_vm, ir_hashmap_get_inline_enabled, ir_sb_prefix_enabled,
    ir_string_search_prefix_kind, register_ir_collection_layouts,
    register_ir_collection_layouts_with_object_array, register_ir_collection_layouts_with_objects,
    IrArrayListLayout, IrCollectionLayouts, IrHashMapLayout, IrStringSearchKind, JitInvokeInfo,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::HashMap;

unsafe extern "C" fn record_throw_bci(_bci: i64) {}

unsafe extern "C" fn deopt_unserviceable_stub(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
    i64::MIN
}

unsafe extern "C" fn fallback_invoke_dispatch(
    _vm: usize,
    _info_ptr: usize,
    _args_ptr: *const i64,
    _n: usize,
) -> i64 {
    0xDEAD_BEEF_usize as i64
}

fn test_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn unexpected_stub() {
        panic!("unexpected runtime helper call in opcost_calls_collections_tests");
    }
    let s = unexpected_stub as *const () as usize;
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: fallback_invoke_dispatch as *const () as usize,
        invoke_virtual_mic: fallback_invoke_dispatch as *const () as usize,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

#[repr(align(16))]
struct FakeObj([u8; 256]);

impl FakeObj {
    fn compact(class_id: u32) -> Self {
        let mut bytes = [0u8; 256];
        bytes[0..4].copy_from_slice(&class_id.to_le_bytes());
        bytes[cratonvm_types::GC_FLAGS_BYTE_OFFSET] = cratonvm_types::GC_FLAG_COMPACT;
        Self(bytes)
    }

    fn write_u64(&mut self, offset: usize, val: u64) {
        self.0[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
    }

    fn write_i32(&mut self, offset: usize, val: i32) {
        self.0[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
    }

    fn as_i64(&self) -> i64 {
        self.0.as_ptr() as usize as i64
    }
}

#[test]
fn opcost_switches_and_vm_scoped_layouts_behave_cleanly() {
    assert!(cratonvm_jit::ir::ir_polymorphic_guarded_splice_enabled());
    assert!(cratonvm_jit::ir::ir_class_word_chain_enabled());
    assert!(cratonvm_jit::ir::ir_arraylist_ir_intrinsics_enabled());
    assert!(ir_hashmap_get_inline_enabled());
    assert!(ir_sb_prefix_enabled());

    let _guard = enter_ir_collection_layouts_scope(None);
    register_ir_collection_layouts(0x1111, None, None);
    assert!(ir_collection_layouts_for_vm(0x1111).is_some());
    assert!(!ir_collection_layouts_cached(0x1111));
    assert!(
        ir_collection_layouts_for_vm(0x2222).is_none(),
        "VM-A layout must never leak to VM-B"
    );
}

#[test]
fn ir_collection_layouts_upgrades_partial_entries_until_complete() {
    let al = IrArrayListLayout {
        class_id: 10,
        element_data_field_index: 0,
        element_data_compact_offset: 16,
        size_field_index: 1,
        size_compact_offset: 12,
    };
    let hm = IrHashMapLayout {
        hashmap_class_id: 20,
        table_field_index: 0,
        table_compact_offset: 16,
        node_class_id: 21,
        node_hash_field_index: 0,
        node_hash_compact_offset: 12,
        node_key_field_index: 1,
        node_key_compact_offset: 16,
        node_value_field_index: 2,
        node_value_compact_offset: 24,
        integer_class_id: 22,
        integer_value_field_index: 0,
        integer_value_compact_offset: 12,
    };

    let _guard = enter_ir_collection_layouts_scope(None);
    assert!(!ir_collection_layouts_cached(0));

    // Early boot: nothing loaded yet. Must not freeze cache as complete.
    register_ir_collection_layouts(0x3333, None, None);
    assert!(!ir_collection_layouts_cached(0x3333));
    assert_eq!(
        ir_collection_layouts_current().map(|l| l.object_array_class_id),
        Some(0)
    );

    // Partial upgrade: ArrayList + Object[] loaded, HashMap not yet loaded.
    register_ir_collection_layouts_with_object_array(0x3333, Some(al), None, 77);
    assert!(!ir_collection_layouts_cached(0x3333));
    assert_eq!(
        ir_collection_layouts_current()
            .and_then(|l| (l.object_array_class_id != 0).then_some(l.object_array_class_id)),
        Some(77)
    );

    // Partial upgrade: ArrayList + HashMap loaded, Object[] missing.
    register_ir_collection_layouts_with_object_array(0x3333, Some(al), Some(hm), 0);
    assert!(!ir_collection_layouts_cached(0x3333));

    // Complete upgrade: ArrayList + HashMap + Object[] missing Object class -> still incomplete.
    register_ir_collection_layouts_with_object_array(0x3333, Some(al), Some(hm), 77);
    assert!(!ir_collection_layouts_cached(0x3333));

    // Full upgrade: all fields populated -> cached() returns true.
    register_ir_collection_layouts_with_objects(0x3333, Some(al), Some(hm), 77, 1);
    assert!(ir_collection_layouts_cached(0x3333));
    assert!(!ir_collection_layouts_cached(0x4444));
    assert!(!ir_collection_layouts_cached(0));
    let cur = ir_collection_layouts_for_vm(0x3333).expect("complete layout present");
    assert_eq!(cur.array_list, Some(al));
    assert_eq!(cur.hash_map, Some(hm));
    assert_eq!(cur.object_array_class_id, 77);
    assert_eq!(cur.object_class_id, 1);
}

#[test]
fn lambda_static_final_indy_root_resolves_without_exact_class_guard() {
    // Bytecode: 0: getstatic #1, 3: aconst_null, 4: pop, 5: iload_0, 6: ireturn
    let code = [0xb2, 0x00, 0x01, 0x57, 0x1a, 0xac];
    let mut b = IrBuilder::new(1, 1);
    b.set_static_field_info(HashMap::from([(0usize, (10u32, 0usize, b'L', false))]));
    let mut folds = ConstFoldTables::default();
    folds
        .static_final_const_refs
        .insert((10, 0), (0x1234_5000usize, 77u32));
    b.set_const_folds(folds);
    b.set_return_descriptor("(I)I");
    let g = b.build(&code, code.len()).expect("graph builds");
    assert!(
        g.nodes.iter().any(|n| n.op
            == Op::IndyRootRef {
                slot_addr: 0x1234_5000usize
            }),
        "static final lambda field must fold to IndyRootRef"
    );
    assert!(verify_graph_at_phase(&g, "build").is_ok());
}

#[test]
fn charat_loop_hoists_string_metadata_and_eliminates_str_char_decode() {
    let mut g = Graph::empty(64);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let recv = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let n_param = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let mask63 = g.add(Op::Const(63), IrType::Int, vec![], None);
    let null_ref = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let coder_f = g.add(Op::Const(1), IrType::Int, vec![], None);
    let value_f = g.add(Op::Const(2), IrType::Int, vec![], None);

    g.push_safepoint(SafepointSnapshot::new(
        10,
        vec![recv, n_param, zero, zero],
        vec![],
        vec![],
    ));
    let sp_idx = 0u32;

    let loop_hdr = g.add(Op::Region, IrType::Control, vec![ctrl0, ctrl0], Some(10));
    let phi_i = g.add(Op::Phi, IrType::Int, vec![loop_hdr, zero, zero], Some(10));
    let phi_sum = g.add(Op::Phi, IrType::Int, vec![loop_hdr, zero, zero], Some(10));
    let phi_mem = g.add(
        Op::Phi,
        IrType::Memory,
        vec![loop_hdr, mem0, mem0],
        Some(10),
    );

    let cond_loop = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![phi_i, n_param], None);
    let iff = g.add(Op::If, IrType::Control, vec![loop_hdr, cond_loop], Some(10));
    let body_ctrl = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(12));
    let exit_ctrl = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(30));

    let recv_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null_ref], None);
    let recv_guard = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body_ctrl, recv_ne],
        Some(14),
    );
    g.set_node_frame_snapshot(recv_guard, sp_idx);

    let coder = g.add(
        Op::Load(MemKind::Byte),
        IrType::Int,
        with_guard_token(vec![body_ctrl, phi_mem, recv, coder_f], recv_guard),
        Some(14),
    );
    g.set_node_frame_snapshot(coder, sp_idx);
    let value = g.add(
        Op::Load(MemKind::Ref),
        IrType::Ref,
        with_guard_token(vec![body_ctrl, coder, recv, value_f], recv_guard),
        Some(14),
    );
    g.set_node_frame_snapshot(value, sp_idx);

    let val_ne = g.add(Op::Cmp(CmpOp::Ne), IrType::Int, vec![value, null_ref], None);
    let val_guard = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body_ctrl, val_ne],
        Some(14),
    );
    g.set_node_frame_snapshot(val_guard, sp_idx);

    let byte_len = g.add(
        Op::ArrayLength,
        IrType::Int,
        with_licensing_guard(vec![body_ctrl, value, value], val_guard),
        Some(14),
    );
    g.set_node_frame_snapshot(byte_len, sp_idx);
    let count = g.add(Op::UShr, IrType::Int, vec![byte_len, coder], None);
    let idx = g.add(Op::And, IrType::Int, vec![phi_i, mask63], None);
    let ge0 = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![idx, zero], None);
    let g_ge0 = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body_ctrl, ge0],
        Some(14),
    );
    g.set_node_frame_snapshot(g_ge0, sp_idx);
    let lt_cnt = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, count], None);
    let bounds_g = g.add(
        Op::Guard { bci: 14 },
        IrType::Void,
        vec![body_ctrl, lt_cnt],
        Some(14),
    );
    g.set_node_frame_snapshot(bounds_g, sp_idx);

    let ch = g.add(
        Op::StrCharDecode,
        IrType::Int,
        with_guard_token(vec![body_ctrl, byte_len, value, idx, coder], bounds_g),
        Some(14),
    );
    let next_sum = g.add(Op::Add, IrType::Int, vec![phi_sum, ch], None);
    let next_i = g.add(Op::Add, IrType::Int, vec![phi_i, one], None);

    g.set_input(loop_hdr, 1, body_ctrl);
    g.set_input(phi_i, 2, next_i);
    g.set_input(phi_sum, 2, next_sum);
    g.set_input(phi_mem, 2, ch);

    let ret = g.add(Op::Return, IrType::Void, vec![exit_ctrl, phi_sum], Some(30));
    g.exit = ret;

    let despec = DespecRegistry::new();
    optimize_with(
        &mut g,
        OptimizeContext {
            despec: Some(&despec),
            method_key: "opcost/Test.charAtLoop:(Ljava/lang/String;I)I",
            osr_route: false,
        },
    );

    assert!(
        !g.nodes.iter().any(|n| n.op == Op::StrCharDecode),
        "StrCharDecode should be replaced by bounds-proven byte ArrayLoad"
    );
    let byte_loads: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.op == Op::ArrayLoad(MemKind::Byte))
        .collect();
    assert!(
        !byte_loads.is_empty(),
        "expected byte ArrayLoad after masked charAt hoisting"
    );
    let ref_loads_in_entry = g
        .nodes
        .iter()
        .filter(|n| n.op == Op::Load(MemKind::Ref) && n.input_opt(0) == Some(ctrl0))
        .count();
    assert_eq!(
        ref_loads_in_entry, 1,
        "String.value load must be hoisted to loop preheader ctrl0"
    );
    assert!(verify_graph_at_phase(&g, "post_optimize").is_ok());
}

#[test]
fn sb_set_length_prefix_recognized_for_sb_and_asb() {
    assert_eq!(
        ir_string_search_prefix_kind("java/lang/StringBuilder", "setLength", "(I)V"),
        Some(IrStringSearchKind::SbSetLength)
    );
    assert_eq!(
        ir_string_search_prefix_kind("java/lang/AbstractStringBuilder", "setLength", "(I)V"),
        Some(IrStringSearchKind::SbSetLength)
    );
    let kind = IrStringSearchKind::SbSetLength;
    assert_eq!(kind.num_params(), 1);
    assert!(kind.is_builder());
    assert!(kind.admits_invoke_kind(0));
    assert!(kind.admits_invoke_kind(1));
}

#[test]
fn arraylist_get_and_size_expand_into_ir_and_hoist_out_of_loop() {
    let layout = IrArrayListLayout {
        class_id: 55,
        element_data_field_index: 1,
        element_data_compact_offset: u32::MAX,
        size_field_index: 2,
        size_compact_offset: u32::MAX,
    };
    let _scope = enter_ir_collection_layouts_scope(Some(IrCollectionLayouts {
        vm_ctx: 0,
        array_list: Some(layout),
        hash_map: None,
        object_array_class_id: 0,
        object_class_id: 0,
    }));

    // Build a small method: `aload_0; iload_1; invokeinterface List.get(I)Object; pop; aload_0; invokeinterface List.size()I; ireturn`
    let code = [
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb9, 0x00, 0x01, 0x02, 0x00, // 2: invokeinterface #1 (List.get)
        0x57, // 7: pop
        0x2a, // 8: aload_0
        0xb9, 0x00, 0x02, 0x01, 0x00, // 9: invokeinterface #2 (List.size)
        0xac, // 14: ireturn
    ];
    let info_get = Box::new(JitInvokeInfo {
        class_name: "java/util/List",
        method_name: "get",
        descriptor: "(I)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let info_size = Box::new(JitInvokeInfo {
        class_name: "java/util/List",
        method_name: "size",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let info_get_ptr = Box::into_raw(info_get) as usize;
    let info_size_ptr = Box::into_raw(info_size) as usize;

    let mut b = IrBuilder::new(2, 2);
    b.set_invoke_info(HashMap::from([
        (2usize, (info_get_ptr, 2usize, b'L')),
        (9usize, (info_size_ptr, 1usize, b'I')),
    ]));
    b.set_return_descriptor("(Ljava/util/List;I)I");
    let g = b.build(&code, code.len()).expect("ArrayList IR builds");

    assert!(
        g.nodes
            .iter()
            .any(|n| n.op == Op::ExactClassIs { class_id: 55 }),
        "List.get must emit ExactClassIs guard for ArrayList"
    );
    assert!(
        g.nodes.iter().any(|n| n.op == Op::ArrayLoad(MemKind::Ref)),
        "List.get must expand to Op::ArrayLoad(MemKind::Ref)"
    );
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "List.get / List.size must expand cleanly without Op::Call"
    );
    assert!(verify_graph_at_phase(&g, "build").is_ok());

    unsafe {
        drop(Box::from_raw(info_get_ptr as *mut JitInvokeInfo));
        drop(Box::from_raw(info_size_ptr as *mut JitInvokeInfo));
    }
}

#[test]
fn hashmap_get_inline_prefix_executes_hit_empty_and_fallback_cleanly() {
    let layout = IrHashMapLayout {
        hashmap_class_id: 101,
        table_field_index: 0,
        table_compact_offset: 16,
        node_class_id: 102,
        node_hash_field_index: 0,
        node_hash_compact_offset: 16,
        node_key_field_index: 1,
        node_key_compact_offset: 24,
        node_value_field_index: 2,
        node_value_compact_offset: 32,
        integer_class_id: 103,
        integer_value_field_index: 0,
        integer_value_compact_offset: 16,
    };
    let _scope = enter_ir_collection_layouts_scope(Some(IrCollectionLayouts {
        vm_ctx: 0,
        array_list: None,
        hash_map: Some(layout),
        object_array_class_id: 0,
        object_class_id: 0,
    }));

    let info = Box::new(JitInvokeInfo {
        class_name: "java/util/Map",
        method_name: "get",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let info_ptr = Box::into_raw(info) as usize;

    let mut g = Graph::empty(16);
    let start = g.add(Op::Start, IrType::Void, vec![], None);
    g.entry = start;
    let ctrl0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let p_map = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let p_key = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    g.push_safepoint(SafepointSnapshot::new(
        0,
        vec![p_map, p_key],
        vec![],
        vec![],
    ));
    let call = g.add(
        Op::Call { info_ptr },
        IrType::Ref,
        vec![ctrl0, mem0, p_map, p_key],
        Some(0),
    );
    g.set_node_frame_snapshot(call, 0);
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl0, call], Some(4));
    g.exit = ret;

    let sched = schedule(&g);
    let helpers = test_helpers();
    let compiled = lower(&g, &sched, 2, 2, &helpers).expect("lower compiles Map.get inline prefix");

    let key_val: i32 = 42;
    let hash = key_val ^ ((key_val as u32 >> 16) as i32);

    let mut key1 = FakeObj::compact(layout.integer_class_id);
    key1.write_i32(layout.integer_value_compact_offset as usize, key_val);

    let mut key_eq = FakeObj::compact(layout.integer_class_id);
    key_eq.write_i32(layout.integer_value_compact_offset as usize, key_val);

    let mut key_miss_bucket = FakeObj::compact(layout.integer_class_id);
    key_miss_bucket.write_i32(layout.integer_value_compact_offset as usize, 43);

    let mut key_collision = FakeObj::compact(layout.integer_class_id);
    key_collision.write_i32(layout.integer_value_compact_offset as usize, 44);

    let val_obj = FakeObj::compact(200);
    let mut node = FakeObj::compact(layout.node_class_id);
    node.write_i32(layout.node_hash_compact_offset as usize, hash);
    node.write_u64(
        layout.node_key_compact_offset as usize,
        key1.as_i64() as u64,
    );
    node.write_u64(
        layout.node_value_compact_offset as usize,
        val_obj.as_i64() as u64,
    );

    let mut table = FakeObj::compact(300);
    table.write_i32(cratonvm_types::ARRAY_LENGTH_OFFSET, 16);
    let idx = ((16 - 1) & hash) as usize;
    table.write_u64(
        cratonvm_types::ARRAY_DATA_OFFSET + idx * 8,
        node.as_i64() as u64,
    );

    let mut map = FakeObj::compact(layout.hashmap_class_id);
    map.write_u64(layout.table_compact_offset as usize, table.as_i64() as u64);

    let other_map = FakeObj::compact(999);
    let other_key = FakeObj::compact(998);

    unsafe {
        // 1. Exact pointer hit (`nk == key1`).
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), key1.as_i64()]),
            Ok(val_obj.as_i64())
        );
        // 2. Distinct Integer hit (`nk.value == key_eq.value`).
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), key_eq.as_i64()]),
            Ok(val_obj.as_i64())
        );
        // 3. Empty bucket (`key = 43` -> slot is null -> returns 0 inline).
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), key_miss_bucket.as_i64()]),
            Ok(0)
        );
        // 4. Hash mismatch at occupied bucket (16-way collide to slot `idx` if hash differs -> fallback).
        key_collision.write_i32(layout.integer_value_compact_offset as usize, key_val + 16);
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), key_collision.as_i64()]),
            Ok(0xDEAD_BEEF_usize as i64)
        );
        // 5. Non-HashMap subclass & non-Integer key -> cleanly decline to fallback dispatch.
        assert_eq!(
            compiled.try_call_with_context(0, &[other_map.as_i64(), key1.as_i64()]),
            Ok(0xDEAD_BEEF_usize as i64)
        );
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), other_key.as_i64()]),
            Ok(0xDEAD_BEEF_usize as i64)
        );
        assert_eq!(
            compiled.try_call_with_context(0, &[map.as_i64(), 0]),
            Ok(0xDEAD_BEEF_usize as i64)
        );

        drop(Box::from_raw(info_ptr as *mut JitInvokeInfo));
    }
}

#[test]
fn hashmap_get_and_sb_expand_into_ir_when_inline_site_present() {
    let layout = IrHashMapLayout {
        hashmap_class_id: 101,
        table_field_index: 0,
        table_compact_offset: 16,
        node_class_id: 102,
        node_hash_field_index: 0,
        node_hash_compact_offset: 16,
        node_key_field_index: 1,
        node_key_compact_offset: 24,
        node_value_field_index: 2,
        node_value_compact_offset: 32,
        integer_class_id: 103,
        integer_value_field_index: 0,
        integer_value_compact_offset: 16,
    };
    let _scope = enter_ir_collection_layouts_scope(Some(IrCollectionLayouts {
        vm_ctx: 0,
        array_list: None,
        hash_map: Some(layout),
        object_array_class_id: 0,
        object_class_id: 0,
    }));

    // 0: aload_0; 1: aload_1; 2: invokeinterface Map.get; 7: areturn
    let code = [0x2a, 0x2b, 0xb9, 0x00, 0x01, 0x02, 0x00, 0xb0];
    let info = Box::into_raw(Box::new(JitInvokeInfo {
        class_name: "java/util/Map",
        method_name: "get",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as usize;

    let mut b = IrBuilder::new(2, 2);
    b.set_invoke_info(HashMap::from([(2usize, (info, 2usize, b'L'))]));
    b.set_return_descriptor("(Ljava/util/Map;Ljava/lang/Object;)Ljava/lang/Object;");
    let g = b.build(&code, code.len()).expect("HashMap IR builds");
    assert!(
        g.nodes.iter().any(|n| n.op == Op::ArrayLoad(MemKind::Ref)),
        "Map.get must expand to Op::ArrayLoad(MemKind::Ref)"
    );
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "Map.get must expand without Op::Call"
    );
    assert!(verify_graph_at_phase(&g, "build").is_ok());
    unsafe {
        drop(Box::from_raw(info as *mut JitInvokeInfo));
    }
}
