// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 11, lane `hashmap`.
//!
//! `CratonBench.hashMapPutGet` never got an optimizing body:
//! `IrBuilder::build refused at ir.rs:11491 (bytecode pc 85)`. Pc 85 is the
//! `putfield loadFactor` of `HashMap.<init>()`, spliced onto the caller's own
//! `new HashMap`. The splice store rule admitted only stores into objects the
//! SPLICE allocated, because a trap inside a splice re-executes the whole
//! `invoke` and would commit such a store twice. The receiver of a spliced
//! constructor is the caller's allocation, so every method that splices a
//! field-writing constructor onto its own `new` was refused outright.
//!
//! The builder now admits a store into an object the splice did not allocate
//! when nothing after it, up to the end of the outermost splice, can trap.
//! These tests pin both halves: the constructor shape builds, and a trapping
//! instruction, a branch, or a store through an unproven base after the
//! committed store still refuses.

use cratonvm_jit::ir::{IrBuilder, IrInlineSite, MemKind, Op};
use std::collections::{HashMap, HashSet};

/// The constructor's body is appended at `base`; the caller's
/// `invokespecial` is at pc 4.
fn ctor_site(base: usize, code_len: usize) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: false,
        receiver_is_arg0: true,
        method_key: "T.<init>:()V".to_string(),
        class_id: 0,
    }
}

/// Caller: `new T; dup; invokespecial T.<init>()V; pop; <tail>`.
const CALLER_HEAD: [u8; 8] = [
    0xbb, 0x00, 0x01, // 0: new #1
    0x59, // 3: dup
    0xb7, 0x00, 0x02, // 4: invokespecial #2 (spliced)
    0x57, // 7: pop
];

/// Build `CALLER_HEAD ++ tail` with `body` spliced at pc 4. `field_pcs` are
/// callee-relative `putfield`/`getfield` pcs, all naming int field 0.
fn build(tail: &[u8], body: &[u8], field_pcs: &[usize]) -> Option<cratonvm_jit::ir::Graph> {
    let mut combined = CALLER_HEAD.to_vec();
    combined.extend_from_slice(tail);
    let code_len = combined.len();
    let base = code_len;
    combined.extend_from_slice(body);
    let mut b = IrBuilder::new(0, 1);
    b.set_new_info(HashMap::from([(0usize, (7u32, 1usize))]), HashSet::new());
    b.set_field_info(
        field_pcs
            .iter()
            .map(|&p| (base + p, (0usize, b'I')))
            .collect(),
    );
    b.set_inline_sites(HashMap::from([(4usize, ctor_site(base, body.len()))]));
    b.build(&combined, code_len)
}

/// `this.f = 5; return` — the `HashMap.<init>` shape (`this.loadFactor =
/// 0.75f`), with an int so the test does not depend on the FP gate.
const CTOR_ONE_STORE: [u8; 6] = [
    0x2a, // 0: aload_0
    0x08, // 1: iconst_5
    0xb5, 0x00, 0x03, // 2: putfield #3
    0xb1, // 5: return
];

#[test]
fn a_constructor_spliced_onto_the_callers_new_builds() {
    let graph = build(&[0xb1], &CTOR_ONE_STORE, &[2])
        .expect("a trailing store into the caller's fresh object must build");
    let store = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Store(MemKind::Int)))
        .expect("the constructor's store is in the graph");
    let base = store.inputs.as_slice()[2];
    assert!(
        matches!(graph.nodes[base as usize].op, Op::New { .. }),
        "the store writes the caller's allocation, not a copy",
    );
}

#[test]
fn stores_and_reads_through_the_committed_base_may_follow() {
    // this.f = 5; this.f = this.f + 1; return
    let body = [
        0x2a, // 0: aload_0
        0x08, // 1: iconst_5
        0xb5, 0x00, 0x03, // 2: putfield
        0x2a, // 5: aload_0
        0x2a, // 6: aload_0
        0xb4, 0x00, 0x03, // 7: getfield
        0x04, // 10: iconst_1
        0x60, // 11: iadd
        0xb5, 0x00, 0x03, // 12: putfield
        0xb1, // 15: return
    ];
    assert!(
        build(&[0xb1], &body, &[2, 7, 12]).is_some(),
        "a base an earlier committed store wrote through cannot be null, so \
         nothing after the first store can trap",
    );
}

#[test]
fn a_trap_after_the_committed_store_refuses() {
    // this.f = 5; 1 / 1; return — the division's zero test is a trap that
    // would re-execute the invoke after the store.
    let body = [
        0x2a, // 0: aload_0
        0x08, // 1: iconst_5
        0xb5, 0x00, 0x03, // 2: putfield
        0x04, // 5: iconst_1
        0x04, // 6: iconst_1
        0x6c, // 7: idiv
        0x57, // 8: pop
        0xb1, // 9: return
    ];
    assert!(
        build_static(&body, &[2]).is_none(),
        "a trap after a committed caller-visible store must refuse the method",
    );
}

