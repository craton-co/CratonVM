// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

fn lookupswitch_body(keys: &[i32]) -> Vec<u8> {
    let n = keys.len();
    let pad = 2usize;
    let switch_pc = 1usize;
    let table_len = 1 + pad + 8 + 8 * n;
    let arm_base = switch_pc + table_len;
    let default_pc = arm_base + 4 * n;
    let end_pc = default_pc + 2;

    let mut code = vec![0x1a, 0xab, 0x00, 0x00];
    let default_off = (default_pc - switch_pc) as i32;
    code.extend_from_slice(&default_off.to_be_bytes());
    code.extend_from_slice(&(n as i32).to_be_bytes());
    for (i, &k) in keys.iter().enumerate() {
        let target = (arm_base + 4 * i - switch_pc) as i32;
        code.extend_from_slice(&k.to_be_bytes());
        code.extend_from_slice(&target.to_be_bytes());
    }
    for i in 0..n {
        let arm_pc = arm_base + 4 * i;
        code.push(0x03 + (i as u8 % 5));
        let goto_pc = arm_pc + 1;
        let off = (end_pc - goto_pc) as i16;
        code.push(0xa7);
        code.extend_from_slice(&off.to_be_bytes());
    }
    // default arm: iconst_m1; ireturn (and end_pc: ireturn)
    code.push(0x02);
    code.push(0xac);
    code.push(0xac);
    code
}

fn build_graph(code: &[u8]) -> Graph {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    IrBuilder::new(1, 1)
        .build(&padded, code.len())
        .expect("graph should build")
}

fn dispatch_and_depth(g: &Graph, key: i32) -> (usize, usize) {
    let first_if = g
        .nodes
        .iter()
        .position(|n| n.op == Op::If)
        .expect("at least one If");
    let mut cur = g.nodes[first_if].inputs[0];
    let mut compares = 0usize;
    for _ in 0..g.nodes.len() {
        let Some(if_id) = g
            .nodes
            .iter()
            .position(|n| n.op == Op::If && n.inputs.first().copied() == Some(cur))
        else {
            break;
        };
        compares += 1;
        let cmp = &g.nodes[g.nodes[if_id].inputs[1] as usize];
        let Op::Const(c) = g.nodes[cmp.inputs[1] as usize].op else {
            panic!("expected Const RHS");
        };
        let taken = match cmp.op {
            Op::Cmp(CmpOp::Eq) => i64::from(key) == c,
            Op::Cmp(CmpOp::Lt) => i64::from(key) < c,
            ref other => panic!("unexpected compare {other:?}"),
        };
        let want = if taken { 0 } else { 1 };
        let next = g
            .nodes
            .iter()
            .position(|n| {
                n.op == Op::Proj(want) && n.inputs.first().copied() == Some(if_id as NodeId)
            })
            .expect("If projection");
        cur = next as NodeId;
    }
    let merge = g
        .nodes
        .iter()
        .find(|n| n.op == Op::Merge && n.inputs.iter().any(|&i| i == cur))
        .expect("edge enters merge");
    (merge.bytecode_pc.unwrap_or(usize::MAX), compares)
}

#[test]
fn twenty_case_sparse_lookupswitch_builds_balanced_search_tree() {
    // 20 scattered keys resembling String.hashCode() values.
    let keys = [
        -1_987_654_321,
        -1_400_000_000,
        -912_345_678,
        -450_000_000,
        -123_456_789,
        -55_555,
        -3_333,
        -17,
        0,
        42,
        1_234,
        98_765,
        3_141_592,
        102_400_000,
        310_203_040,
        555_666_777,
        900_000_001,
        1_234_567_890,
        1_777_888_999,
        2_000_000_000,
    ];
    let code = lookupswitch_body(&keys);
    let g = build_graph(&code);

    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Switch { .. })),
        "sparse 20-key lookupswitch must not emit Op::Switch"
    );
    let lt_count = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Lt)))
        .count();
    let eq_count = g
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Cmp(CmpOp::Eq)))
        .count();
    assert!(
        lt_count >= 5,
        "expected balanced binary search splits, got {lt_count}"
    );
    assert_eq!(eq_count, 20, "each live key must be tested once");

    let arm_base = 12 + 8 * 20;
    let default_pc = arm_base + 4 * 20;
    let mut max_compares = 0usize;
    for (i, &k) in keys.iter().enumerate() {
        let (target, compares) = dispatch_and_depth(&g, k);
        assert_eq!(target, arm_base + 4 * i, "key {k}");
        max_compares = max_compares.max(compares);
    }
    for miss in [i32::MIN, -1_000_000, -1, 1, 50_000, i32::MAX] {
        let (target, compares) = dispatch_and_depth(&g, miss);
        assert_eq!(target, default_pc, "miss key {miss}");
        max_compares = max_compares.max(compares);
    }
    assert!(
        max_compares <= 6,
        "balanced search tree depth must stay O(log2 N) (~5-6 compares max), got {max_compares}"
    );
}

