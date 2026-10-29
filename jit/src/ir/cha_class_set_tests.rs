// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 5 (lane PROXY, `CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE`):
//! the class-set guard of an `invokeinterface` site, through the builder and
//! the optimizer.

use super::cha_class_set::{admitted_class_set, CHA_CLASS_SET_MAX};
use super::*;

fn info() -> usize {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "I",
        method_name: "index",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 2,
        declaring_class_id: 7,
        owner_class_id: 0,
    }));
    info as *const JitInvokeInfo as usize
}

fn count(g: &Graph, f: impl Fn(&Op) -> bool) -> usize {
    g.nodes.iter().filter(|n| f(&n.op)).count()
}

/// `static int f(I p) { return p.index(); }` -- `aload_0; invokeinterface #3
/// 1 0; ireturn` -- with the inherited body `iconst_5; ireturn` (declared by
/// class 7) spliced at pc 1 for a receiver of exactly class 9, guarded, and
/// registered with `set` when given. Returns the graph and the call's info.
fn build(set: Option<&[u32]>) -> (Graph, usize) {
    let code: [u8; 11] = [
        0x2a, 0xb9, 0x00, 0x03, 0x01, 0x00, 0xac, 0x08, 0xac, 0x00, 0x00,
    ];
    let info = info();
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[IrType::Ref]);
    b.set_invoke_info(HashMap::from([(1usize, (info, 1usize, b'I'))]));
    let mut tables = IrInlineTables::default();
    tables.sites.insert(
        1,
        IrInlineSite {
            base: 7,
            code_len: 2,
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: true,
            receiver_is_arg0: true,
            method_key: "A.index:()I".to_string(),
            class_id: 7,
        },
    );
    b.apply_inline_tables(tables);
    b.add_exact_receiver_site(1, 9);
    if let Some(set) = set {
        assert!(b.add_cha_class_set_site(1, 9, set), "the set is admitted");
    }
    b.add_guarded_receiver_site(1);
    (b.build(&code, 7).expect("builds"), info)
}

fn verifies_through_optimize(mut g: Graph) -> Graph {
    crate::ir_verify::verify_graph(&g, "post-build", crate::ir_verify::VerifyOptions::structural())
        .expect("the built graph verifies");
    crate::ir_optimize::optimize(&mut g);
    crate::ir_verify::verify_graph(
        &g,
        "post-optimize",
        crate::ir_verify::VerifyOptions::structural(),
    )
    .expect("the optimized graph verifies");
    let schedule = crate::ir_schedule::schedule(&g);
    assert_eq!(
        schedule.misplaced_data_nodes(&g),
        Vec::<NodeId>::new(),
        "every read is dominated by its definition"
    );
    g
}

/// The set site compares the receiver's class word with every listed class
/// (one compare each, joined by `Or`), never one exact class, keeps the
/// interface call as its miss edge, splices the shared body on the hit edge,
/// and survives the optimizer with the call still there.
#[test]
fn j27_proxy_a_class_set_site_tests_every_listed_class_once() {
    let (g, info) = build(Some(&[9, 11, 12]));
    assert_eq!(count(&g, |op| matches!(op, Op::ExactClassIs { .. })), 0, "no one-class test");
    assert_eq!(count(&g, |op| *op == Op::ClassWordOf), 1, "one class word read");
    assert_eq!(count(&g, |op| *op == Op::Cmp(CmpOp::Eq)), 3, "one compare per class");
    assert_eq!(count(&g, |op| *op == Op::Or), 2, "the compares are joined");
    for id in [9, 11, 12] {
        assert!(g.nodes.iter().any(|n| n.op == Op::Const(id)), "class {id} is compared");
    }
    assert_eq!(
        count(&g, |op| matches!(op, Op::Call { info_ptr } if *info_ptr == info)),
        1,
        "the interface call is the miss edge"
    );
    assert!(g.nodes.iter().any(|n| n.op == Op::Const(5)), "the body is spliced");
    let g = verifies_through_optimize(g);
    assert_eq!(count(&g, |op| matches!(op, Op::Call { .. })), 1, "the optimizer keeps the miss edge");
}

/// The control: the same site without a set is the one-class guarded splice
/// of round 13 (one test of class 9), so the set is what changed the test.
#[test]
fn j27_proxy_without_a_set_the_site_tests_its_one_class() {
    let (g, _) = build(None);
    assert_eq!(count(&g, |op| *op == Op::Or), 0);
    let one_class = count(&g, |op| *op == Op::ExactClassIs { class_id: 9 })
        + count(&g, |op| *op == Op::Cmp(CmpOp::Eq));
    assert_eq!(one_class, 1, "one test of the profile's class");
}

/// The admitted set puts the profile's class first, drops `0`, ids outside
/// the `int` range and repeats, cuts at the cap, and refuses a list that does
/// not name the profile's class or names no other class.
#[test]
fn j27_proxy_admitted_class_set_shapes_the_list() {
    assert_eq!(admitted_class_set(&[11, 9, 0, 11, 12], 9), Some(vec![9, 11, 12]));
    assert_eq!(admitted_class_set(&[11, 12], 9), None, "the profile's class must be listed");
    assert_eq!(admitted_class_set(&[9], 9), None, "one class is the one-class splice");
    assert_eq!(admitted_class_set(&[9, 0x8000_0001], 9), None, "a lambda-proxy range id");
    assert_eq!(admitted_class_set(&[0, 5], 0), None, "the unknown class");
    let long: Vec<u32> = (1..=40).collect();
    let cut = admitted_class_set(&long, 30).expect("admitted");
    assert_eq!(cut.len(), CHA_CLASS_SET_MAX);
    assert_eq!(cut[0], 30);
}

/// A refused set registers nothing: the caller then adds no guarded row.
#[test]
fn j27_proxy_a_refused_set_registers_nothing() {
    let mut b = IrBuilder::new(1, 1);
    assert!(!b.add_cha_class_set_site(1, 9, &[11, 12]));
    assert!(!b.cha_guarded_site(1));
    assert!(b.add_cha_class_set_site(1, 9, &[9, 11]));
    assert!(b.cha_guarded_site(1));
}

/// The switch reads opt-in: unset is off, `1` is on.
#[test]
fn j27_proxy_switch_is_opt_in() {
    const KEY: &str = "CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE";
    cratonvm_types::flags::with_thread_overrides(&[(KEY, None)], || {
        assert!(!ir_cha_interface_class_set_enabled());
    });
    cratonvm_types::flags::with_thread_overrides(&[(KEY, Some("1"))], || {
        assert!(ir_cha_interface_class_set_enabled());
    });
}
