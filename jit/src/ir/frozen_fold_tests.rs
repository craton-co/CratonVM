// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Zero-fault CHA + `@Stable` + `IndyRootRef` heap-specialization IR tests
//! (`docs/feature-designs/jit-ir-optimizations.md`).

use super::*;
use crate::frozen_facts::{
    CHA_FIELD_DEP_PREFIX, CompileClasses, FrozenFacts, FrozenFactsState, FrozenMiss, FrozenObject,
    FrozenValue,
};
use std::collections::HashMap;
use std::sync::Arc;

fn count(g: &Graph, pred: impl Fn(&Op) -> bool) -> usize {
    g.nodes.iter().filter(|n| pred(&n.op)).count()
}

fn returned(g: &Graph) -> &Op {
    let ret = g.nodes.iter().find(|n| n.op == Op::Return).expect("returns");
    &g.nodes[ret.inputs[1] as usize].op
}

/// `static int f() { return ROOT.next.sides; }` folds both the intermediate
/// reference (`IndyRootRef`) and terminal primitive (`Const(42)`) with zero
/// `Op::Load`, zero `ClassInitTest`, and zero non-null `Op::Const` of `Ref`.
#[test]
fn static_final_chain_folds_to_const_without_page_guards_or_raw_ref_consts() {
    let code = [0xb2, 0, 1, 0xb4, 0, 2, 0xb4, 0, 3, 0xac, 0, 0];
    let mut b = IrBuilder::new(0, 2);
    b.set_static_field_info(HashMap::from([(0usize, (9u32, 0usize, b'L', false))]));
    b.set_static_final_exact_classes(HashMap::from([((9, 0), 10)]));
    b.set_field_info(HashMap::from([(3usize, (1usize, b'L')), (6usize, (0usize, b'I'))]));
    b.set_frozen_facts(Arc::new(FrozenFacts {
        roots: HashMap::from([((9, 0), 0)]),
        objects: vec![
            FrozenObject {
                addr: 0x1000,
                slot_addr: 0x1000,
                class_id: 10,
                fields: vec![None, Some(FrozenValue::Ref(1))],
                field_cha_owners: vec![None, Some(Arc::from("pkg/Root"))],
                ..Default::default()
            },
            FrozenObject {
                addr: 0x2000,
                slot_addr: 0x2000,
                class_id: 11,
                fields: vec![Some(FrozenValue::Int(42))],
                field_cha_owners: vec![Some(Arc::from("pkg/Child"))],
                ..Default::default()
            },
        ],
        ..Default::default()
    }));
    let g = b.build(&code, 10).expect("builds");
    assert_eq!(returned(&g), &Op::Const(42));
    assert_eq!(count(&g, |op| matches!(op, Op::Load(_))), 0);
    assert_eq!(count(&g, |op| matches!(op, Op::ClassInitTest { .. })), 0);
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| n.ty == IrType::Ref && matches!(n.op, Op::Const(c) if c != 0))
            .count(),
        0,
        "non-null Ref constants must never use Op::Const (GC oop-map invariant)"
    );
    assert_eq!(
        g.frozen_cha_deps,
        vec![
            format!("{CHA_FIELD_DEP_PREFIX}pkg/Root"),
            format!("{CHA_FIELD_DEP_PREFIX}pkg/Child"),
        ]
    );
    assert!(g.frozen_misses.is_empty());
    crate::ir_verify::verify_graph_at_phase(&g, "cha-static-chain").expect("verifies");
}