#[test]
fn sparse_switch_threshold_engages_at_six_live_cases() {
    let six = build_graph(&lookupswitch_body(&[
        1, 1000, 5000, 90_000, 300_000, 900_000,
    ]));
    assert!(
        six.nodes.iter().any(|n| matches!(n.op, Op::Cmp(CmpOp::Lt))),
        "6 sparse cases must use search tree"
    );
    let five = build_graph(&lookupswitch_body(&[1, 1000, 5000, 90_000, 300_000]));
    assert!(
        !five
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Cmp(CmpOp::Lt))),
        "5 sparse cases must keep linear chain"
    );
}

#[test]
fn dispatcher_splice_filter_skips_loop_callees_and_keeps_leaf_intrinsics() {
    // Root with 16 invokestatic sites: sites at pc=0 (loop-bearing callee) and
    // pc=3 (leaf callee, e.g. String.length-sized).
    let mut root = Vec::new();
    for _ in 0..16 {
        root.extend_from_slice(&[0xb8, 0x00, 0x01]); // invokestatic #1
    }
    root.push(0xb1); // return
    let code_len = root.len();

    // Callee A (loop-bearing): iconst_0, istore_0, iinc 0 1, goto -3, return
    let loop_callee: [u8; 9] = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xff, 0xfd, 0xb1];
    // Callee B (leaf, no loop): return
    let leaf_callee: [u8; 1] = [0xb1];

    let mut combined = root.clone();
    let loop_base = combined.len();
    combined.extend_from_slice(&loop_callee);
    let leaf_base = combined.len();
    combined.extend_from_slice(&leaf_callee);
    combined.extend_from_slice(&[0, 0]);

    let make_site = |base: usize, len: usize, key: &str| IrInlineSite {
        base,
        code_len: len,
        num_args: 0,
        max_locals: 1,
        returns_value: false,
        receiver_is_arg0: false,
        arg_local_slots: Vec::new(),
        method_key: key.to_string(),
        class_id: 1,
    };

    let mut builder = IrBuilder::new(0, 1);
    let mut invoke_info = HashMap::new();
    for i in 0..16 {
        invoke_info.insert(i * 3, (0x1000 + i, 0, b'V'));
    }
    builder.set_invoke_info(invoke_info);
    builder.set_inline_sites(HashMap::from([
        (
            0usize,
            make_site(loop_base, loop_callee.len(), "OpCostRows.throwCatch:(I)V"),
        ),
        (
            3usize,
            make_site(leaf_base, leaf_callee.len(), "java/lang/String.length:()I"),
        ),
    ]));

    let verified = cratonvm_reader::verified_code(&combined[..code_len]).unwrap();
    super::sparse_switch::prepare_build_inline_sites(&mut builder, &combined, code_len, &verified);

    assert!(
        !builder.inline_sites.contains_key(&0),
        "loop-bearing callee in >=16-invoke dispatcher must not splice"
    );
    assert!(
        builder.inline_sites.contains_key(&3),
        "small leaf callee must still splice"
    );
}

#[test]
fn c2_upgrade_admits_immediate_throw_block_new_and_rejects_escaping_new() {
    // Throw-only new: iconst_0; ifeq +10; new #1; dup; aload_0; invokespecial #2; athrow; return
    let throw_only: [u8; 15] = [
        0x03, 0x99, 0x00, 0x0b, // 0..4: iconst_0; ifeq 14
        0xbb, 0x00, 0x01, // 4..7: new #1
        0x59, // 7: dup
        0x2a, // 8: aload_0
        0xb7, 0x00, 0x02, // 9..12: invokespecial #2
        0xbf, // 12: athrow
        0xb1, // 13: return (target 14 -> adjust offset below)
        0x00,
    ];
    let mut throw_code = throw_only;
    throw_code[3] = 0x0a; // pc=1 + 10 = 11 -> fix: 1 + 12 = 13 (return)
    throw_code[3] = 0x0c;
    let throw_len = 14;
    assert!(
        crate::c2_upgrade_would_engage(
            &throw_code,
            throw_len,
            "(Ljava/lang/String;)V",
            true,
            true,
            true
        ),
        "throw-only `new` block must pass c2_upgrade_would_engage"
    );

    // Escaping new: new #1; dup; invokespecial #2; areturn
    let escaping: [u8; 10] = [0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0xb0, 0x00, 0x00];
    assert!(
        !crate::c2_upgrade_would_engage(&escaping, 8, "()Ljava/lang/Object;", true, true, true),
        "escaping `new` without athrow must still be refused"
    );
}
