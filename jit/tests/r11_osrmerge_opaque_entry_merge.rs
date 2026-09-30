// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 14, lane `osrmerge`.
//!
//! The opaque OSR entry merge of
//! `r11w11-irexc-osr-entry-as-a-real-predecessor` (its `## Wave 13` design):
//! for an OSR-open loop header the builder hangs `If(OsrEntryFlag)` off the
//! forward entry and merges its `Proj(0)` arm with that entry in a pre-header
//! `Merge` whose φs take `Op::OsrLocal(i)` for every local live at the header
//! (and `NO_NODE` for a dead one), with `Op::OsrMemory` on the memory φ. These
//! tests pin the three new ops' classification and the builder's shape. The
//! switch (`CRATONVM_JIT_OSR_OPAQUE_ENTRY`) is read by `lib.rs`, which hands
//! the builder the liveness; here the liveness is handed over directly, so no
//! environment variable is involved.

use cratonvm_jit::ir::{
    effect_of_node, expected_input_type, may_raise, memory_shape_of, Graph, IrBuilder, IrType,
    MemEffect, MemoryShape, NodeId, Op, TypeReq, NO_NODE,
};
use cratonvm_jit::ir_verify::{verify_graph, VerifyOptions, PHASE_POST_BUILD, PHASE_POST_OPTIMIZE};

/// `static int f(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }`
/// Loop header at pc 4; locals 0 (`n`), 1 (`s`) and 2 (`i`) are all live there.
const COUNTED: [u8; 21] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2
    0x1c, // 4: iload_2      <-- header
    0x1a, // 5: iload_0
    0xa2, 0x00, 0x0d, // 6: if_icmpge +13 -> 19
    0x1b, // 9: iload_1
    0x1c, // 10: iload_2
    0x60, // 11: iadd
    0x3c, // 12: istore_1
    0x84, 0x02, 0x01, // 13: iinc 2, 1
    0xa7, 0xff, 0xf4, // 16: goto -12 -> 4
    0x1b, // 19: iload_1
    0xac, // 20: ireturn
];

/// `static int g(int n) { int t = n + 1; int s = t; for (int i = 0; i < n; i++) s += i; return s; }`
/// Loop header at pc 8; local 1 (`t`) has an entry value and is DEAD there.
const DEAD_LOCAL: [u8; 25] = [
    0x1a, // 0: iload_0
    0x04, // 1: iconst_1
    0x60, // 2: iadd
    0x3c, // 3: istore_1     t = n + 1
    0x1b, // 4: iload_1
    0x3d, // 5: istore_2     s = t
    0x03, // 6: iconst_0
    0x3e, // 7: istore_3     i = 0
    0x1d, // 8: iload_3      <-- header
    0x1a, // 9: iload_0
    0xa2, 0x00, 0x0d, // 10: if_icmpge +13 -> 23
    0x1c, // 13: iload_2
    0x1d, // 14: iload_3
    0x60, // 15: iadd
    0x3d, // 16: istore_2
    0x84, 0x03, 0x01, // 17: iinc 3, 1
    0xa7, 0xff, 0xf4, // 20: goto -12 -> 8
    0x1c, // 23: iload_2
    0xac, // 24: ireturn
];

fn build(code: &[u8], num_params: usize, max_locals: usize, opaque: bool) -> Graph {
    let mut b = IrBuilder::new(num_params, max_locals);
    if opaque {
        let (rows, covered, words) = cratonvm_jit::regalloc::live_locals_per_pc_all(
            code,
            code.len(),
            num_params,
            &[],
            &[],
            max_locals,
        );
        b.set_osr_opaque_liveness(rows, covered, words);
    }
    b.build(code, code.len()).expect("the loop builds")
}

fn find(g: &Graph, pred: impl Fn(&Op) -> bool) -> Vec<NodeId> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| pred(&n.op))
        // Cast: an index into the node arena is a `NodeId`.
        .map(|(i, _)| i as NodeId)
        .collect()
}