#[test]
fn a_missed_method_is_handed_back_once_its_fact_is_available() {
    let state = FrozenFactsState::default();
    let name =
        |m: &str| -> crate::frozen_facts::MethodName { ("FrozenFoldT".into(), m.into(), "()V".into()) };
    state.note_misses(name("a"), &[FrozenMiss::Object(0x10), FrozenMiss::Root(7, 1)]);
    state.note_misses(name("b"), &[FrozenMiss::Object(0x20)]);
    assert!(state.any_wanted());
    assert!(state.take_wanting(true, |_| false).is_empty());
    assert_eq!(
        state.take_wanting(true, |m| *m == FrozenMiss::Root(7, 1)),
        vec![name("a")]
    );
    assert!(state.take_wanting(true, |m| *m == FrozenMiss::Root(7, 1)).is_empty());
    state.note_misses(name("b"), &[]);
    assert!(state.take_wanting(true, |_| true).is_empty());
    state.note_misses(name("c"), &[FrozenMiss::Object(0x30)]);
    for _ in 0..8 {
        assert!(state.take_wanting(false, |_| false).is_empty());
    }
    assert!(state.any_wanted());
    for _ in 0..4 {
        assert!(state.take_wanting(true, |_| false).is_empty());
    }
    assert!(!state.any_wanted());
}

fn build_on_receiver(withdrawn_at: Option<u32>, store_between: bool) -> Graph {
    let mut code = vec![0x2a, 0xb4, 0, 1, 0xb4, 0, 2];
    let mut fields = HashMap::from([(1usize, (1usize, b'L')), (4usize, (0usize, b'I'))]);
    if store_between {
        fields.insert(code.len() + 2, (2usize, b'I'));
        code.extend([0x2a, 0x04, 0xb5, 0, 4]);
    }
    fields.insert(code.len() + 1, (0usize, b'I'));
    code.extend([0x2a, 0xb4, 0, 3, 0x60, 0xac]);
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.graph.receiver_param = Some(0);
    b.set_field_info(fields);
    b.set_frozen_facts(Arc::new(FrozenFacts {
        roots: HashMap::new(),
        objects: vec![
            FrozenObject {
                addr: 0x1000,
                slot_addr: 0x1000,
                class_id: 10,
                fields: vec![Some(FrozenValue::Int(7)), Some(FrozenValue::Ref(1)), None],
                field_cha_owners: vec![
                    Some(Arc::from("T")),
                    Some(Arc::from("T")),
                    None,
                ],
                ..Default::default()
            },
            FrozenObject {
                addr: 0x2000,
                slot_addr: 0x2000,
                class_id: 11,
                fields: vec![Some(FrozenValue::Int(42))],
                field_cha_owners: vec![Some(Arc::from("Child"))],
                ..Default::default()
            },
        ],
        ..Default::default()
    }));
    let despec = Arc::new(crate::deopt::DespecRegistry::new());
    if let Some(bci) = withdrawn_at {
        despec.insert("T.f:()I", bci, crate::deopt::SPECULATION_ID_ANY);
    }
    let seen = HashMap::from([("T".to_string(), 0x1000usize)]);
    b.set_frozen_receivers(Some(&seen), Some(despec), "T.f:()I".into());
    b.build(&code, code.len()).expect("builds")
}

fn indy_identity_guards(g: &Graph, slot_addr: usize) -> usize {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::Guard { .. }))
        .filter(|n| {
            let test = &g.nodes[n.inputs[1] as usize];
            test.op == Op::Cmp(CmpOp::Eq)
                && g.nodes[test.inputs[1] as usize].op == Op::IndyRootRef { slot_addr }
        })
        .count()
}

#[test]
fn receiver_singleton_folds_behind_single_indy_root_identity_guard() {
    let g = build_on_receiver(None, false);
    let ret = g.nodes.iter().find(|n| n.op == Op::Return).expect("returns");
    let sum = &g.nodes[ret.inputs[1] as usize];
    let constant = |i: NodeId| match g.nodes[i as usize].op {
        Op::Const(v) => v,
        ref op => panic!("not folded: {op:?}"),
    };
    let total = match sum.op {
        Op::Const(v) => v,
        _ => sum.inputs.iter().map(|&i| constant(i)).sum(),
    };
    assert_eq!(total, 49);
    assert_eq!(indy_identity_guards(&g, 0x1000), 1);
    assert_eq!(g.frozen_identity_guards, 1);
    assert_eq!(count(&g, |op| matches!(op, Op::ClassInitTest { .. })), 0);
    crate::ir_verify::verify_graph_at_phase(&g, "frozen-identity").expect("verifies");

    // A store to an unrelated mutable field does not emit a second identity guard.
    let g_store = build_on_receiver(None, true);
    assert_eq!(indy_identity_guards(&g_store, 0x1000), 1);
    assert_eq!(count(&g_store, |op| matches!(op, Op::ClassInitTest { .. })), 0);
}