#[test]
fn a_branch_after_the_committed_store_refuses() {
    // this.f = 5; if (0 == 0) {} return — any branch after the store is
    // refused, because a loop body can precede it in walk order.
    let body = [
        0x2a, // 0: aload_0
        0x08, // 1: iconst_5
        0xb5, 0x00, 0x03, // 2: putfield
        0x03, // 5: iconst_0
        0x99, 0x00, 0x03, // 6: ifeq -> 9
        0xb1, // 9: return
    ];
    assert!(build_static(&body, &[2]).is_none());
}

/// `static f(T a)` with `body`, spliced at pc 1 of `aload_0; invokestatic f;
/// return`. Its base is a PARAMETER: not splice-allocated and not a
/// constructor receiver this method allocated (`splice_store_is_ctor_receiver`,
/// which admits a store into the caller's own `new` whatever follows it), so
/// only the committed-store rule can admit its stores.
fn build_static(body: &[u8], field_pcs: &[usize]) -> Option<cratonvm_jit::ir::Graph> {
    let caller = [0x2a, 0xb8, 0x00, 0x01, 0xb1];
    let mut combined = caller.to_vec();
    let base = combined.len();
    combined.extend_from_slice(body);
    let mut b = IrBuilder::new(1, 1);
    b.set_param_types(&[cratonvm_jit::ir::IrType::Ref]);
    b.set_field_info(
        field_pcs
            .iter()
            .map(|&p| (base + p, (0usize, b'I')))
            .collect(),
    );
    b.set_inline_sites(HashMap::from([(
        1usize,
        IrInlineSite {
            base,
            code_len: body.len(),
            num_args: 1,
            max_locals: 1,
            arg_local_slots: vec![0],
            returns_value: false,
            receiver_is_arg0: false,
            method_key: "T.f:(LT;)V".to_string(),
            class_id: 0,
        },
    )]));
    b.build(&combined, base)
}

#[test]
fn the_static_vehicle_admits_a_lone_committed_store() {
    // The control for the two refusals above: the same store with nothing
    // after it builds, so they refuse for the trap / branch alone.
    assert!(build_static(&CTOR_ONE_STORE, &[2]).is_some());
}

#[test]
fn a_store_through_an_unproven_base_after_the_commit_refuses() {
    // Static `f(T a, T b) { a.f = 1; b.f = 2; }` spliced at pc 2 of
    // `aload_0; aload_1; invokestatic f; return`: `b` may be null, and its
    // null check would re-execute the call after `a.f` was written.
    let caller = [0x2a, 0x2b, 0xb8, 0x00, 0x01, 0xb1];
    let body = [
        0x2a, // 0: aload_0
        0x04, // 1: iconst_1
        0xb5, 0x00, 0x03, // 2: putfield
        0x2b, // 5: aload_1
        0x05, // 6: iconst_2
        0xb5, 0x00, 0x03, // 7: putfield
        0xb1, // 10: return
    ];
    let site = |base: usize| IrInlineSite {
        base,
        code_len: body.len(),
        num_args: 2,
        max_locals: 2,
        arg_local_slots: vec![0, 1],
        returns_value: false,
        receiver_is_arg0: false,
        method_key: "T.f:(LT;LT;)V".to_string(),
        class_id: 0,
    };
    let run = |second_base_local: u8| {
        let mut body = body;
        body[5] = second_base_local;
        let mut combined = caller.to_vec();
        let base = combined.len();
        combined.extend_from_slice(&body);
        let mut b = IrBuilder::new(2, 2);
        b.set_param_types(&[cratonvm_jit::ir::IrType::Ref, cratonvm_jit::ir::IrType::Ref]);
        b.set_field_info(HashMap::from([
            (base + 2, (0usize, b'I')),
            (base + 7, (0usize, b'I')),
        ]));
        b.set_inline_sites(HashMap::from([(2usize, site(base))]));
        b.build(&combined, base)
    };
    assert!(
        run(0x2b).is_none(),
        "a second store through a different, possibly-null base must refuse",
    );
    assert!(
        run(0x2a).is_some(),
        "a second store through the base the first store already wrote through builds",
    );
}

#[test]
fn the_caller_may_trap_again_once_the_splice_has_returned() {
    // After the splice: `1 / 1` in the CALLER. Its trap resumes after the
    // invoke, so the committed store is never repeated.
    let tail = [
        0x04, // iconst_1
        0x04, // iconst_1
        0x6c, // idiv
        0x57, // pop
        0xb1, // return
    ];
    assert!(
        build(&tail, &CTOR_ONE_STORE, &[2]).is_some(),
        "the commit is scoped to the splice that made it",
    );
}
