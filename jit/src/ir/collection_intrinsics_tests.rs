// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use crate::ir::{CmpOp, IrBuilder, IrType, JitInvokeInfo, MemKind, Op};
use crate::ir_verify::verify_graph_at_phase;
use std::collections::HashMap;

#[test]
fn pre_splice_collection_intrinsics_expand_hashmap_and_sb_before_splice() {
    let hm = crate::IrHashMapLayout {
        hashmap_class_id: 21,
        table_field_index: 0,
        table_compact_offset: 16,
        node_class_id: 22,
        node_hash_field_index: 0,
        node_hash_compact_offset: 12,
        node_key_field_index: 1,
        node_key_compact_offset: 16,
        node_value_field_index: 2,
        node_value_compact_offset: 24,
        integer_class_id: 23,
        integer_value_field_index: 0,
        integer_value_compact_offset: 12,
    };
    let _col = crate::enter_ir_collection_layouts_scope(Some(crate::IrCollectionLayouts {
        vm_ctx: 0,
        array_list: None,
        hash_map: Some(hm),
        object_array_class_id: 0,
        object_class_id: 0,
    }));
    let sb = crate::tests::synthetic_string_builder_layout(24);
    let str_layout = crate::tests::synthetic_string_layout(25).with_builder(Some(sb));

    // 0: aload_0; 1: aload_1; 2: invokeinterface Map.get; 7: pop;
    // 8: aload_2; 9: bipush 65; 11: invokevirtual SB.append(C); 14: invokevirtual SB.length()I; 17: ireturn
    let code = [
        0x2a, 0x2b, 0xb9, 0x00, 0x01, 0x02, 0x00, 0x57, 0x2c, 0x10, 0x41, 0xb6, 0x00, 0x02, 0xb6,
        0x00, 0x03, 0xac,
    ];
    let info_get = Box::into_raw(Box::new(JitInvokeInfo {
        class_name: "java/util/Map",
        method_name: "get",
        descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 2,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as usize;
    let info_append = Box::into_raw(Box::new(JitInvokeInfo {
        class_name: "java/lang/StringBuilder",
        method_name: "append",
        descriptor: "(C)Ljava/lang/StringBuilder;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as usize;
    let info_len = Box::into_raw(Box::new(JitInvokeInfo {
        class_name: "java/lang/StringBuilder",
        method_name: "length",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as usize;

    let mut b = IrBuilder::new(3, 3);
    b.set_string_layout(Some(str_layout));
    b.set_invoke_info(HashMap::from([
        (2usize, (info_get, 2usize, b'L')),
        (11usize, (info_append, 2usize, b'L')),
        (14usize, (info_len, 1usize, b'I')),
    ]));
    b.set_return_descriptor("(Ljava/util/Map;Ljava/lang/Object;Ljava/lang/StringBuilder;)I");
    let g = b.build(&code, code.len()).expect("graph builds");
    assert!(!g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })));
    assert!(g.nodes.iter().any(|n| n.op == Op::ArrayLoad(MemKind::Ref)));
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::ArrayStore(MemKind::Byte)));
    assert!(verify_graph_at_phase(&g, "build").is_ok());

    // SAFETY: these pointers came from Box::into_raw and are reclaimed once after graph construction.
    unsafe {
        drop(Box::from_raw(info_get as *mut JitInvokeInfo));
        drop(Box::from_raw(info_append as *mut JitInvokeInfo));
        drop(Box::from_raw(info_len as *mut JitInvokeInfo));
    }

    // Verify `String.equals(Object)Z` expands to pointer-identity diamond.
    let eq_code = [0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xac];
    let info_eq = Box::into_raw(Box::new(JitInvokeInfo {
        class_name: "java/lang/String",
        method_name: "equals",
        descriptor: "(Ljava/lang/Object;)Z",
        num_jit_args: 2,
        return_type: b'Z',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    })) as usize;

    let mut eq_b = IrBuilder::new(2, 2);
    eq_b.set_param_types(&[IrType::Ref, IrType::Ref]);
    eq_b.set_invoke_info(HashMap::from([(2usize, (info_eq, 2usize, b'Z'))]));
    eq_b.set_return_descriptor("(Ljava/lang/String;Ljava/lang/Object;)Z");
    let eq_g = eq_b.build(&eq_code, eq_code.len()).expect("graph builds");
    assert!(eq_g.nodes.iter().any(|n| n.op == Op::If));
    assert!(eq_g.nodes.iter().any(|n| n.op == Op::Cmp(CmpOp::Eq)));
    assert!(eq_g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })));
    assert!(verify_graph_at_phase(&eq_g, "build").is_ok());

    // SAFETY: this pointer came from Box::into_raw and is reclaimed once after graph construction.
    unsafe {
        drop(Box::from_raw(info_eq as *mut JitInvokeInfo));
    }
}