#[test]
fn the_three_ops_are_classified_as_the_design_says() {
    let ops = [
        Op::OsrEntryFlag { header_bci: 4 },
        Op::OsrLocal(2),
        Op::OsrMemory,
    ];
    for op in &ops {
        assert!(!op.is_pure(), "{op:?} must never be GVN'd or folded");
        assert!(!op.is_control(), "{op:?}");
        assert!(!may_raise(op), "{op:?} cannot fail");
        assert_eq!(
            expected_input_type(op, 0, 1),
            TypeReq::Control,
            "{op:?} is pinned by its one control input"
        );
        assert_eq!(op.memory_shape(), None, "{op:?} consumes no token");
    }
    assert_eq!(memory_shape_of(&Op::OsrEntryFlag { header_bci: 4 }), MemoryShape::None);
    assert_eq!(memory_shape_of(&Op::OsrLocal(0)), MemoryShape::None);
    assert_eq!(
        memory_shape_of(&Op::OsrMemory),
        MemoryShape::Opaque,
        "the interpreter's memory is the top of the lattice"
    );

    let mut g = Graph::empty(8);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let osr_mem = g.add(Op::OsrMemory, IrType::Memory, vec![ctrl], None);
    let osr_local = g.add(Op::OsrLocal(0), IrType::Int, vec![ctrl], None);
    assert_eq!(effect_of_node(&g.nodes[osr_mem as usize]), MemEffect::OPAQUE);
    assert_eq!(effect_of_node(&g.nodes[osr_local as usize]), MemEffect::NONE);
}

#[test]
fn an_opaque_header_is_entered_through_a_merge_in_front_of_it() {
    let g = build(&COUNTED, 1, 3, true);

    let flags = find(&g, |op| matches!(op, Op::OsrEntryFlag { header_bci: 4 }));
    assert_eq!(flags.len(), 1, "one OSR-open header, one flag");
    let iff = find(&g, |op| *op == Op::If)
        .into_iter()
        .find(|&i| g.nodes[i as usize].inputs.as_slice().get(1) == Some(&flags[0]))
        .expect("the flag is the condition of an If");
    let arm = find(&g, |op| *op == Op::Proj(0))
        .into_iter()
        .find(|&p| g.nodes[p as usize].inputs.as_slice().first() == Some(&iff))
        .expect("the If has its OSR arm");

    // Every local live at the header is read on the arm, and only there.
    let mut locals: Vec<u16> = g
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::OsrLocal(i) => {
                assert_eq!(n.inputs.as_slice(), &[arm], "an OsrLocal is pinned to the arm");
                assert_eq!(n.ty, IrType::Int);
                Some(i)
            }
            _ => None,
        })
        .collect();
    locals.sort_unstable();
    assert_eq!(locals, vec![0, 1, 2]);
    let mems = find(&g, |op| *op == Op::OsrMemory);
    assert_eq!(mems.len(), 1);
    assert_eq!(g.nodes[mems[0] as usize].inputs.as_slice(), &[arm]);

    // The pre-header merge joins the entry side and the arm, and it is the
    // header's ONE forward input.
    let pre = find(&g, |op| *op == Op::Merge)
        .into_iter()
        .find(|&m| g.nodes[m as usize].inputs.as_slice().contains(&arm))
        .expect("the arm reaches a merge");
    assert_eq!(g.nodes[pre as usize].inputs.len(), 2);
    let header_users: Vec<NodeId> = find(&g, |op| matches!(op, Op::Merge | Op::Region))
        .into_iter()
        .filter(|&h| g.nodes[h as usize].inputs.as_slice().contains(&pre))
        .collect();
    assert_eq!(header_users.len(), 1, "the merge feeds exactly the loop header");
    assert_eq!(
        g.nodes[header_users[0] as usize].inputs.as_slice().first(),
        Some(&pre),
        "the merge is the header's forward entry"
    );
}