#[test]
fn a_withdrawn_identity_guard_is_not_planted_again() {
    let g = build_on_receiver(Some(1), false);
    assert_eq!(count(&g, |op| matches!(op, Op::Load(MemKind::Ref))), 1);
    assert!(g.frozen_failed_classes.contains(&10));
    assert_eq!(indy_identity_guards(&g, 0x1000), 1);
    assert_eq!(count(&g, |op| matches!(op, Op::Load(MemKind::Int))), 1);
}

#[test]
fn compile_classes_and_splice_site_objects_restore_nested_tls_on_drop() {
    let outer_facts = FrozenFacts {
        roots: HashMap::new(),
        objects: vec![FrozenObject {
            addr: 0x1000,
            slot_addr: 0x1000,
            class_id: 10,
            fields: vec![Some(FrozenValue::Int(1))],
            ..Default::default()
        }],
        ..Default::default()
    };
    let inner_facts = FrozenFacts {
        roots: HashMap::new(),
        objects: vec![FrozenObject {
            addr: 0x2000,
            slot_addr: 0x2000,
            class_id: 20,
            fields: vec![Some(FrozenValue::Int(2))],
            ..Default::default()
        }],
        ..Default::default()
    };
    let _outer_cc = CompileClasses::of_facts(Some(&outer_facts));
    let _outer_site = SpliceSiteObjects::set(vec![Some(0x1000)]);
    assert!(crate::frozen_facts::compile_has_class(10));
    assert_eq!(splice_site_objects(), vec![Some(0x1000)]);
    {
        let _inner_cc = CompileClasses::of_facts(Some(&inner_facts));
        let _inner_site = SpliceSiteObjects::set(vec![Some(0x2000)]);
        assert!(!crate::frozen_facts::compile_has_class(10));
        assert!(crate::frozen_facts::compile_has_class(20));
        assert_eq!(splice_site_objects(), vec![Some(0x2000)]);
    }
    // Outer compile's planned classes and site objects must survive nested drop!
    assert!(crate::frozen_facts::compile_has_class(10));
    assert!(!crate::frozen_facts::compile_has_class(20));
    assert_eq!(splice_site_objects(), vec![Some(0x1000)]);
}