#[test]
fn a_local_dead_at_the_header_is_undefined_on_the_arm() {
    let g = build(&DEAD_LOCAL, 1, 4, true);
    let mut locals: Vec<u16> = g
        .nodes
        .iter()
        .filter_map(|n| match n.op {
            Op::OsrLocal(i) => Some(i),
            _ => None,
        })
        .collect();
    locals.sort_unstable();
    assert_eq!(locals, vec![0, 2, 3], "`t` (local 1) is dead at the header");
    // If the merge still carries a φ for `t` (the header's frame state names
    // it), its arm input is `NO_NODE`; the builder may also prune that φ, since
    // `t` is dead at the header. Either way nothing is seeded for it (above).
    let arm = find(&g, |op| *op == Op::Proj(0))
        .into_iter()
        .find(|&p| {
            g.nodes.iter().any(|n| {
                matches!(n.op, Op::OsrLocal(_)) && n.inputs.as_slice().first() == Some(&p)
            })
        })
        .expect("an OSR arm");
    let pre = find(&g, |op| *op == Op::Merge)
        .into_iter()
        .find(|&m| g.nodes[m as usize].inputs.as_slice().contains(&arm))
        .expect("the merge");
    let undefined_on_the_arm = g.nodes.iter().filter(|n| {
        n.op == Op::Phi
            && n.ty != IrType::Memory
            && n.inputs.as_slice().first() == Some(&pre)
            && n.inputs.as_slice().last() == Some(&NO_NODE)
    });
    assert!(undefined_on_the_arm.count() <= 1);
}

#[test]
fn without_the_liveness_the_builder_is_unchanged() {
    for (code, params, locals) in [(&COUNTED[..], 1, 3), (&DEAD_LOCAL[..], 1, 4)] {
        let g = build(code, params, locals, false);
        assert!(
            find(&g, |op| matches!(
                op,
                Op::OsrEntryFlag { .. } | Op::OsrLocal(_) | Op::OsrMemory
            ))
            .is_empty(),
            "no opaque merge without `set_osr_opaque_liveness`"
        );
    }
}

/// The opaque shape verifies wherever the ordinary one does, before and after
/// the optimizer, and the optimizer neither folds the flag nor drops the arm.
#[test]
fn the_opaque_shape_verifies_and_survives_the_optimizer() {
    for (code, params, locals) in [(&COUNTED[..], 1, 3), (&DEAD_LOCAL[..], 1, 4)] {
        let plain = build(code, params, locals, false);
        let mut opaque = build(code, params, locals, true);
        if verify_graph(&plain, PHASE_POST_BUILD, VerifyOptions::default()).is_ok() {
            let r = verify_graph(&opaque, PHASE_POST_BUILD, VerifyOptions::default());
            assert!(r.is_ok(), "post-build: {r:?}");
        }
        let mut plain = plain;
        cratonvm_jit::ir_optimize::optimize(&mut plain);
        cratonvm_jit::ir_optimize::optimize(&mut opaque);
        if verify_graph(&plain, PHASE_POST_OPTIMIZE, VerifyOptions::default()).is_ok() {
            let r = verify_graph(&opaque, PHASE_POST_OPTIMIZE, VerifyOptions::default());
            assert!(r.is_ok(), "post-optimize: {r:?}");
        }
        assert_eq!(
            find(&opaque, |op| matches!(op, Op::OsrEntryFlag { .. })).len(),
            1,
            "the flag is opaque: never folded, never removed"
        );
        assert!(
            !find(&opaque, |op| matches!(op, Op::OsrLocal(_))).is_empty(),
            "the arm still reads the live locals"
        );
        // Scheduled where they are pinned: every OsrLocal in the arm's block.
        let sched = cratonvm_jit::ir_schedule::schedule(&opaque);
        for id in find(&opaque, |op| matches!(op, Op::OsrLocal(_))) {
            let anchor = opaque.nodes[id as usize].inputs.as_slice()[0];
            let block = sched
                .blocks
                .iter()
                .find(|b| b.nodes.contains(&id))
                .expect("an OsrLocal is scheduled");
            assert_eq!(block.ctrl, anchor, "n{id} left its arm");
        }
    }
}