/// Regression for Finding #2: a branch-local `frozen_prove_site_receiver`
/// inside `if (flag)` must NOT insert the parameter node into global
/// `exact_receiver_nodes`, which would elide the receiver class check after the
/// `if` join on the unguarded path!
#[test]
fn branch_scoped_site_identity_guard_does_not_leak_exact_class_past_join() {
    // static int f(Object x, int flag) {
    //   if (flag == 0) goto join;
    //   return_or_ignore(x.g()); // bci 5: site-guarded to class 10, spliced
    //   join (bci 9):
    //   return x.g();            // bci 10: NO site receiver row -> MUST stay a Call!
    // }
    // Bytecode:
    // 0: iload_1 (0x1b)
    // 1: ifeq +8 -> bci 9 (0x99, 0x00, 0x08)
    // 4: aload_0 (0x2a)
    // 5: invokevirtual #1 (0xb6, 0x00, 0x01)
    // 8: pop (0x57)
    // 9: aload_0 (0x2a)
    // 10: invokevirtual #1 (0xb6, 0x00, 0x01)
    // 13: ireturn (0xac)
    // Spliced body at 14..19: aload_0 (0x2a), getfield #2 (0xb4, 0, 2), ireturn (0xac)
    let code = [
        0x1b, 0x99, 0x00, 0x08, 0x2a, 0xb6, 0x00, 0x01, 0x57, 0x2a, 0xb6, 0x00, 0x01, 0xac, 0x2a,
        0xb4, 0x00, 0x02, 0xac,
    ];
    let mut b = IrBuilder::new(2, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    b.graph.scope_method_key = "S.f:(Ljava/lang/Object;I)I".to_string();
    b.set_invoke_info(HashMap::from([
        (5usize, (0usize, 1usize, b'I')),
        (10usize, (0usize, 1usize, b'I')),
    ]));
    let site_body = IrInlineSite {
        base: 14,
        code_len: 5,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: true,
        method_key: "T.g:()I".to_string(),
        class_id: 10,
    };
    let mut tables = IrInlineTables::default();
    tables.sites.insert(5, site_body.clone());
    tables.sites.insert(10, site_body);
    tables.field_info.insert(15, (0, b'I'));
    b.apply_inline_tables(tables);
    b.add_exact_receiver_site(5, 10);
    b.add_exact_receiver_site(10, 10);
    b.set_frozen_facts(Arc::new(FrozenFacts {
        roots: HashMap::new(),
        objects: vec![FrozenObject {
            addr: 0x1000,
            slot_addr: 0x1000,
            class_id: 10,
            fields: vec![Some(FrozenValue::Int(99))],
            field_cha_owners: vec![Some(Arc::from("T"))],
            ..Default::default()
        }],
        ..Default::default()
    }));
    // Only bci 5 has a site-dominant receiver; bci 10 after the join has NONE!
    let seen = HashMap::from([("S.f:(Ljava/lang/Object;I)I@5".to_string(), 0x1000usize)]);
    let despec = Arc::new(crate::deopt::DespecRegistry::new());
    b.set_frozen_receivers(
        Some(&seen),
        Some(despec),
        "S.f:(Ljava/lang/Object;I)I".into(),
    );
    let g = b.build(&code, 14).expect("builds");
    assert_eq!(indy_identity_guards(&g, 0x1000), 1, "bci 5 spliced behind identity guard");
    assert_eq!(
        count(&g, |op| matches!(op, Op::Call { .. })),
        1,
        "bci 10 after join MUST remain a Call instead of eliding class check on Param(0)!"
    );
    crate::ir_verify::verify_graph_at_phase(&g, "branch-scoped-site").expect("verifies");
}

#[test]
fn wide_floating_and_stable_array_elements_fold_cleanly() {
    let facts = Arc::new(FrozenFacts {
        roots: HashMap::from([((9, 0), 0), ((9, 1), 1), ((9, 2), 2), ((9, 3), 3)]),
        objects: vec![
            FrozenObject {
                addr: 0x1000,
                slot_addr: 0x1000,
                class_id: 0,
                array_len: Some(3),
                elements: vec![
                    Some(FrozenValue::Int(11)),
                    Some(FrozenValue::Int(22)),
                    Some(FrozenValue::Int(33)),
                ],
                elem_cha_owner: Some(Arc::from("pkg/Holder")),
                ..Default::default()
            },
            FrozenObject {
                addr: 0x2000,
                slot_addr: 0x2000,
                class_id: 0,
                array_len: Some(1024),
                ..Default::default()
            },
            FrozenObject {
                addr: 0x3000,
                slot_addr: 0x3000,
                class_id: 0,
                array_len: Some(1),
                elements: vec![Some(FrozenValue::Ref(4))],
                elem_cha_owner: Some(Arc::from("pkg/Holder")),
                ..Default::default()
            },
            FrozenObject {
                addr: 0x4000,
                slot_addr: 0x4000,
                class_id: 10,
                fields: vec![
                    Some(FrozenValue::Long(0x1234_5678_9abc_def0)),
                    Some(FrozenValue::FloatBits(1.5f32.to_bits())),
                    Some(FrozenValue::DoubleBits(2.25f64.to_bits())),
                ],
                ..Default::default()
            },
            FrozenObject {
                addr: 0x5000,
                slot_addr: 0x5000,
                class_id: 11,
                fields: vec![Some(FrozenValue::Int(77))],
                ..Default::default()
            },
        ],
        ..Default::default()
    });

    // `stable_int_arr[1]` -> 22, zero ArrayLoad, zero ClassInitTest.
    let mut b = IrBuilder::new(0, 2);
    b.set_static_field_info(HashMap::from([(0usize, (9u32, 0usize, b'[', false))]));
    b.set_static_final_exact_classes(HashMap::from([(
        (9, 0),
        crate::ir::STATIC_FINAL_OBJECT_FILED,
    )]));
    b.set_frozen_facts(Arc::clone(&facts));
    let g = b.build(&[0xb2, 0, 0, 0x04, 0x2e, 0xac], 6).expect("iaload folds");
    assert_eq!(count(&g, |op| matches!(op, Op::ArrayLoad(_))), 0);
    assert_eq!(count(&g, |op| matches!(op, Op::ClassInitTest { .. })), 0);
    assert!(matches!(returned(&g), Op::Const(22)));

    // `stable_obj_arr[0].x` -> 77, zero ArrayLoad AND zero Load!
    let mut b = IrBuilder::new(0, 2);
    b.set_static_field_info(HashMap::from([(0usize, (9u32, 2usize, b'[', false))]));
    b.set_static_final_exact_classes(HashMap::from([(
        (9, 2),
        crate::ir::STATIC_FINAL_OBJECT_FILED,
    )]));
    b.set_field_info(HashMap::from([(5usize, (0usize, b'I'))]));
    b.set_frozen_facts(Arc::clone(&facts));
    let g = b
        .build(&[0xb2, 0, 0, 0x03, 0x32, 0xb4, 0, 1, 0xac], 9)
        .expect("aaload + getfield fold");
    assert_eq!(count(&g, |op| matches!(op, Op::ArrayLoad(_))), 0);
    assert_eq!(count(&g, |op| matches!(op, Op::Load(_))), 0);
    assert!(matches!(returned(&g), Op::Const(77)));

    // `arraylength` of non-@Stable array -> 1024.
    let mut b = IrBuilder::new(0, 2);
    b.set_static_field_info(HashMap::from([(0usize, (9u32, 1usize, b'[', false))]));
    b.set_static_final_exact_classes(HashMap::from([(
        (9, 1),
        crate::ir::STATIC_FINAL_OBJECT_FILED,
    )]));
    b.set_frozen_facts(Arc::clone(&facts));
    let g = b.build(&[0xb2, 0, 0, 0xbe, 0xac], 5).expect("arraylength folds");
    assert_eq!(count(&g, |op| matches!(op, Op::ArrayLength)), 0);
    assert!(matches!(returned(&g), Op::Const(1024)));

    // Long / Float / Double fields fold cleanly.
    for (idx, tag, ret_op) in [(0usize, b'J', 0xadu8), (1, b'F', 0xae), (2, b'D', 0xaf)] {
        let mut b = IrBuilder::new(0, 2);
        b.set_wide_field_gates(true, true);
        b.set_static_field_info(HashMap::from([(0usize, (9u32, 3usize, b'L', false))]));
        b.set_static_final_exact_classes(HashMap::from([((9, 3), 10)]));
        b.set_field_info(HashMap::from([(3usize, (idx, tag))]));
        b.set_frozen_facts(Arc::clone(&facts));
        let g = b.build(&[0xb2, 0, 0, 0xb4, 0, 3, ret_op], 7).expect("wide/fp folds");
        assert_eq!(count(&g, |op| matches!(op, Op::Load(_))), 0);
    }
}
